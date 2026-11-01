//! Clipboard: publish metadata on copy, serve bounded reads only on paste.
//! OLE callbacks never block the network reader. Source and target operations live
//! in separate maps, scoped to a locally unique connection key.
use crate::EventSink;
use pb::clipboard_request::Action;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc, Mutex, Weak,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vnt_clip::image::{ClipboardImage, PreparedImage};
use vnt_clip::native::{FileOperation, FileProvider, NativeClipboard};
use vnt_clip::source::{SourceOffer, SourceOperation};
use vnt_clip::ClipboardContent;
use vnt_proto::clipboard::ImageAssembly;
type ImagePublisher = Arc<
    dyn Fn(PreparedImage, u32, vnt_clip::native::ImageValidity) -> Result<(), String> + Send + Sync,
>;
struct IncomingImage {
    assembly: ImageAssembly,
    generation: u64,
    sequence: u32,
    epoch: u64,
}
type ImageJob = (u64, u32, vnt_clip::win::RawImage);
use vnt_proto::{control::ControlMessage, pb};

pub const INBOUND_QUEUE: usize = 16;
pub const OUT_QUEUE: usize = 8;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OPERATIONS: usize = 32;
pub type PasteWriter = Arc<dyn Fn(String,vnt_clip::native::ImageValidity)->Result<(),String> + Send + Sync>;
type ClipboardWriter = Arc<dyn Fn(ClipboardContent, Vec<PathBuf>) + Send + Sync>;
pub type NativePublisher = Arc<
    dyn Fn(Arc<pb::ClipboardOffer>, Arc<dyn FileProvider>, u32) -> Result<(), String> + Send + Sync,
