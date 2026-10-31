use crate::*;
pub struct MacInputInjector {
    display: u32,
}
impl MacInputInjector {
    pub fn for_display(display: u32) -> InputResult<Self> {
        let permission = vnt_macos::query(1).map_err(InputError::Os)?;
        if permission["accessibility"] != true {
            return Err(InputError::Os("请授予辅助功能权限".into()));
        }
        Ok(Self { display })
    }
    fn send(&self, kind: i32, a: f64, b: f64, down: bool) -> InputResult<()> {
        vnt_macos::input(self.display, kind, a, b, down).map_err(InputError::Os)
    }
}
impl InputInjector for MacInputInjector {
    fn mouse_move(&self, x: f32, y: f32) -> InputResult<()> {
        self.send(0, x as f64, y as f64, false)
    }
    fn mouse_button(&self, b: MouseButton, down: bool) -> InputResult<()> {
        self.send(
            1,
            match b {
                MouseButton::Left => 0.,
                MouseButton::Right => 1.,
                MouseButton::Middle => 2.,
                MouseButton::X1 => 3.,
                MouseButton::X2 => 4.,
            },
            0.,
            down,
        )
    }
    fn wheel(&self, x: f32, y: f32) -> InputResult<()> {
        self.send(2, x as f64, y as f64, false)
    }
    fn key(&self, scan: u16, extended: bool, down: bool) -> InputResult<()> {
        if let Some(key) = mac_key(scan, extended) {
            self.send(3, key as f64, 0., down)
        } else {
            Ok(())
        }
    }
}
/// Physical Set-1 -> Apple virtual key codes; Meta is Command and Alt is Option.
pub fn mac_key(scan: u16, extended: bool) -> Option<u16> {
    if extended {
        return Some(match scan {
            0x1c => 76,
            0x1d => 62,
            0x35 => 75,
            0x38 => 61,
            0x47 => 115,
            0x48 => 126,
            0x49 => 116,
            0x4b => 123,
            0x4d => 124,
            0x4f => 119,
            0x50 => 125,
            0x51 => 121,
            0x52 => 114,
            0x53 => 117,
            0x5b => 55,
            0x5c => 54,
            _ => return None,
        });
    }
    Some(match scan {
        0x01 => 53,
        0x02 => 18,
        0x03 => 19,
        0x04 => 20,
        0x05 => 21,
        0x06 => 23,
        0x07 => 22,
        0x08 => 26,
        0x09 => 28,
        0x0a => 25,
        0x0b => 29,
        0x0c => 27,
        0x0d => 24,
        0x0e => 51,
        0x0f => 48,
        0x10 => 12,
        0x11 => 13,
        0x12 => 14,
        0x13 => 15,
        0x14 => 17,
        0x15 => 16,
        0x16 => 32,
        0x17 => 34,
        0x18 => 31,
        0x19 => 35,
        0x1a => 33,
        0x1b => 30,
        0x1c => 36,
        0x1d => 59,
        0x1e => 0,
        0x1f => 1,
        0x20 => 2,
        0x21 => 3,
        0x22 => 5,
        0x23 => 4,
        0x24 => 38,
        0x25 => 40,
        0x26 => 37,
        0x27 => 41,
        0x28 => 39,
        0x29 => 50,
        0x2a => 56,
        0x2b => 42,
        0x2c => 6,
        0x2d => 7,
        0x2e => 8,
        0x2f => 9,
        0x30 => 11,
        0x31 => 45,
        0x32 => 46,
        0x33 => 43,
        0x34 => 47,
        0x35 => 44,
        0x36 => 60,
        0x37 => 67,
        0x38 => 58,
        0x39 => 49,
        0x3a => 57,
        0x3b => 122,
        0x3c => 120,
        0x3d => 99,
        0x3e => 118,
        0x3f => 96,
        0x40 => 97,
        0x41 => 98,
        0x42 => 100,
        0x43 => 101,
        0x44 => 109,
        0x45 => 71,
        0x47 => 89,
        0x48 => 91,
        0x49 => 92,
        0x4a => 78,
        0x4b => 86,
        0x4c => 87,
        0x4d => 88,
        0x4e => 69,
        0x4f => 83,
        0x50 => 84,
        0x51 => 85,
        0x52 => 82,
        0x53 => 65,
        0x56 => 10,
        0x57 => 103,
        0x58 => 111,
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modifiers_navigation_and_letters_are_physical() {
        assert_eq!(mac_key(0x1e, false), Some(0));
        assert_eq!(mac_key(0x1d, false), Some(59));
        assert_eq!(mac_key(0x1d, true), Some(62));
        assert_eq!(mac_key(0x5b, true), Some(55));
        assert_eq!(mac_key(0x38, false), Some(58));
        assert_eq!(mac_key(0x4b, true), Some(123));
        assert_eq!(mac_key(0xffff, false), None);
    }
}
