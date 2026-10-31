//! One source of truth for remote channel ownership. Local IPC lifecycle messages
//! are deliberately excluded from the remote application protocol.
use crate::{control::ControlMessage as Message, pb::ChannelKind, Transport};

pub fn channel_for(message: &Message, transport: Transport) -> ChannelKind {
    if let Message::ScreenPacket(packet) = message {
        return match crate::screen::kind(packet) {
            Some(crate::control::MessageType::VideoFrame) => ChannelKind::ChannelMedia,
            Some(crate::control::MessageType::StreamReset) if transport == Transport::Tcp => {
                ChannelKind::ChannelMedia
            }
            _ => ChannelKind::ChannelControl,
        };
    }
    match message {
        Message::ClipboardReply(_) | Message::ClipboardImageChunk(_) => ChannelKind::ChannelFiles,
        Message::VideoFrame(_) | Message::AudioFrame(_) => ChannelKind::ChannelMedia,
        Message::StreamReset(_) | Message::AudioState(_) if transport == Transport::Tcp => {
            ChannelKind::ChannelMedia
        }
        _ => ChannelKind::ChannelControl,
    }
}
pub fn allowed_on(message: &Message, channel: ChannelKind, transport: Transport) -> bool {
    !matches!(
        message,
        Message::ScreenThrottle(_)
            | Message::Agent(_)
            | Message::StopVideo(_)
            | Message::CursorPosition(_)
            | Message::TransportState(_)
            | Message::ResetInput(_)
    ) && channel_for(message, transport) == channel
        && (transport == Transport::Tcp
            || !(crate::screen::is_video(message) || matches!(message, Message::AudioFrame(_))))
}
