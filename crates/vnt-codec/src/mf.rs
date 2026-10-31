//! Media Foundation MFT 视频编码器（Windows）。
//!
//! - 硬件优先：`MFTEnumEx(HARDWARE | ASYNCMFT)` 自动枚举 NVENC/QSV/AMF 的
//!   H264/HEVC 编码器；异步 MFT 按 `MF_TRANSFORM_ASYNC_UNLOCK` + 事件驱动使用
//!   （事件在同线程以 NO_WAIT 轮询消费，语义对齐 ffmpeg mfenc）。
//! - 软件兜底：微软内置 "Microsoft H.264 Video Encoder MFT"（同步 MFT）。
//! - 输出为 Annex B 裸流（含起始码），SPS/PPS 随 IDR 内联。
//! - 平均目标码率 VBR，保留低延迟模式；目标码率支持运行时热更新。
//! - AV1 硬件 MFT 输出为 IVF 容器封装（`DKIF` 文件头 + 逐帧 12 字节帧头），
//!   出编码器前剥成裸 OBU 低开销码流（见 [`av1_strip_ivf`]）。
//!
//! 输入统一 BGRA → NV12（见 `color`），NV12 是编码 MFT 的通用输入格式。
//!
//! 注：windows crate 0.61 的 COM 包装不含 `Send`；编码器固定在单线程
//! 使用（服务端管线线程），跨线程传递处用 [`SendPtr`] 显式声明。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use vnt_proto::negotiate::EncoderCap;

use crate::color;
use crate::{EncodeError, EncodeParams, EncodedPacket, VideoEncoder};

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows_core::{Interface, GUID};

/// COM 指针的 Send 包装（调用方保证实际使用线程安全：MFT 为自由线程模型）。
struct SendPtr<T>(T);
// These specific MF interfaces are free-threaded. Do not make arbitrary T Send.
unsafe impl Send for SendPtr<IMFTransform> {}
unsafe impl Send for SendPtr<ICodecAPI> {}
unsafe impl Send for SendPtr<IMFMediaEventGenerator> {}
// MFCreateSample / MFCreateMemoryBuffer 产出的是自由线程敏捷对象。
unsafe impl Send for SendPtr<IMFSample> {}
unsafe impl Send for SendPtr<IMFMediaBuffer> {}

impl<T> std::ops::Deref for SendPtr<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// 进程级初始化（MFStartup / COM）
// ---------------------------------------------------------------------------

fn ensure_mf_started() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    });
}

// Parallel encoder discovery/teardown on the test machine produced an Event
// 1000 crash in IntelControlLib.dll_unloaded (0xc0000005). Intel callbacks can
// outlive the last MFT reference. Pin only an already loaded module, using the
// same lifetime rule as the WGC callback module; encoder/GPU objects still drop.
fn pin_loaded_intel_control_module() {
    use windows::{
        core::w,
        Win32::{
            Foundation::HMODULE,
            System::LibraryLoader::{GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_PIN},
        },
    };
    static PINNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if PINNED.get().is_some() {
        return;
    }
    let mut module = HMODULE::default();
    if unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_PIN,
            w!("IntelControlLib.dll"),
            &mut module,
        )
    }
    .is_ok()
    {
        let _ = PINNED.set(());
    }
}

fn com_init_on_this_thread() {
    // Balance every successful initialization, including S_FALSE. Repeated
    // capability probes/reconfigurations must not increment the COM count forever.
    thread_local! {
        static APARTMENT: ComApartment = ComApartment(unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).is_ok()
        });
    }
    APARTMENT.with(|_| {});
}

struct ComApartment(bool);

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        (*var.Anonymous.Anonymous).Anonymous.lVal = v as i32;
    }
    var
}

fn variant_bool(b: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_BOOL;
        (*var.Anonymous.Anonymous).Anonymous.boolVal = VARIANT_BOOL(if b { -1 } else { 0 });
    }
    var
}

/// 枚举的媒体类型可能携带默认分辨率的裁剪、步长和样本长度。
/// 调整尺寸时必须一起替换，否则不同厂商会按旧布局读取 NV12。
fn set_frame_layout(mt: &IMFMediaType, p: &EncodeParams, raw: bool) -> windows_core::Result<()> {
    unsafe {
        mt.SetUINT64(
            &MF_MT_FRAME_SIZE,
            ((p.width as u64) << 32) | p.height as u64,
        )?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, ((p.fps as u64) << 32) | 1)?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        for key in [
            MF_MT_GEOMETRIC_APERTURE,
            MF_MT_MINIMUM_DISPLAY_APERTURE,
            MF_MT_PAN_SCAN_APERTURE,
            MF_MT_MPEG_SEQUENCE_HEADER,
        ] {
            mt.DeleteItem(&key)?;
        }
        mt.SetUINT32(&MF_MT_PAN_SCAN_ENABLED, 0)?;
        if raw {
            mt.SetUINT32(&MF_MT_DEFAULT_STRIDE, p.width)?;
            mt.SetUINT32(&MF_MT_SAMPLE_SIZE, p.width * p.height * 3 / 2)?;
            mt.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 编码器枚举
// ---------------------------------------------------------------------------

fn subtype_for(codec: vnt_proto::Codec) -> Option<GUID> {
    match codec {
        vnt_proto::Codec::H264 => Some(MFVideoFormat_H264),
        vnt_proto::Codec::Hevc => Some(MFVideoFormat_HEVC),
        // Windows 无内置软件 AV1 编码 MFT；硬件（NVENC 40 系/Intel Arc/AMD
        // RDNA3）经 MFTEnumEx 枚举，枚举不到即无能力。
        vnt_proto::Codec::Av1 => Some(MFVideoFormat_AV1),
    }
}

/// 枚举一类编码器（返回 (名称, Activate) 列表，未 Activate）。
fn enum_activates(codec: vnt_proto::Codec, hardware: bool) -> Vec<(String, IMFActivate)> {
    let Some(subtype) = subtype_for(codec) else {
        return Vec::new();
    };
    com_init_on_this_thread();
    ensure_mf_started();

    let out_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT
    };
    unsafe {
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        let result = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&out_type),
            &mut arr,
            &mut count,
        );
        pin_loaded_intel_control_module();
        if result.is_err() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !arr.is_null() {
            for i in 0..count as usize {
                // take() 转移数组元素的所有权（而非 clone 后只释放数组内存）：
                // 后者会让每个 IMFActivate 的原始引用永远泄漏，每次探测累积一份
                let Some(a) = (*arr.add(i)).take() else {
                    continue;
                };
                let name = attr_string(&a, &MFT_FRIENDLY_NAME_Attribute)
                    .unwrap_or_else(|| "unknown".into());
                out.push((name, a));
            }
            // 数组元素已全部置 None（所有权已转移），此处仅释放数组内存
            CoTaskMemFree(Some(arr as *const core::ffi::c_void));
        }
        out
    }
}

