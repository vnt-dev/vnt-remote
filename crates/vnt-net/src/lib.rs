//! 传输层（tokio）：
//! - [`tcp`]: TCP 控制通道（认证/协商/输入/心跳），5 字节帧头 + protobuf
//! - [`udp`]: UDP 视频通道（分片发送 + 重组接收 + 会话绑定）
//! - [`quic`]: 同端口直连 QUIC，可靠控制/文件流与音视频 DATAGRAM

pub mod auth;
pub mod fec;
pub mod repair;
pub mod tcp;
pub mod traffic;
pub mod udp;

pub mod timing;

pub mod session;

pub mod datagram;
pub mod overlay;

/// Direct QUIC, independent of the VNT overlay.
pub mod quic;
