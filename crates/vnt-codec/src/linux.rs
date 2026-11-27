//! Linux hardware encoding is isolated behind a replaceable, private C ABI.
use crate::{
    color, dependencies::FrameTracker, EncodeError, EncodeParams, EncodedPacket, VideoEncoder,
};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    path::PathBuf,
    ptr::NonNull,
    sync::{Arc, OnceLock},
};
use vnt_proto::{negotiate::EncoderCap, Codec};
type Open = unsafe extern "C" fn(
    i32,
    i32,
    *const c_char,
    u32,
    u32,
    u32,
    u32,
    u32,
    *mut c_char,
    usize,
) -> *mut c_void;
type Close = unsafe extern "C" fn(*mut c_void);
type Encode = unsafe extern "C" fn(
    *mut c_void,
    *const u8,
    usize,
    u64,
    i32,
    *mut *const u8,
    *mut usize,
    *mut i32,
    *mut c_char,
    usize,
) -> i32;
struct Api {
    _library: libloading::os::unix::Library,
    open: Open,
    close: Close,
    encode: Encode,
    count: unsafe extern "C" fn() -> i32,
    path: PathBuf,
}
static PATH: OnceLock<PathBuf> = OnceLock::new();
static API: OnceLock<Result<Arc<Api>, String>> = OnceLock::new();
/// Called by the desktop app before starting hosts. An explicitly selected
/// resource path never falls back to a build-machine path in packaged apps.
pub fn set_backend_path(path: PathBuf) {
    let _ = PATH.set(path);
}
fn api() -> Result<Arc<Api>, String> {
    API.get_or_init(|| {
        let path = PATH
            .get()
            .cloned()
            .or_else(|| std::env::var_os("VNT_LINUX_ENCODER_LIB").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(env!("VNT_LINUX_ENCODER_BUILD_PATH")));
        // The module has private FFmpeg symbols and distinct VA SONAMEs.
        // Deep binding prevents a previously loaded GStreamer library from
        // interposing its versions of the module's private dependencies.
        let library = unsafe {
            libloading::os::unix::Library::open(
                Some(&path),
                libc::RTLD_NOW | libc::RTLD_LOCAL | libc::RTLD_DEEPBIND,
            )
        }
        .map_err(|e| format!("{}: {e}", path.display()))?;
        // All symbol signatures match linux/encoder.c; the Library stays alive
        // through the Arc held by every native handle.
        unsafe {
            let version: libloading::os::unix::Symbol<unsafe extern "C" fn() -> u32> = library
                .get(b"vnt_hw_abi_version\0")
                .map_err(|e| e.to_string())?;
            if version() != 1 {
                return Err("Linux encoder ABI mismatch".into());
            }
            Ok(Arc::new(Api {
                open: *library
                    .get::<Open>(b"vnt_hw_open\0")
                    .map_err(|e| e.to_string())?,
                close: *library
                    .get::<Close>(b"vnt_hw_close\0")
                    .map_err(|e| e.to_string())?,
                encode: *library
                    .get::<Encode>(b"vnt_hw_encode\0")
                    .map_err(|e| e.to_string())?,
                count: *library
                    .get(b"vnt_hw_nvenc_devices\0")
                    .map_err(|e| e.to_string())?,
                _library: library,
                path,
            }))
        }
    })
    .clone()
}
#[derive(Clone, Debug)]
struct Candidate {
    vaapi: bool,
    device: String,
}
impl Candidate {
    fn name(&self) -> String {
        format!(
            "{} ({})",
            if self.vaapi { "VA-API" } else { "NVENC" },
            self.device
        )
    }
}
fn candidates(api: &Api) -> Vec<Candidate> {
    // CUDA enumeration is read-only and returns zero without a working driver.
    let count = unsafe { (api.count)() }.clamp(0, 32);
    let mut result: Vec<_> = (0..count)
        .map(|i| Candidate {
            vaapi: false,
            device: i.to_string(),
        })
        .collect();
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    result.extend(nodes.into_iter().map(|p| Candidate {
        vaapi: true,
        device: p.to_string_lossy().into_owned(),
    }));
    result
}
fn codec_id(codec: Codec) -> i32 {
    match codec {
        Codec::H264 => 0,
        Codec::Hevc => 1,
        Codec::Av1 => 2,
    }
}
struct Handle {
    pointer: NonNull<c_void>,
    api: Arc<Api>,
}
// Native contexts have exclusive ownership; FFmpeg handles their device context
// binding on each call. No object is accessed concurrently after moving it.
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { (self.api.close)(self.pointer.as_ptr()) };
    }
}
fn native_error(buffer: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}
struct Hardware {
    codec: Codec,
    candidate: Candidate,
    name: String,
    api: Arc<Api>,
    handle: Option<Handle>,
    params: Option<EncodeParams>,
    nv12: Vec<u8>,
    key: bool,
}
impl Hardware {
    fn new(codec: Codec, candidate: Candidate, api: Arc<Api>) -> Self {
        Self {
            codec,
            name: candidate.name(),
            candidate,
            api,
            handle: None,
            params: None,
            nv12: Vec::new(),
            key: true,
        }
    }
}
impl VideoEncoder for Hardware {
    fn codec(&self) -> Codec {
        self.codec
    }
    fn is_hardware(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn configure(&mut self, p: &EncodeParams) -> Result<(), EncodeError> {
        p.validate()?;
        self.handle.take();
        self.params = None;
        if self.candidate.vaapi {
            // Report permission/device removal explicitly rather than the
            // generic AVERROR(EINVAL) emitted by some VA drivers.
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.candidate.device)
                .map_err(|e| EncodeError::Create(format!("{}: {e}", self.name)))?;
        }
        let device = CString::new(self.candidate.device.as_str())
            .map_err(|e| EncodeError::Create(e.to_string()))?;
        let mut error = [0 as c_char; 1024];
        let ptr = unsafe {
            (self.api.open)(
                codec_id(self.codec),
                i32::from(self.candidate.vaapi),
                device.as_ptr(),
                p.width,
                p.height,
                p.fps,
                p.bitrate_kbps,
                p.gop,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let pointer = NonNull::new(ptr).ok_or_else(|| {
            EncodeError::Create(format!("{}: {}", self.name, native_error(&error)))
        })?;
        self.handle = Some(Handle {
            pointer,
            api: self.api.clone(),
        });
        self.params = Some(*p);
        self.nv12
            .resize(p.width as usize * p.height as usize * 3 / 2, 0);
        self.key = true;
        Ok(())
    }
    fn encode(
        &mut self,
        bgra: &[u8],
        pts: u64,
        out: &mut Vec<EncodedPacket>,
    ) -> Result<(), EncodeError> {
        let p = self
            .params
            .ok_or_else(|| EncodeError::Encode("编码器尚未配置".into()))?;
        if bgra.len() != p.width as usize * p.height as usize * 4 {
            return Err(EncodeError::Encode("BGRA 尺寸不匹配".into()));
        }
        color::bgra_to_nv12(bgra, p.width as usize, p.height as usize, &mut self.nv12);
        let handle = self.handle.as_ref().unwrap();
        let mut bytes = std::ptr::null();
        let mut length = 0;
        let mut key = 0;
        let mut error = [0 as c_char; 1024];
        let result = unsafe {
            (self.api.encode)(
                handle.pointer.as_ptr(),
                self.nv12.as_ptr(),
                self.nv12.len(),
                pts,
                i32::from(self.key),
                &mut bytes,
                &mut length,
                &mut key,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if result != 0 {
            return Err(EncodeError::Encode(native_error(&error)));
        }
        if bytes.is_null() || length == 0 || length > vnt_proto::udp::MAX_FRAME_BYTES {
            return Err(EncodeError::Encode("硬编输出长度无效".into()));
        }
        let data = unsafe { std::slice::from_raw_parts(bytes, length) }.to_vec();
        out.push(EncodedPacket {
            keyframe: key != 0,
            pts_ms: pts,
            data,
        });
        self.key = false;
        Ok(())
    }
    fn force_keyframe(&mut self) -> Result<(), EncodeError> {
        self.key = true;
        Ok(())
    }
}
pub(crate) fn software(codec: Codec) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    Ok(match codec {
        Codec::H264 => Box::new(crate::openh264_enc::OpenH264Encoder::new()?),
        Codec::Hevc => Box::new(crate::kvazaar::KvazaarEncoder::new()?),
        Codec::Av1 => Box::new(crate::svt_av1::SvtAv1Encoder::new()?),
    })
}
fn trial(encoder: &mut dyn VideoEncoder) -> Result<(), EncodeError> {
    let mut p = EncodeParams::new(256, 256, 30, 1000);
    p.gop = 30;
    trial_parameters(encoder, &p)
}
fn trial_parameters(encoder: &mut dyn VideoEncoder, p: &EncodeParams) -> Result<(), EncodeError> {
    encoder.configure(p)?;
    let mut tracker = FrameTracker::new(encoder.codec());
    for i in 0..3 {
        if i == 2 {
            encoder.force_keyframe()?;
            tracker.reset(2);
        }
        let mut packets = Vec::new();
        encoder.encode(
            &vec![64 + i as u8; p.width as usize * p.height as usize * 4],
            i * 33,
            &mut packets,
        )?;
        if packets.is_empty() {
            return Err(EncodeError::Encode("探测未输出低延迟视频帧".into()));
        }
        for packet in packets {
            let dependencies = tracker.frame(&packet.data, packet.keyframe);
            if dependencies.unknown
                || ((i == 0 || i == 2) && (!packet.keyframe || !dependencies.independent))
            {
                return Err(EncodeError::Encode("探测码流或强制关键帧无效".into()));
            }
        }
    }
    Ok(())
}
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub codec: Codec,
    pub backend: String,
    pub available: bool,
    pub reason: String,
}
pub fn diagnostics() -> Vec<Diagnostic> {
    let mut result = Vec::new();
    match api() {
        Err(reason) => result.push(Diagnostic {
            codec: Codec::H264,
            backend: "Linux hardware library".into(),
            available: false,
            reason,
        }),
        Ok(api) => {
            let choices = candidates(&api);
            if choices.is_empty() {
                result.push(Diagnostic {
                    codec: Codec::H264,
                    backend: api.path.display().to_string(),
                    available: false,
                    reason: "未发现可用的 NVENC / DRM render 设备".into(),
                });
            }
            for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
                for choice in &choices {
                    let mut encoder = Hardware::new(codec, choice.clone(), api.clone());
                    let error = trial(&mut encoder).err();
                    result.push(Diagnostic {
                        codec,
                        backend: encoder.name().into(),
                        available: error.is_none(),
                        reason: error.map(|e| e.to_string()).unwrap_or_default(),
                    });
                }
            }
        }
    }
    result
}
pub(crate) fn probe_caps() -> Vec<EncoderCap> {
    let mut caps = Vec::new();
    let diagnostics = diagnostics();
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        if let Some(d) = diagnostics.iter().find(|d| d.codec == codec && d.available) {
            caps.push(EncoderCap {
                codec,
                hardware: true,
                name: d.backend.clone(),
            });
        }
    }
    static SOFTWARE: OnceLock<Vec<EncoderCap>> = OnceLock::new();
    caps.extend(
        SOFTWARE
            .get_or_init(|| {
                let mut software_caps = Vec::new();
                for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
                    if let Ok(mut encoder) = software(codec) {
                        match trial(encoder.as_mut()) {
                            Ok(()) => software_caps.push(EncoderCap {
                                codec,
                                hardware: false,
                                name: encoder.name().into(),
                            }),
                            Err(error) => log::warn!("不公布 {codec:?} 软件编码能力：{error}"),
                        }
                    }
                }
                software_caps
            })
            .iter()
            .cloned(),
    );
    caps
}
/// Strict mode is used by GPU acceptance tests and must never pass via software.
pub fn strict_hardware(
    codec: Codec,
    p: &EncodeParams,
) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    hardware(codec, p)
}
fn hardware(codec: Codec, p: &EncodeParams) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    let api = api().map_err(EncodeError::Create)?;
    let mut failures = Vec::new();
    for candidate in candidates(&api) {
        let mut encoder = Hardware::new(codec, candidate, api.clone());
        // Prove the requested dimensions, rate and bitrate before Welcome.
        // Reopen after the trial so probe timestamps/references never enter the stream.
        match trial_parameters(&mut encoder, p).and_then(|_| encoder.configure(p)) {
            Ok(()) => return Ok(Box::new(encoder)),
            Err(e) => failures.push(e.to_string()),
        }
    }
    Err(EncodeError::Create(if failures.is_empty() {
        "未发现可访问的硬编设备".into()
    } else {
        failures.join("; ")
    }))
}
/// The wrapper latches a software fallback for this stream. A newly created
/// stream may probe hardware again, but a failing device cannot cause flapping.
type HardwareOpen = fn(Codec, &EncodeParams) -> Result<Box<dyn VideoEncoder>, EncodeError>;

