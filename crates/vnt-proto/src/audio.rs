//! One Opus packet per UDP datagram, sharing the video socket but not its sequencing.
use crate::pb;
use crate::udp::{FragHeader, FRAG_HEADER_LEN};
use prost::Message;

pub const FLAG_AUDIO: u8 = 0x02;
pub const MAX_OPUS_BYTES: usize = 1275;
pub const MAX_AUDIO_PAYLOAD: usize = MAX_OPUS_BYTES + 32;
pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: u64 = 480;

pub fn valid_frame(frame: &pb::AudioFrame) -> bool {
    frame.generation != 0
        && !frame.data.is_empty()
        && frame.data.len() <= MAX_OPUS_BYTES
        && frame.timestamp_samples.is_multiple_of(FRAME_SAMPLES)
        && (frame.timestamp_samples / FRAME_SAMPLES) as u32 == frame.sequence
}

pub fn encode_datagram(session_id: u32, frame: &pb::AudioFrame) -> Option<Vec<u8>> {
    if !valid_frame(frame) {
        return None;
    }
    let mut out = vec![0; FRAG_HEADER_LEN];
    FragHeader {
        session_id,
        frame_seq: 0,
        frag_idx: 0,
        frag_count: 1,
        flags: FLAG_AUDIO,
        pts_ms: 0,
    }
    .encode_to(&mut out);
    frame.encode(&mut out).ok()?;
    Some(out)
}

pub fn decode_datagram(packet: &[u8], session_id: u32) -> Option<pb::AudioFrame> {
    let (header, body) = FragHeader::decode(packet)?;
    if header.session_id != session_id
        || header.flags != FLAG_AUDIO
        || header.frag_count != 1
        || header.frag_idx != 0
        || body.len() > MAX_AUDIO_PAYLOAD
    {
        return None;
    }
    let frame = pb::AudioFrame::decode(body).ok()?;
    valid_frame(&frame).then_some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_audio_is_session_scoped_and_does_not_fragment() {
        let mut frame = pb::AudioFrame {
            generation: 1,
            sequence: u32::MAX,
            timestamp_samples: u32::MAX as u64 * 480,
            data: vec![42; MAX_OPUS_BYTES],
        };
        let packet = encode_datagram(7, &frame).unwrap();
        assert!(packet.len() < 1400);
        assert_eq!(decode_datagram(&packet, 7), Some(frame.clone()));
        assert!(decode_datagram(&packet, 8).is_none());
        frame.data.push(0);
        assert!(encode_datagram(7, &frame).is_none());
        frame.data = vec![1];
        frame.timestamp_samples += 480;
        frame.sequence = 0;
        assert!(valid_frame(&frame));
        frame.timestamp_samples += 1;
        assert!(!valid_frame(&frame));
    }
}