fn enum_names(codec: vnt_proto::Codec, hardware: bool) -> Vec<String> {
    enum_activates(codec, hardware)
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

fn attr_string(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    unsafe {
        let len = attrs.GetStringLength(key).ok()? as usize;
        let mut buf = vec![0u16; len + 1];
        attrs.GetString(key, &mut buf, None).ok()?;
        let mut used = len;
        if used > 0 && buf[used - 1] == 0 {
            used -= 1;
        }
        Some(String::from_utf16_lossy(&buf[..used]))
    }
}

/// 探测本机 MF 编码能力（硬件 AV1/HEVC/H264 + 微软软件 H264）。
pub fn probe_caps() -> Vec<EncoderCap> {
    let mut caps = Vec::new();
    for (codec, hw) in [
        (vnt_proto::Codec::Av1, true),
        (vnt_proto::Codec::Hevc, true),
        (vnt_proto::Codec::H264, true),
    ] {
        if enum_names(codec, hw)
            .iter()
            .any(|n| !n.contains("Microsoft"))
        {
            caps.push(EncoderCap {
                codec,
                hardware: true,
                name: "MF hardware encoder".into(),
            });
        }
    }
    if enum_names(vnt_proto::Codec::H264, false)
        .iter()
        .any(|n| n.contains("Microsoft"))
    {
        caps.push(EncoderCap {
            codec: vnt_proto::Codec::H264,
            hardware: false,
            name: "MF: Microsoft H.264 Encoder".into(),
        });
    }
    caps
}

/// AV1 硬件 MFT（Intel QSV / NVIDIA 等）输出的不是裸 OBU 码流，而是 IVF
/// 容器封装：12 字节 IVF 帧头（u32le 载荷长度 + u64le pts）逐帧前缀，流
/// 开始时可能还有 32 字节 `DKIF` 文件头。解码端（WebCodecs）只接受裸
/// OBU，容器字节漏进码流会被当成 OBU 头解析，表现为首个"关键帧"解码
/// 报错。
///
/// Intel 驱动的 `DKIF` 文件头**每个进程只随第一个 MFT 实例发出**：进程内
/// 重建编码器（自适应参数变化）或第二个会话创建的新实例只带 12 字节帧
/// 头。因此不能以"见过 `DKIF`"为剥离条件。优先按 IVF 帧链精确匹配，
/// 剥离结果须通过 OBU 校验，否则原样透传。不能先检查裸 OBU：较小的
/// IVF 长度字段可能恰好是合法 OBU 头，吞掉容器头和部分压缩数据后，
/// 整包也可能通过结构校验，但透传的容器字节无法被真正解码。
fn av1_strip_ivf(mut data: Vec<u8>) -> Vec<u8> {
    if data.starts_with(b"DKIF") {
        data.drain(..32.min(data.len())); // IVF 文件头（仅流首携带，有则剥掉）
    }
    if let Some(stripped) = collect_ivf_frames_exact(&data) {
        if obu_stream_ok(&stripped) {
            return stripped;
        }
    }
    data
}

/// 按 AV1 低开销码流语法整包校验：每个 OBU 头合法（禁用/保留位为零、
/// 类型为已定义值、带 obu_size）、leb128 长度不越界、整包被精确消费。
/// 这里只校验封装结构，不解析压缩帧内容，不能据此排除 IVF 封装。
fn obu_stream_ok(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i < data.len() {
        let b = data[i];
        // 禁用位、保留位必须为零；obu_size 必须存在
        if b & 0x81 != 0 || b & 0x2 == 0 {
            return false;
        }
        let obu_type = (b >> 3) & 0xF;
        // 已定义类型：1..7 与 15（PADDING）；8..14 保留
        if !(1..=7).contains(&obu_type) && obu_type != 15 {
            return false;
        }
        let mut j = i + 1;
        if b & 0x4 != 0 {
            let Some(&extension) = data.get(j) else {
                return false;
            };
            if extension & 0x7 != 0 {
                return false; // extension_header_reserved_3bits
            }
            j += 1;
        }
        let mut size: u64 = 0;
        let mut shift = 0;
        loop {
            // AV1 leb128 is limited to eight bytes; malformed candidate data
            // must not overflow the shift while probing its framing.
            if shift == 56 {
                return false;
            }
            let Some(&byte) = data.get(j) else {
                return false;
            };
            size |= u64::from(byte & 0x7F) << shift;
            j += 1;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        let Ok(size) = usize::try_from(size) else {
            return false;
        };
        let Some(end) = j.checked_add(size) else {
            return false;
        };
        if end > data.len() {
            return false;
        }
        i = end;
    }
    true
}

/// 按 IVF 帧链精确匹配剥离：从 12 字节帧头（u32le 长度 + u64le pts）逐帧
/// 行走，长度必须恰好链满整个缓冲（至少一帧），否则返回 `None`。
fn collect_ivf_frames_exact(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0usize;
    while i < data.len() {
        if data.len() - i < 12 {
            return None;
        }
        let size = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        if size == 0 || size > data.len() - i - 12 {
            return None;
        }
        out.extend_from_slice(&data[i + 12..i + 12 + size]);
        i += 12 + size;
    }
    (!out.is_empty()).then_some(out)
}

// ---------------------------------------------------------------------------
// MfEncoder
// ---------------------------------------------------------------------------

pub struct MfEncoder {
    codec: vnt_proto::Codec,
    hardware: bool,
    name: String,
    transform: SendPtr<IMFTransform>,
    codec_api: Option<SendPtr<ICodecAPI>>,
    /// 异步 MFT 事件接口（同线程轮询消费）。
    gen: Option<SendPtr<IMFMediaEventGenerator>>,
    /// 待消费的 NeedInput 令牌数。
    need_input_tokens: u32,
    /// 每个 HaveOutput 事件对应一次 ProcessOutput，不能合并事件。
    have_output: usize,
    params: Option<EncodeParams>,
    /// MFT 是否自行提供输出样本。
    mft_provides_samples: bool,
    output_buffer_size: u32,
    /// 已填充、等待 NeedInput 令牌的输入样本（FIFO）。
    queued: VecDeque<SendPtr<IMFSample>>,
    progress: OutputProgress,
    /// 因编码器饱和被丢弃的帧数（诊断）。
    dropped_inputs: u64,
    force_idr: bool,
    first_sample: bool,
}

/// 排队上限：等 NeedInput 令牌时最多缓存几帧。
/// 超出的帧直接丢弃（原始帧彼此独立，丢帧不影响后续解码）。
const MAX_QUEUED: usize = 1;
/// 每段无输出期间只等待一次；跳帧不能让之后每帧都阻塞 100ms。
const OUTPUT_WAIT: Duration = Duration::from_millis(100);
const OUTPUT_STALL: Duration = Duration::from_secs(3);

#[derive(Default)]
struct OutputProgress {
    waiting_since: Option<Instant>,
    wait_exhausted: bool,
    failed: bool,
}

impl OutputProgress {
    fn begin(&mut self, now: Instant) {
        self.waiting_since.get_or_insert(now);
    }

    fn output(&mut self) {
        self.waiting_since = None;
        self.wait_exhausted = false;
    }

    fn check(&mut self, now: Instant) -> Result<(), EncodeError> {
        self.failed |= self
            .waiting_since
            .is_some_and(|start| now.saturating_duration_since(start) >= OUTPUT_STALL);
        if self.failed {
            return Err(EncodeError::Encode(
                "MFT 持续 3 秒无视频输出，编码器已停止；结束会话后请重新连接".into(),
            ));
        }
        Ok(())
    }
}

/// 厂商 MF 编码器（Intel QSV 等）的并发创建不安全：多线程同时枚举/
/// ActivateObject 会触发驱动内访问冲突（进程级崩溃，非可捕获错误）。
/// 与 SVT 的 INIT_LOCK 同理，所有实例创建全程持锁串行化；创建完成后
/// 各实例在各自线程独立编解码，无需持锁。
static MFT_CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl MfEncoder {
    /// 按能力优先级创建：硬件 → 微软软件 H264。
    pub fn new(codec: vnt_proto::Codec) -> Result<Self, EncodeError> {
        com_init_on_this_thread();
        ensure_mf_started();
        let _guard = MFT_CREATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let hw = enum_activates(codec, true)
            .into_iter()
            .find(|(n, _)| !n.contains("Microsoft"));
        let (name, activate, hardware) = match hw {
            Some((n, a)) => (n, a, true),
            None => {
                if codec != vnt_proto::Codec::H264 {
                    return Err(EncodeError::Create(format!("无可用 {codec:?} 硬件编码器")));
                }
                enum_activates(codec, false)
                    .into_iter()
                    .find(|(n, _)| n.contains("Microsoft"))
                    .map(|(n, a)| (n, a, false))
                    .ok_or_else(|| EncodeError::Create(format!("无可用 {codec:?} MFT 编码器")))?
            }
        };

        let activated = unsafe { activate.ActivateObject::<IMFTransform>() };
        pin_loaded_intel_control_module();
        let transform: IMFTransform = activated
            .map_err(|e| EncodeError::Create(format!("ActivateObject({name}) 失败: {e}")))?;
        let codec_api = transform.cast::<ICodecAPI>().ok().map(SendPtr);

        let is_async = unsafe {
            transform
                .GetAttributes()
                .and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC))
                .unwrap_or(0)
        } == 1;

        let gen = if is_async {
            unsafe {
                transform
                    .GetAttributes()?
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?
            }
            let gen: IMFMediaEventGenerator = transform
                .cast()
                .map_err(|e| EncodeError::Create(format!("异步 MFT 缺少事件接口: {e}")))?;
            Some(SendPtr(gen))
        } else {
            None
        };

        log::info!("MfEncoder 创建: {name} (hw={hardware}, async={is_async})");
        Ok(Self {
            codec,
            hardware,
            name,
            transform: SendPtr(transform),
            codec_api,
            gen,
            need_input_tokens: 0,
            have_output: 0,
            params: None,
            mft_provides_samples: false,
            output_buffer_size: 0,
            queued: VecDeque::new(),
            progress: OutputProgress::default(),
            dropped_inputs: 0,
            force_idr: false,
            first_sample: true,
        })
    }

    fn configure_initial(&mut self, p: &EncodeParams) -> Result<(), EncodeError> {
        // 注意：部分 ICodecAPI 选项必须在 SetOutputType 之前设置（参考 ffmpeg mfenc）
        self.apply_codec_api(p);
        let out_mt = self.make_output_type(p)?;
        unsafe {
            self.transform.SetOutputType(0, &out_mt, 0)?;
            let in_mt = self
                .find_input_type(p)?
                .ok_or_else(|| EncodeError::Reconfigure("编码器不接受 NV12 输入".into()))?;
            self.transform.SetInputType(0, &in_mt, 0)?;
        }
        // 清空初始化期间积压的事件，再启动流
        self.need_input_tokens = 0;
        self.have_output = 0;
        self.pump_pending_events();
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            if self.gen.is_some() {
                self.transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            }
        }
        self.refresh_output_info()?;
        self.params = Some(*p);
        self.force_idr = true; // 新编码器首帧要求 IDR
        self.first_sample = true;
        Ok(())
    }

    /// 同线程消费所有待处理事件（ffmpeg mf_wait_events 语义）。
    fn pump_pending_events(&mut self) {
        let Some(gen) = &self.gen else { return };
        loop {
            let ev = unsafe { gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            match ev {
                Ok(e) => {
                    let kind = unsafe { e.GetType() }.unwrap_or(0);
                    match kind {
                        x if x == METransformNeedInput.0 as u32 => self.need_input_tokens += 1,
                        x if x == METransformHaveOutput.0 as u32 => self.have_output += 1,
                        x if x == METransformDrainComplete.0 as u32 => {}
                        other => log::debug!("MF 事件 {other}"),
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// 提交一个已填充的输入样本。返回 false 表示当时没有输入令牌
    /// （异步 MFT 尚未请求输入），调用方应排队等待，而不是干等。
    fn submit_input(&mut self, sample: &IMFSample) -> Result<bool, EncodeError> {
        if self.gen.is_some() {
            if self.need_input_tokens == 0 {
                return Ok(false);
            }
            self.need_input_tokens -= 1;
        }
        unsafe {
            match self.transform.ProcessInput(0, sample, 0) {
                Ok(()) => Ok(true),
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // 令牌计数与 MFT 实际状态不同步：归还令牌，下轮再试
                    if self.gen.is_some() {
                        self.need_input_tokens += 1;
                    }
                    Ok(false)
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    log::info!("输入流变更通知，忽略本帧");
                    Ok(true) // 帧已消费（丢弃），无需排队
                }
                Err(e) => Err(EncodeError::Encode(format!("ProcessInput: {e}"))),
            }
        }
    }

    /// 非阻塞收发电：有令牌就提交排队帧，有输出就取走。
    /// 输入与输出两侧完全独立——不必等令牌才收输出，也不必等输出才提交。
    fn pump(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        self.pump_pending_events();
        while self.need_input_tokens > 0 {
            let Some(sample) = self.queued.pop_front() else {
                break;
            };
            if self.submit_input(&sample)? {
                continue;
            }
            self.queued.push_front(sample); // 没提交进去：放回队首等下一轮
            break;
        }
        self.collect_outputs(out)
            .map_err(|e| EncodeError::Encode(format!("ProcessOutput: {e}")))?;
        Ok(())
    }

    /// 有界等待下一个 MF 事件。自适应退避（50µs 起步、拉到 1ms 封顶），
    /// 相比固定 1ms 轮询把常见路径的抖动压低一个量级；驱动始终不响应时
    /// 仍有界返回，不会挂死管线。
    fn wait_event(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut delay = Duration::from_micros(50);
        loop {
            self.pump_pending_events();
            // 输出随时值得取；令牌只在有排队帧可提交时才有意义——
            // 否则一个用不上的令牌会让这里立刻返回、退化成热旋
            if self.have_output > 0 || (self.need_input_tokens > 0 && !self.queued.is_empty()) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            std::thread::sleep(delay.min(deadline - now));
            delay = (delay * 2).min(Duration::from_millis(1));
        }
    }

    /// 选择输出媒体类型：优先取 MFT 枚举的可用类型再覆写参数（硬件 MFT 挑剔），
    /// 找不到才手工构造。流变更后重新协商同样走这里。
    fn make_output_type(&self, p: &EncodeParams) -> windows_core::Result<IMFMediaType> {
        let want = subtype_for(self.codec).unwrap();
        unsafe {
            for i in 0..64u32 {
                let Ok(mt) = self.transform.GetOutputAvailableType(0, i) else {
                    break;
                };
                let Ok(sub) = mt.GetGUID(&MF_MT_SUBTYPE) else {
                    continue;
                };
                if sub == want {
                    set_frame_layout(&mt, p, false)?;
                    mt.SetUINT32(&MF_MT_AVG_BITRATE, p.bitrate_kbps * 1000)?;
                    log::debug!("输出类型取自 GetOutputAvailableType[{i}]");
                    return Ok(mt);
                }
            }
            let mt = MFCreateMediaType()?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            mt.SetGUID(&MF_MT_SUBTYPE, &want)?;
            mt.SetUINT32(&MF_MT_AVG_BITRATE, p.bitrate_kbps * 1000)?;
            set_frame_layout(&mt, p, false)?;
            Ok(mt)
        }
    }

    fn find_input_type(&self, p: &EncodeParams) -> windows_core::Result<Option<IMFMediaType>> {
        unsafe {
            for i in 0..64u32 {
                let Ok(mt) = self.transform.GetInputAvailableType(0, i) else {
                    break;
                };
                let Ok(sub) = mt.GetGUID(&MF_MT_SUBTYPE) else {
                    continue;
                };
                if sub == MFVideoFormat_NV12 {
                    set_frame_layout(&mt, p, true)?;
                    return Ok(Some(mt));
                }
            }
            Ok(None)
        }
    }

    /// 通过 ICodecAPI 设置低延迟参数；失败仅降级记录。
    fn apply_codec_api(&self, p: &EncodeParams) {
        let Some(api) = &self.codec_api else {
            log::warn!(
                "{} 缺少 ICodecAPI，无法设置 VBR，使用驱动默认码控",
                self.name
            );
            return;
        };
        let set_u32 = |guid: *const GUID, v: u32, what: &str| unsafe {
            let var = variant_u32(v);
            if let Err(e) = api.SetValue(guid, &var) {
                log::debug!("CodecAPI {what} 不受支持: {e}");
            }
        };
        unsafe {
            // VBR varies output with screen complexity while MeanBitRate remains
            // the adaptive average target. It does not add a peak property that
            // could become stale after a bitrate-only hot update; UDP pacing
            // independently bounds send bursts. Keep AVLowLatencyMode below.
            match api.SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &variant_u32(eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32),
            ) {
                Ok(()) => log::info!(
                    "{} 已接受 VBR 码控，目标 {} kbps",
                    self.name,
                    p.bitrate_kbps
                ),
                Err(e) => log::warn!("{} 不支持设置 VBR，使用驱动默认码控: {e}", self.name),
            }
            set_u32(
                &CODECAPI_AVEncCommonMeanBitRate,
                p.bitrate_kbps * 1000,
                "MeanBitRate",
            );
            set_u32(&CODECAPI_AVEncMPVGOPSize, p.gop, "GOPSize");
            if let Err(e) = api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true)) {
                log::debug!("CodecAPI LowLatency 不受支持: {e}");
            }
        }
    }

    /// 按帧创建输入样本，把 BGRA 直接转换进样本自带的 NV12 缓冲。
    /// 不保留输入样本池：样本随 ProcessInput 后本管线引用释放而释放
    /// （驱动若仍在读，会用自己的引用保持其存活）。
    fn fill_input_sample(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        pts_ms: u64,
        fps: u32,
    ) -> Result<IMFSample, EncodeError> {
        let size = width * height * 3 / 2;
        unsafe {
            let sample: IMFSample =
                MFCreateSample().map_err(|e| EncodeError::Encode(format!("创建输入样本: {e}")))?;
            let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(size as u32)
                .map_err(|e| EncodeError::Encode(format!("创建输入缓冲: {e}")))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| EncodeError::Encode(format!("挂载输入缓冲: {e}")))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer
                .Lock(&mut ptr, None, None)
                .map_err(|e| EncodeError::Encode(format!("锁定输入缓冲: {e}")))?;
            // 转换目标即 MF 输入缓冲本身
            color::bgra_to_nv12(
                bgra,
                width,
                height,
                std::slice::from_raw_parts_mut(ptr, size),
            );
            let length_result = buffer.SetCurrentLength(size as u32);
            let unlock_result = buffer.Unlock();
            length_result.map_err(|e| EncodeError::Encode(format!("设置输入长度: {e}")))?;
            unlock_result.map_err(|e| EncodeError::Encode(format!("解锁输入缓冲: {e}")))?;
            sample
                .SetSampleTime((pts_ms * 10_000) as i64)
                .map_err(|e| EncodeError::Encode(format!("设置样本时间: {e}")))?;
            sample
                .SetSampleDuration(10_000_000i64 / fps.max(1) as i64)
                .map_err(|e| EncodeError::Encode(format!("设置样本时长: {e}")))?;
            // 流的首帧标记不连续。
            let _ = sample.SetUINT32(
                &MFSampleExtension_Discontinuity,
                u32::from(self.first_sample),
            );
            Ok(sample)
        }
    }

    /// 取走一个输出样本的数据。
    fn read_sample(
        &mut self,
        sample: &IMFSample,
        out: &mut Vec<EncodedPacket>,
    ) -> windows_core::Result<()> {
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let cur = buffer.GetCurrentLength()? as usize;
            if cur == 0 {
                return Ok(());
            }
            if cur > vnt_proto::udp::MAX_FRAME_BYTES {
                return Err(windows_core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    "编码输出超过 8 MiB",
                ));
            }
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            let mut data = std::slice::from_raw_parts(ptr, cur).to_vec();
            buffer.Unlock()?;
            if self.codec == vnt_proto::Codec::Av1 {
                data = av1_strip_ivf(data);
                if data.is_empty() {
                    return Ok(());
                }
            }
            let keyframe = sample
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .map(|v| v != 0)
                .unwrap_or(false);
            let pts_ms = sample
                .GetSampleTime()
                .map(|t| (t.max(0) as u64) / 10_000)
                .unwrap_or(0);
            out.push(EncodedPacket {
                keyframe,
                pts_ms,
                data,
            });
            Ok(())
        }
    }

    /// ProcessOutput（一次），处理常见返回码。
    /// 部分硬件 MFT（Intel QSV H.264）在首个输出上返回流格式变更：必须
    /// 重新 SetOutputType 确认新格式后 MFT 才继续产出，否则它不再发出任何
    /// 事件，表现为持续无输出（ffmpeg mfenc 对流变更加同处理）。
    fn process_output_once(&mut self, out: &mut Vec<EncodedPacket>) -> windows_core::Result<bool> {
        let mut renegotiated = false;
        loop {
            // MFT 不自提供样本时按帧创建（不保留样本池）：编码字节在
            // read_sample 里拷出后样本即释放，任何时刻只有一份输出缓冲
            let sample: Option<IMFSample> = if self.mft_provides_samples {
                None
            } else {
                let size = self.output_buffer_size.max(1);
                let sample = unsafe { MFCreateSample()? };
                let buffer = unsafe { MFCreateMemoryBuffer(size)? };
                unsafe { sample.AddBuffer(&buffer)? };
                Some(sample)
            };
            let mut buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let hr = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status)
            };
            // windows-rs represents these COM references as ManuallyDrop. Every
            // ProcessOutput result owns both fields, including error paths.
            let sample = unsafe { std::mem::ManuallyDrop::take(&mut buf.pSample) };
            let _events = unsafe { std::mem::ManuallyDrop::take(&mut buf.pEvents) };
            match hr {
                Ok(()) => {
                    let produced = out.len();
                    if let Some(s) = sample {
                        self.read_sample(&s, out)?;
                    }
                    if out.len() > produced {
                        self.progress.output();
                    }
                    return Ok(true);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e)
                    if e.code() == windows::Win32::Foundation::E_UNEXPECTED
                        && self.gen.is_some() =>
                {
                    // Intel QSV 等异步硬件 MFT 在无待取输出时对 ProcessOutput 返回
                    // E_UNEXPECTED 而非 NEED_MORE_INPUT（流变更重协商后事件计数与
                    // 驱动状态短暂失步时偶发）。等同"暂无输出"处理，等下一个
                    // HaveOutput 事件；持续无输出由 OutputProgress 超时兜底
                    log::debug!("ProcessOutput E_UNEXPECTED（无待取输出），等待下一事件");
                    return Ok(false);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if renegotiated {
                        // 连续两次流变更不属于正常协商，交由上层按编码错误处理
                        return Err(e);
                    }
                    renegotiated = true;
                    let p = self.params.expect("编码前不应出现输出流变更");
                    let mt = self.make_output_type(&p)?;
                    unsafe {
                        self.transform.SetOutputType(0, &mt, 0)?;
                    }
                    self.refresh_output_info()?;
                    // 确认新类型后 MFT 会重新广播 HaveOutput；异步 MFT 该事件
                    // 可能耗时数毫秒才入队，立即重试会得到 E_UNEXPECTED，
                    // 回到事件循环等下一次 HaveOutput（同步 MFT 无事件，直接重试）
                    self.pump_pending_events();
                    if self.gen.is_some() && self.have_output == 0 {
                        return Ok(false);
                    }
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 取输出：
    /// - 异步 MFT（硬件）：仅在收到 HaveOutput 事件后取
    /// - 同步 MFT（微软软件编码器）：无事件接口，直接循环 ProcessOutput
    ///   直到 NEED_MORE_INPUT
    fn collect_outputs(&mut self, out: &mut Vec<EncodedPacket>) -> windows_core::Result<()> {
        if self.gen.is_none() {
            while self.process_output_once(out)? {}
            return Ok(());
        }
        while self.have_output > 0 {
            self.have_output -= 1;
            if !self.process_output_once(out)? {
                break;
            }
            self.pump_pending_events();
        }
        Ok(())
    }

    fn refresh_output_info(&mut self) -> windows_core::Result<()> {
        let info = unsafe { self.transform.GetOutputStreamInfo(0)? };
        self.mft_provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 | MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0)
                as u32
            != 0;
        self.output_buffer_size = info.cbSize;
        log::debug!(
            "输出流信息: flags={:#x} cbSize={} provides_samples={}",
            info.dwFlags,
            info.cbSize,
            self.mft_provides_samples
        );
        Ok(())
    }
}

