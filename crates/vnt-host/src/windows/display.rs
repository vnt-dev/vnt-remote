//! Display mode operations executed only inside an authenticated session agent.
use serde::{Deserialize, Serialize};
use windows::{
    core::PCWSTR,
    Win32::{Foundation::POINTL, Graphics::Gdi::*},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Mode {
    fields: u32,
    width: u32,
    height: u32,
    bits: u32,
    frequency: u32,
    x: i32,
    y: i32,
    orientation: u32,
    fixed_output: u32,
    flags: u32,
    panning_width: u32,
    panning_height: u32,
    log_pixels: u16,
}
impl Mode {
    fn from_native(mode: &DEVMODEW) -> Self {
        let display = unsafe { mode.Anonymous1.Anonymous2 };
        Self {
            fields: mode.dmFields.0
                & (DM_POSITION
                    | DM_DISPLAYORIENTATION
                    | DM_DISPLAYFIXEDOUTPUT
                    | DM_BITSPERPEL
                    | DM_PELSWIDTH
                    | DM_PELSHEIGHT
                    | DM_DISPLAYFLAGS
                    | DM_DISPLAYFREQUENCY
                    | DM_PANNINGWIDTH
                    | DM_PANNINGHEIGHT
                    | DM_LOGPIXELS)
                    .0,
            width: mode.dmPelsWidth,
            height: mode.dmPelsHeight,
            bits: mode.dmBitsPerPel,
            frequency: mode.dmDisplayFrequency,
            x: display.dmPosition.x,
            y: display.dmPosition.y,
            orientation: display.dmDisplayOrientation.0,
            fixed_output: display.dmDisplayFixedOutput.0,
            flags: unsafe { mode.Anonymous2.dmDisplayFlags },
            panning_width: mode.dmPanningWidth,
            panning_height: mode.dmPanningHeight,
            log_pixels: mode.dmLogPixels,
        }
    }
    fn native(&self) -> DEVMODEW {
        DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            dmFields: DEVMODE_FIELD_FLAGS(self.fields),
            dmPelsWidth: self.width,
            dmPelsHeight: self.height,
            dmBitsPerPel: self.bits,
            dmDisplayFrequency: self.frequency,
            Anonymous1: DEVMODEW_0 {
                Anonymous2: DEVMODEW_0_1 {
                    dmPosition: POINTL {
                        x: self.x,
                        y: self.y,
                    },
                    dmDisplayOrientation: DEVMODE_DISPLAY_ORIENTATION(self.orientation),
                    dmDisplayFixedOutput: DEVMODE_DISPLAY_FIXED_OUTPUT(self.fixed_output),
                },
            },
            Anonymous2: DEVMODEW_1 {
                dmDisplayFlags: self.flags,
            },
            dmPanningWidth: self.panning_width,
            dmPanningHeight: self.panning_height,
            dmLogPixels: self.log_pixels,
            ..Default::default()
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub device: String,
    pub original: Mode,
    pub desired: Mode,
}
#[derive(Clone, Serialize, Deserialize)]
pub enum Operation {
    Prepare {
        monitor_index: u32,
        width: u32,
        height: u32,
    },
    Apply(Plan),
    Restore(Plan),
}

fn current(device: PCWSTR) -> anyhow::Result<DEVMODEW> {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    anyhow::ensure!(
        unsafe {
            EnumDisplaySettingsExW(
                device,
                ENUM_CURRENT_SETTINGS,
                &mut mode,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
            .as_bool()
        },
        "读取当前显示模式失败"
    );
    Ok(mode)
}
fn select_mode(
    modes: impl Iterator<Item = DEVMODEW>,
    original: &DEVMODEW,
    width: u32,
    height: u32,
) -> Option<DEVMODEW> {
    modes
        .filter(|mode| {
            mode.dmPelsWidth == width
                && mode.dmPelsHeight == height
                && mode.dmBitsPerPel == original.dmBitsPerPel
                && unsafe {
                    mode.Anonymous1.Anonymous2.dmDisplayOrientation
                        == original.Anonymous1.Anonymous2.dmDisplayOrientation
                }
        })
        .max_by_key(|mode| {
            (
                mode.dmDisplayFrequency == original.dmDisplayFrequency,
                mode.dmDisplayFrequency,
            )
        })
}
pub fn execute(operation: Operation) -> anyhow::Result<Option<Plan>> {
    anyhow::ensure!(
        super::native::desktop_name().is_ok_and(|n| n.eq_ignore_ascii_case("default")),
        "安全桌面暂不执行显示模式操作"
    );
    let original = unsafe {
        windows::Win32::System::StationsAndDesktops::GetThreadDesktop(
            windows::Win32::System::Threading::GetCurrentThreadId(),
        )?
    };
    let desktop = super::native::Desktop::input()?;
    anyhow::ensure!(desktop.1.eq_ignore_ascii_case("default"), "桌面已变化");
    desktop.bind()?;
    struct Binding {
        original: windows::Win32::System::StationsAndDesktops::HDESK,
        _desktop: super::native::Desktop,
    }
    impl Drop for Binding {
        fn drop(&mut self) {
            unsafe {
                let _ =
                    windows::Win32::System::StationsAndDesktops::SetThreadDesktop(self.original);
            }
        }
    }
    let _binding = Binding {
        original,
        _desktop: desktop,
    };
    match operation {
        Operation::Prepare {
            monitor_index,
            width,
            height,
        } => {
            anyhow::ensure!(
                crate::resolution::validate(2, width, height),
                "目标分辨率无效"
            );
            let monitor = vnt_capture::list_monitors()?
                .into_iter()
                .find(|m| m.index == monitor_index)
                .ok_or_else(|| anyhow::anyhow!("指定的显示器不存在"))?;
            let device = super::native::wide(&monitor.name);
            let device_ptr = PCWSTR(device.as_ptr());
            let original = current(device_ptr)?;
            let modes = (0..).map_while(|i| {
                let mut mode = DEVMODEW {
                    dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                    ..Default::default()
                };
                unsafe {
                    EnumDisplaySettingsExW(
                        device_ptr,
                        ENUM_DISPLAY_SETTINGS_MODE(i),
                        &mut mode,
                        ENUM_DISPLAY_SETTINGS_FLAGS(0),
                    )
                    .as_bool()
                }
                .then_some(mode)
            });
            let mut desired = select_mode(modes, &original, width, height)
                .ok_or_else(|| anyhow::anyhow!("被控显示器不支持 {width}×{height}"))?;
            // Keep the display's position and orientation in the desktop layout.
            desired.Anonymous1 = original.Anonymous1;
            desired.dmFields |= DM_POSITION | DM_DISPLAYORIENTATION;
            Ok(Some(Plan {
                device: monitor.name,
                original: Mode::from_native(&original),
                desired: Mode::from_native(&desired),
            }))
        }
        Operation::Apply(plan) => {
            change(&plan.device, &plan.desired)?;
            Ok(None)
        }
        Operation::Restore(plan) => {
            change(&plan.device, &plan.original)?;
            Ok(None)
        }
    }
}
fn change(device: &str, mode: &Mode) -> anyhow::Result<()> {
    let device = super::native::wide(device);
    let device = PCWSTR(device.as_ptr());
    let native = mode.native();
    let tested = unsafe { ChangeDisplaySettingsExW(device, Some(&native), None, CDS_TEST, None) };
    anyhow::ensure!(
        tested == DISP_CHANGE_SUCCESSFUL,
        "显示模式测试失败：{}",
        tested.0
    );
    let result =
        unsafe { ChangeDisplaySettingsExW(device, Some(&native), None, CDS_TYPE(0), None) };
    anyhow::ensure!(
        result == DISP_CHANGE_SUCCESSFUL,
        "显示模式切换失败：{}",
        result.0
    );
    let actual = current(device)?;
    anyhow::ensure!(
        (actual.dmPelsWidth, actual.dmPelsHeight) == (mode.width, mode.height),
        "显示模式切换后尺寸不符"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefers_current_refresh_then_highest_supported() {
        let make = |hz| DEVMODEW {
            dmPelsWidth: 1920,
            dmPelsHeight: 1080,
            dmDisplayFrequency: hz,
            dmBitsPerPel: 32,
            ..Default::default()
        };
        let modes = vec![make(60), make(144), make(120)];
        assert_eq!(
            select_mode(modes.clone().into_iter(), &make(60), 1920, 1080)
                .unwrap()
                .dmDisplayFrequency,
            60
        );
        assert_eq!(
            select_mode(modes.clone().into_iter(), &make(75), 1920, 1080)
                .unwrap()
                .dmDisplayFrequency,
            144
        );
        assert!(select_mode(modes.into_iter(), &make(60), 3840, 2160).is_none());
        let original = Mode::from_native(&make(144));
        assert_eq!(original.native().dmDisplayFrequency, 144);
    }
    #[test]
    #[ignore = "temporarily changes the real primary display; run explicitly and serially"]
    fn native_display_mode_roundtrip_restores_original() {
        let monitors = vnt_capture::list_monitors().expect("enumerate real monitors");
        let monitor = monitors
            .iter()
            .find(|m| m.primary)
            .or(monitors.first())
            .unwrap();
        let device = super::super::native::wide(&monitor.name);
        let original = current(PCWSTR(device.as_ptr())).unwrap();
        let candidates = (0..).map_while(|i| {
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            unsafe {
                EnumDisplaySettingsExW(
                    PCWSTR(device.as_ptr()),
                    ENUM_DISPLAY_SETTINGS_MODE(i),
                    &mut mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
                .as_bool()
            }
            .then_some(mode)
        });
        let target = candidates
            .filter(|m| {
                m.dmPelsWidth >= 640
                    && m.dmPelsHeight >= 360
                    && m.dmPelsWidth <= original.dmPelsWidth
                    && m.dmPelsHeight <= original.dmPelsHeight
                    && (m.dmPelsWidth, m.dmPelsHeight)
                        != (original.dmPelsWidth, original.dmPelsHeight)
                    && m.dmBitsPerPel == original.dmBitsPerPel
                    && unsafe {
                        m.Anonymous1.Anonymous2.dmDisplayOrientation
                            == original.Anonymous1.Anonymous2.dmDisplayOrientation
                    }
            })
            .max_by_key(|m| m.dmPelsWidth as u64 * m.dmPelsHeight as u64);
        let Some(target) = target else {
            eprintln!("No alternate supported smaller mode; validating current mode only");
            let plan = execute(Operation::Prepare {
                monitor_index: monitor.index,
                width: original.dmPelsWidth,
                height: original.dmPelsHeight,
            })
            .unwrap()
            .unwrap();
            let mode = plan.desired.native();
            assert_eq!(
                unsafe {
                    ChangeDisplaySettingsExW(
                        PCWSTR(device.as_ptr()),
                        Some(&mode),
                        None,
                        CDS_TEST,
                        None,
                    )
                },
                DISP_CHANGE_SUCCESSFUL
            );
            return;
        };
        let plan = execute(Operation::Prepare {
            monitor_index: monitor.index,
            width: target.dmPelsWidth,
            height: target.dmPelsHeight,
        })
        .unwrap()
        .unwrap();
        struct RestoreGuard(Option<Plan>);
        impl Drop for RestoreGuard {
            fn drop(&mut self) {
                if let Some(plan) = self.0.take() {
                    if let Err(error) = execute(Operation::Restore(plan)) {
                        eprintln!("DISPLAY RESTORE FAILED: {error:#}");
                    }
                }
            }
        }
        let mut guard = RestoreGuard(Some(plan.clone()));
        execute(Operation::Apply(plan.clone())).expect("apply alternate display mode");
        eprintln!(
            "Switched {}x{} to {}x{}",
            original.dmPelsWidth, original.dmPelsHeight, target.dmPelsWidth, target.dmPelsHeight
        );
        execute(Operation::Restore(plan)).expect("restore original mode");
        guard.0 = None;
        let actual = current(PCWSTR(device.as_ptr())).unwrap();
        assert_eq!(
            (
                actual.dmPelsWidth,
                actual.dmPelsHeight,
                actual.dmDisplayFrequency,
                actual.dmBitsPerPel
            ),
            (
                original.dmPelsWidth,
                original.dmPelsHeight,
                original.dmDisplayFrequency,
                original.dmBitsPerPel
            )
        );
    }
}
