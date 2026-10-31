//! Sample the input desktop on a windowless, DPI-aware native thread. Service
//! brokers never read cursor coordinates from their own session-zero desktop.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CursorSample {
    pub available: bool,
    pub x: i32,
    pub y: i32,
    pub monitor_x: i32,
    pub monitor_y: i32,
    pub monitor_width: u32,
    pub monitor_height: u32,
}
pub struct CursorObserver(Arc<AtomicBool>);
impl Drop for CursorObserver {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl CursorObserver {
    pub fn start(name: String, publish: impl Fn(CursorSample) + Send + 'static) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        std::thread::Builder::new()
            .name("desktop-cursor".into())
            .spawn(move || {
                #[cfg(windows)]
                unsafe {
                    use windows::Win32::UI::HiDpi::*;
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                }
                let mut previous = None;
                while !done.load(Ordering::Acquire) {
                    let sample = sample(&name).unwrap_or_default();
                    if previous != Some(sample) {
                        publish(sample);
                        previous = Some(sample);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            })
            .ok()?;
        Some(Self(stop))
    }
}
#[cfg(windows)]
fn sample(name: &str) -> Option<CursorSample> {
    use windows::{
        core::PCWSTR,
        Win32::{Foundation::POINT, Graphics::Gdi::*, UI::WindowsAndMessaging::GetCursorPos},
    };
    let _desktop = crate::bind_input_desktop().ok()?;
    let mut point = POINT::default();
    unsafe {
        GetCursorPos(&mut point).ok()?;
    }
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    if !unsafe {
        EnumDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
    }
    .as_bool()
    {
        return None;
    }
    let position = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
    Some(CursorSample {
        available: true,
        x: point.x,
        y: point.y,
        monitor_x: position.x,
        monitor_y: position.y,
        monitor_width: mode.dmPelsWidth,
        monitor_height: mode.dmPelsHeight,
    })
}
#[cfg(not(any(windows, target_os = "macos")))]
fn sample(_name: &str) -> Option<CursorSample> {
    None
}

#[cfg(target_os = "macos")]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MacDisplay {
    name: String,
    x: f64,
    y: f64,
    width: u32,
    height: u32,
    logical_width: f64,
    logical_height: f64,
}
#[cfg(target_os = "macos")]
fn mac_sample(displays: &[MacDisplay], name: &str, x: f64, y: f64) -> Option<CursorSample> {
    // Internal routing coordinates use physical pixels within each display and
    // uniformly scaled origins. Mixed Retina layouts cannot overlap, and region
    // offsets remain in the same physical units as capture and input mapping.
    let scale = displays.iter().fold(1.0_f64, |s, d| {
        s.max(d.width as f64 / d.logical_width)
            .max(d.height as f64 / d.logical_height)
    });
    let pointed = displays.iter().find(|d| {
        x >= d.x && x < d.x + d.logical_width && y >= d.y && y < d.y + d.logical_height
    })?;
    let monitor = displays.iter().find(|d| d.name == name)?;
    Some(CursorSample {
        available: true,
        x: (pointed.x * scale + (x - pointed.x) * pointed.width as f64 / pointed.logical_width)
            .round() as i32,
        y: (pointed.y * scale + (y - pointed.y) * pointed.height as f64 / pointed.logical_height)
            .round() as i32,
        monitor_x: (monitor.x * scale).round() as i32,
        monitor_y: (monitor.y * scale).round() as i32,
        monitor_width: monitor.width,
        monitor_height: monitor.height,
    })
}
#[cfg(target_os = "macos")]
fn sample(name: &str) -> Option<CursorSample> {
    let displays: Vec<MacDisplay> = serde_json::from_value(vnt_macos::query(0).ok()?).ok()?;
    let p = vnt_macos::query(10).ok()?;
    mac_sample(&displays, name, p["x"].as_f64()?, p["y"].as_f64()?)
}
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn mixed_retina_negative_origins_keep_regions_and_cursor_in_one_space() {
        let displays = vec![
            MacDisplay {
                name: "left".into(),
                x: -1920.0,
                y: 0.0,
                width: 1920,
                height: 1080,
                logical_width: 1920.0,
                logical_height: 1080.0,
            },
            MacDisplay {
                name: "retina".into(),
                x: 0.0,
                y: 0.0,
                width: 2880,
                height: 1800,
                logical_width: 1440.0,
                logical_height: 900.0,
            },
        ];
        let left = mac_sample(&displays, "left", -1800.0, 50.0).unwrap();
        assert_eq!(
            (left.x - left.monitor_x, left.y - left.monitor_y),
            (120, 50)
        );
        let retina = mac_sample(&displays, "retina", 100.0, 50.0).unwrap();
        assert_eq!((retina.x, retina.y), (200, 100));
        let left_looking_right = mac_sample(&displays, "left", 100.0, 50.0).unwrap();
        assert!(
            left_looking_right.x
                >= left_looking_right.monitor_x + left_looking_right.monitor_width as i32
        );
        assert!(mac_sample(&displays, "retina", 5000.0, 50.0).is_none());
    }
}