impl VideoEncoder for MfEncoder {
    fn codec(&self) -> vnt_proto::Codec {
        self.codec
    }

    fn is_hardware(&self) -> bool {
        self.hardware
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn configure(&mut self, p: &EncodeParams) -> Result<(), EncodeError> {
        p.validate()?;
        com_init_on_this_thread();
        ensure_mf_started();

        if let Some(old) = &self.params {
            if old.width == p.width
                && old.height == p.height
                && old.fps == p.fps
                && old.gop == p.gop
            {
                if old.bitrate_kbps == p.bitrate_kbps {
                    return Ok(());
                }
                // Runtime changes must actually be acknowledged by the
                // driver. Optional initialization hints may fail silently,
                // but claiming a new applied bitrate cannot.
                if let Some(api) = &self.codec_api {
                    let result = unsafe {
                        api.SetValue(
                            &CODECAPI_AVEncCommonMeanBitRate,
                            &variant_u32(p.bitrate_kbps * 1000),
                        )
                    };
                    if result.is_ok() {
                        self.params = Some(*p);
                        return Ok(());
                    }
                    log::debug!("MFT 不支持码率热更新，使用新实例配置: {result:?}");
                }
                // No usable CodecAPI (or changed GOP): negotiate a replacement
                // below. Failure preserves the old encoder and its parameters.
            }
        }

        if self.params.is_none() {
            return self.configure_initial(p);
        }
        // 在新的 MFT 上完成全部协商后再替换。失败时旧流、参数和队列保持可用；
        // 成功后销毁旧 MFT，旧尺寸的在途样本随之丢弃（上层发送 StreamReset）。
        let mut replacement = Self::new(self.codec)?;
        replacement.configure_initial(p)?;
        *self = replacement;
        Ok(())
    }

    fn encode(
        &mut self,
        bgra: &[u8],
        pts_ms: u64,
        out: &mut Vec<EncodedPacket>,
    ) -> Result<(), EncodeError> {
        let p = self
            .params
            .ok_or_else(|| EncodeError::Encode("未 configure".into()))?;
        if bgra.len() != p.width as usize * p.height as usize * 4 {
            return Err(EncodeError::Encode(format!(
                "帧尺寸不符: {} != {}x{}x4",
                bgra.len(),
                p.width,
                p.height
            )));
        }
        // Collect already available output before checking the wall clock:
        // capture can be idle while an asynchronous output is waiting for us.
        if self.progress.failed {
            return self.progress.check(Instant::now());
        }
        self.pump(out)?;
        self.progress.check(Instant::now())?;
        self.progress.begin(Instant::now());
        if self.force_idr {
            if let Some(api) = &self.codec_api {
                unsafe {
                    let v = variant_u32(1);
                    if let Err(e) = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v) {
                        log::debug!("强制关键帧不受支持: {e}");
                    }
                }
            }
            self.force_idr = false;
        }
        let sample = self
            .fill_input_sample(bgra, p.width as usize, p.height as usize, pts_ms, p.fps)
            .map_err(|e| EncodeError::Encode(format!("构造输入样本失败: {e}")))?;
        self.first_sample = false;

        if self.gen.is_none() {
            // 同步 MFT（微软软件编码器）：无事件接口，提交后直接取输出
            if !self.submit_input(&sample)? {
                // NOTACCEPTING 表示还有输出待取；先取输出，再原样重试一次。
                self.collect_outputs(out)
                    .map_err(|e| EncodeError::Encode(format!("ProcessOutput: {e}")))?;
                if !self.submit_input(&sample)? {
                    self.dropped_inputs += 1;
                    self.first_sample = true;
                    return Err(EncodeError::Encode(format!(
                        "同步 MFT 取输出后仍拒收输入（累计丢帧 {}）",
                        self.dropped_inputs
                    )));
                }
            }
            self.collect_outputs(out)
                .map_err(|e| EncodeError::Encode(format!("ProcessOutput: {e}")))?;
            return self.progress.check(Instant::now());
        }

        // 异步 MFT：先收发电（可能腾出令牌、带走上一帧的输出），再决定本帧
        // 去向——有令牌立即提交，否则排队，队列也满则丢帧。全程不预等令牌，
        // 输入输出两侧各自推进。
        let produced = out.len();
        if self.submit_input(&sample)? {
            // 提交成功：输出可能立刻就绪
            self.collect_outputs(out)
                .map_err(|e| EncodeError::Encode(format!("ProcessOutput: {e}")))?;
        } else if self.queued.len() < MAX_QUEUED {
            self.queued.push_back(SendPtr(sample));
        } else {
            // 编码器饱和：驱动迟迟不请求输入。丢帧而不是阻塞管线——
            // 原始帧彼此独立，丢一帧不影响后续帧解码
            self.dropped_inputs += 1;
            if self.dropped_inputs % 60 == 1 {
                log::warn!(
                    "编码器输入饱和，持续丢帧（排队 {}，累计 {}）",
                    self.queued.len(),
                    self.dropped_inputs
                );
            }
        }
        // 等待本轮首个输出，而不是假设每帧输入都对应输出。
        // 首次超时后继续按 NeedInput 令牌送帧、非阻塞收包：码控跳帧不会
        // 泄漏计数并把帧率压到 10fps。持续无输出则报错，由服务端结束会话。
        let deadline = Instant::now() + OUTPUT_WAIT;
        while out.len() == produced && !self.progress.wait_exhausted {
            if !self.wait_event(deadline.saturating_duration_since(Instant::now())) {
                self.progress.wait_exhausted = true;
                break;
            }
            self.pump(out)?;
            if Instant::now() >= deadline {
                self.progress.wait_exhausted = true;
                break;
            }
        }
        self.progress.check(Instant::now())
    }

