//! macOS system APIs behind a small C ABI. No native pointer crosses the wire.
#![cfg(target_os = "macos")]
use std::ffi::{c_char, c_void, CStr, CString};

unsafe extern "C" {
    fn vnt_json(op: i32, input: *const c_char) -> *mut c_char;
    fn vnt_free(pointer: *mut c_void);
    fn vnt_clipboard_image(
        bytes: *const u8,
        length: usize,
        sequence: u32,
        context: *const c_void,
        valid: extern "C" fn(*const c_void) -> bool,
    ) -> bool;
    fn vnt_stream_open(display: u32, audio: bool) -> *mut c_void;
    fn vnt_stream_close(stream: *mut c_void);
    fn vnt_stream_read(
        stream: *mut c_void,
        output: *mut u8,
        capacity: usize,
        width: *mut u32,
        height: *mut u32,
    ) -> isize;
    fn vnt_input(display: u32, kind: i32, a: f64, b: f64, down: bool) -> bool;
    fn vnt_encoder_open(hevc: bool, width: u32, height: u32, fps: u32, bitrate: u32)
        -> *mut c_void;
    fn vnt_encoder_close(encoder: *mut c_void);
    fn vnt_encoder_frame(
        encoder: *mut c_void,
        pixels: *const u8,
        pts: u64,
        key: bool,
        output: *mut u8,
        capacity: usize,
        is_key: *mut bool,
    ) -> isize;
    fn vnt_render_open(device: *const c_char) -> *mut c_void;
    fn vnt_render_close(render: *mut c_void);
    fn vnt_render_write(render: *mut c_void, samples: *const f32, count: usize) -> bool;
    fn vnt_keyboard_open(window: usize) -> *mut c_void;
    fn vnt_keyboard_close(keyboard: *mut c_void);
    fn vnt_keyboard_read(
        keyboard: *mut c_void,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    ) -> *mut c_char;
}
pub fn json(op: i32, input: &serde_json::Value) -> Result<serde_json::Value, String> {
    let input = CString::new(input.to_string()).map_err(|e| e.to_string())?;
    unsafe {
        let result = vnt_json(op, input.as_ptr());
        if result.is_null() {
            return Err("macOS 原生接口无响应".into());
        }
        let value = serde_json::from_slice::<serde_json::Value>(CStr::from_ptr(result).to_bytes());
        vnt_free(result.cast());
        let value = value.map_err(|e| e.to_string())?;
        if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
            return Err(error.into());
        }
        Ok(value)
    }
}
pub fn query(op: i32) -> Result<serde_json::Value, String> {
    json(op, &serde_json::Value::Null)
}
pub fn clipboard_image(png: &[u8], sequence: u32, valid: impl Fn() -> bool) -> Result<(), String> {
    extern "C" fn check<F: Fn() -> bool>(context: *const c_void) -> bool {
        // The synchronous C call cannot retain the callback or its borrowed context.
        let valid = unsafe { &*context.cast::<F>() };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(valid)).unwrap_or(false)
    }
    fn write<F: Fn() -> bool>(png: &[u8], sequence: u32, valid: &F) -> bool {
        unsafe {
            vnt_clipboard_image(
                png.as_ptr(),
                png.len(),
                sequence,
                (valid as *const F).cast(),
                check::<F>,
            )
        }
    }
    if png.len() > 32 * 1024 * 1024 {
        return Err("图片超过剪贴板上限".into());
    }
    if write(png, sequence, &valid) {
        Ok(())
    } else {
        Err("图片已取消、剪贴板已变化或写入失败".into())
    }
}
pub struct Stream(*mut c_void);
impl Stream {
    pub fn open(display: u32, audio: bool) -> Result<Self, String> {
        let p = unsafe { vnt_stream_open(display, audio) };
        if p.is_null() {
            Err("无法启动屏幕/系统音频采集，请检查屏幕录制权限和显示器状态".into())
        } else {
            Ok(Self(p))
        }
    }
    pub fn read(&mut self, output: &mut [u8]) -> Result<(usize, u32, u32), String> {
        let (mut w, mut h) = (0, 0);
        let n =
            unsafe { vnt_stream_read(self.0, output.as_mut_ptr(), output.len(), &mut w, &mut h) };
        match n {
            -2 => Err("采集尺寸变化或缓冲区不足".into()),
            n if n < 0 => Err("采集已停止或设备不可用".into()),
            n => Ok((n as usize, w, h)),
        }
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { vnt_stream_close(self.0) }
    }
}
pub fn input(display: u32, kind: i32, a: f64, b: f64, down: bool) -> Result<(), String> {
    if unsafe { vnt_input(display, kind, a, b, down) } {
        Ok(())
    } else {
        Err("无法注入输入，请检查辅助功能权限和显示器状态".into())
    }
}
pub struct Encoder(*mut c_void, usize);
// VTCompressionSession is serially accessed by its owning pipeline; callbacks finish before return.
unsafe impl Send for Encoder {}
impl Encoder {
    pub fn open(
        hevc: bool,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, String> {
        let p = unsafe { vnt_encoder_open(hevc, width, height, fps, bitrate) };
        if p.is_null() {
            Err("VideoToolbox 硬件编码器不可用".into())
        } else {
            Ok(Self(p, width as usize * height as usize * 4))
        }
    }
    pub fn frame(
        &mut self,
        pixels: &[u8],
        pts: u64,
        key: bool,
        out: &mut [u8],
    ) -> Result<(usize, bool), String> {
        if pixels.len() != self.1 {
            return Err("BGRA 帧大小不匹配".into());
        }
        let mut is_key = false;
        let n = unsafe {
            vnt_encoder_frame(
                self.0,
                pixels.as_ptr(),
                pts,
                key,
                out.as_mut_ptr(),
                out.len(),
                &mut is_key,
            )
        };
        if n < 0 {
            Err("VideoToolbox 编码失败或帧超过上限".into())
        } else {
            Ok((n as usize, is_key))
        }
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { vnt_encoder_close(self.0) }
    }
}
pub struct Render(*mut c_void);
impl Render {
    pub fn open(device: Option<&str>) -> Result<Self, String> {
        let device = CString::new(device.unwrap_or("")).map_err(|e| e.to_string())?;
        let p = unsafe { vnt_render_open(device.as_ptr()) };
        if p.is_null() {
            Err("无法打开音频播放设备".into())
        } else {
            Ok(Self(p))
        }
    }
    pub fn write(&mut self, samples: &[[f32; 2]]) -> Result<(), String> {
        if unsafe { vnt_render_write(self.0, samples.as_ptr().cast(), samples.len() * 2) } {
            Ok(())
        } else {
            Err("音频设备已断开".into())
        }
    }
}
impl Drop for Render {
    fn drop(&mut self) {
        unsafe { vnt_render_close(self.0) }
    }
}
pub struct Keyboard(*mut c_void);
// Native event delivery is marshalled to the main queue; reads are serialized by one worker.
unsafe impl Send for Keyboard {}
impl Keyboard {
    /// `window` must be a live NSWindow obtained from Tauri. The native object retains it.
    ///
    /// # Safety
    /// The address must point to an NSWindow in this process.
    pub unsafe fn open(window: usize) -> Result<Self, String> {
        let p = unsafe { vnt_keyboard_open(window) };
        if p.is_null() {
            Err("无法接管当前窗口键盘".into())
        } else {
            Ok(Self(p))
        }
    }
    pub fn read(&mut self, area: [i32; 4]) -> Result<serde_json::Value, String> {
        unsafe {
            let p = vnt_keyboard_read(self.0, area[0], area[1], area[2], area[3]);
            if p.is_null() {
                return Err("键盘接管已停止".into());
            }
            let result =
                serde_json::from_slice(CStr::from_ptr(p).to_bytes()).map_err(|e| e.to_string());
            vnt_free(p.cast());
            result
        }
    }
}
impl Drop for Keyboard {
    fn drop(&mut self) {
        unsafe { vnt_keyboard_close(self.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_queries_preserve_json_boolean_types() {
        let permissions = query(1).unwrap();
        assert!(permissions["screenRecording"].is_boolean());
        assert!(permissions["accessibility"].is_boolean());
        assert!(query(9).unwrap().is_boolean());
        for monitor in query(0).unwrap().as_array().unwrap() {
            assert!(monitor["primary"].is_boolean());
            assert!(monitor["width"].as_u64().unwrap() > 0);
            assert!(monitor["logicalWidth"].as_f64().unwrap() > 0.0);
        }
        for device in query(7).unwrap().as_array().unwrap() {
            assert!(device["isDefault"].is_boolean());
        }
    }

    #[test]
    #[ignore = "requires screen recording permission; captures real pixels in memory"]
    fn native_capture_and_encoding() {
        let monitors = query(0).unwrap();
        let monitor = monitors.as_array().unwrap().first().expect("display");
        let display = monitor["index"].as_u64().unwrap() as u32;
        let mut capture = Stream::open(display, false).unwrap();
        let mut pixels = vec![
            0;
            monitor["width"].as_u64().unwrap() as usize
                * monitor["height"].as_u64().unwrap() as usize
                * 4
        ];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (n, w, h) = loop {
            let frame = capture.read(&mut pixels).unwrap();
            if frame.0 > 0 {
                break frame;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "first frame timed out"
            );
        };
        assert_eq!(n, w as usize * h as usize * 4);
        let mut encoder = Encoder::open(false, w, h, 30, 4000).unwrap();
        let mut packet = vec![0; 8 * 1024 * 1024];
        let (n, key) = encoder.frame(&pixels, 0, true, &mut packet).unwrap();
        assert!(key && n > 4);
        assert_eq!(&packet[..4], &[0, 0, 0, 1]);
    }
}
