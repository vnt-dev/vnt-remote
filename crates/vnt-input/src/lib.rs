//! 键鼠事件注入：把控制端发来的输入事件注入本机。
//!
//! Windows 实现：`SendInput`（绝对坐标鼠标 + 扫描码键盘）。
//! 抽象成 [`InputInjector`] trait，其他平台可实现各自后端（如 enigo）。

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("注入输入事件失败: {0}")]
    Os(String),
}

pub type InputResult<T> = Result<T, InputError>;

/// Tag our own SendInput events so a local control window cannot feed them
/// back into another capture loop. Other software's injected input is valid.
pub const INPUT_EXTRA_INFO: usize = 0x564e_5444;

/// 输入注入器。坐标为相对目标显示器的归一化值 (0.0~1.0)。
pub trait InputInjector: Send {
    fn mouse_move(&self, x_norm: f32, y_norm: f32) -> InputResult<()>;
    fn mouse_button(&self, button: MouseButton, down: bool) -> InputResult<()>;
    /// 滚轮：dy>0 向上，dx>0 向右，单位为行/格。
    fn wheel(&self, dx: f32, dy: f32) -> InputResult<()>;
    /// 键盘：Windows Set-1 扫描码；`extended` 对应 KEYEVENTF_EXTENDEDKEY。
    fn key(&self, scancode: u16, extended: bool, down: bool) -> InputResult<()>;
}

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::SendInputInjector;

#[cfg(windows)]
pub use windows::Win32::Graphics::Gdi::HMONITOR;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{mac_key, MacInputInjector};

/// Paste into this host's focused application. Platform selection never comes from the peer.
/// Key-up is attempted even if a preceding injection fails.
pub fn paste_shortcut() -> InputResult<()> {
    #[cfg(windows)]
    let input = SendInputInjector::primary()?;
    #[cfg(target_os = "macos")]
    let input = MacInputInjector::for_display(0)?;
    #[cfg(any(windows, target_os = "macos"))]
    {
        paste_with(&input, cfg!(target_os = "macos"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(InputError::Os("当前系统不支持粘贴".into()))
    }
}
fn paste_with(input: &dyn InputInjector, command: bool) -> InputResult<()> {
    let modifier = if command { 0x5b } else { 0x1d };
    let result = input
        .key(modifier, command, true)
        .and_then(|_| input.key(0x2f, false, true));
    let release_v = input.key(0x2f, false, false);
    let release_modifier = input.key(modifier, command, false);
    result.and(release_v).and(release_modifier)
}

#[cfg(test)]
mod paste_tests {
    use super::*;
    use std::sync::Mutex;
    struct Input {
        events: Mutex<Vec<(u16, bool, bool)>>,
        fail: bool,
    }
    impl InputInjector for Input {
        fn mouse_move(&self, _: f32, _: f32) -> InputResult<()> {
            Ok(())
        }
        fn mouse_button(&self, _: MouseButton, _: bool) -> InputResult<()> {
            Ok(())
        }
        fn wheel(&self, _: f32, _: f32) -> InputResult<()> {
            Ok(())
        }
        fn key(&self, scan: u16, extended: bool, down: bool) -> InputResult<()> {
            self.events.lock().unwrap().push((scan, extended, down));
            if self.fail && down {
                Err(InputError::Os("test".into()))
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn selects_host_modifier_and_releases_keys_on_failure() {
        for command in [false, true] {
            let input = Input {
                events: Mutex::new(vec![]),
                fail: false,
            };
            paste_with(&input, command).unwrap();
            let modifier = if command { 0x5b } else { 0x1d };
            assert_eq!(
                *input.events.lock().unwrap(),
                vec![
                    (modifier, command, true),
                    (0x2f, false, true),
                    (0x2f, false, false),
                    (modifier, command, false)
                ]
            );
            let failed = Input {
                events: Mutex::new(vec![]),
                fail: true,
            };
            assert!(paste_with(&failed, command).is_err());
            assert_eq!(
                failed.events.lock().unwrap().last(),
                Some(&(modifier, command, false))
            );
        }
    }
}