pub struct LinuxEncoder {
    codec: Codec,
    prefer_hardware: bool,
    hardware_open: HardwareOpen,
    inner: Option<Box<dyn VideoEncoder>>,
    params: Option<EncodeParams>,
}
impl LinuxEncoder {
    pub fn new(codec: Codec, prefer_hardware: bool) -> Self {
        Self {
            codec,
            prefer_hardware,
            hardware_open: hardware,
            inner: None,
            params: None,
        }
    }
}
impl VideoEncoder for LinuxEncoder {
    fn codec(&self) -> Codec {
        self.codec
    }
    fn is_hardware(&self) -> bool {
        self.inner.as_ref().is_some_and(|e| e.is_hardware())
    }
    fn name(&self) -> &str {
        self.inner.as_ref().map_or("unconfigured", |e| e.name())
    }
    fn configure(&mut self, p: &EncodeParams) -> Result<(), EncodeError> {
        p.validate()?;
        if self.params == Some(*p) {
            return Ok(());
        }
        if let Some(inner) = &mut self.inner {
            match inner.configure(p) {
                Ok(()) => {
                    self.params = Some(*p);
                    return Ok(());
                }
                Err(e) if inner.is_hardware() => {
                    log::warn!("硬编重配失败，回退同格式软件编码：{e}")
                }
                Err(e) => return Err(e),
            }
        } else if self.prefer_hardware {
            match (self.hardware_open)(self.codec, p) {
                Ok(encoder) => {
                    self.inner = Some(encoder);
                    self.params = Some(*p);
                    return Ok(());
                }
                Err(e) => log::info!("硬编不可用，使用同格式软件编码：{e}"),
            }
        }
        let mut software = software(self.codec)?;
        software.configure(p)?;
        self.inner = Some(software);
        self.prefer_hardware = false;
        self.params = Some(*p);
        Ok(())
    }
    fn encode(
        &mut self,
        bgra: &[u8],
        pts: u64,
        out: &mut Vec<EncodedPacket>,
    ) -> Result<(), EncodeError> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| EncodeError::Encode("编码器尚未配置".into()))?;
        let mut packets = Vec::new();
        match inner.encode(bgra, pts, &mut packets) {
            Ok(()) => {
                out.extend(packets);
                Ok(())
            }
            Err(e) if inner.is_hardware() => {
                log::warn!("硬编运行失败，回退同格式软件编码：{e}");
                let mut encoder = software(self.codec)?;
                encoder.configure(&self.params.unwrap())?;
                encoder.force_keyframe()?;
                let mut fresh = Vec::new();
                encoder.encode(bgra, pts, &mut fresh)?;
                self.inner = Some(encoder);
                self.prefer_hardware = false;
                out.extend(fresh);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    fn force_keyframe(&mut self) -> Result<(), EncodeError> {
        self.inner.as_mut().map_or(Ok(()), |e| e.force_keyframe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Broken {
        codec: Codec,
        configure_fails: bool,
    }
    impl VideoEncoder for Broken {
        fn codec(&self) -> Codec {
            self.codec
        }
        fn is_hardware(&self) -> bool {
            true
        }
        fn name(&self) -> &str {
            "test hardware"
        }
        fn configure(&mut self, _: &EncodeParams) -> Result<(), EncodeError> {
            if self.configure_fails {
                Err(EncodeError::Reconfigure("device removed".into()))
            } else {
                Ok(())
            }
        }
        fn force_keyframe(&mut self) -> Result<(), EncodeError> {
            Ok(())
        }
        fn encode(
            &mut self,
            _: &[u8],
            pts: u64,
            out: &mut Vec<EncodedPacket>,
        ) -> Result<(), EncodeError> {
            out.push(EncodedPacket {
                keyframe: false,
                pts_ms: pts,
                data: vec![0xff],
            });
            Err(EncodeError::Encode(
                "device removed after partial output".into(),
            ))
        }
    }
    #[test]
    fn initialization_reconfiguration_and_partial_encode_failures_preserve_codec() {
        for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
            let p = EncodeParams::new(128, 96, 30, 1000);
            let mut initialize = LinuxEncoder::new(codec, true);
            initialize.hardware_open = |_, _| {
                Err(EncodeError::Create(
                    "permission denied / unsupported format".into(),
                ))
            };
            initialize.configure(&p).unwrap();
            assert!(!initialize.is_hardware());
            assert_eq!(initialize.codec(), codec);
            let mut resize = LinuxEncoder::new(codec, true);
            resize.inner = Some(Box::new(Broken {
                codec,
                configure_fails: true,
            }));
            resize.params = Some(EncodeParams::new(256, 256, 30, 1000));
            resize.configure(&p).unwrap();
            assert!(!resize.is_hardware());
            assert_eq!(resize.codec(), codec);
            let mut running = LinuxEncoder::new(codec, true);
            running.inner = Some(Box::new(Broken {
                codec,
                configure_fails: false,
            }));
            running.params = Some(p);
            let mut output = Vec::new();
            running
                .encode(&vec![80; 128 * 96 * 4], 77, &mut output)
                .unwrap();
            assert!(!running.is_hardware());
            assert_eq!(running.codec(), codec);
            assert!(!running.prefer_hardware);
            assert!(!output.is_empty());
            assert!(output[0].keyframe);
            assert_eq!(output[0].pts_ms, 77);
            assert!(output.iter().all(|p| p.data != [0xff]));
            let mut parser = FrameTracker::new(codec);
            assert!(parser.frame(&output[0].data, true).independent);
            running
                .configure(&EncodeParams::new(128, 96, 25, 1500))
                .unwrap();
            assert!(!running.is_hardware());
        }
    }
    #[test]
    fn concurrent_software_streams_reconfigure_and_exit_with_recoverable_frames() {
        std::thread::scope(|scope| {
            let jobs: Vec<_> = [Codec::H264, Codec::Hevc, Codec::Av1]
                .into_iter()
                .flat_map(|codec| [codec, codec])
                .map(|codec| {
                    scope.spawn(move || {
                        let mut encoder = LinuxEncoder::new(codec, false);
                        let mut pts = 0;
                        for (width, height, fps, kbps) in
                            [(128, 96, 30, 1000), (160, 128, 25, 1500)]
                        {
                            encoder
                                .configure(&EncodeParams::new(width, height, fps, kbps))
                                .unwrap();
                            let mut tracker = FrameTracker::new(codec);
                            for index in 0..4 {
                                if index == 2 {
                                    encoder.force_keyframe().unwrap();
                                    tracker.reset(2);
                                }
                                let mut packets = Vec::new();
                                encoder
                                    .encode(
                                        &vec![90; width as usize * height as usize * 4],
                                        pts,
                                        &mut packets,
                                    )
                                    .unwrap();
                                assert_eq!(packets.len(), 1, "low-delay frame count");
                                assert_eq!(packets[0].pts_ms, pts);
                                let frame = tracker.frame(&packets[0].data, packets[0].keyframe);
                                assert!(!frame.unknown);
                                if index == 0 || index == 2 {
                                    assert!(frame.independent && packets[0].keyframe);
                                }
                                pts += 40;
                            }
                        }
                        assert!(!encoder.is_hardware());
                        // Drop each configured encoder while the other sessions
                        // remain live: teardown must not corrupt their global state.
                    })
                })
                .collect();
            for job in jobs {
                job.join().unwrap();
            }
        });
    }
    #[test]
    #[ignore = "requires an explicitly prepared GPU runner; never uses software"]
    fn gpu_keyframes_timestamps_reconfiguration_and_shutdown() {
        let requested =
            std::env::var("VNT_GPU_TEST_CODECS").expect("set the GPU's required codecs");
        for name in requested.split(',') {
            let codec = match name {
                "h264" => Codec::H264,
                "hevc" => Codec::Hevc,
                "av1" => Codec::Av1,
                _ => panic!("invalid codec"),
            };
            let mut encoder =
                strict_hardware(codec, &EncodeParams::new(640, 480, 30, 2000)).unwrap();
            eprintln!(
                "GPU acceptance {codec:?}: {} hardware={}",
                encoder.name(),
                encoder.is_hardware()
            );
            assert!(encoder.is_hardware());
            let mut pts = 0;
            for (width, height, fps, kbps) in [(640, 480, 30, 2000), (384, 256, 25, 1500)] {
                encoder
                    .configure(&EncodeParams::new(width, height, fps, kbps))
                    .unwrap();
                let mut tracker = FrameTracker::new(codec);
                for index in 0..6 {
                    if index == 3 {
                        encoder.force_keyframe().unwrap();
                        tracker.reset(2);
                    }
                    let mut packets = Vec::new();
                    encoder
                        .encode(
                            &vec![80; width as usize * height as usize * 4],
                            pts,
                            &mut packets,
                        )
                        .unwrap();
                    assert_eq!(packets.len(), 1);
                    assert_eq!(packets[0].pts_ms, pts);
                    let parsed = tracker.frame(&packets[0].data, packets[0].keyframe);
                    assert!(!parsed.unknown);
                    if index == 0 || index == 3 {
                        assert!(packets[0].keyframe && parsed.independent);
                    }
                    assert!(encoder.is_hardware());
                    pts += 40;
                }
            }
        }
    }
    #[test]
    fn bundled_abi_loads_without_exporting_ffmpeg_symbols() {
        let api = api().expect("build must produce the replaceable module");
        assert!(unsafe {
            api._library
                .get::<unsafe extern "C" fn() -> u32>(b"avcodec_version\0")
        }
        .is_err());
    }
}
