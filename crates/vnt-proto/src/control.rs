//! TCP 控制通道消息：帧格式 `[u8 msg_type][u32 BE len][protobuf payload]`。

use crate::pb;
use prost::Message;

/// TCP 帧最大 payload（防异常长度撑爆内存；视频消息使用独立上限）。
const MAX_PAYLOAD: usize = 64 * 1024;

/// 剪贴板文本上限。文本整体内联在一条控制消息里（本地局域网，可容忍
/// 一次 4 MiB 的分配），超过则拒绝而不是分片。
pub const MAX_CLIPBOARD_TEXT: usize = 4 * 1024 * 1024;

/// 剪贴板文件清单（元数据）上限：4096 个 item、每个 rel_path ≤ 1 KiB 时
/// 约 4.5 MiB，留出余量后取 8 MiB。
pub const MAX_CLIPBOARD_FILES: usize = 8 * 1024 * 1024;

/// 文件数据分片大小（与 control 帧的 64 KiB 上限匹配，留出消息头余量）。
pub const CLIPBOARD_CHUNK_BYTES: usize = 32 * 1024;

/// UDP 通道上的绑定消息类型号（与 TCP MessageType 命名空间独立）。
pub const UDP_TYPE_UDP_BIND: u8 = 100;

/// 构造 UDP 绑定包：`[100][u32 len][protobuf UdpBind]`。
pub fn encode_udp_bind_frame(session_id: u32, bind_token: &[u8]) -> Vec<u8> {
    let body = pb::UdpBind {
        session_id,
        bind_token: bind_token.to_vec(),
    }
    .encode_to_vec();
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.push(UDP_TYPE_UDP_BIND);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// 解析 UDP 绑定包，返回 (session_id, bind_token)。
pub fn decode_udp_bind_frame(buf: &[u8]) -> Option<(u32, Vec<u8>)> {
    if buf.len() < HEADER_LEN || buf[0] != UDP_TYPE_UDP_BIND {
        return None;
    }
    let len = u32::from_be_bytes(buf[1..5].try_into().ok()?) as usize;
    let body = &buf[HEADER_LEN..];
    if body.len() != len || len > MAX_PAYLOAD {
        return None;
    }
    let message = pb::UdpBind::decode(body).ok()?;
    if message.bind_token.len() != 16 {
        return None;
    }
    Some((message.session_id, message.bind_token))
}

pub const HEADER_LEN: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub enum ControlMessage {
    ClipboardPaste(pb::ClipboardPaste),
    ClipboardPasteResult(pb::ClipboardPasteResult),
    ScreenThrottle(pb::ScreenThrottle),
    VideoNack(pb::VideoNack),
    CursorPosition(pb::CursorPosition),
    StopVideo(pb::StopVideo),
    ScreensState(pb::ScreensState),
    ScreenCommand(pb::ScreenCommand),
    ScreenPacket(pb::ScreenPacket),
    Agent(pb::AgentMessage),
    ResetInput(pb::ResetInput),
    TransportState(pb::TransportState),
    ClipboardImageReady(pb::ClipboardImageReady),
    SessionPacket(pb::SessionPacket),
    DesktopState(pb::DesktopState),
    SendSas,
    AuthChallenge(pb::AuthChallenge),
    AuthResponse(pb::AuthResponse),
    Hello(pb::Hello),
    Welcome(pb::Welcome),
    Denied(pb::Denied),
    UdpBindAck(pb::UdpBindAck),
    StreamReset(pb::StreamReset),
    InputEvent(pb::InputEvent),
    RequestKeyframe,
    Ping(pb::Ping),
    Pong(pb::Pong),
    SessionEnd(pb::SessionEnd),
    StatsReport(pb::StatsReport),
    LinkFeedback(pb::LinkFeedback),
    VideoFrame(pb::VideoFrame),
    AudioFrame(pb::AudioFrame),
    AudioState(pb::AudioState),
    ClipboardOffer(pb::ClipboardOffer),
    ClipboardRequest(pb::ClipboardRequest),
    ClipboardReply(pb::ClipboardReply),
    ClipboardText(pb::ClipboardText),
    ClipboardImage(pb::ClipboardImage),
    ClipboardImageCancel(pb::ClipboardImageCancel),
    ClipboardImageChunk(pb::ClipboardImageChunk),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    ClipboardPaste = 42,
    ClipboardPasteResult = 43,
    ScreenThrottle = 41,
    VideoNack = 40,
    CursorPosition = 39,
    StopVideo = 38,
    ScreensState = 37,
    ScreenCommand = 36,
    ScreenPacket = 35,
    Agent = 34,
    ResetInput = 33,
    TransportState = 32,
    ClipboardImageReady = 31,
    SessionPacket = 30,
    ClipboardImageCancel = 29,
    ClipboardImage = 27,
    ClipboardImageChunk = 28,
    DesktopState = 23,
    SendSas = 24,
    AuthChallenge = 25,
    AuthResponse = 26,
    Hello = 1,
    Welcome = 2,
    Denied = 3,
    UdpBindAck = 4,
    StreamReset = 5,
    InputEvent = 6,
    RequestKeyframe = 7,
    Ping = 8,
    Pong = 9,
    SessionEnd = 10,
    StatsReport = 11,
    LinkFeedback = 12,
    VideoFrame = 13,
    AudioFrame = 21,
    AudioState = 22,
    ClipboardOffer = 18,
    ClipboardRequest = 19,
    ClipboardReply = 20,
    ClipboardText = 14,
}

impl ControlMessage {
    pub fn msg_type(&self) -> MessageType {
        match self {
            ControlMessage::ScreenThrottle(_) => MessageType::ScreenThrottle,
            ControlMessage::VideoNack(_) => MessageType::VideoNack,
            ControlMessage::CursorPosition(_) => MessageType::CursorPosition,
            ControlMessage::StopVideo(_) => MessageType::StopVideo,
            ControlMessage::ScreensState(_) => MessageType::ScreensState,
            ControlMessage::ScreenCommand(_) => MessageType::ScreenCommand,
            ControlMessage::ScreenPacket(_) => MessageType::ScreenPacket,
            ControlMessage::Agent(_) => MessageType::Agent,
            ControlMessage::ResetInput(_) => MessageType::ResetInput,
            ControlMessage::TransportState(_) => MessageType::TransportState,
            ControlMessage::ClipboardImageReady(_) => MessageType::ClipboardImageReady,
            ControlMessage::SessionPacket(_) => MessageType::SessionPacket,
            ControlMessage::ClipboardImage(_) => MessageType::ClipboardImage,
            ControlMessage::ClipboardImageCancel(_) => MessageType::ClipboardImageCancel,
            ControlMessage::ClipboardImageChunk(_) => MessageType::ClipboardImageChunk,
            ControlMessage::DesktopState(_) => MessageType::DesktopState,
            ControlMessage::SendSas => MessageType::SendSas,
            ControlMessage::AuthChallenge(_) => MessageType::AuthChallenge,
            ControlMessage::AuthResponse(_) => MessageType::AuthResponse,
            ControlMessage::Hello(_) => MessageType::Hello,
            ControlMessage::Welcome(_) => MessageType::Welcome,
            ControlMessage::Denied(_) => MessageType::Denied,
            ControlMessage::UdpBindAck(_) => MessageType::UdpBindAck,
            ControlMessage::StreamReset(_) => MessageType::StreamReset,
            ControlMessage::InputEvent(_) => MessageType::InputEvent,
            ControlMessage::RequestKeyframe => MessageType::RequestKeyframe,
            ControlMessage::Ping(_) => MessageType::Ping,
            ControlMessage::Pong(_) => MessageType::Pong,
            ControlMessage::SessionEnd(_) => MessageType::SessionEnd,
            ControlMessage::StatsReport(_) => MessageType::StatsReport,
            ControlMessage::LinkFeedback(_) => MessageType::LinkFeedback,
            ControlMessage::VideoFrame(_) => MessageType::VideoFrame,
            ControlMessage::AudioFrame(_) => MessageType::AudioFrame,
            ControlMessage::AudioState(_) => MessageType::AudioState,
            ControlMessage::ClipboardOffer(_) => MessageType::ClipboardOffer,
            ControlMessage::ClipboardRequest(_) => MessageType::ClipboardRequest,
            ControlMessage::ClipboardReply(_) => MessageType::ClipboardReply,
            ControlMessage::ClipboardText(_) => MessageType::ClipboardText,
            ControlMessage::ClipboardPaste(_) => MessageType::ClipboardPaste,
            ControlMessage::ClipboardPasteResult(_) => MessageType::ClipboardPasteResult,
        }
    }

    /// 序列化为完整 TCP 帧（含 5 字节头）。
    pub fn encode_frame(&self) -> Vec<u8> {
        let (ty, body) = match self {
            ControlMessage::ScreenThrottle(m) => (MessageType::ScreenThrottle, m.encode_to_vec()),
            ControlMessage::VideoNack(m) => (MessageType::VideoNack, m.encode_to_vec()),
            ControlMessage::CursorPosition(m) => (MessageType::CursorPosition, m.encode_to_vec()),
            ControlMessage::StopVideo(m) => (MessageType::StopVideo, m.encode_to_vec()),
            ControlMessage::ScreensState(m) => (MessageType::ScreensState, m.encode_to_vec()),
            ControlMessage::ScreenCommand(m) => (MessageType::ScreenCommand, m.encode_to_vec()),
            ControlMessage::ScreenPacket(m) => (MessageType::ScreenPacket, m.encode_to_vec()),
            ControlMessage::Agent(m) => (MessageType::Agent, m.encode_to_vec()),
            ControlMessage::ResetInput(m) => (MessageType::ResetInput, m.encode_to_vec()),
            ControlMessage::TransportState(m) => (MessageType::TransportState, m.encode_to_vec()),
            ControlMessage::ClipboardImageReady(m) => {
                (MessageType::ClipboardImageReady, m.encode_to_vec())
            }
            ControlMessage::SessionPacket(m) => (MessageType::SessionPacket, m.encode_to_vec()),
            ControlMessage::ClipboardImageCancel(m) => {
                (MessageType::ClipboardImageCancel, m.encode_to_vec())
            }
            ControlMessage::ClipboardImage(m) => (MessageType::ClipboardImage, m.encode_to_vec()),
            ControlMessage::ClipboardImageChunk(m) => {
                (MessageType::ClipboardImageChunk, m.encode_to_vec())
            }
            ControlMessage::DesktopState(m) => (MessageType::DesktopState, m.encode_to_vec()),
            ControlMessage::SendSas => (MessageType::SendSas, Vec::new()),
            ControlMessage::AuthChallenge(m) => (MessageType::AuthChallenge, m.encode_to_vec()),
            ControlMessage::AuthResponse(m) => (MessageType::AuthResponse, m.encode_to_vec()),
            ControlMessage::Hello(m) => (MessageType::Hello, m.encode_to_vec()),
            ControlMessage::Welcome(m) => (MessageType::Welcome, m.encode_to_vec()),
            ControlMessage::Denied(m) => (MessageType::Denied, m.encode_to_vec()),
            ControlMessage::UdpBindAck(m) => (MessageType::UdpBindAck, m.encode_to_vec()),
            ControlMessage::StreamReset(m) => (MessageType::StreamReset, m.encode_to_vec()),
            ControlMessage::InputEvent(m) => (MessageType::InputEvent, m.encode_to_vec()),
            ControlMessage::RequestKeyframe => (MessageType::RequestKeyframe, Vec::new()),
            ControlMessage::Ping(m) => (MessageType::Ping, m.encode_to_vec()),
            ControlMessage::Pong(m) => (MessageType::Pong, m.encode_to_vec()),
            ControlMessage::SessionEnd(m) => (MessageType::SessionEnd, m.encode_to_vec()),
            ControlMessage::StatsReport(m) => (MessageType::StatsReport, m.encode_to_vec()),
            ControlMessage::LinkFeedback(m) => (MessageType::LinkFeedback, m.encode_to_vec()),
            ControlMessage::VideoFrame(m) => (MessageType::VideoFrame, m.encode_to_vec()),
            ControlMessage::AudioFrame(m) => (MessageType::AudioFrame, m.encode_to_vec()),
            ControlMessage::AudioState(m) => (MessageType::AudioState, m.encode_to_vec()),
            ControlMessage::ClipboardOffer(m) => (MessageType::ClipboardOffer, m.encode_to_vec()),
            ControlMessage::ClipboardRequest(m) => {
                (MessageType::ClipboardRequest, m.encode_to_vec())
            }
            ControlMessage::ClipboardReply(m) => (MessageType::ClipboardReply, m.encode_to_vec()),
            ControlMessage::ClipboardText(m) => (MessageType::ClipboardText, m.encode_to_vec()),
            ControlMessage::ClipboardPaste(m) => (MessageType::ClipboardPaste, m.encode_to_vec()),
            ControlMessage::ClipboardPasteResult(m) => (MessageType::ClipboardPasteResult, m.encode_to_vec()),
        };
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.push(ty as u8);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("未知消息类型 {0}")]
    UnknownType(u8),
    #[error("payload 超限: {0} > {1}")]
    Oversized(usize, usize),
    #[error("protobuf 解码失败: {0}")]
    Protobuf(#[from] prost::DecodeError),
    #[error("消息类型与 payload 不匹配")]
    Mismatch,
}

/// 解析帧头，返回 (消息类型, payload 长度)。
pub fn parse_header(header: &[u8; HEADER_LEN]) -> Result<(MessageType, usize), DecodeError> {
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let ty = match header[0] {
        41 => MessageType::ScreenThrottle,
        39 => MessageType::CursorPosition,
        38 => MessageType::StopVideo,
        37 => MessageType::ScreensState,
        36 => MessageType::ScreenCommand,
        35 => MessageType::ScreenPacket,
        34 => MessageType::Agent,
        33 => MessageType::ResetInput,
        32 => MessageType::TransportState,
        31 => MessageType::ClipboardImageReady,
        30 => MessageType::SessionPacket,
        29 => MessageType::ClipboardImageCancel,
        27 => MessageType::ClipboardImage,
        28 => MessageType::ClipboardImageChunk,
        23 => MessageType::DesktopState,
        24 => MessageType::SendSas,
        25 => MessageType::AuthChallenge,
        26 => MessageType::AuthResponse,
        1 => MessageType::Hello,
        2 => MessageType::Welcome,
        3 => MessageType::Denied,
        4 => MessageType::UdpBindAck,
        5 => MessageType::StreamReset,
        6 => MessageType::InputEvent,
        7 => MessageType::RequestKeyframe,
        40 => MessageType::VideoNack,
        8 => MessageType::Ping,
        9 => MessageType::Pong,
        10 => MessageType::SessionEnd,
        11 => MessageType::StatsReport,
        12 => MessageType::LinkFeedback,
        13 => MessageType::VideoFrame,
        21 => MessageType::AudioFrame,
        22 => MessageType::AudioState,
        18 => MessageType::ClipboardOffer,
        19 => MessageType::ClipboardRequest,
        20 => MessageType::ClipboardReply,
        14 => MessageType::ClipboardText,
        42 => MessageType::ClipboardPaste,
        43 => MessageType::ClipboardPasteResult,
        other => return Err(DecodeError::UnknownType(other)),
    };
    let limit = match ty {
        MessageType::VideoFrame => crate::udp::MAX_FRAME_BYTES + crate::recovery::MAX_METADATA + 32,
        MessageType::ScreenPacket => crate::udp::MAX_FRAME_BYTES + 1024,
        MessageType::Agent => crate::udp::MAX_FRAME_BYTES + 1024,
        MessageType::AudioFrame => crate::audio::MAX_AUDIO_PAYLOAD,
        MessageType::AudioState => 1040,
        MessageType::ClipboardText => MAX_CLIPBOARD_TEXT,
        MessageType::ClipboardPaste => MAX_CLIPBOARD_TEXT + 32,
        MessageType::ClipboardPasteResult => 1056,
        MessageType::ClipboardOffer => MAX_CLIPBOARD_FILES,
        _ => MAX_PAYLOAD,
    };
    if len > limit {
        return Err(DecodeError::Oversized(len, limit));
    }
    Ok((ty, len))
}

/// 从 (类型, payload 字节) 解出消息。
pub fn decode_message(ty: MessageType, body: &[u8]) -> Result<ControlMessage, DecodeError> {
    let msg = match ty {
        MessageType::CursorPosition => {
            ControlMessage::CursorPosition(pb::CursorPosition::decode(body)?)
        }
        MessageType::StopVideo => ControlMessage::StopVideo(pb::StopVideo::decode(body)?),
        MessageType::ScreensState => {
            let state = pb::ScreensState::decode(body)?;
            let mut ids = std::collections::HashSet::new();
            if state.screens.is_empty()
                || state.screens.len() > 128
                || state.revision == 0
                || !state
                    .screens
                    .iter()
                    .any(|s| s.stream_id == state.active_stream_id)
                || state.screens.iter().any(|s| {
                    s.stream_id == 0
                        || s.stream_id > 128
                        || !ids.insert(s.stream_id)
                        || s.generation == 0
                        || s.name.len() > 256
                        || s.reason.len() > 1024
                        || (s.available
                            && (!crate::validation::valid_video_size(s.width, s.height)
                                || !(1..=120).contains(&s.fps)
                                || crate::Codec::from_proto(s.codec()).is_none()))
                })
            {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ScreensState(state)
        }
        MessageType::ScreenCommand => {
            let command = pb::ScreenCommand::decode(body)?;
            if command.stream_id == 0
                || command.stream_id > 128
                || pb::screen_command::Action::try_from(command.action).is_err()
            {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ScreenCommand(command)
        }
        MessageType::ScreenPacket => {
            let packet = pb::ScreenPacket::decode(body)?;
            if crate::screen::kind(&packet).is_none() {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ScreenPacket(packet)
        }
        MessageType::Agent => {
            let value = pb::AgentMessage::decode(body)?;
            if value.generation == 0 || value.payload.is_none() {
                return Err(DecodeError::Mismatch);
            }
            if let Some(pb::agent_message::Payload::Desktop(d)) = &value.payload {
                if d.desktop.len() > 256 || d.reason.len() > 1024 || d.monitors_json.len() > 65536 {
                    return Err(DecodeError::Mismatch);
                }
            }
            ControlMessage::Agent(value)
        }
        MessageType::ResetInput => ControlMessage::ResetInput(pb::ResetInput::decode(body)?),
        MessageType::TransportState => {
            ControlMessage::TransportState(pb::TransportState::decode(body)?)
        }
        MessageType::ClipboardImageReady => {
            ControlMessage::ClipboardImageReady(pb::ClipboardImageReady::decode(body)?)
        }
        MessageType::SessionPacket => {
            let value = pb::SessionPacket::decode(body)?;
            if pb::SessionOperation::try_from(value.op).is_err()
                || value.op == 0
                || pb::ChannelKind::try_from(value.kind).is_err()
                || value.session_id == 0
                || value.attempt.len() > 16
                || value.nonce.len() > 32
                || value.credential.len() > 32
                || value.reason.len() > 1024
            {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::SessionPacket(value)
        }
        MessageType::ClipboardImageCancel => {
            let m = pb::ClipboardImageCancel::decode(body)?;
            if m.image_id == 0 {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ClipboardImageCancel(m)
        }
        MessageType::ClipboardImage => {
            let m = pb::ClipboardImage::decode(body)?;
            if !crate::clipboard::valid_image(&m) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ClipboardImage(m)
        }
        MessageType::ClipboardImageChunk => {
            let m = pb::ClipboardImageChunk::decode(body)?;
            if !crate::clipboard::valid_chunk(&m) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ClipboardImageChunk(m)
        }
        MessageType::AuthResponse => {
            let m = pb::AuthResponse::decode(body)?;
            if m.proof.len() != crate::auth::PROOF_LEN {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::AuthResponse(m)
        }
        MessageType::AuthChallenge => {
            let m = pb::AuthChallenge::decode(body)?;
            if m.nonce.len() != crate::auth::NONCE_LEN {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::AuthChallenge(m)
        }
        MessageType::DesktopState => {
            let state = pb::DesktopState::decode(body)?;
            if state.desktop.len() > 256 || state.reason.len() > 1024 {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::DesktopState(state)
        }
        MessageType::SendSas => {
            if !body.is_empty() {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::SendSas
        }
        MessageType::ScreenThrottle => {
            ControlMessage::ScreenThrottle(pb::ScreenThrottle::decode(body)?)
        }
        MessageType::VideoNack => {
            let nack = pb::VideoNack::decode(body)?;
            if !crate::recovery::valid_nack(&nack) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::VideoNack(nack)
        }
        MessageType::VideoFrame => {
            let frame = pb::VideoFrame::decode(body)?;
            if frame.data.is_empty()
                || frame.data.len() > crate::udp::MAX_FRAME_BYTES
                || frame
                    .dependencies
                    .as_ref()
                    .is_some_and(|d| !crate::recovery::valid_dependencies(d))
            {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::VideoFrame(frame)
        }
        MessageType::AudioFrame => {
            let frame = pb::AudioFrame::decode(body)?;
            if !crate::audio::valid_frame(&frame) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::AudioFrame(frame)
        }
        MessageType::AudioState => {
            let state = pb::AudioState::decode(body)?;
            if state.reason.len() > 1024 || (state.available && state.generation == 0) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::AudioState(state)
        }
        MessageType::Hello => ControlMessage::Hello(pb::Hello::decode(body)?),
        MessageType::Welcome => ControlMessage::Welcome(pb::Welcome::decode(body)?),
        MessageType::Denied => ControlMessage::Denied(pb::Denied::decode(body)?),
        MessageType::UdpBindAck => ControlMessage::UdpBindAck(pb::UdpBindAck::decode(body)?),
        MessageType::StreamReset => ControlMessage::StreamReset(pb::StreamReset::decode(body)?),
        MessageType::InputEvent => ControlMessage::InputEvent(pb::InputEvent::decode(body)?),
        MessageType::RequestKeyframe => {
            if !body.is_empty() {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::RequestKeyframe
        }
        MessageType::Ping => ControlMessage::Ping(pb::Ping::decode(body)?),
        MessageType::Pong => ControlMessage::Pong(pb::Pong::decode(body)?),
        MessageType::SessionEnd => ControlMessage::SessionEnd(pb::SessionEnd::decode(body)?),
        MessageType::StatsReport => ControlMessage::StatsReport(pb::StatsReport::decode(body)?),
        MessageType::LinkFeedback => ControlMessage::LinkFeedback(pb::LinkFeedback::decode(body)?),
        MessageType::ClipboardOffer => {
            let value = pb::ClipboardOffer::decode(body)?;
            if !crate::validation::valid_clipboardoffer(&value) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ClipboardOffer(value)
        }
        MessageType::ClipboardRequest => {
            let value = pb::ClipboardRequest::decode(body)?;
            if !crate::validation::valid_clipboardrequest(&value) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ClipboardRequest(value)
        }
        MessageType::ClipboardReply => {
            let value = pb::ClipboardReply::decode(body)?;
            if !crate::validation::valid_clipboardreply(&value) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ClipboardReply(value)
        }
        MessageType::ClipboardPaste => {
            let m=pb::ClipboardPaste::decode(body)?;
            if m.request_id==0 || !crate::validation::valid_clipboard_text(&m.text) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ClipboardPaste(m)
        }
        MessageType::ClipboardPasteResult => {
            let m=pb::ClipboardPasteResult::decode(body)?;
            if m.request_id==0 || m.reason.len()>1024 {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ClipboardPasteResult(m)
        }
        MessageType::ClipboardText => {
            let text = pb::ClipboardText::decode(body)?;
            if !crate::validation::valid_clipboard_text(&text.text) {
                return Err(DecodeError::Mismatch);
            }
            ControlMessage::ClipboardText(text)
        }
    };
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_desktop_messages_are_bounded_and_sas_has_no_payload() {
        let frame = ControlMessage::DesktopState(pb::DesktopState {
            min_pts_ms: 0,
            ready: false,
            desktop: "Winlogon".into(),
            reason: "切换中".into(),
            generation: 2,
        })
        .encode_frame();
        let (ty, len) = parse_header(frame[..5].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - 5);
        assert!(
            matches!(decode_message(ty,&frame[5..]).unwrap(),ControlMessage::DesktopState(state) if !state.ready && state.generation==2)
        );
        assert!(decode_message(MessageType::SendSas, &[1]).is_err());
        assert!(decode_message(
            MessageType::DesktopState,
            &pb::DesktopState {
                min_pts_ms: 0,
                ready: false,
                desktop: "x".repeat(257),
                reason: String::new(),
                generation: 1
            }
            .encode_to_vec()
        )
        .is_err());
    }
    #[test]
    fn audio_messages_roundtrip_and_reject_unbounded_or_invalid_packets() {
        let frame = pb::AudioFrame {
            generation: 1,
            sequence: 3,
            timestamp_samples: 1440,
            data: vec![42; 80],
        };
        let state = pb::AudioState {
            generation: 1,
            available: true,
            reason: String::new(),
        };
        for message in [
            ControlMessage::AudioFrame(frame.clone()),
            ControlMessage::AudioState(state),
        ] {
            let encoded = message.encode_frame();
            let (ty, len) = parse_header(encoded[..5].try_into().unwrap()).unwrap();
            assert_eq!(len, encoded.len() - 5);
            assert_eq!(
                decode_message(ty, &encoded[5..]).unwrap().encode_frame(),
                encoded
            );
        }
        let mut invalid = frame;
        invalid.data = vec![1; crate::audio::MAX_OPUS_BYTES + 1];
        assert!(decode_message(MessageType::AudioFrame, &invalid.encode_to_vec()).is_err());
        let mut header = [MessageType::AudioFrame as u8, 0, 0, 0, 0];
        header[1..].copy_from_slice(&65536u32.to_be_bytes());
        assert!(parse_header(&header).is_err());
        let invalid = pb::AudioState {
            generation: 0,
            available: true,
            reason: String::new(),
        };
        assert!(decode_message(MessageType::AudioState, &invalid.encode_to_vec()).is_err());
    }

    #[test]
    fn video_frames_have_an_independent_bounded_payload() {
        for size in [70_000, crate::udp::MAX_FRAME_BYTES] {
            let video = pb::VideoFrame {
                dependencies: None,
                keyframe: true,
                pts_ms: u32::MAX,
                data: vec![42; size],
            };
            let frame = ControlMessage::VideoFrame(video.clone()).encode_frame();
            let (ty, len) = parse_header(frame[..5].try_into().unwrap()).unwrap();
            assert_eq!(len, frame.len() - 5);
            assert!(
                matches!(decode_message(ty, &frame[5..]).unwrap(), ControlMessage::VideoFrame(v) if v == video)
            );
        }
        let mut header = [13, 0, 0, 0, 0];
        header[1..].copy_from_slice(
            &((crate::udp::MAX_FRAME_BYTES + crate::recovery::MAX_METADATA + 33) as u32)
                .to_be_bytes(),
        );
        assert!(matches!(
            parse_header(&header),
            Err(DecodeError::Oversized(_, _))
        ));
        header[0] = MessageType::StatsReport as u8;
        header[1..].copy_from_slice(&70_000u32.to_be_bytes());
        assert!(matches!(
            parse_header(&header),
            Err(DecodeError::Oversized(_, _))
        ));
        for size in [0, crate::udp::MAX_FRAME_BYTES + 1] {
            let frame = pb::VideoFrame {
                data: vec![1; size],
                ..Default::default()
            }
            .encode_to_vec();
            assert!(decode_message(MessageType::VideoFrame, &frame).is_err());
        }
        for value in [0, -1, 5, i32::MAX] {
            assert!(crate::Transport::from_wire(value).is_err());
        }
        assert_eq!(crate::Transport::from_wire(2), Ok(crate::Transport::Tcp));
        assert_eq!(crate::Transport::from_wire(4), Ok(crate::Transport::Quic));
    }

    #[test]
    fn feedback_and_extended_stats_roundtrip() {
        assert_eq!(crate::PROTO_VER, 1);
        let report = pb::LinkFeedback {
            fec: Vec::new(),
            sequence: 7,
            interval_ms: 1000,
            received_frames: 28,
            lost_frames: 2,
            received_bytes: 800_000,
            frame_gap_ms: Some(33),
            rtt_ms: Some(75),
        };
        let frame = ControlMessage::LinkFeedback(report.clone()).encode_frame();
        assert_eq!(frame[0], 12);
        let (ty, _) = parse_header(frame[..5].try_into().unwrap()).unwrap();
        let ControlMessage::LinkFeedback(decoded) = decode_message(ty, &frame[5..]).unwrap() else {
            panic!()
        };
        assert_eq!(decoded, report);
        let stats = pb::StatsReport {
            target_fps: 24,
            target_bitrate_kbps: 4000,
            applied_fps: 30,
            applied_bitrate_kbps: 8000,
            loss_permille: Some(0),
            ..Default::default()
        };
        let frame = ControlMessage::StatsReport(stats).encode_frame();
        let decoded = decode_message(MessageType::StatsReport, &frame[5..]).unwrap();
        assert!(matches!(decoded, ControlMessage::StatsReport(value) if value == stats));
    }

    #[test]
    fn bind_rejects_wrong_length_and_short_credentials() {
        let mut frame = encode_udp_bind_frame(7, &[1; 16]);
        assert!(decode_udp_bind_frame(&frame).is_some());
        frame[4] = frame[4].wrapping_add(1);
        assert!(decode_udp_bind_frame(&frame).is_none());
        assert!(decode_udp_bind_frame(&encode_udp_bind_frame(7, &[1; 4])).is_none());
    }

    #[test]
    fn frame_roundtrip() {
        let msgs = vec![
            ControlMessage::AuthResponse(pb::AuthResponse { proof: vec![1; 32] }),
            ControlMessage::AuthChallenge(pb::AuthChallenge { nonce: vec![1; 32] }),
            ControlMessage::Hello(pb::Hello {
                hardware_codecs: Vec::new(),
                color_depth: 24,
                desktop_effects: None,
                capabilities: crate::platform_capabilities(),
                fec_version: 0,
                resolution_mode: 0,
                target_width: 0,
                target_height: 0,
                audio_enabled: false,
                transport: crate::Transport::Hybrid.to_proto() as i32,
                fps: 30,
                bitrate_kbps: 8000,
                proto_ver: crate::PROTO_VER,
                codecs: vec![
                    pb::Codec::Av1.into(),
                    pb::Codec::Hevc.into(),
                    pb::Codec::H264.into(),
                ],
                client: Some(pb::ClientInfo {
                    name: "webview".into(),
                }),
            }),
            ControlMessage::Welcome(pb::Welcome {
                color_depth: 24,
                capabilities: crate::platform_capabilities(),
                fec_version: 0,
                screens: Vec::new(),
                host_edition: pb::HostEdition::Portable as i32,
                resolution_mode: 0,
                resolution_reason: String::new(),
                session_token: vec![0x34; 32],
                audio_enabled: false,
                transport: crate::Transport::Hybrid.to_proto() as i32,
                proto_ver: crate::PROTO_VER,
                session_id: 0xdead_beef,
                bind_token: vec![0x12; 16],
                codec: pb::Codec::Hevc.into(),
                width: 2560,
                height: 1440,
                fps: 60,
                hardware: true,
                decoder_config: vec![1, 2, 3, 4],
                encoder_name: "MF hw".into(),
                view_only: true,
            }),
            ControlMessage::InputEvent(pb::InputEvent {
                ev: Some(pb::input_event::Ev::MouseMove(pb::MouseMove {
                    x: 0.5,
                    y: 0.25,
                })),
            }),
            ControlMessage::RequestKeyframe,
            ControlMessage::Pong(pb::Pong { timestamp_ms: 42 }),
        ];
        for m in &msgs {
            let frame = m.encode_frame();
            let mut header = [0u8; HEADER_LEN];
            header.copy_from_slice(&frame[..HEADER_LEN]);
            let (ty, len) = parse_header(&header).unwrap();
            assert_eq!(len, frame.len() - HEADER_LEN);
            let decoded = decode_message(ty, &frame[HEADER_LEN..]).unwrap();
            assert_eq!(m.msg_type(), decoded.msg_type());
            match (m, decoded) {
                (ControlMessage::AuthChallenge(a), ControlMessage::AuthChallenge(b)) => {
                    assert_eq!(a.nonce, b.nonce);
                }
                (ControlMessage::AuthResponse(a), ControlMessage::AuthResponse(b)) => {
                    assert_eq!(a.proof, b.proof);
                }
                (ControlMessage::Welcome(a), ControlMessage::Welcome(b)) => {
                    assert_eq!(a.session_id, b.session_id);
                    assert_eq!(a.decoder_config, b.decoder_config);
                    assert_eq!(a.view_only, b.view_only);
                }
                (ControlMessage::Hello(a), ControlMessage::Hello(b)) => {
                    assert_eq!(a.codecs, b.codecs);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn oversized_rejected() {
        let header = [6u8, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(matches!(
            parse_header(&header),
            Err(DecodeError::Oversized(_, _))
        ));
    }

    #[test]
    fn unknown_type_rejected() {
        let header = [99u8, 0, 0, 0, 0];
        assert!(matches!(
            parse_header(&header),
            Err(DecodeError::UnknownType(99))
        ));
    }

    #[test]
    fn clipboard_text_roundtrip() {
        let message = ControlMessage::ClipboardText(pb::ClipboardText {
            text: "复制的文本 🎉".into(),
        });
        let frame = message.encode_frame();
        let (ty, len) = parse_header(frame[..HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - HEADER_LEN);
        assert_eq!(decode_message(ty, &frame[HEADER_LEN..]).unwrap(), message);
    }

    #[test]
    fn clipboard_payloads_have_dedicated_bounds() {
        let text = "x".repeat(MAX_CLIPBOARD_TEXT + 1);
        let mut header = [MessageType::ClipboardText as u8, 0, 0, 0, 0];
        header[1..].copy_from_slice(&((text.len() + 8) as u32).to_be_bytes());
        assert!(matches!(
            parse_header(&header),
            Err(DecodeError::Oversized(_, _))
        ));
        assert!(decode_message(
            MessageType::ClipboardText,
            &pb::ClipboardText {
                text: "x".repeat(MAX_CLIPBOARD_TEXT + 1),
            }
            .encode_to_vec()
        )
        .is_err());
    }
}

#[cfg(test)]
mod local_agent_tests {
    use super::*;
    #[test]
    fn local_video_roundtrips_but_is_rejected_by_every_remote_lane() {
        let m = ControlMessage::Agent(pb::AgentMessage {
            generation: 123,
            payload: Some(pb::agent_message::Payload::Video(pb::VideoFrame {
                dependencies: None,
                keyframe: true,
                pts_ms: 456,
                data: vec![9; 200_000],
            })),
        });
        let wire = m.encode_frame();
        let hdr = wire[..HEADER_LEN].try_into().unwrap();
        let (ty, len) = parse_header(&hdr).unwrap();
        assert_eq!(len, wire.len() - HEADER_LEN);
        assert_eq!(decode_message(ty, &wire[HEADER_LEN..]).unwrap(), m);
        for t in [
            crate::Transport::Tcp,
            crate::Transport::Hybrid,
            crate::Transport::Quic,
        ] {
            for c in [
                pb::ChannelKind::ChannelControl,
                pb::ChannelKind::ChannelFiles,
                pb::ChannelKind::ChannelMedia,
            ] {
                assert!(!crate::session::allowed_on(&m, c, t));
            }
        }
        assert!(decode_message(
            MessageType::Agent,
            &pb::AgentMessage::default().encode_to_vec()
        )
        .is_err());
    }
}

#[cfg(test)]
mod desktop_effects_tests {
    use crate::pb;
    use prost::Message;
    #[test]
    fn desktop_effects_roundtrip() {
        for wallpaper in [false, true] {
            let hello = pb::Hello {
                color_depth: 24,
                proto_ver: crate::PROTO_VER,
                desktop_effects: Some(pb::DesktopEffects {
                    show_wallpaper: wallpaper,
                    drag_full_windows: false,
                }),
                ..Default::default()
            };
            assert_eq!(
                pb::Hello::decode(hello.encode_to_vec().as_slice()).unwrap(),
                hello
            );
        }
    }
}
