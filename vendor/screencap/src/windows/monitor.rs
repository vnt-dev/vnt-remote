use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromPoint, DEVMODEW,
    ENUM_CURRENT_SETTINGS, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTOPRIMARY,
};

#[derive(Clone, Debug)]
pub struct Monitor {
    index: u32,
    h_monitor: HMONITOR,
}

impl Monitor {
    pub fn primary() -> io::Result<Self> {
        let h_monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        if h_monitor.is_invalid() {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Primary monitor not found",
            ))
        } else {
            Self::all()?
                .into_iter()
                .find(|m| m.h_monitor == h_monitor)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "Primary monitor not enumerated")
                })
        }
    }

    pub fn from_index(index: u32) -> io::Result<Self> {
        let monitors = Self::all()?;
        monitors
            .get(index as usize)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Monitor index out of range"))
    }

    pub fn all() -> io::Result<Vec<Self>> {
        extern "system" fn monitor_enum_proc(
            h_monitor: HMONITOR,
            _: HDC,
            _: *mut RECT,
            lparam: LPARAM,
        ) -> BOOL {
            let monitors = unsafe { &mut *(lparam.0 as *mut Vec<Monitor>) };
            let index = monitors.len() as u32;
            monitors.push(Monitor { index, h_monitor });
            true.into()
        }

        let mut monitors = Vec::<Monitor>::new();
        let lparam = LPARAM(&mut monitors as *mut _ as isize);
        let success = unsafe { EnumDisplayMonitors(None, None, Some(monitor_enum_proc), lparam) };
        if !success.as_bool() {
            return Err(display_error(
                "EnumDisplayMonitors failed; no accessible desktop",
            ));
        }
        Ok(monitors)
    }
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn h_monitor(&self) -> HMONITOR {
        self.h_monitor
    }

    pub fn size(&self) -> io::Result<(u32, u32)> {
        let name = self.device_name_wide()?;
        unsafe {
            let mut device_mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            if !EnumDisplaySettingsW(
                PCWSTR(name.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut device_mode,
            )
            .as_bool()
            {
                return Err(display_error(
                    "EnumDisplaySettingsW failed for the selected monitor",
                ));
            }
            Ok((device_mode.dmPelsWidth, device_mode.dmPelsHeight))
        }
    }
    pub fn device_name_wide(&self) -> io::Result<Vec<u16>> {
        let device_name = self.device_name()?;
        let wide = OsStr::new(&device_name)
            .encode_wide()
            .chain(Some(0))
            .collect();
        Ok(wide)
    }
    pub fn device_name(&self) -> io::Result<String> {
        unsafe {
            // EnumDisplayMonitors indexes active monitors; EnumDisplayDevicesW
            // indexes adapters (including inactive devices). Their indices are
            // unrelated. Resolve the device name using this exact HMONITOR.
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if !GetMonitorInfoW(self.h_monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
                return Err(display_error(
                    "GetMonitorInfoW failed for the selected monitor",
                ));
            }

            let len = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            let os_str = OsString::from_wide(&info.szDevice[..len]);
            Ok(os_str.to_string_lossy().into_owned())
        }
    }
}

fn display_error(context: &str) -> io::Error {
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(0) {
        io::Error::other(context)
    } else {
        io::Error::new(error.kind(), format!("{context}: {error}"))
    }
}
