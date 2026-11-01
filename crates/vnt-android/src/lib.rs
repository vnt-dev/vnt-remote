//! JNI controller. Registry handles never expose pointers, and every session has an epoch.
mod audio;
mod clipboard;
mod decoder;
mod media;
mod queue;
use jni::{
    objects::{GlobalRef, JByteBuffer, JClass, JObject, JString, JValue},
    sys::{jint, jlong},
    JNIEnv, JavaVM,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vnt_client::{
    platform::Outputs, ClientHandle, ClientSessionInfo, ConnectParams, InputArg, ScreenInputTarget,
};
use vnt_link::{LinkConfig, LinkTarget, ServerLinks};
use vnt_proto::{control::ControlMessage, pb, EventSink};
static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static REGISTRY: OnceLock<Mutex<HashMap<i64, Arc<Controller>>>> = OnceLock::new();
static NEXT: AtomicI64 = AtomicI64::new(1);
fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("vnt-io")
            .enable_all()
            .build()
            .expect("network runtime")
    })
}
fn registry() -> &'static Mutex<HashMap<i64, Arc<Controller>>> {
    REGISTRY.get_or_init(Default::default)
}
struct Callback {
    vm: JavaVM,
    object: Option<GlobalRef>,
    closed: AtomicBool,
}
impl EventSink for Callback {
    fn emit_json(&self, event: &str, payload: String) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut env) = self.vm.attach_current_thread() {
            let _ = env.with_local_frame(8, |env| -> jni::errors::Result<()> {
                let event = env.new_string(event)?;
                let payload = env.new_string(payload)?;
                env.call_method(
                    self.object.as_ref().unwrap().as_obj(),
                    "onNativeEvent",
                    "(Ljava/lang/String;Ljava/lang/String;)V",
                    &[
                        JValue::Object(event.as_ref()),
                        JValue::Object(payload.as_ref()),
                    ],
                )?;
                Ok(())
            });
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
                log::error!("Android event callback threw");
            }
        }
    }
}
impl Drop for Callback {
    fn drop(&mut self) {
        if let Ok(_env) = self.vm.attach_current_thread() {
            drop(self.object.take());
        }
    }
}
struct ScopedEvents {
    callback: Arc<Callback>,
    epoch: Arc<AtomicU64>,
    expected: u64,
    internal: mpsc::UnboundedSender<Internal>,
}
impl EventSink for ScopedEvents {
    fn emit_json(&self, event: &str, payload: String) {
        if self.epoch.load(Ordering::Acquire) != self.expected {
            return;
        }
        let mut value =
            serde_json::from_str::<Value>(&payload).unwrap_or_else(|_| json!({"message":payload}));
        value["epoch"] = self.expected.into();
        if event == "client://decoderFailed" {
            let _ = self.internal.send(Internal::DecoderFailed(
                self.expected,
                value["name"].as_str().unwrap_or("").into(),
                value["reason"].as_str().unwrap_or("解码失败").into(),
            ));
        }
        if event == "client://closed" {
            let _ = self.internal.send(Internal::Closed(self.expected));
        }
        self.callback.emit_json(event, value.to_string());
    }
}
struct Controller {
    tx: mpsc::Sender<Command>,
    cancel: CancellationToken,
    epoch: Arc<AtomicU64>,
    callback: Arc<Callback>,
    renderer: Arc<media::Renderer>,
    audio: Arc<audio::Audio>,
}
#[derive(Clone, Deserialize)]
struct Request {
    params: ConnectParams,
    decoders: Vec<media::DecoderChoice>,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum Command {
    Connect {
        request: Box<Request>,
    },
    Disconnect,
    Configure {
        profiles: Vec<LinkConfig>,
    },
    Foreground {
        visible: bool,
    },
    SelectScreen {
        epoch: u64,
        #[serde(rename = "streamId")]
        stream_id: u32,
    },
    Clipboard {
        epoch: u64,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        paste: bool,
    },
    ResetInput {
        epoch: u64,
    },
    Keyframe {
        epoch: u64,
    },
    Sas {
        epoch: u64,
    },
    #[serde(skip)]
    Input {
        epoch: u64,
        target: ScreenInputTarget,
        events: Vec<InputArg>,
    },
}
enum Internal {
    Connected(u64, Box<Result<(ClientHandle, ClientSessionInfo), String>>),
    Closed(u64),
    DecoderFailed(u64, String, String),
}
impl Controller {
    fn send(&self, command: Command) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            return Err("控制器已关闭".into());
        }
        if self.tx.try_send(command).is_err() {
            self.cancel.cancel();
            return Err("命令队列已满，会话已结束以释放远端按键".into());
        }
        Ok(())
    }
}
fn choices(request: &mut Request) -> Result<(), String> {
    if request.decoders.is_empty() || request.decoders.len() > 64 {
        return Err("未发现可用的视频解码器".into());
    }
    request.decoders.retain(|d| {
        matches!(d.codec.as_str(), "h264" | "hevc" | "av1")
            && (d.hardware || d.codec == "h264")
            && !d.name.is_empty()
            && d.name.len() < 256
    });
    request.params.codecs = ["av1", "hevc", "h264"]
        .iter()
        .filter(|c| request.decoders.iter().any(|d| &d.codec == *c))
        .map(|c| c.to_string())
        .collect();
    let rate = request
        .params
        .codecs
        .iter()
        .filter_map(|codec| {
            request
                .decoders
                .iter()
                .filter(|d| &d.codec == codec)
                .map(|d| d.max_fps)
                .max()
        })
        .min()
        .unwrap_or(30);
    request.params.fps = request.params.fps.min(rate.max(1));
    request.params.hardware_codecs = request
        .params
        .codecs
        .iter()
        .filter(|c| {
            request
                .decoders
                .iter()
                .any(|d| &d.codec == *c && d.hardware)
        })
        .cloned()
        .collect();
    if request.params.codecs.is_empty() {
        return Err("没有兼容的硬件或 H.264 解码器".into());
    }
    if request.params.hardware_codecs.is_empty() {
        request.params.fps = request.params.fps.min(30);
        request.params.target_width = request.params.target_width.min(1280);
        request.params.target_height = request.params.target_height.min(720);
    }
    request.params.validate()
}
fn start_connect(
    owner: &Arc<Controller>,
    links: Arc<ServerLinks>,
    clipboard: Arc<clipboard::TextClipboard>,
    mut request: Request,
    internal: mpsc::UnboundedSender<Internal>,
    pending: CancellationToken,
) -> Result<Request, String> {
    choices(&mut request)?;
    let epoch = owner.epoch.fetch_add(1, Ordering::AcqRel) + 1;
    let events: Arc<dyn EventSink> = Arc::new(ScopedEvents {
        callback: owner.callback.clone(),
        epoch: owner.epoch.clone(),
        expected: epoch,
        internal: internal.clone(),
    });
    events.emit_json(
        "client://connecting",
        json!({"softwareFallback":request.params.hardware_codecs.is_empty()}).to_string(),
    );
    owner
        .renderer
        .begin(epoch, request.decoders.clone(), events.clone());
    let renderer = owner.renderer.clone();
    let audio = owner.audio.clone();
    let work = request.clone();
    runtime().spawn(async move {
        let connect=async {
            let mut params=work.params;let prepared=if params.transport==vnt_proto::Transport::Vnt {
                let target=LinkTarget{config_id:params.server_config_id.clone().ok_or("请选择 VNT 网络")?,network_code:params.server_network_code.clone(),node:params.server_node_id.clone().ok_or("请选择在线节点")?};
                let link=links.select(&target).await.map_err(|e|format!("{e:#}"))?;
                let prepared=vnt_net::overlay::prepare(link.peer(&target.node).await.map_err(|e|format!("{e:#}"))?).await.map_err(|e|e.to_string())?;
                params.host=prepared.peer.ip().to_string();params.port=prepared.peer.port();Some(prepared)
            }else{None};
            let outputs=Outputs{client_name:"VNTRemote Android".into(),media:Some(Arc::new(media::Sink{renderer,epoch})),audio:Arc::new(audio::Factory(audio))};
            vnt_client::connect_with(events,params,clipboard,outputs,prepared).await
        };
        let result=tokio::select!{biased;_ = pending.cancelled()=>return,result=tokio::time::timeout(Duration::from_secs(30),connect)=>result.unwrap_or_else(|_|Err("连接或握手超时".into()))};
        let _=internal.send(Internal::Connected(epoch,Box::new(result)));
    });
    Ok(request)
}
async fn actor(
    owner: Arc<Controller>,
    links: Arc<ServerLinks>,
    mut commands: mpsc::Receiver<Command>,
) {
    let clipboard = clipboard::TextClipboard::new();
    let (internal, mut replies) = mpsc::unbounded_channel();
    let mut session: Option<(ClientHandle, ClientSessionInfo)> = None;
    let mut pending = CancellationToken::new();
    let mut request: Option<Request> = None;
    let mut visible = true;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_report = Instant::now();
    let mut last_key = Instant::now() - Duration::from_secs(1);
    let mut throttles = HashMap::<u32, (u32, bool)>::new();
    loop {
        tokio::select! {biased;
            _ = owner.cancel.cancelled()=>break,
            Some(reply)=replies.recv()=>match reply{
                Internal::Connected(epoch,result)=>{
                    if owner.epoch.load(Ordering::Acquire)!=epoch{drop(result);continue;}
                    match *result {Ok((handle,info))=>{let mut v=serde_json::to_value(&info).unwrap();v["epoch"]=epoch.into();v["clipboardEnabled"]=(info.clipboard_enabled && info.capabilities & vnt_proto::CAP_TEXT != 0).into();session=Some((handle,info));owner.callback.emit_json("client://connected",v.to_string());},Err(reason)=>{owner.renderer.end();owner.callback.emit_json("client://closed",json!({"epoch":epoch,"reason":reason}).to_string());}}
                }
                Internal::Closed(epoch)=>{if owner.epoch.load(Ordering::Acquire)==epoch{session.take();owner.renderer.end();owner.audio.foreground(false);}}
                Internal::DecoderFailed(epoch,name,reason)=>{
                    if owner.epoch.load(Ordering::Acquire)!=epoch{continue;}
                    if let Some(mut retry)=request.clone(){let before=retry.decoders.len();retry.decoders.retain(|d|d.name!=name);
                        if before==retry.decoders.len(){retry.decoders.retain(|d|d.codec!="av1"&&d.codec!="hevc");}
                        pending.cancel();session.take();owner.epoch.fetch_add(1,Ordering::AcqRel);pending=CancellationToken::new();throttles.clear();
                        owner.callback.emit_json("client://fallback",json!({"reason":reason}).to_string());
                        match start_connect(&owner,links.clone(),clipboard.clone(),retry,internal.clone(),pending.clone()){Ok(r)=>request=Some(r),Err(reason)=>{owner.renderer.end();owner.callback.emit_json("client://closed",json!({"epoch":owner.epoch.load(Ordering::Acquire),"reason":reason}).to_string());}}
                    }
                }
            },
            Some(command)=commands.recv()=>{
                let result:Result<(),String>=async {match command {
                    Command::Connect{request:next}=>{pending.cancel();owner.epoch.fetch_add(1,Ordering::AcqRel);session.take();owner.renderer.end();owner.audio.foreground(visible);pending=CancellationToken::new();throttles.clear();request=Some(start_connect(&owner,links.clone(),clipboard.clone(),*next,internal.clone(),pending.clone())?);}
                    Command::Disconnect=>{pending.cancel();let epoch=owner.epoch.fetch_add(1,Ordering::AcqRel);session.take();request=None;owner.renderer.end();owner.audio.foreground(false);clipboard.clear();owner.callback.emit_json("client://closed",json!({"epoch":epoch,"reason":"已断开"}).to_string());}
                    Command::Configure{profiles}=>{links.configure(profiles).await?;}
                    Command::Foreground{visible:next}=>{
                        visible=next;owner.renderer.foreground(visible);owner.audio.foreground(visible);clipboard.foreground(visible);throttles.clear();
                        if let Some((handle,_))=&session{handle.reset_screen_input()?;}
                    }
                    Command::Input{epoch,target,events}=>{let (handle,info)=current(&owner,&session,epoch)?;
                        if !visible||info.view_only||!handle.control_available()||!handle.screen_input_enabled.load(Ordering::Acquire){return Ok(());}
                        if let Err(e)=vnt_client::send_screen_input_events(&handle.control_tx,events,&handle.screens,Some(target)).await{handle.end(&e);return Err(e);}
                    }
                    Command::ResetInput{epoch}=>{current(&owner,&session,epoch)?.0.reset_screen_input()?;}
                    Command::Keyframe{epoch}=>{let (handle,_)=current(&owner,&session,epoch)?;request_keyframe(handle)?;}
                    Command::SelectScreen{epoch,stream_id}=>{
                        let (handle,_)=current(&owner,&session,epoch)?;let state=handle.screens.lock().unwrap().clone();if !state.screens.iter().any(|s|s.stream_id==stream_id&&s.available){return Err("屏幕不可用".into());}
                        handle.reset_screen_input()?;handle.screen_input_enabled.store(false,Ordering::Release);
                        handle.control_tx.try_send(ControlMessage::ScreenCommand(pb::ScreenCommand{action:0,stream_id,revision:state.revision})).map_err(|_|"切屏通道拥塞")?;
                        owner.renderer.reset();throttles.clear();
                    }
                    Command::Clipboard{epoch,text,paste}=>{
                        let (handle,info)=current(&owner,&session,epoch)?;if !visible||!info.clipboard_enabled||info.capabilities&vnt_proto::CAP_TEXT==0||info.view_only||!handle.control_available(){return Err("当前会话不能同步剪贴板".into());}
                        if let Some(text)=text {if text.len()>vnt_proto::control::MAX_CLIPBOARD_TEXT{return Err("文本超过协议上限".into());}
                            if paste {handle.paste_text(text)?;} else {handle.control_tx.try_send(ControlMessage::ClipboardText(pb::ClipboardText{text})).map_err(|_|"剪贴板通道拥塞")?;}
                        }else if let Some(text)=clipboard.latest(){owner.callback.emit_json("client://clipboard",json!({"epoch":epoch,"text":text}).to_string());}else{return Err("远端尚未发布文本，请先在远端复制".into());}
                    }
                    Command::Sas{epoch}=>{let (handle,info)=current(&owner,&session,epoch)?;if visible&&!info.view_only{handle.send_sas()?;}}
                }Ok(())}.await;
                if let Err(reason)=result{owner.callback.emit_json("client://error",json!({"reason":reason,"epoch":owner.epoch.load(Ordering::Acquire)}).to_string());}
            },
            incoming=links.next_incoming()=>{drop(incoming);},
            _ = tick.tick()=>{
                if let Some((handle,_))=&session {
                    if visible&&last_key.elapsed()>=Duration::from_millis(500)&&owner.renderer.keyframe_needed(){let _=request_keyframe(handle);last_key=Instant::now();}
                    let state=handle.screens.lock().unwrap().clone();
                    if handle.control_available(){for screen in &state.screens{let background=!visible||screen.stream_id!=state.active_stream_id;let value=(screen.generation,background);if throttles.get(&screen.stream_id)!=Some(&value)&&handle.control_tx.try_send(vnt_proto::screen::wrap(screen.stream_id,screen.generation,ControlMessage::ScreenThrottle(pb::ScreenThrottle{background}))).is_ok(){throttles.insert(screen.stream_id,value);}}}else{throttles.clear();}
                }
                if last_report.elapsed()>=Duration::from_secs(1){last_report=Instant::now();let mut stats=owner.renderer.stats();stats["epoch"]=owner.epoch.load(Ordering::Acquire).into();owner.callback.emit_json("client://renderStats",stats.to_string());owner.callback.emit_json("client://networks",serde_json::to_string(&links.snapshot().await).unwrap());}
            }
        }
    }
    pending.cancel();
    session.take();
    owner.renderer.end();
    owner.audio.foreground(false);
    links.stop().await;
}
fn current<'a>(
    owner: &Controller,
    session: &'a Option<(ClientHandle, ClientSessionInfo)>,
    epoch: u64,
) -> Result<(&'a ClientHandle, &'a ClientSessionInfo), String> {
    if owner.epoch.load(Ordering::Acquire) != epoch {
        return Err("会话已变化".into());
    }
    session
        .as_ref()
        .map(|(h, i)| (h, i))
        .ok_or_else(|| "尚未连接".into())
}
fn request_keyframe(handle: &ClientHandle) -> Result<(), String> {
    let state = handle.screens.lock().unwrap();
    let s = state
        .screens
        .iter()
        .find(|s| s.stream_id == state.active_stream_id)
        .ok_or("未选择屏幕")?;
    handle
        .control_tx
        .try_send(vnt_proto::screen::wrap(
            s.stream_id,
            s.generation,
            ControlMessage::RequestKeyframe,
        ))
        .map_err(|_| "控制通道拥塞".into())
}
fn lookup(handle: i64) -> Result<Arc<Controller>, String> {
    registry()
        .lock()
        .unwrap()
        .get(&handle)
        .cloned()
        .ok_or_else(|| "控制器句柄已失效".into())
}
fn guarded<T: Default>(env: &mut JNIEnv, f: impl FnOnce(&mut JNIEnv) -> Result<T, String>) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(env))) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let _ = env.throw_new("java/lang/IllegalStateException", e);
            T::default()
        }
        Err(_) => {
            let _ = env.throw_new("java/lang/IllegalStateException", "原生控制器发生错误");
            T::default()
        }
    }
}
#[no_mangle]
pub extern "system" fn Java_com_rustvnt_remote_NativeBridge_nativeCreate(
    mut env: JNIEnv,
    _: JClass,
    callback: JObject,
    files: JString,
    sdk: jint,
) -> jlong {
    guarded(&mut env, |env| {
        #[cfg(target_os = "android")]
        android_logger::init_once(
            android_logger::Config::default()
                .with_tag("VNTRemote")
                .with_max_level(log::LevelFilter::Info),
        );
        let files: String = env.get_string(&files).map_err(|e| e.to_string())?.into();
        let links = Arc::new(
            ServerLinks::new(&Path::new(&files).join("vnt-device-id"))
                .map_err(|e| e.to_string())?,
        );
        let callback = Arc::new(Callback {
            vm: env.get_java_vm().map_err(|e| e.to_string())?,
            object: Some(env.new_global_ref(callback).map_err(|e| e.to_string())?),
            closed: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel(128);
        let owner = Arc::new(Controller {
            tx,
            cancel: CancellationToken::new(),
            epoch: Arc::new(AtomicU64::new(0)),
            callback,
            renderer: media::Renderer::new(sdk as u32),
            audio: audio::Audio::new(),
        });
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        registry().lock().unwrap().insert(id, owner.clone());
        runtime().spawn(actor(owner, links, rx));
        Ok(id)
    })
}
#[no_mangle]
pub extern "system" fn Java_com_rustvnt_remote_NativeBridge_nativeCommand(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    command: JString,
) {
    guarded(&mut env, |env| {
        let text: String = env.get_string(&command).map_err(|e| e.to_string())?.into();
        if text.len() > 5 * 1024 * 1024 {
            return Err("命令过大".into());
        }
        let command = serde_json::from_str(&text).map_err(|e| format!("命令参数无效: {e}"))?;
        if let Command::Configure { profiles } = &command {
            vnt_link::validate_profiles(profiles)?;
        }
        lookup(handle)?.send(command)
    })
}
#[no_mangle]
pub extern "system" fn Java_com_rustvnt_remote_NativeBridge_nativeDestroy(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
) {
    guarded(&mut env, |_| {
        if let Some(owner) = registry().lock().unwrap().remove(&handle) {
            owner.callback.closed.store(true, Ordering::Release);
            owner.epoch.fetch_add(1, Ordering::AcqRel);
            owner.cancel.cancel();
        }
        Ok(())
    })
}
#[no_mangle]
pub extern "system" fn Java_com_rustvnt_remote_NativeBridge_nativeSurface(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    surface: JObject,
) {
    guarded(&mut env, |env| {
        let owner = lookup(handle)?;
        #[cfg(target_os = "android")]
        {
            let window = if surface.is_null() {
                None
            } else {
                let pointer = unsafe {
                    ndk_sys::ANativeWindow_fromSurface(
                        env.get_raw().cast(),
                        surface.as_raw().cast(),
                    )
                };
                if pointer.is_null() {
                    return Err("Surface 已失效".into());
                }
                Some(media::Window(pointer))
            };
            owner.renderer.surface(window);
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (env, surface, owner);
        }
        Ok(())
    })
}
#[no_mangle]
pub extern "system" fn Java_com_rustvnt_remote_NativeBridge_nativeReadAudio(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    buffer: JByteBuffer,
) -> jint {
    guarded(&mut env, |env| {
        let owner = lookup(handle)?;
        let ptr = env
            .get_direct_buffer_address(&buffer)
            .map_err(|e| e.to_string())?;
        let len = env
            .get_direct_buffer_capacity(&buffer)
            .map_err(|e| e.to_string())?;
        if len > 65536 {
            return Err("音频缓冲区过大".into());
        }
        let out = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        Ok(owner.audio.read(out) as i32)
    })
}
#[no_mangle]
pub extern "system" fn Java_com_rustvnt_remote_NativeBridge_nativeInput(
    mut env: JNIEnv,
    _: JClass,
    handle: jlong,
    epoch: jlong,
    stream: jint,
    generation: jint,
    revision: jlong,
    buffer: JByteBuffer,
    count: jint,
) {
    guarded(&mut env, |env| {
        let owner = lookup(handle)?;
        if !(1..=64).contains(&count) {
            return Err("输入批次无效".into());
        }
        let ptr = env
            .get_direct_buffer_address(&buffer)
            .map_err(|e| e.to_string())?;
        let len = env
            .get_direct_buffer_capacity(&buffer)
            .map_err(|e| e.to_string())?;
        if len < (count as usize) * 24 {
            return Err("输入缓冲区不足".into());
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, count as usize * 24) };
        let events = decode_input(bytes)?;
        owner.send(Command::Input {
            epoch: epoch as u64,
            target: ScreenInputTarget {
                stream_id: stream as u32,
                generation: generation as u32,
                revision: revision as u64,
            },
            events,
        })
    })
}
fn decode_input(bytes: &[u8]) -> Result<Vec<InputArg>, String> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(24) || bytes.len() > 64 * 24 {
        return Err("输入缓冲格式无效".into());
    }
    let mut events = Vec::with_capacity(bytes.len() / 24);
    for r in bytes.as_chunks::<24>().0 {
        let i = |n: usize| i32::from_le_bytes(r[n..n + 4].try_into().unwrap());
        let f = |n: usize| f32::from_le_bytes(r[n..n + 4].try_into().unwrap());
        let event = match i(0) {
            0 if f(16).is_finite()
                && f(20).is_finite()
                && (0.0..=1.0).contains(&f(16))
                && (0.0..=1.0).contains(&f(20)) =>
            {
                InputArg::MouseMove { x: f(16), y: f(20) }
            }
            1 if (1..=5).contains(&i(4)) && (0..=1).contains(&i(8)) => InputArg::MouseButton {
                button: i(4) as u8,
                down: i(8) != 0,
            },
            2 if f(16).is_finite()
                && f(20).is_finite()
                && f(16).abs() <= 100.0
                && f(20).abs() <= 100.0 =>
            {
                InputArg::Wheel {
                    dx: f(16),
                    dy: f(20),
                }
            }
            3 if (1..=255).contains(&i(4))
                && (0..=1).contains(&i(8))
                && (0..=1).contains(&i(12)) =>
            {
                InputArg::Key {
                    scancode: i(4) as u16,
                    extended: i(8) != 0,
                    down: i(12) != 0,
                }
            }
            _ => return Err("无效的键鼠输入".into()),
        };
        events.push(event);
    }
    Ok(events)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_handles_are_rejected() {
        assert!(lookup(i64::MAX).is_err());
    }
    #[test]
    fn validates_whole_input_batch_before_enqueuing() {
        let mut b = vec![0; 24];
        b[16..20].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(decode_input(&b).is_err());
        b[16..20].copy_from_slice(&0.5f32.to_le_bytes());
        assert!(decode_input(&b).is_ok());
        b.extend([0; 23]);
        assert!(decode_input(&b).is_err());
    }
    #[test]
    fn software_fallback_is_h264_only_and_bounded_to_720p30() {
        let mut request: Request = serde_json::from_value(json!({
            "params":{"host":"127.0.0.1","port":29875,"password":"","transport":"quic",
              "colorDepth":24,"showWallpaper":true,"dragFullWindows":false,"fecEnabled":true,
              "resolutionMode":"scale","targetWidth":1920,"targetHeight":1080,"fps":60,
              "bitrateKbps":8000,"codecs":["h264"],"hardwareCodecs":[],"clipboardEnabled":true,"audioEnabled":true},
            "decoders":[
              {"codec":"av1","name":"software-av1","hardware":false,"maxWidth":1920,"maxHeight":1080,"maxFps":60},
              {"codec":"h264","name":"software-avc","hardware":false,"maxWidth":1920,"maxHeight":1080,"maxFps":60}]
        })).unwrap();
        choices(&mut request).unwrap();
        assert_eq!(request.params.codecs, vec!["h264"]);
        assert!(request.params.hardware_codecs.is_empty());
        assert_eq!(
            (
                request.params.target_width,
                request.params.target_height,
                request.params.fps
            ),
            (1280, 720, 30)
        );
    }
}