    fn force_keyframe(&mut self) -> Result<(), EncodeError> {
        self.force_idr = true;
        Ok(())
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        pin_loaded_intel_control_module();
        if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
            unsafe {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_ivf_packet_must_not_be_mistaken_for_bare_obus() {
        // 0x1232 bytes is a small encoded frame. Its IVF length field starts
        // with a valid FRAME OBU header and a size of 18, swallowing the rest
        // of the IVF header and the first eight payload bytes. The remaining
        // compressed bytes can coincidentally pass the structural OBU check.
        let mut bare = vec![0; 0x1232];
        bare[..3].copy_from_slice(&[0x32, 0xaf, 0x24]); // FRAME, size 4655
        bare[8..11].copy_from_slice(&[0x7a, 0xa7, 0x24]); // PADDING, size 4647
        let mut ivf = (bare.len() as u32).to_le_bytes().to_vec();
        ivf.extend_from_slice(&0u64.to_le_bytes());
        ivf.extend_from_slice(&bare);
        assert!(obu_stream_ok(&bare));
        assert!(
            obu_stream_ok(&ivf),
            "fixture must exercise ambiguous framing"
        );
        assert!(av1_strip_ivf(ivf.clone()) == bare, "IVF header leaked");
        let mut with_header = b"DKIF".to_vec();
        with_header.extend_from_slice(&[0; 28]);
        with_header.extend_from_slice(&ivf);
        assert!(
            av1_strip_ivf(with_header) == bare,
            "IVF header leaked after DKIF"
        );
        assert!(
            av1_strip_ivf(bare.clone()) == bare,
            "bare OBU stream changed"
        );
    }

    #[test]
    fn obu_validation_rejects_malformed_lengths_and_extensions() {
        assert!(obu_stream_ok(&[
            0x32, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0
        ]));
        let mut overlong = vec![0x32];
        overlong.extend_from_slice(&[0x80; 10]);
        overlong.push(0);
        assert!(!obu_stream_ok(&overlong));
        assert!(!obu_stream_ok(&[0x32, 0x80]));
        assert!(!obu_stream_ok(&[0x32, 0x7f]));
        assert!(!obu_stream_ok(&[0x36]));
        assert!(!obu_stream_ok(&[0x36, 0x01, 0]));
        assert!(obu_stream_ok(&[0x36, 0, 0]));
        assert!(collect_ivf_frames_exact(&[]).is_none());
    }

    #[test]
    fn ivf_stripping_matches_container_layout() {
        // 合法 OBU 载荷：TD + 序列头 + 帧（各带 obu_size）
        let obu = |header: u8, payload: &[u8]| {
            let mut v = vec![header, payload.len() as u8];
            v.extend_from_slice(payload);
            v
        };
        let bare = [obu(0x12, &[]), obu(0x0a, b"seq"), obu(0x32, b"frame")].concat();
        let frame = |payload: &[u8]| {
            let mut v = (payload.len() as u32).to_le_bytes().to_vec();
            v.extend_from_slice(&33u64.to_le_bytes());
            v.extend_from_slice(payload);
            v
        };

        // 首样本：DKIF 文件头 + 一帧 IVF → 剥出裸 OBU
        let mut first = b"DKIF".to_vec();
        first.extend_from_slice(&[0u8; 28]);
        first.extend_from_slice(&frame(&bare));
        assert_eq!(av1_strip_ivf(first), bare);

        // 后续样本：仅 12 字节帧头。Intel 驱动在进程内重建 MFT 后不再发
        // DKIF，因此没有文件头时也需要识别并剥离帧头
        assert_eq!(av1_strip_ivf(frame(&bare)), bare);

        // 一个样本内的 IVF 帧链（多帧拼接）
        let chained = [frame(&bare[..5]), frame(&bare[5..])].concat();
        assert_eq!(av1_strip_ivf(chained), bare);

        // 非 IVF（其他厂商裸流输出）：原样透传
        assert_eq!(av1_strip_ivf(bare.clone()), bare);

        // 既不是合法 OBU 也不是完整 IVF 帧链：不静默丢帧，按原样交出
        let mut bad = Vec::new();
        bad.extend_from_slice(&9999u32.to_le_bytes());
        bad.extend_from_slice(&[0u8; 8]);
        bad.extend_from_slice(b"xyz");
        assert_eq!(av1_strip_ivf(bad.clone()), bad);
    }

    /// 解析整包低开销 OBU 码流，返回逐个 OBU 类型；解析不完整消费整包
    /// 则 panic（说明存在容器残留或非法 OBU 头）。
    fn walk_obus(data: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0usize;
        while i < data.len() {
            let b = data[i];
            assert_eq!(b & 0x81, 0, "OBU 头禁用/保留位非零（疑似 IVF 残留）");
            let has_ext = b & 0x4 != 0;
            assert!(b & 0x2 != 0, "低开销码流的 OBU 必须带 obu_size");
            let mut j = i + 1 + usize::from(has_ext);
            let mut size: u64 = 0;
            let mut shift = 0;
            loop {
                assert!(j < data.len(), "obu_size 被截断");
                let byte = data[j];
                size |= u64::from(byte & 0x7F) << shift;
                j += 1;
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            let ty = (b >> 3) & 0xF;
            assert_ne!(ty, 0, "OBU 类型 0 非法");
            types.push(ty);
            i = j + size as usize;
        }
        types
    }

    /// 回归：Intel MF AV1 编码器输出 IVF 容器封装（首样本 `DKIF` + 逐帧
    /// 12 字节帧头）。透传时 WebCodecs 把容器字节当 OBU 头解析，报
    /// "chunk 标记为 key 但不是关键帧"。剥离后输出必须是能被完整解析的
    /// 裸低开销 OBU 码流，关键帧内联序列头。
    #[test]
    fn av1_output_is_stripped_to_bare_obu_stream() {
        let Ok(mut encoder) = MfEncoder::new(vnt_proto::Codec::Av1) else {
            return; // 本机无 MF AV1 硬件编码器
        };
        encoder
            .configure(&EncodeParams::new(640, 480, 30, 4000))
            .unwrap();
        let frame = vec![80u8; 640 * 480 * 4];
        let mut packets = Vec::new();
        for i in 0..8u64 {
            encoder.encode(&frame, i * 33, &mut packets).unwrap();
        }
        assert!(!packets.is_empty(), "编码器未产出任何包");
        let key = packets
            .iter()
            .find(|p| p.keyframe && !p.data.is_empty())
            .expect("首帧必须 IDR");
        let types = walk_obus(&key.data);
        assert!(types.contains(&1), "关键帧缺序列头 OBU（{types:?}）");
        assert!(
            types.contains(&6) || (types.contains(&3) && types.contains(&4)),
            "缺帧 OBU（{types:?}）"
        );
        for p in &packets {
            if p.data.is_empty() {
                continue;
            }
            walk_obus(&p.data); // 每个包（含 delta）都必须完整解析为 OBU 流
        }
    }

    /// 回归：Intel 驱动的 `DKIF` 文件头只随进程内第一个 MFT 实例发出；
    /// 自适应参数变化重建编码器后，样本只带 12 字节 IVF 帧头。旧的
    /// "见过 DKIF 才剥"检测会漏剥，IVF 帧头混入码流，WebCodecs 对重建
    /// 解码器后的首个 key 分片解析失败（"marked as key but wasn't a key
    /// frame"，概率出现——重建/第二个会话才触发）。重建后的每个包仍必须
    /// 是纯 OBU 流。
    #[test]
    fn av1_output_stays_bare_after_encoder_rebuild() {
        let Ok(mut encoder) = MfEncoder::new(vnt_proto::Codec::Av1) else {
            return; // 本机无 MF AV1 硬件编码器
        };
        encoder
            .configure(&EncodeParams::new(640, 480, 30, 4000))
            .unwrap();
        let frame = vec![80u8; 640 * 480 * 4];
        // fps 变化触发 MFT 整体重建（见 configure 的 replacement 路径）
        encoder
            .configure(&EncodeParams::new(640, 480, 60, 4000))
            .unwrap();
        let mut packets = Vec::new();
        for i in 0..8u64 {
            encoder.encode(&frame, i * 33, &mut packets).unwrap();
        }
        assert!(!packets.is_empty(), "重建后编码器未产出任何包");
        for p in &packets {
            if p.data.is_empty() {
                continue;
            }
            walk_obus(&p.data);
            assert!(!p.data.starts_with(b"DKIF"), "重建后仍有 IVF 文件头残留");
        }
        assert!(
            packets.iter().any(|p| p.keyframe && !p.data.is_empty()),
            "重建后首帧必须 IDR"
        );
    }

    #[test]
    fn av1_small_resolutions_keep_bare_obus_across_reconfiguration() {
        let Ok(mut encoder) = MfEncoder::new(vnt_proto::Codec::Av1) else {
            return; // No hardware AV1 encoder on this machine.
        };
        for (w, h) in [(320, 180), (640, 360), (960, 540), (320, 180)] {
            encoder
                .configure(&EncodeParams::new(w, h, 30, 4000))
                .unwrap();
            let frame = vec![80; w as usize * h as usize * 4];
            let mut packets = Vec::new();
            for i in 0..8 {
                encoder.encode(&frame, i * 33, &mut packets).unwrap();
            }
            assert!(packets.iter().any(|p| p.keyframe && !p.data.is_empty()));
            for packet in packets {
                walk_obus(&packet.data);
                if packet.keyframe {
                    assert!(
                        walk_obus(&packet.data).contains(&1),
                        "keyframe needs sequence header"
                    );
                }
            }
        }
    }

    #[test]
    fn skipped_inputs_do_not_extend_the_output_deadline() {
        let start = Instant::now();
        let mut progress = OutputProgress::default();
        progress.begin(start);
        progress.wait_exhausted = true;
        for ms in [100, 500, 1000, 2999] {
            let now = start + Duration::from_millis(ms);
            progress.begin(now);
            assert!(progress.check(now).is_ok());
            assert!(progress.wait_exhausted);
        }
        assert!(progress.check(start + OUTPUT_STALL).is_err());
        // Late output must not turn a permanently failed encoder into success
        // between the server's consecutive-error checks.
        progress.output();
        assert!(progress.check(start + OUTPUT_STALL).is_err());
    }

    #[test]
    fn successful_output_clears_the_deadline_across_idle_periods() {
        let start = Instant::now();
        let mut progress = OutputProgress::default();
        progress.begin(start);
        progress.wait_exhausted = true;
        progress.output();
        let later = start + Duration::from_secs(60);
        assert!(progress.check(later).is_ok());
        assert!(!progress.wait_exhausted);
        progress.begin(later);
        assert!(progress
            .check(later + OUTPUT_STALL - Duration::from_millis(1))
            .is_ok());
        assert!(progress.check(later + OUTPUT_STALL).is_err());
    }

    #[test]
    fn retained_input_sample_is_not_overwritten_by_later_frames() {
        let Ok(mut encoder) = MfEncoder::new(vnt_proto::Codec::H264) else {
            return;
        };
        let first = encoder
            .fill_input_sample(&vec![0; 64 * 48 * 4], 64, 48, 0, 30)
            .unwrap();
        let snapshot = |sample: &IMFSample| unsafe {
            let buffer = sample.ConvertToContiguousBuffer().unwrap();
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None).unwrap();
            let data = std::slice::from_raw_parts(ptr, 64 * 48 * 3 / 2).to_vec();
            buffer.Unlock().unwrap();
            data
        };
        let original = snapshot(&first);
        // Hold the first COM reference like an MFT retaining input beyond output.
        for i in 1..8 {
            let next = encoder
                .fill_input_sample(&vec![255; 64 * 48 * 4], 64, 48, i * 33, 30)
                .unwrap();
            assert_ne!(snapshot(&next), original);
            assert_eq!(snapshot(&first), original);
            assert_eq!(unsafe { first.GetSampleTime().unwrap() }, 0);
        }
    }

    #[test]
    fn resolution_change_replaces_stalled_state_and_failed_change_preserves_it() {
        let Ok(mut encoder) = MfEncoder::new(vnt_proto::Codec::H264) else {
            return;
        };
        encoder
            .configure(&EncodeParams::new(640, 480, 30, 4000))
            .unwrap();
        encoder.progress.begin(Instant::now());
        encoder.progress.failed = true;
        let old_transform = encoder.transform.0.clone();
        assert!(encoder
            .configure(&EncodeParams::new(0, 720, 30, 4000))
            .is_err());
        assert_eq!(encoder.transform.0, old_transform);
        assert!(encoder.progress.failed);
        encoder
            .configure(&EncodeParams::new(1280, 720, 30, 4000))
            .unwrap();
        assert_ne!(encoder.transform.0, old_transform);
        assert!(!encoder.progress.failed);
        assert!(encoder.progress.waiting_since.is_none());
        assert!(encoder.queued.is_empty());
    }

    #[test]
    fn encoding_continues_after_resolution_change() {
        let Ok(mut encoder) = MfEncoder::new(vnt_proto::Codec::H264) else {
            return;
        };
        for (w, h) in [(640u32, 480u32), (1280, 720), (640, 480)] {
            encoder
                .configure(&EncodeParams::new(w, h, 30, 4000))
                .unwrap();
            let frame = vec![80; w as usize * h as usize * 4];
            let mut output = Vec::new();
            for i in 0..8 {
                encoder.encode(&frame, i * 33, &mut output).unwrap();
            }
            assert!(
                output.iter().any(|p| p.keyframe && !p.data.is_empty()),
                "{w}x{h} must produce a keyframe"
            );
        }
    }

    /// Intel QSV H.264 等异步硬件 MFT 在首个输出上返回流格式变更：
    /// 未重新协商输出类型时 MFT 停止发出事件，3 秒无输出判死（服务端
    /// 表现为"推流故障"）。本用例用连续多帧编码覆盖该路径。
    #[test]
    fn sustained_encoding_survives_output_stream_change() {
        let Ok(mut encoder) = MfEncoder::new(vnt_proto::Codec::H264) else {
            return;
        };
        encoder
            .configure(&EncodeParams::new(1920, 1080, 30, 8000))
            .unwrap();
        let frame = vec![80u8; 1920 * 1080 * 4];
        let mut packets = Vec::new();
        for i in 0..40u64 {
            encoder
                .encode(&frame, i * 33, &mut packets)
                .expect("流变更后编码必须继续（重协商失败会表现为持续无输出）");
        }
        // 验证连续编码的实际输出，不限制整批耗时：Debug 下色彩转换可能较慢。
        // 连续无输出的 3 秒超时由 OutputProgress 的独立测试验证。
        assert!(packets.first().is_some_and(|p| p.keyframe));
        assert!(
            packets.len() >= 39,
            "应近乎逐帧产出，实际 {} 包",
            packets.len()
        );
    }

    #[test]
    fn fps_reconfiguration_produces_a_new_idr_and_invalid_updates_keep_the_stream_usable() {
        let Ok(mut encoder) = MfEncoder::new(vnt_proto::Codec::H264) else {
            return;
        };
        let frame = vec![80; 640 * 480 * 4];
        let mut pts = 0;
        for fps in [60, 24, 30] {
            encoder
                .configure(&EncodeParams::new(640, 480, fps, 4000))
                .unwrap();
            let mut packets = Vec::new();
            for _ in 0..8 {
                encoder.encode(&frame, pts, &mut packets).unwrap();
                pts += 1000 / fps as u64;
            }
            assert!(packets.first().is_some_and(|packet| packet.keyframe));
            assert!(encoder
                .configure(&EncodeParams::new(640, 480, 0, 4000))
                .is_err());
            assert_eq!(encoder.params.unwrap().fps, fps);
        }
    }

    #[test]
    fn resizing_media_type_replaces_default_layout() {
        com_init_on_this_thread();
        ensure_mf_started();
        unsafe {
            let mt = MFCreateMediaType().unwrap();
            mt.SetUINT32(&MF_MT_DEFAULT_STRIDE, 1280).unwrap();
            mt.SetUINT32(&MF_MT_SAMPLE_SIZE, 1280 * 720 * 3 / 2)
                .unwrap();
            mt.SetBlob(&MF_MT_GEOMETRIC_APERTURE, &[0; 16]).unwrap();
            mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (3u64 << 32) | 1)
                .unwrap();
            set_frame_layout(&mt, &EncodeParams::new(3840, 2160, 30, 8000), true).unwrap();
            assert_eq!(mt.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap(), 3840);
            assert_eq!(
                mt.GetUINT32(&MF_MT_SAMPLE_SIZE).unwrap(),
                3840 * 2160 * 3 / 2
            );
            assert_eq!(
                mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap(),
                (3840u64 << 32) | 2160
            );
            assert_eq!(
                mt.GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO).unwrap(),
                (1u64 << 32) | 1
            );
            assert!(mt.GetBlobSize(&MF_MT_GEOMETRIC_APERTURE).is_err());
        }
    }
}

#[cfg(test)]
mod test_transform;