>;
pub type InboundClipboard = ControlMessage;
fn allocate(counter: &AtomicU32) -> Result<u32, String> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
        .map_err(|_| "剪贴板标识已耗尽，请重新启动应用".into())
}
fn reason(text: &str) -> String {
    text.chars().take(250).collect()
}
fn request(offer: u32, op: u32, action: Action) -> pb::ClipboardRequest {
    pb::ClipboardRequest {
        offer_id: offer,
        operation_id: op,
        action: action as i32,
        ..Default::default()
    }
}
pub struct SessionSpec {
    pub capabilities: u32,
    pub session_id: u32,
    pub file_tx: mpsc::Sender<ControlMessage>,
    /// Same prioritized channel used for input/heartbeats; data replies use file_tx.
    pub control_tx: mpsc::Sender<ControlMessage>,
    pub in_rx: mpsc::Receiver<InboundClipboard>,
    pub session_cancel: CancellationToken,
    pub sink: Arc<dyn EventSink>,
    pub enabled: bool,
}
pub struct ClipboardRegistration {
    service: Arc<ClipboardService>,
    key: u64,
}
impl ClipboardRegistration {
    pub fn key(&self) -> u64 {
        self.key
    }
}
impl Drop for ClipboardRegistration {
    fn drop(&mut self) {
        if let Some(entry) = self
            .service
            .inner
            .sessions
            .lock()
            .unwrap()
            .remove(&self.key)
        {
            entry.shutdown("连接已断开");
        }
    }
}
struct Pending {
    expected: pb::ClipboardRequest,
    tx: std::sync::mpsc::SyncSender<Result<pb::ClipboardReply, String>>,
}
struct SourceTransfer {
    offer: Arc<SourceOffer>,
    operation: Arc<Mutex<SourceOperation>>,
    event_id: u32,
    progress: Progress,
}
struct Progress {
    started: Instant,
    last_emit: Instant,
    high_water: HashMap<u32, u64>,
    bytes: u64,
    index: usize,
}
impl Progress {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_emit: Instant::now() - Duration::from_secs(1),
            high_water: HashMap::new(),
            bytes: 0,
            index: 0,
        }
    }
    fn advance(&mut self, index: u32, end: u64) {
        self.index = index as usize;
        let old = self.high_water.entry(index).or_default();
        if end > *old {
            self.bytes += end - *old;
            *old = end;
        }
    }
}
struct SessionEntry {
    remote_paste: AtomicBool,
    last_paste: AtomicU32,
    paste_epoch: AtomicU64,
    paste_received: Mutex<HashMap<u32,(Instant,u64)>>,
    capabilities: u32,
    file_supported: AtomicBool,
    key: u64,
    session_id: u32,
    enabled: AtomicBool,
    closed: AtomicBool,
    file_tx: mpsc::Sender<ControlMessage>,
    control_tx: mpsc::Sender<ControlMessage>,
    cancel: CancellationToken,
    sink: Arc<dyn EventSink>,
    ids: Arc<AtomicU32>,
    requests: AtomicU32,
    source_offer: Mutex<Option<Arc<SourceOffer>>>,
    sources: Mutex<HashMap<u32, SourceTransfer>>,
    targets: Mutex<HashMap<u32, Weak<RemoteOperation>>>,
    pending: Mutex<HashMap<u32, Pending>>,
    target_offer: Mutex<Option<Weak<RemoteOffer>>>,
    image: Mutex<Option<IncomingImage>>,
    image_decode: Mutex<Option<IncomingImage>>,
    image_decoding: AtomicBool,
    epoch: AtomicU64,
    source_image: AtomicU32,
    cached_image: Mutex<Option<(pb::ClipboardImage, Arc<Vec<u8>>)>>,
    control_online: AtomicBool,
    file_online: AtomicBool,
    replies: Mutex<HashMap<u32, (pb::ClipboardRequest, pb::ClipboardReply)>>,
    jobs: Arc<tokio::sync::Semaphore>,
    inflight: Mutex<HashSet<u32>>,
    received_image: AtomicU32,
}
impl SessionEntry {
    fn active(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
            && !self.closed.load(Ordering::Acquire)
            && !self.cancel.is_cancelled()
    }
    fn control(&self, msg: ControlMessage) -> Result<(), String> {
        if !permits(&msg, self.capabilities) {
            return Err("对端未协商该剪贴板能力".into());
        }
        if self.closed.load(Ordering::Acquire) {
            return Err("连接已断开".into());
        }
        self.control_tx
            .try_send(msg)
            .map_err(|_| "剪贴板控制通道拥塞或已关闭".into())
    }
    fn queue_request(
        &self,
        mut value: pb::ClipboardRequest,
        ended: Option<&AtomicBool>,
    ) -> Result<std::sync::mpsc::Receiver<Result<pb::ClipboardReply, String>>, String> {
        if !self.active() {
            return Err("剪贴板同步已关闭或连接已断开".into());
        }
        if !self.file_online.load(Ordering::Acquire) {
            return Err("文件通道恢复中，请稍后重新粘贴".into());
        }
        value.request_id = allocate(&self.requests)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        {
            let mut pending = self.pending.lock().unwrap();
            // Check under the same lock used by cancellation to drain waiters.
            if !self.active() || ended.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Err("文件复制已取消或连接已断开".into());
            }
            if pending.len() >= 64 {
                return Err("剪贴板读取请求过多".into());
            }
            pending.insert(
                value.request_id,
                Pending {
                    expected: value.clone(),
                    tx,
                },
            );
        }
        if let Err(e) = self.control(ControlMessage::ClipboardRequest(value.clone())) {
            self.pending.lock().unwrap().remove(&value.request_id);
            return Err(e);
        }
        Ok(rx)
    }
    fn reply(&self, reply: pb::ClipboardReply) {
        let pending = self.pending.lock().unwrap().remove(&reply.request_id);
        if let Some(p) = pending {
            let expected = &p.expected;
            let valid = reply.offer_id == expected.offer_id
                && reply.operation_id == expected.operation_id
                && reply.item_index == expected.item_index
                && reply.offset == expected.offset
                && reply.data.len() <= expected.length as usize;
            let _ = p.tx.send(if !valid {
                Err("文件读取响应与请求不匹配".into())
            } else if !reply.reason.is_empty() {
                Err(reply.reason)
            } else {
                Ok(reply)
            });
        }
    }
    fn fail_pending(&self, operation: Option<u32>, why: &str) {
        let mut pending = self.pending.lock().unwrap();
        let keys: Vec<_> = pending
            .iter()
            .filter(|(_, p)| operation.is_none_or(|id| p.expected.operation_id == id))
            .map(|(key, _)| *key)
            .collect();
        for key in keys {
            if let Some(p) = pending.remove(&key) {
                let _ = p.tx.send(Err(why.into()));
            }
        }
    }
    fn retire_offer(&self) {
        if let Some(offer) = self
            .target_offer
            .lock()
            .unwrap()
            .take()
            .and_then(|w| w.upgrade())
        {
            offer.valid.store(false, Ordering::Release);
        }
    }
    fn cancel_source_image(&self) {
        let image_id = self.source_image.swap(0, Ordering::AcqRel);
        if image_id != 0 {
            let _ = self.control(ControlMessage::ClipboardImageCancel(
                pb::ClipboardImageCancel { image_id },
            ));
        }
    }
    fn stop_operations(&self, why: &str) {
        self.cancel_source_image();
        self.replies.lock().unwrap().clear();
        self.received_image.store(0, Ordering::Release);
        self.retire_offer();
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.paste_epoch.fetch_add(1,Ordering::AcqRel);
        self.image.lock().unwrap().take();
        self.image_decode.lock().unwrap().take();
        self.source_offer.lock().unwrap().take();
        self.fail_pending(None, why);
        let targets: Vec<_> = self
            .targets
            .lock()
            .unwrap()
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for target in targets {
            target.finish(false, why);
        }
        let sources = std::mem::take(&mut *self.sources.lock().unwrap());
        for (id, source) in sources {
            let mut end = request(source.offer.manifest.offer_id, id, Action::Abort);
            end.reason = reason(why);
            let _ = self.control(ControlMessage::ClipboardRequest(end));
            emit(
                self,
                source.event_id,
                "send",
                "cancelled",
                &source.offer.manifest,
                &source.progress,
                Some(why),
            );
        }
    }
    fn shutdown(&self, why: &str) {
        self.closed.store(true, Ordering::Release);
        self.stop_operations(why);
        self.cancel.cancel();
    }
}
type EnumerationJob = (u64, u32, Vec<PathBuf>, u32);
struct Inner {
    publication: Mutex<()>,
    image_publisher: Mutex<Option<ImagePublisher>>,
    image_job: Mutex<Option<ImageJob>>,
    image_encoding: AtomicBool,
    image_workers: Arc<tokio::sync::Semaphore>,
    runtime: Mutex<Option<tokio::runtime::Handle>>,
    enumeration: Mutex<Option<EnumerationJob>>,
    enumerating: AtomicBool,
    enabled: AtomicBool,
    sessions: Mutex<HashMap<u64, Arc<SessionEntry>>>,
    ids: Arc<AtomicU32>,
    generation: AtomicU64,
    writer: ClipboardWriter,
    paste_writer: Mutex<Option<PasteWriter>>,
    publisher: NativePublisher,
    remote_current: Mutex<Option<Weak<RemoteOffer>>>,
}
pub struct ClipboardService {
    inner: Arc<Inner>,
}
impl std::fmt::Debug for ClipboardService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardService").finish_non_exhaustive()
    }
}
impl ClipboardService {
    pub fn shared() -> Arc<Self> {
        Self::try_shared().expect("初始化 OLE 剪贴板失败")
    }
    pub fn try_shared() -> Result<Arc<Self>, String> {
        let (native, updates) = NativeClipboard::start_with_updates()?;
        let text = native.clone();
        let images = native.clone();
        let paste = native.clone();
        let service = Self::with_publishers(
            Arc::new(move |content, _| {
                if let ClipboardContent::Text(value) = content {
                    let _ = text.text(value, vnt_clip::win::sequence_number());
                }
            }),
            Arc::new(move |offer, provider, seq| native.files(offer, provider, seq)),
        );
        *service.inner.image_publisher.lock().unwrap() =
            Some(Arc::new(move |image, sequence, valid| {
                images.image(image, sequence, valid)
            }));
        *service.inner.paste_writer.lock().unwrap()=Some(Arc::new(move |text,valid|paste.paste_text(text,valid)));
        service.spawn_watcher(updates)?;
        Ok(service)
    }
    /// Text-only test adapter; never touches the real desktop clipboard.
    pub fn with_writer(writer: ClipboardWriter) -> Arc<Self> {
        Self::with_publishers(writer, Arc::new(|_, _, _| Ok(())))
    }
    /// Platform/test adapter whose paste operation confirms clipboard write and OS paste.
    pub fn with_text_paster(writer:ClipboardWriter,paste:PasteWriter)->Arc<Self> {
        let service=Self::with_writer(writer);
        *service.inner.paste_writer.lock().unwrap()=Some(paste);
        service
    }
    pub fn with_publishers(writer: ClipboardWriter, publisher: NativePublisher) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                publication: Mutex::new(()),
                image_publisher: Mutex::new(None),
                image_job: Mutex::new(None),
                image_encoding: AtomicBool::new(false),
                image_workers: Arc::new(tokio::sync::Semaphore::new(1)),
                runtime: Mutex::new(None),
                enumeration: Mutex::new(None),
                enumerating: AtomicBool::new(false),
                enabled: AtomicBool::new(true),
                sessions: Mutex::new(HashMap::new()),
                ids: Arc::new(AtomicU32::new(1)),
                generation: AtomicU64::new(0),
                writer,
                paste_writer: Mutex::new(None),
                publisher,
                remote_current: Mutex::new(None),
            }),
        })
    }
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Acquire)
    }
    pub fn set_enabled(&self, enabled: bool) {
        let _guard = self.inner.publication.lock().unwrap();
        if self.inner.enabled.swap(enabled, Ordering::AcqRel) == enabled {
            return;
        }
        self.inner.invalidate_locked();
        for entry in self.inner.entries() {
            if !enabled {
                entry.stop_operations("剪贴板同步已关闭");
            }
        }
    }
    pub fn session_enabled(&self, key: u64) -> bool {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|entry| entry.enabled.load(Ordering::Acquire))
    }
    pub fn set_session_enabled(&self, connection: u64, enabled: bool) -> bool {
        let entry = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&connection)
            .cloned();
        if let Some(entry) = entry {
            entry.enabled.store(enabled, Ordering::Release);
            if !enabled {
                entry.stop_operations("剪贴板同步已关闭");
            }
            true
        } else {
            false
        }
    }
    /// A failed file socket invalidates operations, not the clipboard session.
    pub fn transport_changed(&self, key: u64, available: bool) {
        let entry = self.inner.sessions.lock().unwrap().get(&key).cloned();
        let Some(entry) = entry else { return };
        entry.file_online.store(available, Ordering::Release);
        if !available {
            let source = entry.source_offer.lock().unwrap().clone();
            entry.stop_operations("文件通道已断开，请恢复后重新粘贴");
            *entry.source_offer.lock().unwrap() = source;
        } else {
            if let Some(source) = entry.source_offer.lock().unwrap().clone() {
                let _ = entry.control(ControlMessage::ClipboardOffer(source.manifest.clone()));
            }
            let cached = entry.cached_image.lock().unwrap().clone();
            if let Some((mut header, bytes)) = cached {
                if let Ok(id) = allocate(&entry.ids) {
                    header.image_id = id;
                    *entry.cached_image.lock().unwrap() = Some((header, bytes.clone()));
                    entry.source_image.store(id, Ordering::Release);
                    let _ = entry.control(ControlMessage::ClipboardImage(header));
                    spawn_image_chunks(entry, bytes, id);
                }
            }
        }
    }
    pub fn control_changed(&self, key: u64, available: bool) {
        if let Some(entry) = self.inner.sessions.lock().unwrap().get(&key) {
            if entry.control_online.swap(available, Ordering::AcqRel) && !available {
                entry.paste_epoch.fetch_add(1,Ordering::AcqRel);
            }
        }
    }

    pub fn cancel_transfer(&self, connection: u64, transfer_id: u32) -> bool {
        let entry = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&connection)
            .cloned();
        let Some(entry) = entry else {
            return false;
        };
        let target = entry
            .targets
            .lock()
            .unwrap()
            .values()
            .filter_map(Weak::upgrade)
            .find(|op| op.id == transfer_id);
        if let Some(target) = target {
            target.finish(false, "用户已取消");
            return true;
        }
        let source_id = entry
            .sources
            .lock()
            .unwrap()
            .iter()
            .find(|(_, s)| s.event_id == transfer_id)
            .map(|(id, _)| *id);
        if let Some(id) = source_id {
            if let Some(source) = entry.sources.lock().unwrap().remove(&id) {
                let mut end = request(source.offer.manifest.offer_id, id, Action::Abort);
                end.reason = "用户已取消".into();
                let _ = entry.control(ControlMessage::ClipboardRequest(end));
                emit(
                    &entry,
                    source.event_id,
                    "send",
                    "cancelled",
                    &source.offer.manifest,
                    &source.progress,
                    Some("用户已取消"),
                );
            }
            true
        } else {
            false
        }
    }
    pub fn cancel_remote_paste(&self,key:u64) {
        if let Some(entry)=self.inner.sessions.lock().unwrap().get(&key) {
            entry.paste_epoch.fetch_add(1,Ordering::AcqRel);
        }
    }
    /// Only host/user-desktop registrations accept commands that inject paste.
    pub fn allow_remote_paste(&self,key:u64) {
        if let Some(entry)=self.inner.sessions.lock().unwrap().get(&key) {
            entry.remote_paste.store(true,Ordering::Release);
        }
    }
    pub fn register(self: &Arc<Self>, spec: SessionSpec) -> ClipboardRegistration {
        *self.inner.runtime.lock().unwrap() = Some(tokio::runtime::Handle::current());
        // GUI controller and portable host have independent policies, but their
        // events and cancellation commands share one process-wide namespace.
        static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(SessionEntry {
            remote_paste: AtomicBool::new(false),
            last_paste: AtomicU32::new(0),
            paste_epoch: AtomicU64::new(0),
            paste_received: Mutex::new(HashMap::new()),
            capabilities: spec.capabilities,
            file_supported: AtomicBool::new(
                spec.capabilities & vnt_proto::CAP_FILES != 0 && cfg!(windows),
            ),
            key,
            session_id: spec.session_id,
            enabled: AtomicBool::new(spec.enabled),
            closed: AtomicBool::new(false),
            file_tx: spec.file_tx,
            control_tx: spec.control_tx,
            cancel: spec.session_cancel,
            sink: spec.sink,
            ids: self.inner.ids.clone(),
            requests: AtomicU32::new(1),
            source_offer: Mutex::new(None),
            sources: Mutex::new(HashMap::new()),
            targets: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            target_offer: Mutex::new(None),
            image: Mutex::new(None),
            image_decode: Mutex::new(None),
            image_decoding: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            source_image: AtomicU32::new(0),
            cached_image: Mutex::new(None),
            control_online: AtomicBool::new(true),
            file_online: AtomicBool::new(true),
            replies: Mutex::new(HashMap::new()),
            jobs: Arc::new(tokio::sync::Semaphore::new(MAX_OPERATIONS)),
            inflight: Mutex::new(HashSet::new()),
            received_image: AtomicU32::new(0),
        });
        self.inner
            .sessions
            .lock()
            .unwrap()
            .insert(key, entry.clone());
        let inner = self.inner.clone();
        tokio::spawn(async move {
            receiver_loop(inner, entry, spec.in_rx).await;
        });
        ClipboardRegistration {
            service: self.clone(),
            key,
        }
    }
    /// Publish owned content; file paths are enumerated outside the clipboard lock.
    pub fn on_local_change(self: &Arc<Self>, content: ClipboardContent) {
        match content {
            ClipboardContent::Image(image) => {
                let generation = self.inner.invalidate_local();
                self.inner
                    .send_image(image, generation, vnt_clip::win::sequence_number());
            }
            ClipboardContent::Text(text) => {
                self.inner.local_text(text);
            }
            ClipboardContent::Files(paths) => {
                if let Err(e) = self.offer_paths(&paths) {
                    tracing::warn!("文件剪贴板发布失败: {e}");
                }
            }
        }
    }
    pub fn offer_paths(&self, paths: &[PathBuf]) -> Result<(), String> {
        let generation = self.inner.invalidate_local();
        let id = allocate(&self.inner.ids)?;
        let offer = Arc::new(
            SourceOffer::collect(id, paths)
                .inspect_err(|why| self.inner.source_error(why, generation, None))?,
        );
        self.inner.publish_source(offer, generation, None);
        Ok(())
    }
    fn spawn_watcher(&self, updates: std::sync::mpsc::Receiver<()>) -> Result<(), String> {
        // Do not retain the service while waiting: dropping its native publishers
        // unregisters the listener, closes the channel and releases this thread.
        let inner = Arc::downgrade(&self.inner);
        let mut seq = vnt_clip::win::sequence_number();
        std::thread::Builder::new()
            .name("clipboard-watch".into())
            .spawn(move || {
                let mut retrying = false;
                let mut retries = 0;
                loop {
                    let notified = if retrying {
                        match updates.recv_timeout(Duration::from_millis(50)) {
                            Ok(()) => true,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        if updates.recv().is_err() {
                            break;
                        }
                        true
                    };
                    if notified {
                        retries = 0;
                    }
                    retrying = false;
                    let Some(inner) = inner.upgrade() else {
                        break;
                    };
                    let current = vnt_clip::win::sequence_number();
                    if seq == current {
                        continue;
                    }
                    if inner.entries().is_empty() {
                        seq = current;
                        continue;
                    }
                    let snapshot = vnt_clip::win::snapshot();
                    // Copying again (including delayed rendering) may have
                    // changed the clipboard during the read. Consume the latest
                    // state instead of publishing a mixed or stale snapshot.
                    if vnt_clip::win::sequence_number() != current {
                        retrying = true;
                        retries = 0;
                        continue;
                    }
                    match snapshot {
                        Err(vnt_clip::ClipboardError::Open) if retries < 4 => {
                            retries += 1;
                            retrying = true;
                            tracing::debug!("剪贴板被占用，重试 {retries}/4");
                            continue;
                        }
                        Err(e) => {
                            inner.invalidate_local();
                            tracing::warn!("剪贴板读取失败: {e}");
                        }
                        Ok(vnt_clip::win::LocalSnapshot::Ours) => {}
                        Ok(vnt_clip::win::LocalSnapshot::Image(raw)) => {
                            let generation = inner.invalidate_local();
                            inner.encode_image(raw, generation, current);
                        }
                        Ok(vnt_clip::win::LocalSnapshot::Text(text)) => inner.local_text(text),
                        Ok(vnt_clip::win::LocalSnapshot::Files(paths)) => {
                            let generation = inner.invalidate_local();
                            if let Ok(id) = allocate(&inner.ids) {
                                inner.enumerate(generation, current, paths, id);
                            }
                        }
                        Ok(vnt_clip::win::LocalSnapshot::Other) => {
                            inner.invalidate_local();
                        }
                    }
                    seq = current;
                }
            })
            .map(|_| ())
            .map_err(|e| format!("启动剪贴板监听失败: {e}"))
    }
}
impl Inner {
    fn image_current(&self, generation: u64, sequence: u32) -> bool {
        self.enabled.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) == generation
            && vnt_clip::win::sequence_number() == sequence
    }
    fn encode_image(
        self: &Arc<Self>,
        raw: vnt_clip::win::RawImage,
        generation: u64,
        sequence: u32,
    ) {
        *self.image_job.lock().unwrap() = Some((generation, sequence, raw));
        if self.image_encoding.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = self.clone();
        std::thread::spawn(move || loop {
            let job = {
                let mut slot = inner.image_job.lock().unwrap();
                match slot.take() {
                    Some(job) => job,
                    None => {
                        inner.image_encoding.store(false, Ordering::Release);
                        break;
                    }
                }
            };
            let (generation, sequence, raw) = job;
            if !inner.image_current(generation, sequence) {
                continue;
            }
            match raw.decode() {
                Ok(image) => inner.send_image(image, generation, sequence),
                Err(e) => tracing::warn!("读取图片失败: {e}"),
            }
        });
    }
    fn send_image(self: &Arc<Self>, image: ClipboardImage, generation: u64, sequence: u32) {
        let _guard = self.publication.lock().unwrap();
        if !self.image_current(generation, sequence) {
            return;
        }
        let Ok(id) = allocate(&self.ids) else {
            return;
        };
        let header = pb::ClipboardImage {
            image_id: id,
            width: image.width,
            height: image.height,
            total_bytes: image.png.len() as u32,
        };
        if !vnt_proto::clipboard::valid_image(&header) {
            return;
        }
        let bytes = Arc::new(image.png);
        for entry in self.entries() {
            if !entry.active()
                || entry
                    .control(ControlMessage::ClipboardImage(header))
                    .is_err()
            {
                continue;
            }
            *entry.cached_image.lock().unwrap() = Some((header, bytes.clone()));
            entry.source_image.store(id, Ordering::Release);
            spawn_image_chunks(entry, bytes.clone(), id);
        }
    }
    fn receive_image(self: &Arc<Self>, entry: Arc<SessionEntry>, incoming: IncomingImage) {
        *entry.image_decode.lock().unwrap() = Some(incoming);
        if entry.image_decoding.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = self.clone();
        tokio::spawn(async move {
            loop {
                let incoming = {
                    let mut slot = entry.image_decode.lock().unwrap();
                    match slot.take() {
                        Some(incoming) => incoming,
                        None => {
                            entry.image_decoding.store(false, Ordering::Release);
                            break;
                        }
                    }
                };
                let _permit = tokio::select! {
                    _ = entry.cancel.cancelled() => break,
                    result = inner.image_workers.clone().acquire_owned() => match result { Ok(p) => p, Err(_) => break },
                };
                let IncomingImage {
                    assembly,
                    generation,
                    sequence,
                    epoch,
                } = incoming;
                if !entry.active()
                    || entry.epoch.load(Ordering::Acquire) != epoch
                    || !inner.image_current(generation, sequence)
                {
                    continue;
                }
                let header = assembly.header;
                let pixels = tokio::task::spawn_blocking(move || {
                    let image = ClipboardImage {
                        width: header.width,
                        height: header.height,
                        png: assembly.into_bytes(),
                    };
                    PreparedImage::new(image)
                })
                .await;
                match pixels {
                    Ok(Ok(prepared)) => {
                        let _guard = inner.publication.lock().unwrap();
                        if !entry.active()
                            || entry.epoch.load(Ordering::Acquire) != epoch
                            || !inner.image_current(generation, sequence)
                        {
                            continue;
                        }
                        let weak = Arc::downgrade(&inner);
                        let source = Arc::downgrade(&entry);
                        let valid: vnt_clip::native::ImageValidity = Arc::new(move || {
                            source.upgrade().is_some_and(|e| {
                                e.active() && e.epoch.load(Ordering::Acquire) == epoch
                            }) && weak
                                .upgrade()
                                .is_some_and(|i| i.image_current(generation, sequence))
                        });
                        if let Some(publish) = inner.image_publisher.lock().unwrap().as_ref() {
                            if let Err(e) = publish(prepared, sequence, valid) {
                                tracing::warn!("发布图片失败: {e}");
                            }
                        } else {
                            (inner.writer)(
                                ClipboardContent::Image(ClipboardImage {
                                    width: header.width,
                                    height: header.height,
                                    png: prepared.png,
                                }),
                                vec![],
                            );
                        }
                    }
                    Ok(Err(e)) => tracing::warn!("图片校验失败: {e}"),
                    Err(e) => tracing::warn!("图片解码任务失败: {e}"),
                }
            }
        });
    }
    fn entries(&self) -> Vec<Arc<SessionEntry>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }
    fn source_error(&self, why: &str, generation: u64, sequence: Option<u32>) {
        let _guard = self.publication.lock().unwrap();
        if !self.enabled.load(Ordering::Acquire)
            || self.generation.load(Ordering::Acquire) != generation
            || sequence.is_some_and(|seq| vnt_clip::win::sequence_number() != seq)
        {
            return;
        }
        tracing::warn!("文件清单不可用: {why}");
        let offer = pb::ClipboardOffer {
            error: reason(why),
            ..Default::default()
        };
        for entry in self.entries() {
            if !entry.active() || !entry.file_supported.load(Ordering::Acquire) {
                continue;
            }
            let _ = entry.control(ControlMessage::ClipboardOffer(offer.clone()));
            if let Ok(id) = allocate(&self.ids) {
                emit(
                    &entry,
                    id,
                    "send",
                    "failed",
                    &offer,
                    &Progress::new(),
                    Some(why),
                );
            }
        }
    }
    fn retire_remote(&self) {
        if let Some(offer) = self
            .remote_current
            .lock()
            .unwrap()
            .take()
            .and_then(|w| w.upgrade())
        {
            offer.valid.store(false, Ordering::Release);
        }
    }
    fn enumerate(self: &Arc<Self>, generation: u64, sequence: u32, paths: Vec<PathBuf>, id: u32) {
        let mut pending = self.enumeration.lock().unwrap();
        *pending = Some((generation, sequence, paths, id));
        if self.enumerating.swap(true, Ordering::AcqRel) {
            return;
        }
        let worker = self.clone();
        std::thread::spawn(move || loop {
            let job = {
                let mut pending = worker.enumeration.lock().unwrap();
                match pending.take() {
                    Some(job) => job,
                    None => {
                        worker.enumerating.store(false, Ordering::Release);
                        break;
                    }
                }
            };
            let (generation, sequence, paths, id) = job;
            match SourceOffer::collect_checked(id, &paths, &|| {
                worker.generation.load(Ordering::Acquire) == generation
                    && vnt_clip::win::sequence_number() == sequence
            }) {
                Ok(offer) => worker.publish_source(Arc::new(offer), generation, Some(sequence)),
                Err(e) => {
                    if worker.generation.load(Ordering::Acquire) == generation
                        && vnt_clip::win::sequence_number() == sequence
                    {
                        worker.source_error(&e, generation, Some(sequence));
                    }
                }
            }
        });
    }
    fn invalidate_local(&self) -> u64 {
        let _guard = self.publication.lock().unwrap();
        self.invalidate_locked()
    }
    fn invalidate_locked(&self) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.retire_remote();
        for entry in self.entries() {
            entry.cancel_source_image();
            entry.cached_image.lock().unwrap().take();
            entry.received_image.store(0, Ordering::Release);
            entry.image.lock().unwrap().take();
            entry.image_decode.lock().unwrap().take();
            if entry.source_offer.lock().unwrap().take().is_some() {
                let _ =
                    entry.control(ControlMessage::ClipboardOffer(pb::ClipboardOffer::default()));
            }
        }
        generation
    }
    fn local_text(&self, text: String) {
        let _guard = self.publication.lock().unwrap();
        self.invalidate_locked();
        if !self.enabled.load(Ordering::Acquire)
            || !vnt_proto::validation::valid_clipboard_text(&text)
        {
            return;
        }
        for entry in self.entries() {
            if entry.active() {
                let _ = entry.control(ControlMessage::ClipboardText(pb::ClipboardText {
                    text: text.clone(),
                }));
            }
        }
    }
    fn publish_source(&self, offer: Arc<SourceOffer>, generation: u64, sequence: Option<u32>) {
        let _guard = self.publication.lock().unwrap();
        if !self.enabled.load(Ordering::Acquire)
            || sequence.is_some_and(|seq| vnt_clip::win::sequence_number() != seq)
            || self.generation.load(Ordering::Acquire) != generation
        {
            return;
        }
        for entry in self.entries() {
            if entry.active() && entry.file_supported.load(Ordering::Acquire) {
                *entry.source_offer.lock().unwrap() = Some(offer.clone());
                if entry
                    .control(ControlMessage::ClipboardOffer(offer.manifest.clone()))
                    .is_err()
                {
                    entry.source_offer.lock().unwrap().take();
                }
            }
        }
    }
}
struct RemoteOffer {
    entry: Weak<SessionEntry>,
    manifest: Arc<pb::ClipboardOffer>,
    valid: AtomicBool,
}
impl FileProvider for RemoteOffer {
    fn available(&self) -> bool {
        self.valid.load(Ordering::Acquire) && self.entry.upgrade().is_some_and(|e| e.active())
    }
    fn begin(&self) -> Result<Arc<dyn FileOperation>, String> {
        if !self.available() {
            return Err("剪贴板已替换或连接已断开".into());
        }
        let entry = self.entry.upgrade().ok_or("连接已断开")?;
        let id = allocate(&entry.ids)?;
        let mut targets = entry.targets.lock().unwrap();
        targets.retain(|_, op| op.strong_count() > 0);
        if targets.len() >= MAX_OPERATIONS {
            return Err("同时粘贴的操作过多".into());
        }
        let start =
            entry.queue_request(request(self.manifest.offer_id, id, Action::Start), None)?;
        let operation = Arc::new(RemoteOperation {
            entry: Arc::downgrade(&entry),
            manifest: self.manifest.clone(),
            id,
            ended: AtomicBool::new(false),
            start: Mutex::new(Some(start)),
            progress: Mutex::new(Progress::new()),
        });
        targets.insert(id, Arc::downgrade(&operation));
        emit(
            &entry,
            id,
            "receive",
            "active",
            &operation.manifest,
            &operation.progress.lock().unwrap(),
            None,
        );
        Ok(operation)
    }
}
type ReplyReceiver = std::sync::mpsc::Receiver<Result<pb::ClipboardReply, String>>;
struct RemoteOperation {
    entry: Weak<SessionEntry>,
    manifest: Arc<pb::ClipboardOffer>,
    id: u32,
    ended: AtomicBool,
    start: Mutex<Option<ReplyReceiver>>,
    progress: Mutex<Progress>,
}
impl RemoteOperation {
    fn wait(&self, rx: ReplyReceiver) -> Result<pb::ClipboardReply, String> {
        let mut remaining = READ_TIMEOUT;
        loop {
            let entry = self.entry.upgrade().ok_or("连接已断开")?;
            let before = Instant::now();
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("连接已断开".into())
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if !entry.active() || self.ended.load(Ordering::Acquire) {
                return Err("文件操作已结束".into());
            }
            if entry.control_online.load(Ordering::Acquire) {
                remaining = remaining.saturating_sub(before.elapsed());
                if remaining.is_zero() {
                    return Err("远程文件读取超时".into());
                }
            }
        }
    }
}
impl FileOperation for RemoteOperation {
    fn read(&self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>, String> {
        let result: Result<Vec<u8>, String> = (|| {
            if self.ended.load(Ordering::Acquire) {
                return Err("文件复制已结束".into());
            }
            let entry = self
                .entry
                .upgrade()
                .filter(|e| e.active())
                .ok_or("连接已断开或剪贴板同步已关闭")?;
            // Serialize the START acknowledgement; independent stream reads can then run concurrently.
            {
                let mut start = self.start.lock().unwrap();
                if let Some(rx) = start.take() {
                    self.wait(rx)?;
                }
            }
            if self.ended.load(Ordering::Acquire) {
                return Err("文件复制已取消".into());
            }
            let item = self
                .manifest
                .items
                .get(index as usize)
                .ok_or("文件索引无效")?;
            if item.directory
                || offset > item.size
                || length == 0
                || length as usize > vnt_proto::control::CLIPBOARD_CHUNK_BYTES
            {
                return Err("读取范围无效".into());
            }
            let mut value = request(self.manifest.offer_id, self.id, Action::Read);
            value.item_index = index;
            value.offset = offset;
            value.length = length;
            let reply = self.wait(entry.queue_request(value, Some(&self.ended))?)?;
            if reply.data.len() as u64 != (length as u64).min(item.size - offset) {
                return Err("源文件提前结束".into());
            }
            let mut progress = self.progress.lock().unwrap();
            progress.advance(index, offset + reply.data.len() as u64);
            if progress.last_emit.elapsed() >= Duration::from_millis(200) {
                progress.last_emit = Instant::now();
                emit(
                    &entry,
                    self.id,
                    "receive",
                    "active",
                    &self.manifest,
                    &progress,
                    None,
                );
            }
            Ok(reply.data)
        })();
        if let Err(e) = &result {
            self.finish(false, e);
        }
        result
    }
    fn close(&self, index: u32) {
        if !self.ended.load(Ordering::Acquire) {
            if let Some(entry) = self.entry.upgrade() {
                let mut value = request(self.manifest.offer_id, self.id, Action::Close);
                value.item_index = index;
                let _ = entry.control(ControlMessage::ClipboardRequest(value));
            }
        }
    }
    fn finish(&self, ok: bool, why: &str) {
        if self.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(entry) = self.entry.upgrade() {
            entry.fail_pending(Some(self.id), why);
            entry.targets.lock().unwrap().remove(&self.id);
            let cancelled =
                !ok && (why.contains("取消") || why.contains("断开") || why.contains("关闭"));
            let mut value = request(
                self.manifest.offer_id,
                self.id,
                if cancelled {
                    Action::Cancel
                } else {
                    Action::Finish
                },
            );
            value.ok = ok;
            value.reason = reason(why);
            let _ = entry.control(ControlMessage::ClipboardRequest(value));
            let state = if ok {
                "done"
            } else if cancelled {
                "cancelled"
            } else {
                "failed"
            };
            emit(
                &entry,
                self.id,
                "receive",
                state,
                &self.manifest,
                &self.progress.lock().unwrap(),
                (!ok).then_some(why),
            );
        }
    }
}
impl Drop for RemoteOperation {
    fn drop(&mut self) {
        self.finish(false, "数据流已释放，复制未确认完成");
    }
}
fn emit(
    entry: &SessionEntry,
    id: u32,
    direction: &str,
    state: &str,
    offer: &pb::ClipboardOffer,
    progress: &Progress,
    why: Option<&str>,
) {
    let index = progress.index;
    let mut payload = serde_json::json!({
        "connectionId": entry.key.to_string(), "sessionId": entry.session_id, "transferId": id,
        "direction": direction, "state": state, "name": offer.items.get(index).map(|i| i.rel_path.as_str()).unwrap_or("文件复制"),
        "fileIndex": if offer.items.is_empty() { 0 } else { index + 1 }, "fileCount": offer.items.len(), "completedBytes": progress.bytes,
        "totalBytes": offer.total_bytes, "speedKbps": (progress.bytes as f64 * 8.0 / progress.started.elapsed().as_secs_f64().max(0.001) / 1000.0) as u32,
    });
    if let Some(why) = why {
        payload["reason"] = serde_json::Value::String(why.into());
    }
    entry
        .sink
        .emit_json("clipboard://transfer", payload.to_string());
}
async fn receiver_loop(
    inner: Arc<Inner>,
    entry: Arc<SessionEntry>,
    mut rx: mpsc::Receiver<InboundClipboard>,
) {
    let mut cleanup = tokio::time::interval(Duration::from_secs(1));
    loop {
        let message = tokio::select! { biased; _ = entry.cancel.cancelled() => break,
        _ = cleanup.tick() => {
            let mut image = entry.image.lock().unwrap();
            if image.as_ref().is_some_and(|i| i.assembly.expired()) { image.take(); tracing::warn!("图片接收超时"); }
            continue;
        }, message = rx.recv() => match message { Some(m) => m, None => break } };
        match message {
            ControlMessage::ClipboardPaste(request) => {
                let inner=inner.clone();let job_entry=entry.clone();let request_id=request.request_id;
                let Some((received,epoch))=entry.paste_received.lock().unwrap().remove(&request_id) else {continue;};
                let result=tokio::task::spawn_blocking(move || {
                    let check_entry=job_entry.clone();let check_inner=inner.clone();
                    let valid: vnt_clip::native::ImageValidity=Arc::new(move ||
                        received.elapsed()<Duration::from_secs(2)
                        && check_inner.enabled.load(Ordering::Acquire)
                        && check_entry.remote_paste.load(Ordering::Acquire)
                        && check_entry.control_online.load(Ordering::Acquire)
                        && check_entry.paste_epoch.load(Ordering::Acquire)==epoch && check_entry.active());
                    let _guard=inner.publication.lock().unwrap();
                    if !valid() {return Err("当前会话不能粘贴，或请求已过期".into());}
                    let paste=inner.paste_writer.lock().unwrap().clone().ok_or("当前被控端未提供粘贴功能")?;
                    inner.invalidate_locked();
                    paste(request.text,valid)
                }).await.unwrap_or_else(|_|Err("粘贴工作线程失败".into()));
                let _=entry.control(ControlMessage::ClipboardPasteResult(pb::ClipboardPasteResult{
                    request_id,success:result.is_ok(),reason:result.err().map(|e|reason(&e)).unwrap_or_default(),
                }));
            }
            ControlMessage::ClipboardText(text)
                if inner.enabled.load(Ordering::Acquire) && entry.active() =>
            {
                let _guard = inner.publication.lock().unwrap();
                inner.invalidate_locked();
                (inner.writer)(ClipboardContent::Text(text.text), vec![]);
            }
            ControlMessage::ClipboardOffer(offer) => {
                if offer.offer_id != 0
                    && entry
                        .target_offer
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .is_some_and(|old| {
                            old.valid.load(Ordering::Acquire) && *old.manifest == offer
                        })
                {
                    continue;
                }
                entry.retire_offer();
                if !inner.enabled.load(Ordering::Acquire) || !entry.active() {
                    continue;
                }
                if offer.offer_id == 0 {
                    if !offer.error.is_empty() {
                        if let Ok(id) = allocate(&entry.ids) {
                            emit(
                                &entry,
                                id,
                                "receive",
                                "failed",
                                &offer,
                                &Progress::new(),
                                Some(&offer.error),
                            );
                        }
                    }
                    continue;
                }
                let _guard = inner.publication.lock().unwrap();
                inner.invalidate_locked();
                let manifest = Arc::new(offer);
                let provider = Arc::new(RemoteOffer {
                    entry: Arc::downgrade(&entry),
                    manifest: manifest.clone(),
                    valid: AtomicBool::new(true),
                });
                *entry.target_offer.lock().unwrap() = Some(Arc::downgrade(&provider));
                *inner.remote_current.lock().unwrap() = Some(Arc::downgrade(&provider));
                if let Err(e) =
                    (inner.publisher)(manifest, provider.clone(), vnt_clip::win::sequence_number())
                {
                    provider.valid.store(false, Ordering::Release);
                    tracing::warn!("发布文件剪贴板失败: {e}");
                }
            }
            ControlMessage::ClipboardImageCancel(cancel) => {
                let _guard = inner.publication.lock().unwrap();
                if entry.received_image.load(Ordering::Acquire) == cancel.image_id {
                    inner.invalidate_locked();
                }
            }
            ControlMessage::ClipboardImage(header) => {
                if entry.received_image.load(Ordering::Acquire) == header.image_id {
                    let _ = entry.control(ControlMessage::ClipboardImageReady(
                        pb::ClipboardImageReady {
                            image_id: header.image_id,
                            generation: 0,
                        },
                    ));
                    continue;
                }
                let _guard = inner.publication.lock().unwrap();
                if !inner.enabled.load(Ordering::Acquire) || !entry.active() {
                    continue;
                }
                let generation = inner.invalidate_locked();
                if let Some(assembly) = ImageAssembly::new(header) {
                    entry
                        .received_image
                        .store(assembly.header.image_id, Ordering::Release);
                    let image_id = assembly.header.image_id;
                    *entry.image.lock().unwrap() = Some(IncomingImage {
                        assembly,
                        generation,
                        sequence: vnt_clip::win::sequence_number(),
                        epoch: entry.epoch.load(Ordering::Acquire),
                    });
                    let _ = entry.control(ControlMessage::ClipboardImageReady(
                        pb::ClipboardImageReady {
                            image_id,
                            generation: 0,
                        },
                    ));
                }
            }
            ControlMessage::ClipboardImageChunk(chunk) => {
                let complete = {
                    let mut slot = entry.image.lock().unwrap();
                    match slot.as_mut() {
                        Some(image) if image.assembly.header.image_id != chunk.image_id => None,
                        Some(image) => match image.assembly.push(&chunk) {
                            Ok(true) => slot.take(),
                            Ok(false) => None,
                            Err(e) => {
                                tracing::warn!("接收图片失败: {e}");
                                slot.take();
                                None
                            }
                        },
                        None => None,
                    }
                };
                if let Some(image) = complete {
                    inner.receive_image(entry.clone(), image);
                }
            }
            ControlMessage::ClipboardRequest(value) => {
                if matches!(
                    Action::try_from(value.action),
                    Ok(Action::Close | Action::Cancel | Action::Finish | Action::Abort)
                ) {
                    handle_request(&entry, value).await;
                    continue;
                }
                if value.request_id != 0 && !entry.inflight.lock().unwrap().insert(value.request_id)
                {
                    continue;
                }
                match entry.jobs.clone().try_acquire_owned() {
                    Ok(permit) => {
                        let entry = entry.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            tokio::select! {
                                _=entry.cancel.cancelled()=>{},
                                _=handle_request(&entry, value.clone())=>{},
                            }
                            entry.inflight.lock().unwrap().remove(&value.request_id);
                        });
                    }
                    Err(_) => {
                        entry.inflight.lock().unwrap().remove(&value.request_id);
                        let mut abort = request(value.offer_id, value.operation_id, Action::Abort);
                        abort.reason = "文件请求过多，请重新粘贴".into();
                        let _ = entry.control(ControlMessage::ClipboardRequest(abort));
                    }
                }
            }
            _ => {} // Unrecognized clipboard messages are ignored.
        }
    }
    entry.shutdown("连接已断开");
}
fn spawn_image_chunks(entry: Arc<SessionEntry>, bytes: Arc<Vec<u8>>, id: u32) {
    let epoch = entry.epoch.load(Ordering::Acquire);
    tokio::spawn(async move {
        for (index, data) in bytes
            .chunks(vnt_proto::control::CLIPBOARD_CHUNK_BYTES)
            .enumerate()
        {
            let permit = tokio::select! {
                _ = entry.cancel.cancelled() => return,
                result = tokio::time::timeout(Duration::from_secs(30), entry.file_tx.reserve()) => match result { Ok(Ok(p))=>p, _=>return },
            };
            if !entry.active()
                || entry.epoch.load(Ordering::Acquire) != epoch
                || entry.source_image.load(Ordering::Acquire) != id
            {
                return;
            }
            permit.send(ControlMessage::ClipboardImageChunk(
                pb::ClipboardImageChunk {
                    image_id: id,
                    offset: (index * vnt_proto::control::CLIPBOARD_CHUNK_BYTES) as u32,
                    data: data.to_vec(),
                },
            ));
        }
    });
}

