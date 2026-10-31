//! 编解码协商。
//!
//! 服务端上报运行时探测到的编码能力 [`EncoderCap`](crate::pb::EncoderCap)，
//! 客户端上报可解码列表（前端 WebCodecs 探测结果）。
//! 综合服务端硬件编码与客户端硬件解码能力，按固定的九档优先级选择。

use crate::{pb, Codec};

/// 编码器能力（Rust 侧形式，避免在协商逻辑里反复转 proto）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderCap {
    pub codec: Codec,
    pub hardware: bool,
    pub name: String,
}

impl EncoderCap {
    pub fn to_proto(&self) -> pb::EncoderCap {
        pb::EncoderCap {
            codec: self.codec.to_proto().into(),
            hardware: self.hardware,
            name: self.name.clone(),
        }
    }

    pub fn from_proto(c: &pb::EncoderCap) -> Option<Self> {
        Some(EncoderCap {
            codec: Codec::from_proto(c.codec())?,
            hardware: c.hardware,
            name: c.name.clone(),
        })
    }
}

/// 全硬件 AV1 > HEVC > H264 > 单方硬件 HEVC > H264 >
/// 纯软件 H264 > HEVC > 单方硬件 AV1 > 纯软件 AV1。
fn score(cap: &EncoderCap, hardware_decode: bool) -> u32 {
    match (cap.codec, cap.hardware, hardware_decode) {
        (Codec::Av1, true, true) => 9,
        (Codec::Hevc, true, true) => 8,
        (Codec::H264, true, true) => 7,
        (Codec::Hevc, true, false) | (Codec::Hevc, false, true) => 6,
        (Codec::H264, true, false) | (Codec::H264, false, true) => 5,
        (Codec::H264, false, false) => 4,
        (Codec::Hevc, false, false) => 3,
        (Codec::Av1, true, false) | (Codec::Av1, false, true) => 2,
        (Codec::Av1, false, false) => 1,
    }
}

/// 从服务端能力与客户端可解码列表中选出编码。
///
/// `client_codecs` 为空表示客户端全支持（调试/非 webview 客户端）。
pub fn negotiate(server_caps: &[EncoderCap], client_codecs: &[Codec]) -> Option<EncoderCap> {
    negotiate_with_decoder_hardware(server_caps, client_codecs, &[])
}

/// 硬件解码能力列表为空时按软件解码处理。
/// 手动指定编码由 `client_codecs` 限定为单个编码，仍优先该编码的硬件编码器。
pub fn negotiate_with_decoder_hardware(
    server_caps: &[EncoderCap],
    client_codecs: &[Codec],
    client_hardware_codecs: &[Codec],
) -> Option<EncoderCap> {
    server_caps
        .iter()
        .filter(|c| client_codecs.is_empty() || client_codecs.contains(&c.codec))
        .max_by_key(|c| score(c, client_hardware_codecs.contains(&c.codec)))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(codec: Codec, hw: bool) -> EncoderCap {
        EncoderCap {
            codec,
            hardware: hw,
            name: format!("{codec:?}-{hw}"),
        }
    }

    fn all() -> [Codec; 3] {
        [Codec::Av1, Codec::Hevc, Codec::H264]
    }

    #[test]
    fn hardware_beats_newer_software() {
        let caps = [cap(Codec::H264, true), cap(Codec::Av1, false)];
        let got = negotiate_with_decoder_hardware(&caps, &all(), &all()).unwrap();
        assert_eq!(got.codec, Codec::H264);
        assert!(got.hardware);
    }

    #[test]
    fn among_hardware_picks_newest() {
        let caps = [
            cap(Codec::H264, true),
            cap(Codec::Hevc, true),
            cap(Codec::Av1, true),
        ];
        let got = negotiate_with_decoder_hardware(&caps, &all(), &all()).unwrap();
        assert_eq!(got.codec, Codec::Av1);
    }

    #[test]
    fn client_side_filter() {
        let caps = [cap(Codec::H264, true), cap(Codec::Hevc, true)];
        let got = negotiate(&caps, &[Codec::H264]).unwrap();
        assert_eq!(got.codec, Codec::H264);
        // 客户端只能解 HEVC 时选 HEVC
        let got = negotiate(&caps, &[Codec::Hevc]).unwrap();
        assert_eq!(got.codec, Codec::Hevc);
    }

    #[test]
    fn software_fallback() {
        let caps = [cap(Codec::H264, false)];
        let got = negotiate(&caps, &all()).unwrap();
        assert_eq!(got.codec, Codec::H264);
        assert!(!got.hardware);
    }

    #[test]
    fn no_intersection_none() {
        let caps = [cap(Codec::Hevc, true)];
        assert!(negotiate(&caps, &[Codec::H264]).is_none());
        assert!(negotiate(&[], &all()).is_none());
    }

    #[test]
    fn automatic_priority_for_every_pair_and_hardware_direction() {
        // Descending priority; either endpoint may supply the single hardware side.
        let ranked = [
            (Codec::Av1, true, true),
            (Codec::Hevc, true, true),
            (Codec::H264, true, true),
            (Codec::Hevc, true, false),
            (Codec::Hevc, false, true),
            (Codec::H264, true, false),
            (Codec::H264, false, true),
            (Codec::H264, false, false),
            (Codec::Hevc, false, false),
            (Codec::Av1, true, false),
            (Codec::Av1, false, true),
            (Codec::Av1, false, false),
        ];
        for (index, &(codec, encode_hw, decode_hw)) in ranked.iter().enumerate() {
            for &(other_codec, other_encode_hw, other_decode_hw) in &ranked[index + 1..] {
                if codec == other_codec && decode_hw != other_decode_hw {
                    continue; // A decoder has one hardware capability per codec.
                }
                let preferred = cap(codec, encode_hw);
                let other = cap(other_codec, other_encode_hw);
                let hardware: Vec<_> = [(codec, decode_hw), (other_codec, other_decode_hw)]
                    .into_iter()
                    .filter_map(|(c, hw)| hw.then_some(c))
                    .collect();
                for caps in [
                    [preferred.clone(), other.clone()],
                    [other.clone(), preferred.clone()],
                ] {
                    assert_eq!(
                        negotiate_with_decoder_hardware(&caps, &all(), &hardware),
                        Some(preferred.clone()),
                        "candidates={caps:?}, hardware_decode={hardware:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn software_clients_prefer_h264_over_hevc_and_av1() {
        let caps = [
            cap(Codec::Av1, true),
            cap(Codec::Hevc, false),
            cap(Codec::H264, false),
        ];
        assert_eq!(negotiate(&caps, &all()).unwrap(), cap(Codec::H264, false));
        assert_eq!(
            negotiate(&caps[..2], &all()).unwrap(),
            cap(Codec::Hevc, false)
        );
    }

    #[test]
    fn explicit_codec_stays_selected_and_prefers_hardware_encoding() {
        let caps = [
            cap(Codec::H264, true),
            cap(Codec::Av1, true),
            cap(Codec::Av1, false),
        ];
        for hardware in [vec![], all().to_vec()] {
            assert_eq!(
                negotiate_with_decoder_hardware(&caps, &[Codec::Av1], &hardware),
                Some(cap(Codec::Av1, true)),
            );
        }
    }
}
