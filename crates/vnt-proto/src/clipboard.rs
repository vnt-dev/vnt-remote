//! Image wire limits and ordered assembly, also used by the service broker.
use crate::{control::CLIPBOARD_CHUNK_BYTES, pb};
use std::time::{Duration, Instant};

pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_IMAGE_RGBA: usize = 64 * 1024 * 1024;
pub const MAX_IMAGE_DIMENSION: u32 = 16_384;
pub const IMAGE_TIMEOUT: Duration = Duration::from_secs(30);

pub fn valid_dimensions(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_IMAGE_DIMENSION
        && height <= MAX_IMAGE_DIMENSION
        && u64::from(width) * u64::from(height) * 4 <= MAX_IMAGE_RGBA as u64
}
pub fn valid_image(m: &pb::ClipboardImage) -> bool {
    m.image_id != 0
        && valid_dimensions(m.width, m.height)
        && m.total_bytes > 0
        && m.total_bytes as usize <= MAX_IMAGE_BYTES
}
pub fn valid_chunk(m: &pb::ClipboardImageChunk) -> bool {
    m.image_id != 0
        && !m.data.is_empty()
        && m.data.len() <= CLIPBOARD_CHUNK_BYTES
        && u64::from(m.offset) + m.data.len() as u64 <= MAX_IMAGE_BYTES as u64
}

pub struct ImageAssembly {
    pub header: pb::ClipboardImage,
    data: Vec<u8>,
    updated: Instant,
}
impl ImageAssembly {
    pub fn new(header: pb::ClipboardImage) -> Option<Self> {
        valid_image(&header).then(|| Self {
            header,
            data: Vec::new(),
            updated: Instant::now(),
        })
    }
    pub fn expired(&self) -> bool {
        self.updated.elapsed() >= IMAGE_TIMEOUT
    }
    pub fn push(&mut self, chunk: &pb::ClipboardImageChunk) -> Result<bool, &'static str> {
        if self.expired()
            || !valid_chunk(chunk)
            || chunk.image_id != self.header.image_id
            || chunk.offset as usize != self.data.len()
            || self.data.len() + chunk.data.len() > self.header.total_bytes as usize
        {
            return Err("图片分片无效或已超时");
        }
        self.data
            .try_reserve(chunk.data.len())
            .map_err(|_| "图片内存不足")?;
        self.data.extend_from_slice(&chunk.data);
        self.updated = Instant::now();
        Ok(self.data.len() == self.header.total_bytes as usize)
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{decode_message, ControlMessage};
    fn header() -> pb::ClipboardImage {
        pb::ClipboardImage {
            image_id: 1,
            width: 2,
            height: 1,
            total_bytes: 4,
        }
    }
    fn chunk(offset: u32) -> pb::ClipboardImageChunk {
        pb::ClipboardImageChunk {
            image_id: 1,
            offset,
            data: vec![1, 2],
        }
    }
    #[test]
    fn image_limits_and_wire_roundtrip() {
        assert!(!valid_dimensions(0, 1));
        assert!(!valid_dimensions(16385, 1));
        assert!(!valid_dimensions(8192, 8192));
        assert!(valid_dimensions(4096, 4096));
        assert!(!valid_image(&pb::ClipboardImage {
            total_bytes: MAX_IMAGE_BYTES as u32 + 1,
            ..header()
        }));
        for message in [
            ControlMessage::ClipboardImage(header()),
            ControlMessage::ClipboardImageChunk(chunk(0)),
            ControlMessage::ClipboardImageCancel(pb::ClipboardImageCancel { image_id: 1 }),
        ] {
            assert!(decode_message(message.msg_type(), &message.encode_frame()[5..]).is_ok());
        }
        assert!(!valid_chunk(&pb::ClipboardImageChunk {
            data: vec![0; CLIPBOARD_CHUNK_BYTES + 1],
            ..chunk(0)
        }));
    }
    #[test]
    fn ordered_complete_only_and_timeout() {
        let mut a = ImageAssembly::new(header()).unwrap();
        assert!(!a.push(&chunk(0)).unwrap());
        assert!(a.push(&chunk(0)).is_err());
        assert!(a.push(&chunk(3)).is_err());
        assert!(a
            .push(&pb::ClipboardImageChunk {
                image_id: 2,
                ..chunk(2)
            })
            .is_err());
        assert!(a.push(&chunk(2)).unwrap());
        assert_eq!(a.into_bytes(), vec![1, 2, 1, 2]);
        let mut a = ImageAssembly::new(header()).unwrap();
        a.updated = Instant::now() - IMAGE_TIMEOUT;
        assert!(a.push(&chunk(0)).is_err());
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;
    use crate::{control::*, session, Transport};
    #[test]
    fn paste_is_bounded_and_uses_reliable_control_on_all_transports() {
        for message in [
            ControlMessage::ClipboardPaste(pb::ClipboardPaste {
                request_id: 7,
                text: "中文🙂\ntext".into(),
            }),
            ControlMessage::ClipboardPasteResult(pb::ClipboardPasteResult {
                request_id: 7,
                success: false,
                reason: "denied".into(),
            }),
        ] {
            let frame = message.encode_frame();
            let (kind, _) = parse_header(frame[..5].try_into().unwrap()).unwrap();
            assert_eq!(decode_message(kind, &frame[5..]).unwrap(), message);
            for transport in [
                Transport::Tcp,
                Transport::Hybrid,
                Transport::Quic,
                Transport::Vnt,
            ] {
                assert!(session::allowed_on(
                    &message,
                    pb::ChannelKind::ChannelControl,
                    transport
                ));
                assert!(!session::allowed_on(
                    &message,
                    pb::ChannelKind::ChannelFiles,
                    transport
                ));
                assert!(!session::allowed_on(
                    &message,
                    pb::ChannelKind::ChannelMedia,
                    transport
                ));
            }
        }
        for request in [
            pb::ClipboardPaste {
                request_id: 0,
                text: "x".into(),
            },
            pb::ClipboardPaste {
                request_id: 1,
                text: "x".repeat(MAX_CLIPBOARD_TEXT + 1),
            },
        ] {
            let frame = ControlMessage::ClipboardPaste(request).encode_frame();
            assert!(decode_message(MessageType::ClipboardPaste, &frame[5..]).is_err());
        }
    }
}