async fn handle_request(entry: &Arc<SessionEntry>, value: pb::ClipboardRequest) {
    let epoch = entry.epoch.load(Ordering::Acquire);
    let cached = entry
        .replies
        .lock()
        .unwrap()
        .get(&value.request_id)
        .cloned();
    if let Some((request, reply)) = cached {
        if request == value {
            let _ = entry
                .file_tx
                .send(ControlMessage::ClipboardReply(reply))
                .await;
        }
        return;
    }
    let Ok(action) = Action::try_from(value.action) else {
        return;
    };
    if action == Action::Abort {
        let target = entry
            .targets
            .lock()
            .unwrap()
            .get(&value.operation_id)
            .and_then(Weak::upgrade);
        if let Some(target) = target {
            if target.manifest.offer_id == value.offer_id {
                target.finish(false, &value.reason);
            }
        }
        return;
    }
    if matches!(action, Action::Cancel | Action::Finish | Action::Close) {
        let mut sources = entry.sources.lock().unwrap();
        if sources
            .get(&value.operation_id)
            .is_none_or(|s| s.offer.manifest.offer_id != value.offer_id)
        {
            return;
        }
        if action == Action::Close {
            let operation = sources.get(&value.operation_id).unwrap().operation.clone();
            drop(sources);
            // An overlapping disk read retains the handle until its operation
            // finishes; never block the metadata actor on its synchronous lock.
            if let Ok(mut operation) = operation.try_lock() {
                operation.close(value.item_index);
            };
        } else if let Some(source) = sources.remove(&value.operation_id) {
            emit(
                entry,
                source.event_id,
                "send",
                if action == Action::Finish && value.ok {
                    "done"
                } else if action == Action::Cancel {
                    "cancelled"
                } else {
                    "failed"
                },
                &source.offer.manifest,
                &source.progress,
                Some(&value.reason),
            );
        }
        return;
    }
    let mut reply = pb::ClipboardReply {
        offer_id: value.offer_id,
        operation_id: value.operation_id,
        request_id: value.request_id,
        item_index: value.item_index,
        offset: value.offset,
        ..Default::default()
    };
    let result: Result<Vec<u8>, String> = async {
        if !entry.active() {
            return Err("剪贴板同步已关闭".into());
        }
        if action == Action::Start {
            let offer = entry
                .source_offer
                .lock()
                .unwrap()
                .as_ref()
                .filter(|o| o.manifest.offer_id == value.offer_id)
                .cloned()
                .ok_or("文件清单已替换，请重新复制")?;
            let mut sources = entry.sources.lock().unwrap();
            if sources.len() >= MAX_OPERATIONS || sources.contains_key(&value.operation_id) {
                return Err("重复或过多的粘贴操作".into());
            }
            let event_id = allocate(&entry.ids)?;
            let progress = Progress::new();
            emit(
                entry,
                event_id,
                "send",
                "active",
                &offer.manifest,
                &progress,
                None,
            );
            sources.insert(
                value.operation_id,
                SourceTransfer {
                    operation: Arc::new(Mutex::new(SourceOperation::new(offer.clone()))),
                    offer,
                    event_id,
                    progress,
                },
            );
            return Ok(vec![]);
        }
        let operation = entry
            .sources
            .lock()
            .unwrap()
            .get(&value.operation_id)
            .filter(|s| s.offer.manifest.offer_id == value.offer_id)
            .map(|s| s.operation.clone())
            .ok_or("粘贴操作已结束或不存在")?;
        let index = value.item_index;
        let offset = value.offset;
        let length = value.length;
        let bytes = tokio::task::spawn_blocking(move || {
            operation.lock().unwrap().read(index, offset, length)
        })
        .await
        .map_err(|e| e.to_string())??;
        if let Some(source) = entry.sources.lock().unwrap().get_mut(&value.operation_id) {
            source.progress.advance(index, offset + bytes.len() as u64);
            if source.progress.last_emit.elapsed() >= Duration::from_millis(200) {
                source.progress.last_emit = Instant::now();
                emit(
                    entry,
                    source.event_id,
                    "send",
                    "active",
                    &source.offer.manifest,
                    &source.progress,
                    None,
                );
            }
        }
        Ok(bytes)
    }
    .await;
    match result {
        Ok(bytes) => reply.data = bytes,
        Err(e) => {
            reply.reason = reason(&e);
            let source = {
                let mut sources = entry.sources.lock().unwrap();
                if action == Action::Read
                    && sources
                        .get(&value.operation_id)
                        .is_some_and(|source| source.offer.manifest.offer_id == value.offer_id)
                {
                    sources.remove(&value.operation_id)
                } else {
                    None
                }
            };
            if let Some(source) = source {
                emit(
                    entry,
                    source.event_id,
                    "send",
                    "failed",
                    &source.offer.manifest,
                    &source.progress,
                    Some(&e),
                );
            }
        }
    }
    if entry.epoch.load(Ordering::Acquire) != epoch {
        return;
    }
    if value.request_id != 0 {
        let mut replies = entry.replies.lock().unwrap();
        if replies.len() >= 128 {
            replies.clear();
        }
        replies.insert(value.request_id, (value.clone(), reply.clone()));
    }
    let message = ControlMessage::ClipboardReply(reply);
    let tx = &entry.file_tx;
    tokio::select! { _ = entry.cancel.cancelled() => {}, _ = tx.send(message) => {} }
}
/// Replies bypass disk work and the bounded actor queue, so simultaneous transfers
/// cannot deadlock. Publications use bounded backpressure during image bursts;
/// excess file requests fail promptly instead of queuing unbounded work.
pub async fn dispatch_inbound(
    service: &ClipboardService,
    key: u64,
    tx: &mpsc::Sender<InboundClipboard>,
    message: InboundClipboard,
) -> Result<(), String> {
    let entry = service
        .inner
        .sessions
        .lock()
        .unwrap()
        .get(&key)
        .cloned()
        .ok_or("剪贴板连接已结束")?;
    if !permits(&message, entry.capabilities) {
        return Ok(());
    }
    if let ControlMessage::ClipboardPaste(request)=&message {
        let id=request.request_id;
        let fresh=id!=0 && entry.last_paste.try_update(Ordering::AcqRel,Ordering::Acquire,|previous|(id>previous).then_some(id)).is_ok();
        if !fresh || !entry.remote_paste.load(Ordering::Acquire) || !entry.active() || !entry.control_online.load(Ordering::Acquire) {
            let _=entry.control(ControlMessage::ClipboardPasteResult(pb::ClipboardPasteResult{request_id:id,success:false,reason:"当前会话不能粘贴，或请求已过期".into()}));
            return Ok(());
        }
        entry.paste_received.lock().unwrap().insert(id,(Instant::now(),entry.paste_epoch.load(Ordering::Acquire)));
        if tx.try_send(message).is_err() {
            entry.paste_received.lock().unwrap().remove(&id);
            let _=entry.control(ControlMessage::ClipboardPasteResult(pb::ClipboardPasteResult{request_id:id,success:false,reason:"粘贴队列已满".into()}));
        }
        return Ok(());
    }
    if let ControlMessage::ClipboardReply(reply) = message {
        entry.reply(reply);
        return Ok(());
    }
    if matches!(
        message,
        ControlMessage::ClipboardImage(_)
            | ControlMessage::ClipboardImageChunk(_)
            | ControlMessage::ClipboardImageCancel(_)
            | ControlMessage::ClipboardText(_)
            | ControlMessage::ClipboardPaste(_)
            | ControlMessage::ClipboardOffer(_)
    ) {
        return tokio::select! {
            _ = entry.cancel.cancelled() => Err("剪贴板连接已结束".into()),
            result = tx.send(message) => result.map_err(|_| "剪贴板接收通道已关闭".into()),
        };
    }
    if tx.try_send(message).is_err() {
        entry.shutdown("剪贴板接收队列拥塞");
        return Err("剪贴板接收队列拥塞，已取消传输".into());
    }
    Ok(())
}

