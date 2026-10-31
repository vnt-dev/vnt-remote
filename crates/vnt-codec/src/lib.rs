//! 视频编码：统一 [`VideoEncoder`] 抽象。
//!
//! Windows 后端：
//! - `mf`：Media Foundation MFT（硬件 AV1/HEVC/H264 优先，微软软件 H264 兜底）
//! - `svt_av1`：SVT-AV1 纯软件兜底（无硬件 AV1 时）
//! - `openh264`：纯软件 H264 兜底
//! - `kvazaar`：纯软件 HEVC 兜底
//!
//! 协商入口 [`probe_caps`] 输出本机编码能力，供服务端与客户端解码列表求交集；
//! 被控端关闭硬件编码时由上层过滤掉 `hardware` 能力并用
//! [`create_encoder`] 的 `hardware = false` 走纯软件路径。

pub mod color;
pub mod dependencies;

#[cfg(windows)]
pub mod kvazaar;

#[cfg(windows)]
pub mod mf;

#[cfg(any(windows, target_os = "macos"))]
pub mod openh264_enc;
#[cfg(target_os = "macos")]
mod videotoolbox;

#[cfg(windows)]
pub mod svt_av1;
#[cfg(windows)]
pub mod svt_av1_ffi;

use vnt_proto::negotiate::EncoderCap;

/// 编码参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 目标码率 kbps；具体码控模式由编码后端决定（MF 使用平均目标码率 VBR）。
    pub bitrate_kbps: u32,
    /// 关键帧间隔（帧数），另支持运行中 force_keyframe。
    pub gop: u32,
}

impl EncodeParams {
    pub fn validate(&self) -> Result<(), EncodeError> {
        if !vnt_proto::validation::valid_video_size(self.width, self.height)
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(EncodeError::Reconfigure(
                "视频尺寸须为受支持范围内的正偶数".into(),
            ));
        }
        if !(1..=120).contains(&self.fps) || !(100..=200_000).contains(&self.bitrate_kbps) {
            return Err(EncodeError::Reconfigure("帧率或码率超出支持范围".into()));
        }
        Ok(())
    }

    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            bitrate_kbps,
            gop: fps.saturating_mul(2).max(30),
        }
    }
}

/// 编码输出包（Annex B / OBU 裸流，元数据随包）。
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub keyframe: bool,
    pub pts_ms: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("不支持的能力组合")]
    Unsupported,
    #[error("编码器创建失败: {0}")]
    Create(String),
    #[error("编码失败: {0}")]
    Encode(String),
    #[error("重新配置失败: {0}")]
    Reconfigure(String),
    #[cfg(windows)]
    #[error("Media Foundation: {0}")]
    Hr(#[from] windows_core::Error),
}

/// 视频编码器。实现方保证 `Send`；调用方应固定在单线程使用（COM 线程模型）。
pub trait VideoEncoder: Send {
    fn codec(&self) -> vnt_proto::Codec;
    fn is_hardware(&self) -> bool;
    fn name(&self) -> &str;

    /// 配置/重配（分辨率变化时再次调用，内部重建编码器状态）。
    fn configure(&mut self, params: &EncodeParams) -> Result<(), EncodeError>;

    /// 编码一帧 BGRA；输出追加到 `out`（可能 0~N 包，低延迟模式一般恰 1 包）。
    fn encode(
        &mut self,
        bgra: &[u8],
        pts_ms: u64,
        out: &mut Vec<EncodedPacket>,
    ) -> Result<(), EncodeError>;

    /// 强制下一帧为关键帧。
    fn force_keyframe(&mut self) -> Result<(), EncodeError>;
}

/// 探测本机可用编码能力（有序：硬件 AV1/HEVC/H264 → 软件 H264/HEVC/AV1）。
///
/// MF 能力会实际尝试创建一次：枚举到但 ActivateObject 失败的编码器
/// （驱动异常等）不上报，避免协商出一个建不出来的格式。是否采纳硬件
/// 能力由调用方决定（被控端"硬件编码"开关）：关闭时过滤掉
/// `hardware == true` 的条目即可，协商自然落到软件兜底。
pub fn probe_caps() -> Vec<EncoderCap> {
    let mut caps = Vec::new();
    #[cfg(windows)]
    {
        for cap in mf::probe_caps() {
            match mf::MfEncoder::new(cap.codec) {
                Ok(_) => caps.push(cap),
                Err(e) => log::warn!("MF 编码器枚举到但创建失败，不上报: {cap:?}（{e}）"),
            }
        }
        // openh264 兜底：确认可用后才上报（会做一次真实构建）
        if openh264_enc::OpenH264Encoder::new().is_ok() {
            caps.push(EncoderCap {
                codec: vnt_proto::Codec::H264,
                hardware: false,
                name: "openh264".into(),
            });
        }
        if kvazaar::KvazaarEncoder::new().is_ok() {
            caps.push(EncoderCap {
                codec: vnt_proto::Codec::Hevc,
                hardware: false,
                name: "kvazaar".into(),
            });
        }
        // svt-av1 兜底：无硬件 AV1 时仍可协商 AV1（软编实时档位）
        if svt_av1::SvtAv1Encoder::new().is_ok() {
            caps.push(EncoderCap {
                codec: vnt_proto::Codec::Av1,
                hardware: false,
                name: "svt-av1".into(),
            });
        }
    }
    #[cfg(target_os = "macos")]
    {
        for codec in [vnt_proto::Codec::Hevc, vnt_proto::Codec::H264] {
            if videotoolbox::VtEncoder::new(codec).is_ok() {
                caps.push(EncoderCap {
                    codec,
                    hardware: true,
                    name: "VideoToolbox".into(),
                });
            }
        }
        if openh264_enc::OpenH264Encoder::new().is_ok() {
            caps.push(EncoderCap {
                codec: vnt_proto::Codec::H264,
                hardware: false,
                name: "openh264".into(),
            });
        }
    }
    caps
}

/// 创建编码器：`hardware` 为被控端"硬件编码"开关。
///
/// 开启时优先硬件 MF，失败回退同格式软件后端；
/// 关闭时直接使用软件后端（H264 → openh264，AV1 → SVT，HEVC → Kvazaar）。
///
/// **回退不得改变已协商的格式**：openh264 仅用于 H264 请求、svt-av1 仅用于
/// AV1 请求，Kvazaar 仅用于 HEVC 请求；全部后端不可用时由上层结束会话。
pub fn create_encoder(
    codec: vnt_proto::Codec,
    hardware: bool,
) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    #[cfg(windows)]
    {
        if hardware {
            if let Ok(e) = mf::MfEncoder::new(codec) {
                return Ok(Box::new(e));
            }
        }
        match codec {
            vnt_proto::Codec::H264 => {
                if let Ok(e) = openh264_enc::OpenH264Encoder::new() {
                    return Ok(Box::new(e));
                }
            }
            vnt_proto::Codec::Av1 => {
                if let Ok(e) = svt_av1::SvtAv1Encoder::new() {
                    return Ok(Box::new(e));
                }
            }
            vnt_proto::Codec::Hevc => {
                return Ok(Box::new(kvazaar::KvazaarEncoder::new()?));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if hardware {
            if let Ok(e) = videotoolbox::VtEncoder::new(codec) {
                return Ok(Box::new(e));
            }
        }
        if codec == vnt_proto::Codec::H264 {
            return Ok(Box::new(openh264_enc::OpenH264Encoder::new()?));
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = (codec, hardware);
    Err(EncodeError::Unsupported)
}
