//! Persisted regions use physical pixels relative to a named display.
use crate::{CaptureError, MonitorInfo};

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenRegion {
    pub id: String,
    pub name: String,
    pub monitor_name: String,
    pub screen_width: u32,
    pub screen_height: u32,
    pub orientation: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl ScreenRegion {
    pub fn validate(&self) -> Result<(), String> {
        if self.monitor_name.is_empty()
            || self.width < 64
            || self.height < 64
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || self.width > 16384
            || self.height > 16384
            || self.width as u64 * self.height as u64 > 64 * 1024 * 1024
            || self.orientation > 3
            || self
                .x
                .checked_add(self.width)
                .is_none_or(|v| v > self.screen_width)
            || self
                .y
                .checked_add(self.height)
                .is_none_or(|v| v > self.screen_height)
        {
            return Err("屏幕区域无效：尺寸须为至少 64 × 64 的偶数且位于原显示器内".into());
        }
        Ok(())
    }

    pub fn matches(&self, monitor: &MonitorInfo) -> bool {
        self.monitor_name == monitor.name
            && self.screen_width == monitor.width
            && self.screen_height == monitor.height
            && self.orientation == monitor.orientation
            && self.validate().is_ok()
    }

    pub fn resolve<'a>(&self, monitors: &'a [MonitorInfo]) -> Result<&'a MonitorInfo, String> {
        monitors
            .iter()
            .find(|m| self.matches(m))
            .ok_or_else(|| "区域对应的显示器不可用或显示模式已变化，请重新框选区域".into())
    }
}

/// A cheap native descriptor refresh also detects rotation without changing dimensions.
#[cfg(windows)]
pub fn monitor_orientation(name: &str) -> u32 {
    monitor_mode(name).map(|m| m.2).unwrap_or(u32::MAX)
}

#[cfg(windows)]
pub(crate) fn monitor_mode(name: &str) -> Option<(u32, u32, u32)> {
    use windows::{core::PCWSTR, Win32::Graphics::Gdi::*};
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe {
        EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode)
            .as_bool()
            .then_some((
                mode.dmPelsWidth,
                mode.dmPelsHeight,
                mode.Anonymous1.Anonymous2.dmDisplayOrientation.0,
            ))
    }
}

pub fn require_region<'a>(
    region: &ScreenRegion,
    monitors: &'a [MonitorInfo],
) -> Result<&'a MonitorInfo, CaptureError> {
    region.resolve(monitors).map_err(CaptureError::Create)
}

#[cfg(target_os = "macos")]
pub fn monitor_orientation(name: &str) -> u32 {
    crate::macos::Monitor::all()
        .ok()
        .and_then(|ms| ms.into_iter().find(|m| m.info.name == name))
        .map_or(u32::MAX, |m| m.info.orientation)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn region() -> ScreenRegion {
        ScreenRegion {
            id: "r".into(),
            name: "区域".into(),
            monitor_name: "display-a".into(),
            screen_width: 1920,
            screen_height: 1080,
            orientation: 0,
            x: 1856,
            y: 1016,
            width: 64,
            height: 64,
        }
    }
    #[test]
    fn rejects_overflow_odd_sizes_and_regions_outside_the_display() {
        let r = region();
        assert!(r.validate().is_ok());
        for invalid in [
            ScreenRegion {
                x: u32::MAX,
                ..r.clone()
            },
            ScreenRegion {
                width: 65,
                ..r.clone()
            },
            ScreenRegion {
                width: 62,
                ..r.clone()
            },
            ScreenRegion { y: 1018, ..r },
        ] {
            assert!(invalid.validate().is_err());
        }
    }
    #[test]
    fn resolves_by_device_and_mode_instead_of_an_index_or_handle() {
        let r = region();
        let mut m = MonitorInfo {
            index: 7,
            width: 1920,
            height: 1080,
            name: "display-a".into(),
            hmonitor: 999,
            primary: false,
            orientation: 0,
        };
        assert!(r.matches(&m));
        m.name = "display-b".into();
        assert!(!r.matches(&m));
        m.name = r.monitor_name.clone();
        m.orientation = 2;
        assert!(!r.matches(&m));
        m.orientation = 0;
        m.width = 1280;
        assert!(!r.matches(&m));
        assert!(r.resolve(&[]).is_err());
    }
}
