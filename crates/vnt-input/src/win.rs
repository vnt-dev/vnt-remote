//! Windows `SendInput` 注入实现。

use crate::{InputError, InputInjector, InputResult, MouseButton};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WHEEL_DELTA, XBUTTON1, XBUTTON2,
};

/// 基于目标显示器矩形把归一化坐标映射到虚拟桌面绝对坐标。
///
/// 持有显示器句柄值而非缓存矩形：分辨率/布局变化后矩形会更新，
/// 缓存则导致坐标按旧尺寸映射（如 1920→1280 后中心仍映射到 960）。
/// （存 isize 而非 HMONITOR：后者含裸指针，不满足 Send/Sync）
#[derive(Debug, Clone)]
pub struct SendInputInjector {
    monitor: isize,
    region_display: Option<(String, u32, u32, u32)>,
}

impl SendInputInjector {
    /// 主显示器。
    pub fn primary() -> InputResult<Self> {
        let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        Self::for_monitor(hmon)
    }

    /// 指定显示器（HMONITOR，来自 vnt-capture 的显示器枚举）。
    pub fn for_monitor(hmon: HMONITOR) -> InputResult<Self> {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let ok = unsafe { GetMonitorInfoW(hmon, &mut info) };
        if !ok.as_bool() {
            return Err(InputError::Os("GetMonitorInfoW 失败".into()));
        }
        Ok(Self {
            monitor: hmon.0 as isize,
            region_display: None,
        })
    }

    /// Re-resolve a region display after hotplug; never fall back to the primary display.
    pub fn for_region_display(
        name: String,
        width: u32,
        height: u32,
        orientation: u32,
    ) -> InputResult<Self> {
        let input = Self {
            monitor: 0,
            region_display: Some((name, width, height, orientation)),
        };
        input.monitor_rect()?;
        Ok(input)
    }

    /// 实时查询目标显示器矩形（分辨率变化后自动跟随）。
    fn monitor_rect(&self) -> InputResult<RECT> {
        if let Some((name, width, height, orientation)) = &self.region_display {
            use windows::{core::PCWSTR, Win32::Graphics::Gdi::*};
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            if !unsafe {
                EnumDisplaySettingsW(PCWSTR(wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode)
            }
            .as_bool()
            {
                return Err(InputError::Os("区域显示器不可用".into()));
            }
            let position = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
            let rotation = unsafe { mode.Anonymous1.Anonymous2.dmDisplayOrientation.0 };
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let handle = unsafe {
                MonitorFromPoint(
                    POINT {
                        x: position.x,
                        y: position.y,
                    },
                    MONITOR_DEFAULTTONULL,
                )
            };
            let exists = unsafe { GetMonitorInfoW(handle, &mut info.monitorInfo) }.as_bool();
            let device = String::from_utf16_lossy(
                &info.szDevice[..info
                    .szDevice
                    .iter()
                    .position(|&v| v == 0)
                    .unwrap_or(info.szDevice.len())],
            );
            if !exists
                || device != *name
                || mode.dmPelsWidth != *width
                || mode.dmPelsHeight != *height
                || rotation != *orientation
            {
                return Err(InputError::Os(
                    "区域显示器或显示模式已变化，暂停输入".into(),
                ));
            }
            return Ok(info.monitorInfo.rcMonitor);
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let ok = unsafe { GetMonitorInfoW(HMONITOR(self.monitor as *mut _), &mut info) };
        if !ok.as_bool() {
            return Err(InputError::Os("GetMonitorInfoW 失败".into()));
        }
        Ok(info.rcMonitor)
    }

    fn virtual_desktop(&self) -> (i32, i32, i32, i32) {
        // SAFETY: 常量查询，无副作用
        unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        }
    }

    fn send(&self, input: INPUT) -> InputResult<()> {
        // UIPI may reject SendInput without setting last-error. Clear stale
        // errors from earlier desktop/monitor queries before collecting it.
        unsafe {
            windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::ERROR_SUCCESS);
        }
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        if sent != 1 {
            let error = std::io::Error::last_os_error();
            return Err(InputError::Os(format!(
                "SendInput 未接受输入（{sent}/1），Windows 错误：{error}；请检查输入桌面、系统输入阻止及权限限制"
            )));
        }
        Ok(())
    }

