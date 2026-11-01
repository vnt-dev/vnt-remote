//! NDK MediaCodec objects are exclusively owned by the vnt-video thread.
use crate::media::{Config, Window};
#[cfg(target_os = "android")]
mod native {
    use super::*;
    use ndk_sys::*;
    use std::{ffi::CString, ptr};
    pub struct Decoder {
        codec: *mut AMediaCodec,
        _window: Window,
    }
    fn check(status: media_status_t) -> Result<(), String> {
        if status == media_status_t::AMEDIA_OK {
            Ok(())
        } else {
            Err(format!("MediaCodec: {}", status.0))
        }
    }
    unsafe fn int(f: *mut AMediaFormat, key: &str, value: i32) {
        let key = CString::new(key).unwrap();
        AMediaFormat_setInt32(f, key.as_ptr(), value);
    }
    unsafe fn text(f: *mut AMediaFormat, key: &str, value: &str) {
        let key = CString::new(key).unwrap();
        let value = CString::new(value).unwrap();
        AMediaFormat_setString(f, key.as_ptr(), value.as_ptr());
    }
    impl Decoder {
        pub fn new(c: &Config, window: Window, sdk: u32) -> Result<Self, String> {
            if c.name.is_empty() {
                return Err("没有能解码此屏幕尺寸的解码器".into());
            }
            let name = CString::new(c.name.as_str()).map_err(|_| "解码器名称无效")?;
            unsafe {
                let codec = AMediaCodec_createCodecByName(name.as_ptr());
                if codec.is_null() {
                    return Err("无法创建解码器".into());
                }
                let d = Self {
                    codec,
                    _window: window,
                };
                let format = AMediaFormat_new();
                if format.is_null() {
                    return Err("无法创建媒体格式".into());
                }
                text(
                    format,
                    "mime",
                    match c.codec.as_str() {
                        "hevc" => "video/hevc",
                        "av1" => "video/av01",
                        _ => "video/avc",
                    },
                );
                int(format, "width", c.width as i32);
                int(format, "height", c.height as i32);
                int(format, "frame-rate", c.fps as i32);
                int(format, "priority", 0);
                int(format, "max-input-size", 8 * 1024 * 1024);
                if sdk >= 30 {
                    int(format, "low-latency", 1);
                }
                if !c.csd.is_empty() {
                    AMediaFormat_setBuffer(
                        format,
                        c"csd-0".as_ptr(),
                        c.csd.as_ptr().cast(),
                        c.csd.len(),
                    );
                }
                let status = AMediaCodec_configure(codec, format, d._window.0, ptr::null_mut(), 0);
                AMediaFormat_delete(format);
                check(status)?;
                check(AMediaCodec_start(codec))?;
                Ok(d)
            }
        }
        pub fn input(&mut self, data: &[u8], keyframe: bool, pts: i64) -> Result<bool, String> {
            unsafe {
                let index = AMediaCodec_dequeueInputBuffer(self.codec, 0);
                if index == -1 {
                    return Ok(false);
                }
                if index < 0 {
                    return Err(format!("输入缓冲失败: {index}"));
                }
                let mut capacity = 0;
                let buffer = AMediaCodec_getInputBuffer(self.codec, index as usize, &mut capacity);
                if buffer.is_null() || capacity < data.len() {
                    return Err("解码输入缓冲不足".into());
                }
                ptr::copy_nonoverlapping(data.as_ptr(), buffer, data.len());
                check(AMediaCodec_queueInputBuffer(
                    self.codec,
                    index as usize,
                    0,
                    data.len(),
                    pts as u64,
                    if keyframe { 1 } else { 0 },
                ))?;
                Ok(true)
            }
        }
        pub fn drain(&mut self) -> Result<Vec<i64>, String> {
            let mut rendered = Vec::new();
            unsafe {
                for _ in 0..8 {
                    let mut info: AMediaCodecBufferInfo = std::mem::zeroed();
                    let index = AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, 0);
                    if index >= 0 {
                        let render = info.flags & 2 == 0;
                        check(AMediaCodec_releaseOutputBuffer(
                            self.codec,
                            index as usize,
                            render,
                        ))?;
                        if render {
                            rendered.push(info.presentationTimeUs);
                        }
                    } else if index == -2 {
                        let f = AMediaCodec_getOutputFormat(self.codec);
                        if !f.is_null() {
                            AMediaFormat_delete(f);
                        }
                    } else if index == -1 || index == -3 {
                        break;
                    } else {
                        return Err(format!("输出缓冲失败: {index}"));
                    }
                }
            }
            Ok(rendered)
        }
    }
    impl Drop for Decoder {
        fn drop(&mut self) {
            unsafe {
                AMediaCodec_stop(self.codec);
                AMediaCodec_delete(self.codec);
            }
        }
    }
}
#[cfg(target_os = "android")]
pub use native::Decoder;
#[cfg(not(target_os = "android"))]
pub struct Decoder;
#[cfg(not(target_os = "android"))]
impl Decoder {
    pub fn new(_: &Config, _: Window, _: u32) -> Result<Self, String> {
        Err("MediaCodec requires Android".into())
    }
    pub fn input(&mut self, _: &[u8], _: bool, _: i64) -> Result<bool, String> {
        Ok(false)
    }
    pub fn drain(&mut self) -> Result<Vec<i64>, String> {
        Ok(Vec::new())
    }
}
