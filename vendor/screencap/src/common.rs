use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PixelFormat {
    RGB,
    RGBA,
    BGR,
    #[default]
    BGRA,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct Region {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}
impl Region {
    pub fn new(left: u32, top: u32, width: u32, height: u32) -> Self {
        Self {
            left,
            top,
            width,
            height,
        }
    }
    pub fn check(&self, full_width: u32, full_height: u32) -> io::Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "width or height cannot be 0",
            ));
        }
        if self.left > full_width || self.width > full_width - self.left {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Region out of bounds: left({}) + width({}) > full_width({full_width})",
                    self.left, self.width
                ),
            ));
        }
        if self.top > full_height || self.height > full_height - self.top {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Region out of bounds: top({}) + height({}) > full_height({full_height})",
                    self.top, self.height
                ),
            ));
        }
        Ok(())
    }
}

pub(crate) fn convert_bgra_to_bgr(bgra: &mut [u8], width: u32, height: u32) -> usize {
    let pixel_count = width as usize * height as usize;
    let src_len = pixel_count * 4;
    let dst_len = pixel_count * 3;

    assert!(bgra.len() >= src_len);

    let mut write_idx = 0;
    for read_idx in (0..src_len).step_by(4) {
        // 拷贝 B, G, R 三个通道
        bgra[write_idx] = bgra[read_idx]; // B
        bgra[write_idx + 1] = bgra[read_idx + 1]; // G
        bgra[write_idx + 2] = bgra[read_idx + 2]; // R
        write_idx += 3;
    }
    dst_len
}
pub(crate) fn convert_bgra_to_rgba(buf: &mut [u8], width: u32, height: u32) {
    let pixel_count = width as usize * height as usize;
    assert!(buf.len() >= pixel_count * 4);

    for i in 0..pixel_count {
        let base = i * 4;
        buf.swap(base, base + 2); // 交换 B 和 R
    }
}
pub(crate) fn convert_bgra_to_rgb(buf: &mut [u8], width: u32, height: u32) -> usize {
    let pixel_count = width as usize * height as usize;
    let src_len = pixel_count * 4;
    let dst_len = pixel_count * 3;

    assert!(buf.len() >= src_len);

    let mut write = 0;
    for read in (0..src_len).step_by(4) {
        buf[write] = buf[read + 2]; // R
        buf[write + 1] = buf[read + 1]; // G
        buf[write + 2] = buf[read]; // B
        write += 3;
    }

    dst_len
}