    fn mouse(&self, dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> InputResult<()> {
        self.send(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: crate::INPUT_EXTRA_INFO,
                },
            },
        })
    }

    fn keybd(&self, scan: u16, flags: KEYBD_EVENT_FLAGS) -> InputResult<()> {
        self.send(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: crate::INPUT_EXTRA_INFO,
                },
            },
        })
    }
}

impl InputInjector for SendInputInjector {
    fn mouse_move(&self, x_norm: f32, y_norm: f32) -> InputResult<()> {
        let m = self.monitor_rect()?;
        let (vx, vy, vw, vh) = self.virtual_desktop();
        let abs_x = absolute_axis(x_norm, m.left, m.right - m.left, vx, vw)?;
        let abs_y = absolute_axis(y_norm, m.top, m.bottom - m.top, vy, vh)?;
        self.mouse(
            abs_x,
            abs_y,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )
    }

    fn mouse_button(&self, button: MouseButton, down: bool) -> InputResult<()> {
        if down && self.region_display.is_some() {
            self.monitor_rect()?;
        }
        let flags = match (button, down) {
            (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
            (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
            (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            (b, down) => {
                let data = match b {
                    MouseButton::X1 => XBUTTON1,
                    _ => XBUTTON2,
                };
                return self.mouse(
                    0,
                    0,
                    data as i32,
                    if down {
                        MOUSEEVENTF_XDOWN
                    } else {
                        MOUSEEVENTF_XUP
                    },
                );
            }
        };
        self.mouse(0, 0, 0, flags)
    }

    fn wheel(&self, dx: f32, dy: f32) -> InputResult<()> {
        if self.region_display.is_some() {
            self.monitor_rect()?;
        }
        if dy != 0.0 {
            let delta = (dy * WHEEL_DELTA as f32) as i32;
            self.mouse(0, 0, delta, MOUSEEVENTF_WHEEL)?;
        }
        if dx != 0.0 {
            let delta = (dx * WHEEL_DELTA as f32) as i32;
            self.mouse(0, 0, delta, MOUSEEVENTF_HWHEEL)?;
        }
        Ok(())
    }

    fn key(&self, scancode: u16, extended: bool, down: bool) -> InputResult<()> {
        if down && self.region_display.is_some() {
            self.monitor_rect()?;
        }
        let mut flags = windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_SCANCODE;
        if extended {
            flags |= windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_KEYUP;
        }
        self.keybd(scancode, flags)
    }
}

fn absolute_axis(
    value: f32,
    origin: i32,
    size: i32,
    virtual_origin: i32,
    virtual_size: i32,
) -> InputResult<i32> {
    if !value.is_finite() || size <= 0 || virtual_size <= 0 {
        return Err(InputError::Os("无效的鼠标坐标或显示器尺寸".into()));
    }
    // RECT right/bottom are exclusive. At 1.0 stay on the last pixel of
    // this monitor instead of crossing into its neighbour.
    let pixel = origin as f64 + value.clamp(0.0, 1.0) as f64 * (size - 1) as f64;
    Ok(
        ((pixel - virtual_origin as f64) * 65535.0 / (virtual_size - 1).max(1) as f64)
            .round()
            .clamp(0.0, 65535.0) as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_edges_stay_inside_the_selected_display() {
        for origin in [0, -1920] {
            let left = absolute_axis(0.0, origin, 1920, origin, 3840).unwrap();
            let right = absolute_axis(1.0, origin, 1920, origin, 3840).unwrap();
            assert_eq!(left, 0);
            assert_eq!((right as f64 * 3839.0 / 65535.0).round() as i32, 1919);
            assert_eq!(
                absolute_axis(1.0, origin + 1920, 1920, origin, 3840).unwrap(),
                65535
            );
        }
        assert!(absolute_axis(f32::NAN, 0, 1920, 0, 1920).is_err());
        assert!(absolute_axis(0.5, 0, 1920, 0, 0).is_err());
    }

    #[test]
    fn primary_monitor_enumerates() {
        // 仅验证几何信息可读；不真正注入事件（测试环境可能无交互会话）。
        let inj = SendInputInjector::primary().expect("应能读取主显示器信息");
        let m = inj.monitor_rect().expect("应能实时查询显示器矩形");
        assert!(m.right > m.left && m.bottom > m.top);
    }
}
