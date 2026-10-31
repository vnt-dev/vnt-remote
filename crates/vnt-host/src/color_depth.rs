//! Session-local RGB quantization, after crop/scale and before codec conversion.
//! The current capture backends expose full RGB/BGRA only. Never reduce the
//! shared source: other sessions can request a different color precision.
pub(crate) struct Quantizer {
    depth: u32,
    output: Vec<u8>,
}

impl Quantizer {
    pub(crate) fn new(depth: u32) -> Self {
        assert!(vnt_proto::color_depth::valid(depth));
        Self {
            depth,
            output: Vec::new(),
        }
    }

    pub(crate) fn process<'a>(&'a mut self, bgra: &'a [u8]) -> &'a [u8] {
        // RGB888 and BGRA8888 have identical RGB precision. The video encoders
        // ignore alpha, so neither mode needs a copy or a packed intermediate.
        if self.depth >= 24 {
            return bgra;
        }
        self.output.resize(bgra.len(), 0);
        for (src, dst) in bgra
            .as_chunks::<4>()
            .0
            .iter()
            .zip(self.output.as_chunks_mut::<4>().0.iter_mut())
        {
            dst[0] = expand5(src[0]);
            dst[1] = if self.depth == 16 {
                expand6(src[1])
            } else {
                expand5(src[1])
            };
            dst[2] = expand5(src[2]);
            dst[3] = 255;
        }
        &self.output
    }
}

fn expand5(value: u8) -> u8 {
    let high = value >> 3;
    (high << 3) | (high >> 2)
}
fn expand6(value: u8) -> u8 {
    let high = value >> 2;
    (high << 2) | (high >> 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn quantization_levels_boundaries_and_primaries() {
        for depth in [15, 16] {
            let mut q = Quantizer::new(depth);
            let ramp: Vec<u8> = (0..=255).flat_map(|v| [v, v, v, 0]).collect();
            let pixels = q.process(&ramp);
            let channel = |index: usize| {
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|p| p[index])
                    .collect::<std::collections::BTreeSet<_>>()
            };
            assert_eq!(channel(0).len(), 32);
            assert_eq!(channel(1).len(), if depth == 16 { 64 } else { 32 });
            assert_eq!(channel(2).len(), 32);
            assert_eq!(&pixels[..4], &[0, 0, 0, 255]);
            assert_eq!(&pixels[1020..], &[255; 4]);
            assert_eq!(
                &pixels[7 * 4..8 * 4],
                if depth == 16 {
                    &[0, 4, 0, 255]
                } else {
                    &[0, 0, 0, 255]
                }
            );
            assert_eq!(&pixels[8 * 4..9 * 4], &[8, 8, 8, 255]);
            let primaries = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255];
            assert_eq!(q.process(&primaries), primaries);
        }
    }

    #[test]
    fn sessions_are_independent_and_reuse_their_own_buffer() {
        let source = Arc::new(vec![123, 127, 231, 17, 255, 0, 7, 9]);
        let original = source.as_ref().clone();
        let other_session = source.clone();
        let mut low = Quantizer::new(15);
        let mut medium = Quantizer::new(16);
        let first = low.process(&source).as_ptr();
        assert_ne!(low.process(&source), medium.process(&other_session));
        assert_eq!(low.process(&source).as_ptr(), first);
        assert_eq!(*source, original);
        for depth in [24, 32] {
            let mut full = Quantizer::new(depth);
            assert_eq!(full.process(&source).as_ptr(), source.as_ptr());
            assert_eq!(full.process(&source), original);
        }
        // A resized/recreated source still uses the same fixed session depth.
        assert_eq!(low.process(&[7; 16]).len(), 16);
        assert_eq!(&low.process(&[7; 4])[..3], &[0, 0, 0]);
    }
}
