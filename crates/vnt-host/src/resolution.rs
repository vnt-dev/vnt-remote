//! Session-local software scaling. Shared capture frames are never modified.
use vnt_capture::CapturedFrame;

pub fn validate(mode: i32, width: u32, height: u32) -> bool {
    matches!(mode, 0..=2)
        && (mode == 0 || ((2..=16384).contains(&width) && (2..=16384).contains(&height)))
}

pub fn dimensions(width: u32, height: u32, target: (u32, u32)) -> (u32, u32) {
    let ratio = (target.0 as f64 / width as f64)
        .min(target.1 as f64 / height as f64)
        .min(1.0);
    (
        ((width as f64 * ratio).floor() as u32 & !1).max(2),
        ((height as f64 * ratio).floor() as u32 & !1).max(2),
    )
}

#[derive(Default)]
pub struct Scaler {
    pub target: Option<(u32, u32)>,
    output: Option<CapturedFrame>,
    source: (u32, u32),
    xs: Vec<(usize, usize, u32)>,
    ys: Vec<(usize, usize, u32)>,
}
fn axis(source: u32, dest: u32) -> Vec<(usize, usize, u32)> {
    (0..dest)
        .map(|i| {
            let position = ((i as f64 + 0.5) * source as f64 / dest as f64 - 0.5).max(0.0);
            let first = (position.floor() as u32).min(source - 1);
            (
                first as usize,
                (first + 1).min(source - 1) as usize,
                ((position - first as f64) * 256.0).round() as u32,
            )
        })
        .collect()
}
impl Scaler {
    pub fn new(target: Option<(u32, u32)>) -> Self {
        Self {
            target,
            ..Default::default()
        }
    }
    pub fn process<'a>(&'a mut self, frame: &'a CapturedFrame) -> &'a CapturedFrame {
        let Some(target) = self.target else {
            return frame;
        };
        let (width, height) = dimensions(frame.width, frame.height, target);
        if (width, height) == (frame.width, frame.height) {
            self.output = None;
            self.xs.clear();
            self.ys.clear();
            return frame;
        }
        if self.source != (frame.width, frame.height)
            || self.output.as_ref().map(|f| (f.width, f.height)) != Some((width, height))
        {
            self.source = (frame.width, frame.height);
            self.xs = axis(frame.width, width);
            self.ys = axis(frame.height, height);
            let output = self.output.get_or_insert_with(|| CapturedFrame {
                source_monitor: None,
                desktop_generation: 0,
                captured_at: frame.captured_at,
                width,
                height,
                bgra: Vec::new(),
                pts_ms: 0,
                capture_ms: 0,
            });
            output.width = width;
            output.height = height;
            let needed = width as usize * height as usize * 4;
            if output.bgra.capacity() > needed.saturating_mul(2) {
                output.bgra = Vec::with_capacity(needed);
            }
            output.bgra.resize(width as usize * height as usize * 4, 0);
        }
        let out = self.output.as_mut().unwrap();
        let stride = frame.width as usize * 4;
        for (y, &(y0, y1, wy)) in self.ys.iter().enumerate() {
            for (x, &(x0, x1, wx)) in self.xs.iter().enumerate() {
                for c in 0..4 {
                    let a = frame.bgra[y0 * stride + x0 * 4 + c] as u32;
                    let b = frame.bgra[y0 * stride + x1 * 4 + c] as u32;
                    let d = frame.bgra[y1 * stride + x0 * 4 + c] as u32;
                    let e = frame.bgra[y1 * stride + x1 * 4 + c] as u32;
                    out.bgra[(y * width as usize + x) * 4 + c] =
                        (((a * (256 - wx) + b * wx) * (256 - wy)
                            + (d * (256 - wx) + e * wx) * wy
                            + 32768)
                            >> 16) as u8;
                }
            }
        }
        out.source_monitor = frame.source_monitor.clone();
        out.desktop_generation = frame.desktop_generation;
        out.pts_ms = frame.pts_ms;
        out.captured_at = frame.captured_at;
        out.capture_ms = frame.capture_ms;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fit_without_enlargement_and_with_even_dimensions() {
        assert_eq!(dimensions(3840, 2160, (1920, 1080)), (1920, 1080));
        assert_eq!(dimensions(1920, 1200, (1920, 1080)), (1728, 1080));
        assert_eq!(dimensions(1080, 1920, (1920, 1080)), (606, 1080));
        assert_eq!(dimensions(1280, 720, (3840, 2160)), (1280, 720));
        assert_eq!(dimensions(1921, 1081, (1920, 1080)), (1918, 1080));
        assert!(!validate(1, 0, 1080));
        assert!(!validate(3, 1920, 1080));
    }
    #[test]
    fn bilinear_scaling_preserves_source_and_reuses_storage() {
        let frame = CapturedFrame {
            source_monitor: None,
            desktop_generation: 0,
            captured_at: std::time::Instant::now(),
            width: 4,
            height: 2,
            bgra: (0..8).flat_map(|i| [i * 20, i * 20, i * 20, 255]).collect(),
            pts_ms: 99,
            capture_ms: 3,
        };
        let original = frame.bgra.clone();
        let mut scaler = Scaler {
            target: Some((2, 2)),
            ..Default::default()
        };
        let out = scaler.process(&frame);
        assert_eq!((out.width, out.height), (2, 2));
        assert_eq!(&out.bgra[..4], &[10, 10, 10, 255]);
        assert_eq!(out.pts_ms, 99);
        let address = out.bgra.as_ptr();
        assert_eq!(address, scaler.process(&frame).bgra.as_ptr());
        assert_eq!(frame.bgra, original);
        let mut untouched = Scaler::default();
        assert!(std::ptr::eq(untouched.process(&frame), &frame));
    }
}