fn permits(message: &ControlMessage, capabilities: u32) -> bool {
    let required = match message {
        ControlMessage::ClipboardText(_) | ControlMessage::ClipboardPaste(_) | ControlMessage::ClipboardPasteResult(_) => vnt_proto::CAP_TEXT,
        ControlMessage::ClipboardImage(_)
        | ControlMessage::ClipboardImageChunk(_)
        | ControlMessage::ClipboardImageCancel(_) => vnt_proto::CAP_IMAGE,
        ControlMessage::ClipboardOffer(_)
        | ControlMessage::ClipboardRequest(_)
        | ControlMessage::ClipboardReply(_) => vnt_proto::CAP_FILES,
        _ => 0,
    };
    capabilities & required == required
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    #[derive(Default)]
    struct Sink(Mutex<Vec<serde_json::Value>>);
    impl EventSink for Sink {
        fn emit_json(&self, _: &str, text: String) {
            self.0
                .lock()
                .unwrap()
                .push(serde_json::from_str(&text).unwrap());
        }
    }
    type Published = Arc<Mutex<Option<(Arc<pb::ClipboardOffer>, Arc<dyn FileProvider>)>>>;
    struct Peer {
        service: Arc<ClipboardService>,
        published: Published,
        sink: Arc<Sink>,
        writes: Arc<Mutex<Vec<String>>>,
        images: Arc<Mutex<Vec<ClipboardImage>>>,
    }
    impl Peer {
        fn new() -> Self {
            let published: Published = Arc::new(Mutex::new(None));
            let target = published.clone();
            let writes = Arc::new(Mutex::new(Vec::new()));
            let log = writes.clone();
            let images = Arc::new(Mutex::new(Vec::new()));
            let image_log = images.clone();
            let service = ClipboardService::with_publishers(
                Arc::new(move |content, _| match content {
                    ClipboardContent::Text(text) => log.lock().unwrap().push(text),
                    ClipboardContent::Image(image) => image_log.lock().unwrap().push(image),
                    _ => {}
                }),
                Arc::new(move |offer, provider, _| {
                    *target.lock().unwrap() = Some((offer, provider));
                    Ok(())
                }),
            );
            Self {
                service,
                published,
                sink: Arc::new(Sink::default()),
                writes,
                images,
            }
        }
        async fn provider(&self) -> Arc<dyn FileProvider> {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some((_, provider)) = self.published.lock().unwrap().as_ref() {
                        return provider.clone();
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap()
        }
    }
    struct Pipe {
        a: ClipboardRegistration,
        b: ClipboardRegistration,
        tasks: Vec<tokio::task::JoinHandle<()>>,
    }
    impl Drop for Pipe {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }
    fn endpoint(
        peer: &Peer,
    ) -> (
        ClipboardRegistration,
        mpsc::Receiver<ControlMessage>,
        mpsc::Receiver<ControlMessage>,
        mpsc::Sender<ControlMessage>,
    ) {
        let (control_tx, ctrl) = mpsc::channel(128);
        let (file_tx, bulk) = mpsc::channel(OUT_QUEUE);
        let (in_tx, in_rx) = mpsc::channel(INBOUND_QUEUE);
        let reg = peer.service.register(SessionSpec {
            capabilities: vnt_proto::platform_capabilities(),
            session_id: 42,
            control_tx,
            file_tx,
            in_rx,
            session_cancel: CancellationToken::new(),
            sink: peer.sink.clone(),
            enabled: true,
        });
        (reg, ctrl, bulk, in_tx)
    }

    #[tokio::test]
    async fn portable_host_policy_does_not_disable_controller_clipboard_or_reuse_its_connection_id()
    {
        let host = Peer::new();
        let controller = Peer::new();
        let mut host_endpoint = endpoint(&host);
        let mut controller_endpoint = endpoint(&controller);
        assert_ne!(host_endpoint.0.key(), controller_endpoint.0.key());
        host.service.set_enabled(false);
        host.service
            .on_local_change(ClipboardContent::Text("host paused".into()));
        controller
            .service
            .on_local_change(ClipboardContent::Text("controller still active".into()));
        assert!(host_endpoint.1.try_recv().is_err());
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(1), controller_endpoint.1.recv()).await.unwrap(),
            Some(ControlMessage::ClipboardText(text)) if text.text == "controller still active")
        );
    }
    fn route(
        mut control: mpsc::Receiver<ControlMessage>,
        mut bulk: mpsc::Receiver<ControlMessage>,
        service: Arc<ClipboardService>,
        key: u64,
        input: mpsc::Sender<ControlMessage>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let msg = tokio::select! { biased; m = control.recv() => m, m = bulk.recv() => m };
                let Some(msg) = msg else {
                    break;
                };
                dispatch_inbound(&service, key, &input, msg).await.unwrap();
            }
        })
    }
    fn connect(a: &Peer, b: &Peer) -> Pipe {
        let (reg_a, ctrl_a, bulk_a, in_a) = endpoint(a);
        let (reg_b, ctrl_b, bulk_b, in_b) = endpoint(b);
        let tasks = vec![
            route(ctrl_a, bulk_a, b.service.clone(), reg_b.key(), in_b),
            route(ctrl_b, bulk_b, a.service.clone(), reg_a.key(), in_a),
        ];
        Pipe {
            a: reg_a,
            b: reg_b,
            tasks,
        }
    }
    async fn read(
        op: Arc<dyn FileOperation>,
        index: u32,
        offset: u64,
        length: u32,
    ) -> Result<Vec<u8>, String> {
        tokio::task::spawn_blocking(move || op.read(index, offset, length))
            .await
            .unwrap()
    }
    async fn settled() {
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    #[test]
    fn waiting_for_notifications_does_not_retain_the_clipboard_service() {
        let service = ClipboardService::with_writer(Arc::new(|_, _| {}));
        let weak = Arc::downgrade(&service.inner);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        service.spawn_watcher(rx).unwrap();
        drop(service);
        assert!(weak.upgrade().is_none());
        drop(tx);
    }
    #[test]
    #[ignore = "uses the real Windows clipboard; run explicitly and serially"]
    fn notified_watcher_retries_a_busy_clipboard_without_another_notification() {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };
        let _guard = vnt_clip::CLIPBOARD_LOCK.lock().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let peer = Peer::new();
            let (_registration, mut control, _bulk, _input) = endpoint(&peer);
            let (notify, updates) = std::sync::mpsc::sync_channel(1);
            peer.service.spawn_watcher(updates).unwrap();
            // An external-style copy has no vnt-remote origin marker.
            let wide: Vec<u16> = "event-driven retry".encode_utf16().chain([0]).collect();
            let global = unsafe { GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) }.unwrap();
            let ptr = unsafe { GlobalLock(global) }.cast::<u16>();
            assert!(!ptr.is_null());
            unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len()); }
            let _ = unsafe { GlobalUnlock(global) };
            vnt_clip::win::open().unwrap();
            unsafe { EmptyClipboard() }.unwrap();
            unsafe { SetClipboardData(13, Some(HANDLE(global.0))) }.unwrap();
            vnt_clip::win::close();
            // Hold the clipboard past the first OpenClipboard retry budget.
            vnt_clip::win::open().unwrap();
            notify.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(250));
            vnt_clip::win::close();
            let message = tokio::time::timeout(Duration::from_secs(3), control.recv()).await.unwrap().unwrap();
            assert!(matches!(message, ControlMessage::ClipboardText(text) if text.text == "event-driven retry"));
            // Processing the same sequence twice must not republish it.
            notify.send(()).unwrap();
            assert!(tokio::time::timeout(Duration::from_millis(100), control.recv()).await.is_err());
        });
    }
    async fn wait_images(peer: &Peer, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while peer.images.lock().unwrap().len() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    fn sample_image() -> ClipboardImage {
        // Incompressible pixels force the real bounded multi-chunk path.
        let mut seed = 123456789u32;
        let pixels: Vec<u8> = (0..256 * 256 * 4)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        vnt_clip::image::from_rgba(256, 256, &pixels).unwrap()
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn images_sync_in_both_directions_without_echo_and_respect_switches() {
        let a = Peer::new();
        let b = Peer::new();
        let pipe = connect(&a, &b);
        let image = sample_image();
        assert!(image.png.len() > vnt_proto::control::CLIPBOARD_CHUNK_BYTES);
        a.service
            .on_local_change(ClipboardContent::Image(image.clone()));
        wait_images(&b, 1).await;
        assert_eq!(b.images.lock().unwrap()[0], image);
        assert!(a.images.lock().unwrap().is_empty());
        b.service
            .on_local_change(ClipboardContent::Image(image.clone()));
        wait_images(&a, 1).await;
        assert_eq!(a.images.lock().unwrap()[0], image);
        a.service.set_session_enabled(pipe.a.key(), false);
        a.service
            .on_local_change(ClipboardContent::Image(image.clone()));
        settled().await;
        assert_eq!(b.images.lock().unwrap().len(), 1);
        a.service.set_session_enabled(pipe.a.key(), true);
        b.service.set_enabled(false);
        a.service.on_local_change(ClipboardContent::Image(image));
        settled().await;
        assert_eq!(b.images.lock().unwrap().len(), 1);
    }
    async fn inject_image(
        peer: &Peer,
        key: u64,
        input: &mpsc::Sender<ControlMessage>,
        image: &ClipboardImage,
        id: u32,
    ) {
        dispatch_inbound(
            &peer.service,
            key,
            input,
            ControlMessage::ClipboardImage(pb::ClipboardImage {
                image_id: id,
                width: image.width,
                height: image.height,
                total_bytes: image.png.len() as u32,
            }),
        )
        .await
        .unwrap();
        for (i, data) in image
            .png
            .chunks(vnt_proto::control::CLIPBOARD_CHUNK_BYTES)
            .enumerate()
        {
            dispatch_inbound(
                &peer.service,
                key,
                input,
                ControlMessage::ClipboardImageChunk(pb::ClipboardImageChunk {
                    image_id: id,
                    offset: (i * vnt_proto::control::CLIPBOARD_CHUNK_BYTES) as u32,
                    data: data.to_vec(),
                }),
            )
            .await
            .unwrap();
        }
    }
    #[tokio::test]
    async fn corrupt_or_partial_images_never_replace_the_clipboard() {
        let peer = Peer::new();
        let (reg, _control, _bulk, input) = endpoint(&peer);
        let image = sample_image();
        let header = pb::ClipboardImage {
            image_id: 1,
            width: image.width,
            height: image.height,
            total_bytes: image.png.len() as u32,
        };
        dispatch_inbound(
            &peer.service,
            reg.key(),
            &input,
            ControlMessage::ClipboardImage(header),
        )
        .await
        .unwrap();
        dispatch_inbound(
            &peer.service,
            reg.key(),
            &input,
            ControlMessage::ClipboardImageChunk(pb::ClipboardImageChunk {
                image_id: 1,
                offset: 1,
                data: vec![1],
            }),
        )
        .await
        .unwrap();
        settled().await;
        assert!(peer.images.lock().unwrap().is_empty());
        let entry = peer
            .service
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&reg.key())
            .unwrap()
            .clone();
        assert!(entry.image.lock().unwrap().is_none());
        let corrupt = ClipboardImage {
            width: 1,
            height: 1,
            png: vec![1, 2, 3, 4],
        };
        inject_image(&peer, reg.key(), &input, &corrupt, 2).await;
        settled().await;
        assert!(peer.images.lock().unwrap().is_empty());
        let mismatched = ClipboardImage {
            width: 1,
            height: 1,
            png: image.png.clone(),
        };
        inject_image(&peer, reg.key(), &input, &mismatched, 3).await;
        settled().await;
        assert!(peer.images.lock().unwrap().is_empty());
        inject_image(&peer, reg.key(), &input, &image, 4).await;
        wait_images(&peer, 1).await;
    }
    #[tokio::test]
    async fn old_chunks_do_not_cancel_a_newer_image() {
        let peer = Peer::new();
        let (reg, _control, _bulk, input) = endpoint(&peer);
        let image = vnt_clip::image::from_rgba(1, 1, &[10, 20, 30, 255]).unwrap();
        let header = |image_id| {
            ControlMessage::ClipboardImage(pb::ClipboardImage {
                image_id,
                width: 1,
                height: 1,
                total_bytes: image.png.len() as u32,
            })
        };
        for message in [
            header(1),
            header(2),
            ControlMessage::ClipboardImageCancel(pb::ClipboardImageCancel { image_id: 1 }),
            ControlMessage::ClipboardImageChunk(pb::ClipboardImageChunk {
                image_id: 1,
                offset: 0,
                data: vec![0],
            }),
            ControlMessage::ClipboardImageChunk(pb::ClipboardImageChunk {
                image_id: 2,
                offset: 0,
                data: image.png.clone(),
            }),
        ] {
            dispatch_inbound(&peer.service, reg.key(), &input, message)
                .await
                .unwrap();
        }
        wait_images(&peer, 1).await;
        assert_eq!(peer.images.lock().unwrap()[0], image);
    }
    #[tokio::test]
    async fn replacement_disable_and_disconnect_invalidate_pending_decodes() {
        for action in 0..4 {
            let peer = Peer::new();
            let (reg, _control, _bulk, input) = endpoint(&peer);
            let key = reg.key();
            let permit = peer
                .service
                .inner
                .image_workers
                .clone()
                .acquire_owned()
                .await
                .unwrap();
            inject_image(&peer, key, &input, &sample_image(), 1).await;
            settled().await;
            match action {
                0 => peer
                    .service
                    .on_local_change(ClipboardContent::Text("new local copy".into())),
                1 => {
                    peer.service.set_session_enabled(key, false);
                    peer.service.set_session_enabled(key, true);
                }
                2 => drop(reg),
                _ => {
                    dispatch_inbound(
                        &peer.service,
                        key,
                        &input,
                        ControlMessage::ClipboardImageCancel(pb::ClipboardImageCancel {
                            image_id: 1,
                        }),
                    )
                    .await
                    .unwrap();
                    settled().await;
                }
            }
            drop(permit);
            settled().await;
            assert!(peer.images.lock().unwrap().is_empty(), "action {action}");
        }
    }
    fn can_open_exclusively(path: &std::path::Path) -> bool {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .is_ok()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lazy_reads_shared_handles_repeated_pastes_and_clipboard_replacement() {
        let a = Peer::new();
        let b = Peer::new();
        let _pipe = connect(&a, &b);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        let bytes = b"abcdef".repeat(10000);
        std::fs::write(&path, &bytes).unwrap();
        a.service.offer_paths(std::slice::from_ref(&path)).unwrap();
        let provider = b.provider().await;
        assert!(
            a.sink.0.lock().unwrap().is_empty(),
            "publishing metadata is not a transfer"
        );
        assert!(can_open_exclusively(&path));
        let op = provider.begin().unwrap();
        assert_eq!(read(op.clone(), 0, 3, 9).await.unwrap(), bytes[3..12]);
        assert_eq!(read(op.clone(), 0, 0, 4).await.unwrap(), bytes[..4]);
        assert!(!can_open_exclusively(&path));
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_ok());
        // A text copy revokes the offer but must retain the existing source operation.
        a.service
            .on_local_change(ClipboardContent::Text("new copy".into()));
        settled().await;
        assert!(!provider.available());
        assert!(provider.begin().is_err());
        assert_eq!(read(op.clone(), 0, 12, 7).await.unwrap(), bytes[12..19]);
        op.finish(true, "Shell confirmed");
        settled().await;
        assert!(can_open_exclusively(&path));
        assert!(a
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["state"] == "done"));
        assert_eq!(&*b.writes.lock().unwrap(), &["new copy"]);
        a.service.offer_paths(std::slice::from_ref(&path)).unwrap();
        settled().await;
        let provider = b.provider().await;
        for _ in 0..2 {
            let op = provider.begin().unwrap();
            assert_eq!(read(op.clone(), 0, 0, 1).await.unwrap(), b"a");
            op.finish(true, "Shell confirmed");
            settled().await;
        }
        assert!(can_open_exclusively(&path));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file_socket_failure_releases_operations_and_republishes_without_ending_clipboard() {
        let a = Peer::new();
        let b = Peer::new();
        let pipe = connect(&a, &b);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("reconnect-file");
        std::fs::write(&file, b"abcdef").unwrap();
        a.service.offer_paths(std::slice::from_ref(&file)).unwrap();
        let provider = b.provider().await;
        let op = provider.begin().unwrap();
        assert_eq!(read(op.clone(), 0, 0, 2).await.unwrap(), b"ab");
        a.service.control_changed(pipe.a.key(), false);
        b.service.control_changed(pipe.b.key(), false);
        a.service.control_changed(pipe.a.key(), true);
        b.service.control_changed(pipe.b.key(), true);
        let source = a
            .service
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&pipe.a.key())
            .unwrap()
            .clone();
        let manifest = source
            .source_offer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .manifest
            .clone();
        source
            .control(ControlMessage::ClipboardOffer(manifest))
            .unwrap();
        settled().await;
        assert_eq!(read(op.clone(), 0, 2, 2).await.unwrap(), b"cd");
        a.service.transport_changed(pipe.a.key(), false);
        b.service.transport_changed(pipe.b.key(), false);
        assert!(read(op, 0, 4, 2).await.is_err());
        assert!(can_open_exclusively(&file));
        assert!(a.service.session_enabled(pipe.a.key()));
        assert!(b.service.session_enabled(pipe.b.key()));
        a.service.transport_changed(pipe.a.key(), true);
        b.service.transport_changed(pipe.b.key(), true);
        settled().await;
        let operation = b.provider().await.begin().unwrap();
        assert_eq!(read(operation.clone(), 0, 0, 6).await.unwrap(), b"abcdef");
        operation.finish(true, "");
    }

    #[tokio::test]
    async fn unresolved_file_requests_are_idempotent_by_request_id() {
        let peer = Peer::new();
        let (reg, mut control, mut files, _input) = endpoint(&peer);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("idempotent");
        std::fs::write(&file, b"abc").unwrap();
        peer.service.offer_paths(&[file]).unwrap();
        let ControlMessage::ClipboardOffer(offer) = control.recv().await.unwrap() else {
            panic!("expected offer")
        };
        let entry = peer
            .service
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&reg.key())
            .unwrap()
            .clone();
        let mut start = request(offer.offer_id, 1, Action::Start);
        start.request_id = 1;
        handle_request(&entry, start.clone()).await;
        let first = files.recv().await.unwrap();
        handle_request(&entry, start).await;
        let repeated = files.recv().await.unwrap();
        assert_eq!(first.encode_frame(), repeated.encode_frame());
        assert_eq!(entry.sources.lock().unwrap().len(), 1);
        let mut read = request(offer.offer_id, 1, Action::Read);
        read.request_id = 2;
        read.length = 2;
        handle_request(&entry, read.clone()).await;
        let first = files.recv().await.unwrap();
        handle_request(&entry, read).await;
        let repeated = files.recv().await.unwrap();
        assert_eq!(first.encode_frame(), repeated.encode_frame());
        assert_eq!(
            entry
                .sources
                .lock()
                .unwrap()
                .get(&1)
                .unwrap()
                .progress
                .bytes,
            2
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_disconnect_and_disable_release_sources() {
        let a = Peer::new();
        let b = Peer::new();
        let pipe = connect(&a, &b);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, b"abc").unwrap();
        a.service.offer_paths(std::slice::from_ref(&file)).unwrap();
        let provider = b.provider().await;
        let op = provider.begin().unwrap();
        read(op.clone(), 0, 0, 1).await.unwrap();
        assert!(!can_open_exclusively(&file));
        let target_id = b.sink.0.lock().unwrap()[0]["transferId"].as_u64().unwrap() as u32;
        assert!(b.service.cancel_transfer(pipe.b.key(), target_id));
        settled().await;
        assert!(can_open_exclusively(&file));
        assert!(read(op, 0, 0, 1).await.is_err());
        let op = provider.begin().unwrap();
        read(op.clone(), 0, 0, 1).await.unwrap();
        assert!(a.service.set_session_enabled(pipe.a.key(), false));
        settled().await;
        assert!(can_open_exclusively(&file));
        assert!(read(op, 0, 0, 1).await.is_err());
        assert!(!a.service.set_session_enabled(999, true));
        drop(pipe);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_remote_session_numbers_have_distinct_connection_keys() {
        let a = Peer::new();
        let b = Peer::new();
        let c = Peer::new();
        let ab = connect(&a, &b);
        let ac = connect(&a, &c);
        assert_ne!(ab.a.key(), ac.a.key());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, b"abc").unwrap();
        a.service.offer_paths(&[file]).unwrap();
        let bp = b.provider().await;
        let cp = c.provider().await;
        let bo = bp.begin().unwrap();
        let co = cp.begin().unwrap();
        read(bo.clone(), 0, 0, 1).await.unwrap();
        read(co.clone(), 0, 0, 1).await.unwrap();
        let connection_id = ab.a.key().to_string();
        let source_id = a
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|e| e["connectionId"].as_str() == Some(connection_id.as_str()))
            .unwrap()["transferId"]
            .as_u64()
            .unwrap() as u32;
        assert!(!a.service.cancel_transfer(ac.a.key(), source_id));
        assert!(a.service.cancel_transfer(ab.a.key(), source_id));
        settled().await;
        assert!(read(bo, 0, 0, 1).await.is_err());
        assert_eq!(read(co.clone(), 0, 1, 1).await.unwrap(), b"b");
        co.finish(true, "done");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn disconnect_wakes_pending_read_without_waiting_for_timeout() {
        let a = Peer::new();
        let (_reg, _control, _bulk, _input) = endpoint(&a);
        let entry = a.service.inner.entries().pop().unwrap();
        let offer = Arc::new(RemoteOffer {
            entry: Arc::downgrade(&entry),
            manifest: Arc::new(pb::ClipboardOffer {
                offer_id: 1,
                items: vec![pb::NativeFileItem {
                    rel_path: "a".into(),
                    size: 3,
                    ..Default::default()
                }],
                total_bytes: 3,
                ..Default::default()
            }),
            valid: AtomicBool::new(true),
        });
        let op = offer.begin().unwrap();
        let task = tokio::spawn(read(op, 0, 0, 1));
        settled().await;
        entry.shutdown("连接已断开");
        assert!(tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(entry.pending.lock().unwrap().is_empty());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn opposite_directions_with_the_same_wire_operation_id_do_not_cancel_each_other() {
        let a = Peer::new();
        let b = Peer::new();
        let pipe = connect(&a, &b);
        let dir = tempfile::tempdir().unwrap();
        let a_file = dir.path().join("a");
        let b_file = dir.path().join("b");
        std::fs::write(&a_file, b"abc").unwrap();
        std::fs::write(&b_file, b"xyz").unwrap();
        a.service
            .offer_paths(std::slice::from_ref(&a_file))
            .unwrap();
        let bo = b.provider().await.begin().unwrap();
        read(bo.clone(), 0, 0, 1).await.unwrap();
        let peer_wire_id = *a.service.inner.entries()[0]
            .sources
            .lock()
            .unwrap()
            .keys()
            .next()
            .unwrap();
        b.service
            .offer_paths(std::slice::from_ref(&b_file))
            .unwrap();
        let reverse = a.provider().await;
        // Peer and local counters are independent; inject a numerical coincidence.
        a.service.inner.ids.store(peer_wire_id, Ordering::Relaxed);
        let ao = reverse.begin().unwrap();
        read(ao.clone(), 0, 0, 1).await.unwrap();
        assert!(!can_open_exclusively(&a_file));
        assert!(!can_open_exclusively(&b_file));
        assert!(a.service.cancel_transfer(pipe.a.key(), peer_wire_id));
        settled().await;
        assert!(can_open_exclusively(&b_file));
        assert!(!can_open_exclusively(&a_file));
        assert!(read(ao, 0, 1, 1).await.is_err());
        assert_eq!(read(bo.clone(), 0, 1, 1).await.unwrap(), b"b");
        bo.finish(true, "done");
        settled().await;
        assert!(can_open_exclusively(&a_file));
    }
    #[tokio::test]
    async fn mismatched_replies_and_stale_enumeration_are_rejected() {
        let a = Peer::new();
        let (_reg, mut control, _bulk, _input) = endpoint(&a);
        let entry = a.service.inner.entries().pop().unwrap();
        let rx = entry
            .queue_request(request(7, 8, Action::Start), None)
            .unwrap();
        let ControlMessage::ClipboardRequest(sent) = control.recv().await.unwrap() else {
            panic!()
        };
        entry.reply(pb::ClipboardReply {
            offer_id: 99,
            operation_id: 8,
            request_id: sent.request_id,
            ..Default::default()
        });
        assert!(rx.recv_timeout(Duration::from_millis(10)).unwrap().is_err());
        assert!(entry.pending.lock().unwrap().is_empty());
        let ended = AtomicBool::new(true);
        assert!(entry
            .queue_request(request(7, 8, Action::Start), Some(&ended))
            .is_err());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, b"x").unwrap();
        let generation = a.service.inner.invalidate_local();
        let offer = Arc::new(SourceOffer::collect(1, &[file]).unwrap());
        a.service.inner.local_text("new clipboard".into());
        a.service.inner.publish_source(offer, generation, None);
        assert!(entry.source_offer.lock().unwrap().is_none());
        while let Ok(message) = control.try_recv() {
            assert!(!matches!(message, ControlMessage::ClipboardOffer(ref o) if o.offer_id != 0));
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn preparation_errors_are_reported_to_the_peer_without_publishing_partial_files() {
        let a = Peer::new();
        let b = Peer::new();
        let _pipe = connect(&a, &b);
        let dir = tempfile::tempdir().unwrap();
        assert!(a
            .service
            .offer_paths(&[dir.path().join("missing")])
            .is_err());
        settled().await;
        assert!(b.published.lock().unwrap().is_none());
        assert!(b
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["direction"] == "receive"
                && e["state"] == "failed"
                && e["reason"].as_str().is_some_and(|r| !r.is_empty())));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn target_write_failure_is_distinct_from_native_cancellation() {
        let a = Peer::new();
        let b = Peer::new();
        let _pipe = connect(&a, &b);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, b"abc").unwrap();
        a.service.offer_paths(std::slice::from_ref(&file)).unwrap();
        let provider = b.provider().await;
        let failed = provider.begin().unwrap();
        read(failed.clone(), 0, 0, 1).await.unwrap();
        failed.finish(false, "目标文件写入失败");
        settled().await;
        assert!(a
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["state"] == "failed"));
        assert!(b
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["state"] == "failed"));
        let cancelled = provider.begin().unwrap();
        read(cancelled.clone(), 0, 0, 1).await.unwrap();
        cancelled.finish(false, "用户已取消");
        settled().await;
        assert!(a
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["state"] == "cancelled"));
        assert!(b
            .sink
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["state"] == "cancelled"));
        assert!(can_open_exclusively(&file));
    }
}

