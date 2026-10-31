//! PCM conversion and a streaming, band-limited stereo resampler.
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: usize,
    pub bits: u16,
    pub float: bool,
    pub mask: u32,
}
impl PcmFormat {
    pub fn validate(&self) -> Result<(), String> {
        if !(8000..=384000).contains(&self.rate)
            || !(1..=8).contains(&self.channels)
            || (self.float && self.bits != 32)
            || (!self.float && ![16, 24, 32].contains(&self.bits))
        {
            return Err("unsupported audio device PCM format".into());
        }
        Ok(())
    }
    pub fn frame_bytes(&self) -> usize {
        self.channels * self.bits as usize / 8
    }
    fn speakers(&self) -> Vec<u32> {
        if self.mask.count_ones() as usize == self.channels {
            (0..32)
                .filter(|b| self.mask & (1 << b) != 0)
                .map(|b| 1 << b)
                .collect()
        } else {
            // Conventional Windows channel order for formats without a channel mask.
            [1, 2, 4, 8, 16, 32, 512, 1024][..self.channels].to_vec()
        }
    }
    pub fn decode(&self, bytes: &[u8]) -> Vec<[f32; 2]> {
        let weights: Vec<[f32; 2]> = self
            .speakers()
            .into_iter()
            .map(|s| match s {
                _ if self.channels == 1 => [1.0, 1.0],
                1 => [1.0, 0.0],
                2 => [0.0, 1.0],
                4 | 256 => [0.707, 0.707],
                16 | 64 | 512 => [0.707, 0.0],
                32 | 128 | 1024 => [0.0, 0.707],
                _ => [0.0, 0.0], // LFE is not mixed into the stereo stream.
            })
            .collect();
        let gain = weights
            .iter()
            .fold([0.0f32; 2], |a, w| [a[0] + w[0], a[1] + w[1]]);
        bytes
            .chunks_exact(self.frame_bytes())
            .map(|frame| {
                let mut out = [0.0; 2];
                for (channel, sample) in frame.chunks_exact(self.bits as usize / 8).enumerate() {
                    let x = if self.float {
                        f32::from_le_bytes(sample.try_into().unwrap())
                    } else {
                        match self.bits {
                            16 => i16::from_le_bytes(sample.try_into().unwrap()) as f32 / 32768.0,
                            24 => {
                                ((sample[0] as i32
                                    | (sample[1] as i32) << 8
                                    | (sample[2] as i32) << 16)
                                    << 8
                                    >> 8) as f32
                                    / 8388608.0
                            }
                            _ => {
                                i32::from_le_bytes(sample.try_into().unwrap()) as f32 / 2147483648.0
                            }
                        }
                    };
                    let x = if x.is_finite() {
                        x.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                    for c in 0..2 {
                        out[c] += x * weights[channel][c];
                    }
                }
                [out[0] / gain[0].max(1.0), out[1] / gain[1].max(1.0)]
            })
            .collect()
    }
    #[cfg(any(windows, test))]
    pub fn encode(&self, pcm: &[[f32; 2]], bytes: &mut [u8]) {
        let speakers = self.speakers();
        for (frame, stereo) in bytes.chunks_exact_mut(self.frame_bytes()).zip(pcm) {
            for (channel, sample) in frame.chunks_exact_mut(self.bits as usize / 8).enumerate() {
                let x = if self.channels == 1 {
                    (stereo[0] + stereo[1]) * 0.5
                } else {
                    match speakers[channel] {
                        1 => stereo[0],
                        2 => stereo[1],
                        _ => 0.0,
                    }
                }
                .clamp(-1.0, 1.0);
                if self.float {
                    sample.copy_from_slice(&x.to_le_bytes());
                } else {
                    match self.bits {
                        16 => sample.copy_from_slice(
                            &((x * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes(),
                        ),
                        24 => sample.copy_from_slice(
                            &((x * 8388608.0).round().clamp(-8388608.0, 8388607.0) as i32)
                                .to_le_bytes()[..3],
                        ),
                        _ => sample.copy_from_slice(
                            &((x as f64 * 2147483648.0)
                                .round()
                                .clamp(-2147483648.0, 2147483647.0)
                                as i32)
                                .to_le_bytes(),
                        ),
                    }
                }
            }
        }
    }
}

const HALF: usize = 8;
const PHASES: usize = 256;
pub struct Resampler {
    input: VecDeque<[f32; 2]>,
    position: f64,
    step: f64,
    ratio: f64,
    kernel: Vec<[f32; HALF * 2]>,
}
impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        let cutoff = (output_rate as f64 / input_rate as f64).min(1.0) * 0.95;
        let kernel = (0..PHASES)
            .map(|phase| {
                let mut taps = [0.0; HALF * 2];
                for (i, tap) in taps.iter_mut().enumerate() {
                    let x = i as f64 - (HALF - 1) as f64 - phase as f64 / PHASES as f64;
                    let sinc = if x.abs() < 1e-10 {
                        cutoff
                    } else {
                        (std::f64::consts::PI * x * cutoff).sin() / (std::f64::consts::PI * x)
                    };
                    let window = 0.5 + 0.5 * (std::f64::consts::PI * x / HALF as f64).cos();
                    *tap = (sinc * window) as f32;
                }
                let sum: f32 = taps.iter().sum();
                for tap in &mut taps {
                    *tap /= sum;
                }
                taps
            })
            .collect();
        Self {
            input: VecDeque::from(vec![[0.0; 2]; HALF]),
            position: HALF as f64,
            step: input_rate as f64 / output_rate as f64,
            ratio: 1.0,
            kernel,
        }
    }
    pub fn push(&mut self, samples: &[[f32; 2]]) {
        self.input.extend(samples);
    }
    /// Slightly alter consumption rate to track the other machine's audio clock.
    pub fn set_ratio(&mut self, ratio: f64) {
        self.ratio = ratio.clamp(0.995, 1.005);
    }
    pub fn pop(&mut self) -> Option<[f32; 2]> {
        let center = self.position as usize;
        if center + HALF >= self.input.len() {
            return None;
        }
        let phase = ((self.position.fract() * PHASES as f64) as usize).min(PHASES - 1);
        let mut out = [0.0; 2];
        for (i, tap) in self.kernel[phase].iter().enumerate() {
            let sample = self.input[center + i - (HALF - 1)];
            out[0] += sample[0] * tap;
            out[1] += sample[1] * tap;
        }
        self.position += self.step * self.ratio;
        let discard = (self.position as usize).saturating_sub(HALF);
        self.input.drain(..discard.min(self.input.len()));
        self.position -= discard as f64;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pcm_formats_roundtrip_and_surround_downmix_is_finite() {
        for bits in [16, 24, 32] {
            for float in [false, true] {
                if float && bits != 32 {
                    continue;
                }
                let f = PcmFormat {
                    rate: 44100,
                    channels: 2,
                    bits,
                    float,
                    mask: 3,
                };
                let mut bytes = vec![0; f.frame_bytes() * 2];
                f.encode(&[[0.25, -0.5], [1.0, -1.0]], &mut bytes);
                let pcm = f.decode(&bytes);
                assert!((pcm[0][0] - 0.25).abs() < 0.0001);
                assert!((pcm[0][1] + 0.5).abs() < 0.0001);
            }
        }
        let f = PcmFormat {
            rate: 48000,
            channels: 6,
            bits: 32,
            float: true,
            mask: 63,
        };
        let bytes: Vec<u8> = [0.0f32, 0.0, 1.0, 0.0, 0.0, 0.0]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        let center = f.decode(&bytes)[0];
        assert!(center[0] > 0.0 && center[0] == center[1]);
    }
    #[test]
    fn resampling_preserves_count_and_state_across_small_blocks() {
        let mut r = Resampler::new(44100, 48000);
        let mut count = 0;
        for _ in 0..1000 {
            r.push(&[[0.25, -0.25]; 441]);
            while let Some(x) = r.pop() {
                count += 1;
                if count > 100 {
                    assert!((x[0] - 0.25).abs() < 0.001);
                }
            }
        }
        assert!((count as i64 - 480000).abs() < 20);
        assert!(r.input.len() < 32);
    }
}
