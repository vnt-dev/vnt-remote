//! Multiplex existing application messages without mixing reference chains.
use crate::{
    control::{decode_message, parse_header, ControlMessage as Message},
    pb,
};

pub fn wrap(stream_id: u32, generation: u32, message: Message) -> Message {
    Message::ScreenPacket(pb::ScreenPacket {
        stream_id,
        generation,
        message: message.encode_frame(),
        revision: 0,
    })
}

pub fn unwrap(packet: &pb::ScreenPacket) -> Option<Message> {
    let kind = kind(packet)?;
    decode_message(kind, &packet.message[5..]).ok()
}

/// Inspect routing without copying the encoded video payload.
pub fn kind(packet: &pb::ScreenPacket) -> Option<crate::control::MessageType> {
    if packet.stream_id == 0 || packet.stream_id > 128 || packet.generation == 0 {
        return None;
    }
    let header: &[u8; 5] = packet.message.get(..5)?.try_into().ok()?;
    let (kind, length) = parse_header(header).ok()?;
    if packet.message.len() != 5 + length {
        return None;
    }
    use crate::control::MessageType as T;
    if !matches!(
        kind,
        T::VideoFrame
            | T::StreamReset
            | T::DesktopState
            | T::StatsReport
            | T::LinkFeedback
            | T::ScreenThrottle
            | T::RequestKeyframe
            | T::InputEvent
            | T::ClipboardPaste
            | T::ResetInput
    ) {
        return None;
    }
    Some(kind)
}

pub fn inner(message: &Message) -> Option<Message> {
    if let Message::ScreenPacket(packet) = message {
        unwrap(packet)
    } else {
        None
    }
}

pub fn is_video(message: &Message) -> bool {
    matches!(message, Message::VideoFrame(_))
        || matches!(message,Message::ScreenPacket(p) if kind(p)==Some(crate::control::MessageType::VideoFrame))
}

pub fn is_config(message: &Message) -> bool {
    matches!(message, Message::StreamReset(_))
        || matches!(message,Message::ScreenPacket(p) if kind(p)==Some(crate::control::MessageType::StreamReset))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn envelopes_reject_nesting_and_non_screen_messages() {
        let video = Message::VideoFrame(pb::VideoFrame {
            dependencies: None,
            keyframe: true,
            pts_ms: 1,
            data: vec![1],
        });
        assert_eq!(inner(&wrap(2, 3, video.clone())), Some(video));
        assert!(inner(&wrap(2, 3, wrap(1, 1, Message::RequestKeyframe))).is_none());
        assert!(inner(&wrap(2, 3, Message::SendSas)).is_none());
        assert!(inner(&wrap(0, 1, Message::RequestKeyframe)).is_none());
    }

    #[test]
    fn background_control_roundtrips_only_on_the_reliable_control_lane() {
        use crate::{
            control::{decode_message, parse_header},
            pb::ChannelKind,
            session, Transport,
        };
        for background in [false, true] {
            let inner = Message::ScreenThrottle(pb::ScreenThrottle { background });
            let message = wrap(2, 3, inner.clone());
            let frame = message.encode_frame();
            let (kind, _) = parse_header(frame[..5].try_into().unwrap()).unwrap();
            let decoded = decode_message(kind, &frame[5..]).unwrap();
            assert_eq!(self::inner(&decoded), Some(inner.clone()));
            for transport in [Transport::Tcp, Transport::Hybrid, Transport::Quic] {
                assert!(session::allowed_on(
                    &decoded,
                    ChannelKind::ChannelControl,
                    transport
                ));
                assert!(!session::allowed_on(
                    &decoded,
                    ChannelKind::ChannelMedia,
                    transport
                ));
                assert!(!session::allowed_on(
                    &decoded,
                    ChannelKind::ChannelFiles,
                    transport
                ));
                assert!(!session::allowed_on(
                    &inner,
                    ChannelKind::ChannelControl,
                    transport
                ));
            }
            assert!(self::inner(&wrap(2, 0, inner)).is_none());
        }
    }
}
