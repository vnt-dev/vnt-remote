//! Session-local crop and normalized input mapping. Shared frames stay immutable.
use vnt_capture::{CapturedFrame, ScreenRegion};

#[derive(Default)]
pub struct Cropper {
    output: Option<CapturedFrame>,
}
impl Cropper {
    pub fn process<'a>(
        &'a mut self,
        frame: &'a CapturedFrame,
        region: Option<&ScreenRegion>,
    ) -> Option<&'a CapturedFrame> {
        let Some(r) = region else {
            return Some(frame);
        };
        if frame.source_monitor.as_ref().is_none_or(|m| !r.matches(m))
            || frame.width != r.screen_width
            || frame.height != r.screen_height
            || r.validate().is_err()
        {
            return None;
        }
        let out = self.output.get_or_insert_with(|| CapturedFrame {
            source_monitor: None,
            desktop_generation: 0,
            captured_at: frame.captured_at,
            width: r.width,
            height: r.height,
            bgra: vec![0; r.width as usize * r.height as usize * 4],
            pts_ms: 0,
            capture_ms: 0,
        });
        out.width = r.width;
        out.height = r.height;
        out.bgra.resize(r.width as usize * r.height as usize * 4, 0);
        let stride = frame.width as usize * 4;
        let row = r.width as usize * 4;
        if frame.bgra.len() < stride * frame.height as usize {
            return None;
        }
        for y in 0..r.height as usize {
            let start = (r.y as usize + y) * stride + r.x as usize * 4;
            out.bgra[y * row..(y + 1) * row].copy_from_slice(&frame.bgra[start..start + row]);
        }
        out.source_monitor = frame.source_monitor.clone();
        out.desktop_generation = frame.desktop_generation;
        out.captured_at = frame.captured_at;
        out.pts_ms = frame.pts_ms;
        out.capture_ms = frame.capture_ms;
        Some(out)
    }
}

pub struct RegionInjector {
    pub inner: std::sync::Arc<dyn vnt_input::InputInjector + Send + Sync>,
    pub region: ScreenRegion,
}
pub fn map_axis(value: f32, start: u32, length: u32, screen: u32) -> f32 {
    (start as f32 + value.clamp(0.0, 1.0) * (length - 1) as f32) / (screen - 1) as f32
}
impl vnt_input::InputInjector for RegionInjector {
    fn mouse_move(&self, x: f32, y: f32) -> vnt_input::InputResult<()> {
        if !x.is_finite() || !y.is_finite() {
            return Err(vnt_input::InputError::Os("无效的鼠标坐标".into()));
        }
        let r = &self.region;
        self.inner.mouse_move(
            map_axis(x, r.x, r.width, r.screen_width),
            map_axis(y, r.y, r.height, r.screen_height),
        )
    }
    fn mouse_button(
        &self,
        button: vnt_input::MouseButton,
        down: bool,
    ) -> vnt_input::InputResult<()> {
        self.inner.mouse_button(button, down)
    }
    fn wheel(&self, x: f32, y: f32) -> vnt_input::InputResult<()> {
        self.inner.wheel(x, y)
    }
    fn key(&self, scan: u16, extended: bool, down: bool) -> vnt_input::InputResult<()> {
        self.inner.key(scan, extended, down)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crop_has_only_the_selected_pixels_and_preserves_timing() {
        let r = ScreenRegion {
            id: "r".into(),
            name: "test".into(),
            monitor_name: "screen".into(),
            screen_width: 100,
            screen_height: 100,
            orientation: 0,
            x: 10,
            y: 20,
            width: 64,
            height: 64,
        };
        let frame = CapturedFrame {
            source_monitor: Some(vnt_capture::MonitorInfo {
                index: 0,
                width: 100,
                height: 100,
                name: "screen".into(),
                hmonitor: 0,
                primary: true,
                orientation: 0,
            }),
            width: 100,
            height: 100,
            bgra: (0..10000u32).flat_map(u32::to_le_bytes).collect(),
            captured_at: std::time::Instant::now(),
            pts_ms: 17,
            capture_ms: 3,
            desktop_generation: 4,
        };
        let mut cropper = Cropper::default();
        let out = cropper.process(&frame, Some(&r)).unwrap();
        assert_eq!(
            (out.width, out.height, out.pts_ms, out.desktop_generation),
            (64, 64, 17, 4)
        );
        assert_eq!(&out.bgra[..4], &2010u32.to_le_bytes());
        assert_eq!(&out.bgra[out.bgra.len() - 4..], &8373u32.to_le_bytes());
        assert_eq!(frame.bgra.len(), 40000);
        let mut wrong = frame;
        wrong.source_monitor.as_mut().unwrap().name = "another-display".into();
        assert!(cropper.process(&wrong, Some(&r)).is_none());
        wrong.source_monitor.as_mut().unwrap().name = "screen".into();
        wrong.source_monitor.as_mut().unwrap().orientation = 2;
        assert!(cropper.process(&wrong, Some(&r)).is_none());
    }
    #[test]
    fn input_endpoints_and_center_map_to_crop_pixels() {
        assert_eq!(map_axis(0.0, 10, 64, 101), 0.1);
        assert_eq!(map_axis(1.0, 10, 64, 101), 0.73);
        assert_eq!(map_axis(0.5, 10, 64, 101), 0.415);
    }
}
