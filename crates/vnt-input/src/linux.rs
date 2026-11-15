use crate::{InputError, InputInjector, InputResult, MouseButton};
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};
use vnt_linux::x11rb::{
    self,
    connection::Connection,
    protocol::{xkb, xproto, xtest},
};
use x11rb::rust_connection::RustConnection;

struct State {
    connection: RustConnection,
    root: u32,
    keys: HashMap<[u8; 4], u8>,
    held_keys: HashSet<u8>,
    held_buttons: HashSet<u8>,
    wheel: [f32; 2],
}
pub struct LinuxInputInjector {
    state: Mutex<State>,
    monitor_name: String,
}
fn os(error: impl std::fmt::Display) -> InputError {
    InputError::Os(error.to_string())
}
impl LinuxInputInjector {
    pub fn for_display(index: u32) -> InputResult<Self> {
        vnt_linux::session::check_x11().map_err(os)?;
        let monitor = screencap::Monitor::from_index(index).map_err(os)?;
        let (connection, screen) = x11rb::connect(None).map_err(os)?;
        let root = connection.setup().roots[screen].root;
        xkb::use_extension(&connection, 1, 0)
            .map_err(os)?
            .reply()
            .map_err(os)?;
        let names = xkb::get_names(&connection, 0x100u16, xkb::NameDetail::KEY_NAMES)
            .map_err(os)?
            .reply()
            .map_err(os)?;
        let keys = names
            .value_list
            .key_names
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .filter_map(|(i, key)| {
                u8::try_from(usize::from(names.first_key) + i)
                    .ok()
                    .map(|code| (key.name, code))
            })
            .collect();
        Ok(Self {
            state: Mutex::new(State {
                connection,
                root,
                keys,
                held_keys: HashSet::new(),
                held_buttons: HashSet::new(),
                wheel: [0.; 2],
            }),
            monitor_name: monitor.device_name().map_err(os)?,
        })
    }
}
fn emit(state: &State, kind: u8, detail: u8, x: i16, y: i16) -> InputResult<()> {
    xtest::fake_input(
        &state.connection,
        kind,
        detail,
        x11rb::CURRENT_TIME,
        state.root,
        x,
        y,
        0,
    )
    .map_err(os)?
    .check()
    .map_err(os)?;
    state.connection.flush().map_err(os)
}
pub fn map_axis(value: f32, origin: i16, extent: u32) -> InputResult<i16> {
    if !value.is_finite() || extent == 0 {
        return Err(os("无效的显示器坐标"));
    }
    let pixel = i64::from(origin) + (value.clamp(0., 1.) * (extent - 1) as f32).round() as i64;
    i16::try_from(pixel).map_err(os)
}
impl InputInjector for LinuxInputInjector {
    fn mouse_move(&self, x: f32, y: f32) -> InputResult<()> {
        let monitor = screencap::Monitor::all()
            .map_err(os)?
            .into_iter()
            .find(|m| m.device_name().ok().as_deref() == Some(&self.monitor_name))
            .ok_or_else(|| os("显示器已断开"))?;
        let origin = monitor.origin();
        let size = monitor.size().map_err(os)?;
        let state = self.state.lock().unwrap();
        emit(
            &state,
            xproto::MOTION_NOTIFY_EVENT,
            0,
            map_axis(x, origin.0, size.0)?,
            map_axis(y, origin.1, size.1)?,
        )
    }
    fn mouse_button(&self, button: MouseButton, down: bool) -> InputResult<()> {
        let detail = match button {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 3,
            MouseButton::X1 => 8,
            MouseButton::X2 => 9,
        };
        let mut state = self.state.lock().unwrap();
        emit(
            &state,
            if down {
                xproto::BUTTON_PRESS_EVENT
            } else {
                xproto::BUTTON_RELEASE_EVENT
            },
            detail,
            0,
            0,
        )?;
        if down {
            state.held_buttons.insert(detail);
        } else {
            state.held_buttons.remove(&detail);
        }
        Ok(())
    }
    fn wheel(&self, dx: f32, dy: f32) -> InputResult<()> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err(os("无效的滚轮值"));
        }
        let mut state = self.state.lock().unwrap();
        for (axis, delta) in [dx, dy].into_iter().enumerate() {
            state.wheel[axis] = (state.wheel[axis] + delta).clamp(-32., 32.);
            let count = state.wheel[axis].trunc() as i32;
            state.wheel[axis] -= count as f32;
            let button = if axis == 0 {
                if count > 0 {
                    7
                } else {
                    6
                }
            } else if count > 0 {
                4
            } else {
                5
            };
            for _ in 0..count.unsigned_abs() {
                emit(&state, xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
                emit(&state, xproto::BUTTON_RELEASE_EVENT, button, 0, 0)?;
            }
        }
        Ok(())
    }
    fn key(&self, scan: u16, extended: bool, down: bool) -> InputResult<()> {
        let Some(name) = key_name(scan, extended) else {
            return Ok(());
        };
        let mut state = self.state.lock().unwrap();
        let code = *state.keys.get(&name).ok_or_else(|| {
            os(format!(
                "XKB 键位不可用：{}",
                String::from_utf8_lossy(&name)
            ))
        })?;
        emit(
            &state,
            if down {
                xproto::KEY_PRESS_EVENT
            } else {
                xproto::KEY_RELEASE_EVENT
            },
            code,
            0,
            0,
        )?;
        if down {
            state.held_keys.insert(code);
        } else {
            state.held_keys.remove(&code);
        }
        Ok(())
    }
}
impl Drop for LinuxInputInjector {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            for code in &state.held_keys {
                let _ = emit(state, xproto::KEY_RELEASE_EVENT, *code, 0, 0);
            }
            for code in &state.held_buttons {
                let _ = emit(state, xproto::BUTTON_RELEASE_EVENT, *code, 0, 0);
            }
        }
    }
}
/// XKB physical names preserve positions across host keyboard layouts.
pub fn key_name(scan: u16, extended: bool) -> Option<[u8; 4]> {
    if extended {
        return Some(*match scan {
            0x1c => b"KPEN",
            0x1d => b"RCTL",
            0x35 => b"KPDV",
            0x37 => b"PRSC",
            0x38 => b"RALT",
            0x45 => b"PAUS",
            0x47 => b"HOME",
            0x48 => b"UP\0\0",
            0x49 => b"PGUP",
            0x4b => b"LEFT",
            0x4d => b"RGHT",
            0x4f => b"END\0",
            0x50 => b"DOWN",
            0x51 => b"PGDN",
            0x52 => b"INS\0",
            0x53 => b"DELE",
            0x5b => b"LWIN",
            0x5c => b"RWIN",
            0x5d => b"COMP",
            _ => return None,
        });
    }
    for (start, count, prefix) in [
        (0x02, 12, b"AE"),
        (0x10, 12, b"AD"),
        (0x1e, 11, b"AC"),
        (0x2c, 10, b"AB"),
        (0x3b, 10, b"FK"),
    ] {
        if (start..start + count).contains(&scan) {
            let n = (scan - start + 1) as u8;
            return Some([prefix[0], prefix[1], b'0' + n / 10, b'0' + n % 10]);
        }
    }
    Some(*match scan {
        0x01 => b"ESC\0",
        0x0e => b"BKSP",
        0x0f => b"TAB\0",
        0x1c => b"RTRN",
        0x1d => b"LCTL",
        0x29 => b"TLDE",
        0x2a => b"LFSH",
        0x2b => b"BKSL",
        0x36 => b"RTSH",
        0x37 => b"KPMU",
        0x38 => b"LALT",
        0x39 => b"SPCE",
        0x3a => b"CAPS",
        0x45 => b"NMLK",
        0x46 => b"SCLK",
        0x47 => b"KP7\0",
        0x48 => b"KP8\0",
        0x49 => b"KP9\0",
        0x4a => b"KPSU",
        0x4b => b"KP4\0",
        0x4c => b"KP5\0",
        0x4d => b"KP6\0",
        0x4e => b"KPAD",
        0x4f => b"KP1\0",
        0x50 => b"KP2\0",
        0x51 => b"KP3\0",
        0x52 => b"KP0\0",
        0x53 => b"KPDL",
        0x56 => b"LSGT",
        0x57 => b"FK11",
        0x58 => b"FK12",
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_keys_distinguish_modifiers_and_keypad() {
        assert_eq!(key_name(0x1d, false), Some(*b"LCTL"));
        assert_eq!(key_name(0x1d, true), Some(*b"RCTL"));
        assert_eq!(key_name(0x1c, true), Some(*b"KPEN"));
        assert_eq!(key_name(0x2f, false), Some(*b"AB04"));
        assert_eq!(key_name(0xffff, true), None);
    }
    #[test]
    fn coordinates_include_origin_and_clamp_at_last_pixel() {
        assert_eq!(map_axis(1., -1920, 1920).unwrap(), -1);
        assert_eq!(map_axis(-1., 1920, 1920).unwrap(), 1920);
        assert_eq!(map_axis(2., 1920, 1920).unwrap(), 3839);
        assert!(map_axis(f32::NAN, 0, 1920).is_err());
        assert!(map_axis(1., 32760, 1920).is_err());
    }
}
