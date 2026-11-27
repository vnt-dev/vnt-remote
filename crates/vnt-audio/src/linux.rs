//! PulseAudio's nonblocking API also works with PipeWire's pulse server.
//! Native objects are owned by the audio worker; no callback blocks networking.
use crate::{playback::Player, PlaybackDevice};
use libpulse_binding as pulse;
use pulse::{
    callbacks::ListResult,
    context::{
        subscribe::InterestMaskSet, Context, FlagSet as ContextFlags, State as ContextState,
    },
    def::BufferAttr,
    mainloop::standard::{IterateResult, Mainloop},
    operation::State as OperationState,
    sample::{Format, Spec},
    stream::{FlagSet, PeekResult, SeekMode, State as StreamState, Stream},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};
const TIMEOUT: Duration = Duration::from_secs(1);
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
#[derive(Clone)]
struct Device {
    info: PlaybackDevice,
    monitor: String,
}
struct Pulse {
    stream: Option<Stream>,
    context: Context,
    mainloop: Mainloop,
    changed: Rc<Cell<bool>>,
}
impl Pulse {
    fn connect() -> Result<Self, String> {
        let mainloop = Mainloop::new().ok_or("无法创建音频事件循环")?;
        let mut context = Context::new(&mainloop, "VNT Remote").ok_or("无法创建音频上下文")?;
        context
            .connect(None, ContextFlags::NOAUTOSPAWN, None)
            .map_err(error)?;
        let mut pulse = Self {
            stream: None,
            context,
            mainloop,
            changed: Rc::new(Cell::new(false)),
        };
        let due = Instant::now() + TIMEOUT;
        loop {
            match pulse.context.get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated => {
                    return Err("无法连接 PulseAudio / pipewire-pulse".into())
                }
                _ => pulse.wait(due)?,
            }
        }
        let changed = pulse.changed.clone();
        pulse
            .context
            .set_subscribe_callback(Some(Box::new(move |facility, _, _| {
                if matches!(
                    facility,
                    Some(
                        pulse::context::subscribe::Facility::Sink
                            | pulse::context::subscribe::Facility::Server
                    )
                ) {
                    changed.set(true);
                }
            })));
        pulse
            .context
            .subscribe(InterestMaskSet::SINK | InterestMaskSet::SERVER, |_| {});
        Ok(pulse)
    }
    fn tick(&mut self) -> Result<(), String> {
        if matches!(
            self.mainloop.iterate(false),
            IterateResult::Err(_) | IterateResult::Quit(_)
        ) {
            return Err("音频事件循环已停止".into());
        }
        if matches!(
            self.context.get_state(),
            ContextState::Failed | ContextState::Terminated
        ) {
            return Err("音频服务已断开".into());
        }
        if self
            .stream
            .as_ref()
            .is_some_and(|s| matches!(s.get_state(), StreamState::Failed | StreamState::Terminated))
        {
            return Err("音频设备已断开".into());
        }
        Ok(())
    }
    fn wait(&mut self, deadline: Instant) -> Result<(), String> {
        if Instant::now() >= deadline {
            return Err("音频服务响应超时".into());
        }
        self.tick()?;
        std::thread::park_timeout(Duration::from_millis(2));
        Ok(())
    }
    fn devices(&mut self) -> Result<Vec<Device>, String> {
        let default = Rc::new(RefCell::new(String::new()));
        let result = default.clone();
        let mut operation = self.context.introspect().get_server_info(move |info| {
            *result.borrow_mut() = info.default_sink_name.as_deref().unwrap_or_default().into();
        });
        let deadline = Instant::now() + TIMEOUT;
        while operation.get_state() == OperationState::Running {
            if let Err(e) = self.wait(deadline) {
                operation.cancel();
                return Err(e);
            }
        }
        let devices = Rc::new(RefCell::new(Vec::new()));
        let result = devices.clone();
        let default = default.borrow().clone();
        let failed = Rc::new(Cell::new(false));
        let failure = failed.clone();
        let mut operation = self
            .context
            .introspect()
            .get_sink_info_list(move |item| match item {
                ListResult::Item(info) => {
                    if let (Some(name), Some(monitor)) = (&info.name, &info.monitor_source_name) {
                        result.borrow_mut().push(Device {
                            info: PlaybackDevice {
                                id: name.to_string(),
                                name: info.description.as_deref().unwrap_or(name).into(),
                                is_default: name.as_ref() == default,
                            },
                            monitor: monitor.to_string(),
                        });
                    }
                }
                ListResult::Error => failure.set(true),
                _ => (),
            });
        let deadline = Instant::now() + TIMEOUT;
        while operation.get_state() == OperationState::Running {
            if let Err(e) = self.wait(deadline) {
                operation.cancel();
                return Err(e);
            }
        }
        if failed.get() {
            return Err("无法枚举音频设备".into());
        }
        let mut result = devices.borrow().clone();
        result.sort_by(|a, b| a.info.id.cmp(&b.info.id));
        Ok(result)
    }
    fn open(&mut self, record: bool, name: &str) -> Result<(), String> {
        if name.contains('\0') {
            return Err("音频设备名称无效".into());
        }
        let spec = Spec {
            format: Format::FLOAT32NE,
            channels: 2,
            rate: 48000,
        };
        let mut stream = Stream::new(
            &mut self.context,
            if record {
                "System audio"
            } else {
                "Remote audio"
            },
            &spec,
            None,
        )
        .ok_or("无法创建音频流")?;
        let attr = BufferAttr {
            maxlength: 48000 * 8 / 10,
            tlength: 480 * 8 * 2,
            prebuf: 480 * 8,
            minreq: 480 * 8,
            fragsize: 480 * 8,
        };
        let flags = FlagSet::ADJUST_LATENCY | FlagSet::DONT_MOVE;
        if record {
            stream
                .connect_record(Some(name), Some(&attr), flags)
                .map_err(error)?;
        } else {
            stream
                .connect_playback(Some(name), Some(&attr), flags, None, None)
                .map_err(error)?;
        }
        self.stream = Some(stream);
        let deadline = Instant::now() + TIMEOUT;
        while self.stream.as_ref().unwrap().get_state() != StreamState::Ready {
            self.wait(deadline)?;
        }
        self.changed.set(false);
        Ok(())
    }
}
impl Drop for Pulse {
    fn drop(&mut self) {
        if let Some(stream) = &mut self.stream {
            let _ = stream.disconnect();
        }
        self.context.disconnect();
    }
}
pub fn list_playback_devices() -> Result<Vec<PlaybackDevice>, String> {
    Pulse::connect()?
        .devices()
        .map(|v| v.into_iter().map(|d| d.info).collect())
}
fn select(devices: &[Device], id: Option<&str>) -> Result<(Device, bool), String> {
    if let Some(found) = id.and_then(|id| devices.iter().find(|d| d.info.id == id)) {
        return Ok((found.clone(), false));
    }
    devices
        .iter()
        .find(|d| d.info.is_default)
        .cloned()
        .map(|d| (d, id.is_some()))
        .ok_or("暂无默认音频输出设备".into())
}
pub struct Capture {
    pulse: Pulse,
    format: crate::format::PcmFormat,
    device: String,
}
impl Capture {
    pub fn open() -> Result<Self, String> {
        let mut pulse = Pulse::connect()?;
        let (device, _) = select(&pulse.devices()?, None)?;
        pulse.open(true, &device.monitor)?;
        let format = crate::format::PcmFormat {
            rate: 48000,
            channels: 2,
            bits: 32,
            float: true,
            mask: 3,
        };
        format.validate()?;
        Ok(Self {
            pulse,
            format,
            device: device.info.id,
        })
    }
    pub fn rate(&self) -> u32 {
        self.format.rate
    }
    pub fn read(&mut self) -> Result<Vec<[f32; 2]>, String> {
        self.pulse.tick()?;
        if self.pulse.changed.replace(false) {
            let (default, _) = select(&self.pulse.devices()?, None)?;
            if default.info.id != self.device {
                return Err("系统音频输出已变化".into());
            }
        }
        let stream = self.pulse.stream.as_mut().unwrap();
        let result = match stream.peek().map_err(error)? {
            PeekResult::Data(data) => self.format.decode(&data[..data.len().min(4800 * 8)]),
            PeekResult::Hole(n) => vec![[0.; 2]; (n / 8).min(4800)],
            PeekResult::Empty => {
                std::thread::park_timeout(Duration::from_millis(2));
                return Ok(Vec::new());
            }
        };
        stream.discard().map_err(error)?;
        Ok(result)
    }
}
pub struct Render {
    pulse: Pulse,
    device: String,
    follow_default: bool,
    fallback: bool,
}
impl Render {
    pub fn open_on_device(id: Option<&str>) -> Result<Self, String> {
        let mut pulse = Pulse::connect()?;
        let (device, fallback) = select(&pulse.devices()?, id)?;
        pulse.open(false, &device.info.id)?;
        Ok(Self {
            pulse,
            device: device.info.id,
            follow_default: id.is_none() || fallback,
            fallback,
        })
    }
    pub fn rate(&self) -> u32 {
        48000
    }
    pub fn is_fallback(&self) -> bool {
        self.fallback
    }
    pub fn write(&mut self, player: &mut Player) -> Result<(), String> {
        self.pulse.tick()?;
        if self.pulse.changed.replace(false)
            && !self
                .pulse
                .devices()?
                .iter()
                .any(|d| d.info.id == self.device && (!self.follow_default || d.info.is_default))
        {
            return Err("音频播放设备已变化".into());
        }
        let stream = self.pulse.stream.as_mut().unwrap();
        let frames = stream.writable_size().unwrap_or(0).min(480 * 8) / 8;
        if frames == 0 {
            std::thread::park_timeout(Duration::from_millis(2));
            return Ok(());
        }
        let bytes: Vec<u8> = player
            .read(frames)?
            .into_iter()
            .flat_map(|v| v.into_iter().flat_map(f32::to_ne_bytes))
            .collect();
        stream
            .write_copy(&bytes, 0, SeekMode::Relative)
            .map_err(error)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn device(id: &str, default: bool) -> Device {
        Device {
            info: PlaybackDevice {
                id: id.into(),
                name: id.into(),
                is_default: default,
            },
            monitor: format!("{id}.monitor"),
        }
    }
    #[test]
    fn explicit_devices_and_missing_device_fallback() {
        let devices = vec![device("first", true), device("second", false)];
        assert_eq!(
            select(&devices, Some("second")).unwrap().0.info.id,
            "second"
        );
        let (selected, fallback) = select(&devices, Some("removed")).unwrap();
        assert_eq!(selected.monitor, "first.monitor");
        assert!(fallback);
        assert!(!select(&devices, None).unwrap().1);
        assert!(select(&[], None).is_err());
    }
    #[test]
    #[ignore = "requires an isolated PulseAudio server and pacat"]
    fn linux_native_loopback_playback_and_prompt_shutdown() {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let mut capture = Capture::open().unwrap();
        let mut tone = Command::new("pacat")
            .args([
                "--playback",
                "--raw",
                "--rate=48000",
                "--channels=2",
                "--format=float32ne",
                "--device=vnt_test",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = tone.stdin.take().unwrap();
        let writing = std::thread::spawn(move || {
            let data: Vec<u8> = (0..48000)
                .flat_map(|i| {
                    let value = (i as f32 * 440. * std::f32::consts::TAU / 48000.).sin() * 0.2;
                    [value, value].into_iter().flat_map(f32::to_ne_bytes)
                })
                .collect();
            let _ = stdin.write_all(&data);
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut audible = false;
        while Instant::now() < deadline {
            if capture.read().unwrap().iter().any(|v| v[0].abs() > 0.05) {
                audible = true;
                break;
            }
        }
        let _ = tone.kill();
        let _ = tone.wait();
        let _ = writing.join();
        assert!(
            audible,
            "default output monitor must capture the played tone"
        );
        drop(capture);
        let playback = crate::Playback::start_on_device(Some("missing-device".into())).unwrap();
        let sender = playback.sender();
        sender.set_state(vnt_proto::pb::AudioState {
            available: true,
            generation: 1,
            reason: String::new(),
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !sender.status().available {
            assert!(Instant::now() < deadline, "{:?}", sender.status());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(sender.status().reason.contains("默认"));
        let shared = crate::SharedAudio::new(false);
        let subscription = shared.subscribe().unwrap();
        let started = Instant::now();
        drop(subscription);
        shared.stop();
        drop(playback);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