#[cfg(test)]
mod platform_tests {
    use super::*;
    #[tokio::test]
    async fn negotiated_clipboard_types_and_shutdown_do_not_touch_native_clipboard() {
        let service = ClipboardService::with_writer(Arc::new(|_, _| {}));
        let (control_tx, mut control) = mpsc::channel(8);
        let (file_tx, _files) = mpsc::channel(8);
        let (_incoming, in_rx) = mpsc::channel(8);
        let registration = service.register(SessionSpec {
            capabilities: vnt_proto::CAP_TEXT | vnt_proto::CAP_IMAGE,
            session_id: 7,
            file_tx,
            control_tx,
            in_rx,
            session_cancel: CancellationToken::new(),
            sink: Arc::new(crate::NoopEventSink),
            enabled: true,
        });
        let key = registration.key();
        service.on_local_change(ClipboardContent::Text("cross-platform text".into()));
        assert!(matches!(
            control.try_recv(),
            Ok(ControlMessage::ClipboardText(_))
        ));
        let entry = service
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .clone();
        assert!(entry
            .control(ControlMessage::ClipboardOffer(pb::ClipboardOffer::default()))
            .is_err());
        assert!(control.try_recv().is_err());
        assert!(permits(
            &ControlMessage::ClipboardImageCancel(pb::ClipboardImageCancel::default()),
            entry.capabilities
        ));
        service.set_enabled(false);
        service.on_local_change(ClipboardContent::Text("disabled".into()));
        assert!(control.try_recv().is_err());
        drop(registration);
        assert!(!service.session_enabled(key));
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;
    fn register(service:&Arc<ClipboardService>,enabled:bool)->(ClipboardRegistration,mpsc::Sender<ControlMessage>,mpsc::Receiver<ControlMessage>) {
        let (out,rx)=mpsc::channel(16);let (tx,input)=mpsc::channel(4);
        let registration=service.register(SessionSpec{capabilities:vnt_proto::CAP_TEXT,session_id:1,file_tx:out.clone(),control_tx:out,in_rx:input,session_cancel:CancellationToken::new(),sink:Arc::new(crate::NoopEventSink),enabled});
        (registration,tx,rx)
    }
    async fn send(service:&ClipboardService,key:u64,tx:&mpsc::Sender<ControlMessage>,id:u32) {
        dispatch_inbound(service,key,tx,ControlMessage::ClipboardPaste(pb::ClipboardPaste{request_id:id,text:"中文🙂".into()})).await.unwrap();
    }
    async fn reply(rx:&mut mpsc::Receiver<ControlMessage>)->pb::ClipboardPasteResult {
        let message=tokio::time::timeout(Duration::from_secs(3),rx.recv()).await.unwrap().unwrap();
        let ControlMessage::ClipboardPasteResult(result)=message else {panic!("unexpected message")};result
    }
    #[tokio::test]
    async fn confirms_paste_and_rejects_duplicates_or_controller_role() {
        let log=Arc::new(Mutex::new(vec![]));let events=log.clone();
        let service=ClipboardService::with_text_paster(Arc::new(|_,_|{}),Arc::new(move |text,valid|{assert!(valid());events.lock().unwrap().push(text);Ok(())}));
        let (guard,tx,mut rx)=register(&service,true);
        send(&service,guard.key(),&tx,1).await;
        assert!(!reply(&mut rx).await.success); // controller cannot inject input
        service.allow_remote_paste(guard.key());
        send(&service,guard.key(),&tx,2).await;assert!(reply(&mut rx).await.success);
        send(&service,guard.key(),&tx,2).await;assert!(!reply(&mut rx).await.success);
        assert_eq!(*log.lock().unwrap(),vec!["中文🙂"]);
    }
    #[tokio::test]
    async fn denies_disabled_and_offline_sessions_and_reports_native_failure() {
        let service=ClipboardService::with_text_paster(Arc::new(|_,_|{}),Arc::new(|_,_|Err("write failed".into())));
        let (guard,tx,mut rx)=register(&service,false);service.allow_remote_paste(guard.key());
        send(&service,guard.key(),&tx,1).await;assert!(!reply(&mut rx).await.success);
        service.set_session_enabled(guard.key(),true);service.control_changed(guard.key(),false);
        send(&service,guard.key(),&tx,2).await;assert!(!reply(&mut rx).await.success);
        service.control_changed(guard.key(),true);
        send(&service,guard.key(),&tx,3).await;assert_eq!(reply(&mut rx).await.reason,"write failed");
    }
    #[tokio::test]
    async fn queued_paste_expires_and_cannot_survive_channel_recovery() {
        let service=ClipboardService::with_text_paster(Arc::new(|_,_|{}),Arc::new(|_,_|panic!("stale request executed")));
        let (guard,_tx,mut rx)=register(&service,true);service.allow_remote_paste(guard.key());
        let entry=service.inner.sessions.lock().unwrap()[&guard.key()].clone();
        let (queued,mut received)=mpsc::channel(4);
        send(&service,guard.key(),&queued,1).await;
        entry.paste_received.lock().unwrap().get_mut(&1).unwrap().0=Instant::now()-Duration::from_secs(3);
        let message=received.recv().await.unwrap();
        // Send the pre-registered request straight to the receiver, preserving its receipt deadline.
        let (actual_tx,actual_rx)=mpsc::channel(4);
        actual_tx.send(message).await.unwrap();
        let task=tokio::spawn(receiver_loop(service.inner.clone(),entry.clone(),actual_rx));
        assert!(!reply(&mut rx).await.success);
        send(&service,guard.key(),&queued,2).await;
        service.control_changed(guard.key(),false);service.control_changed(guard.key(),true);
        actual_tx.send(received.recv().await.unwrap()).await.unwrap();
        assert!(!reply(&mut rx).await.success);
        task.abort();
    }
}
