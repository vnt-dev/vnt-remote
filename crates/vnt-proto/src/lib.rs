//! vnt-remote 线路协议。
//!
//! - TCP 控制通道：`[u8 msg_type][u32 BE len][protobuf payload]`，见 [`control`]
//! - UDP 视频通道：定长 15 字节分片头 + 数据，见 [`udp`]
//! - 编解码协商：见 [`negotiate`]

pub mod audio;
pub mod auth;
pub mod clipboard;
pub mod color_depth;
pub mod control;
pub mod negotiate;
pub mod recovery;
pub mod screen;
pub mod session;
pub mod udp;
pub mod validation;

/// First desktop release protocol.
pub const PROTO_VER: u32 = 1;

/// 默认端口（TCP 与 UDP 各监听一份）。
pub const DEFAULT_PORT: u16 = 29875;

/// 视频编码器类型（与 proto 的 `Codec` 对应，Rust 侧常用形式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        }
    }

    pub fn from_proto(c: pb::Codec) -> Option<Self> {
        match c {
            pb::Codec::H264 => Some(Codec::H264),
            pb::Codec::Hevc => Some(Codec::Hevc),
            pb::Codec::Av1 => Some(Codec::Av1),
            pb::Codec::Unspecified => None,
        }
    }

    pub fn to_proto(self) -> pb::Codec {
        match self {
            Codec::H264 => pb::Codec::H264,
            Codec::Hevc => pb::Codec::Hevc,
            Codec::Av1 => pb::Codec::Av1,
        }
    }

    /// WebCodecs 解码探测用的 codec 字符串（不带级别，探测时足够）。
    pub fn webcodec_string(self) -> &'static str {
        match self {
            Codec::H264 => "avc1.640028",
            Codec::Hevc => "hev1.1.6.L120.90",
            Codec::Av1 => "av01.0.08M.08",
        }
    }
}

/// prost 生成的 protobuf 类型。
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/vnt.rs"));
}

/// Per-connection network transport shared by IPC and handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Hybrid,
    Tcp,
    Vnt,
    Quic,
}
impl Transport {
    pub fn to_proto(self) -> pb::Transport {
        match self {
            Self::Hybrid => pb::Transport::Hybrid,
            Self::Tcp => pb::Transport::Tcp,
            Self::Vnt => pb::Transport::Vnt,
            Self::Quic => pb::Transport::Quic,
        }
    }
    pub fn from_wire(value: i32) -> Result<Self, &'static str> {
        match pb::Transport::try_from(value) {
            Ok(pb::Transport::Hybrid) => Ok(Self::Hybrid),
            Ok(pb::Transport::Tcp) => Ok(Self::Tcp),
            Ok(pb::Transport::Vnt) => Ok(Self::Vnt),
            Ok(pb::Transport::Quic) => Ok(Self::Quic),
            _ => Err("未知通信协议"),
        }
    }
}

/// Negotiated platform capabilities (protocol v1).
pub const CAP_TEXT: u32 = 1;
pub const CAP_IMAGE: u32 = 2;
pub const CAP_FILES: u32 = 4;
pub const CAP_RESOLUTION: u32 = 8;
pub fn platform_capabilities() -> u32 {
    CAP_TEXT
        | CAP_IMAGE
        | if cfg!(windows) {
            CAP_FILES | CAP_RESOLUTION
        } else {
            0
        }
}
