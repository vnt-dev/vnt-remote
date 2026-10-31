//! Decode dependencies derived from codec syntax, never from a guessed GOP.
//! Unsupported syntax invalidates parser state until a verified reset. Such
//! frames are explicitly Unknown rather than incorrectly safe to discard.
use vnt_proto::{pb::VideoDependencies, Codec};
mod av1;
mod bits;
mod h264;
mod hevc;

pub struct FrameTracker {
    codec: Codec,
    epoch: u64,
    next: u64,
    h264: h264::Parser,
    hevc: hevc::Parser,
    av1: av1::Parser,
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::{create_encoder, EncodeParams};
    #[test]
    fn encoder_bitstreams_have_verified_references_and_forced_resets() {
        for (codec, hardware) in [Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .flat_map(|codec| [(codec, false), (codec, true)])
        {
            let mut encoder = create_encoder(codec, hardware).unwrap();
            eprintln!("dependency parser: {codec:?}, encoder={}", encoder.name());
            let (width, height) = if hardware { (640, 480) } else { (128, 96) };
            let mut params = EncodeParams::new(width, height, 30, 1000);
            params.gop = 1000;
            encoder.configure(&params).unwrap();
            let mut tracker = FrameTracker::new(codec);
            let mut pixels = vec![0u8; width as usize * height as usize * 4];
            let mut first = true;
            for i in 0..36 {
                if i == 32 {
                    encoder.force_keyframe().unwrap();
                }
                for (j, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    pixel.copy_from_slice(&[
                        (j + i * 13) as u8,
                        (j / width as usize + i) as u8,
                        (i * 7) as u8,
                        255,
                    ]);
                }
                let mut packets = vec![];
                encoder
                    .encode(&pixels, i as u64 * 33, &mut packets)
                    .unwrap();
                for packet in packets {
                    let d = tracker.frame(&packet.data, packet.keyframe);
                    assert!(!d.unknown, "{codec:?} frame {i}: {d:?}");
                    assert!(vnt_proto::recovery::valid_dependencies(&d));
                    if first || i == 32 {
                        assert!(d.independent, "{codec:?} frame {i}: {d:?}");
                        first = false;
                    } else if !packet.keyframe {
                        assert!(!d.references.is_empty(), "{codec:?} frame {i}: {d:?}");
                    }
                }
            }
            assert!(!first);
        }
    }
    #[test]
    fn corrupt_headers_are_unknown_even_with_a_keyframe_hint() {
        for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
            let mut tracker = FrameTracker::new(codec);
            for bytes in [vec![], vec![255], vec![0, 0, 1, 255], vec![0; 4096]] {
                let d = tracker.frame(&bytes, true);
                assert!(d.unknown);
                assert!(!d.independent);
            }
            let epoch = tracker.epoch;
            tracker.reset(1);
            assert!(tracker.epoch > epoch);
            assert_eq!(tracker.next, 1);
        }
    }
}
impl FrameTracker {
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            epoch: 1,
            next: 1,
            h264: Default::default(),
            hevc: Default::default(),
            av1: Default::default(),
        }
    }
    pub fn reset(&mut self, epoch: u64) {
        *self = Self {
            epoch: epoch.max(self.epoch.saturating_add(1)),
            ..Self::new(self.codec)
        };
    }
    pub fn frame(&mut self, data: &[u8], _keyframe_hint: bool) -> VideoDependencies {
        let id = self.next;
        self.next += 1;
        let parsed = match self.codec {
            Codec::H264 => self.h264.frame(data, id),
            Codec::Hevc => self.hevc.frame(data, id),
            Codec::Av1 => self.av1.frame(data, id),
        };
        let (independent, mut references, unknown) = match parsed {
            Some((independent, references)) => (independent, references, false),
            None => (false, Vec::new(), true),
        };
        references.sort_unstable();
        references.dedup();
        if references.len() > vnt_proto::recovery::MAX_REFERENCES {
            return VideoDependencies {
                epoch: self.epoch,
                frame_id: id,
                references: vec![],
                independent: false,
                unknown: true,
            };
        }
        VideoDependencies {
            epoch: self.epoch,
            frame_id: id,
            references,
            independent,
            unknown,
        }
    }
}
