//! Bounded PNG and packed DIB conversion. No desktop access here.
use crate::{ClipboardError, ClipboardResult};
use std::io::Cursor;
use vnt_proto::clipboard::{valid_dimensions, MAX_IMAGE_BYTES, MAX_IMAGE_RGBA};

struct PngBuffer(Vec<u8>);
impl std::io::Write for PngBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_IMAGE_BYTES - self.0.len() {
            return Err(std::io::Error::other("PNG 超过 32 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

pub struct PreparedImage {
    pub png: Vec<u8>,
    pub dib_v5: Vec<u8>,
    pub dib: Vec<u8>,
}
impl PreparedImage {
    pub fn new(image: ClipboardImage) -> ClipboardResult<Self> {
        let (w, h, pixels) = decode_png(&image.png)?;
        if (w, h) != (image.width, image.height) {
            return Err(invalid("PNG 尺寸不匹配"));
        }
        Ok(Self {
            dib_v5: to_dib(w, h, &pixels, true)?,
            dib: to_dib(w, h, &pixels, false)?,
            png: image.png,
        })
    }
}

fn invalid(why: impl std::fmt::Display) -> ClipboardError {
    ClipboardError::Read(format!("图片无效: {why}"))
}

pub fn decode_png(data: &[u8]) -> ClipboardResult<(u32, u32, Vec<u8>)> {
    if data.is_empty() || data.len() > MAX_IMAGE_BYTES {
        return Err(invalid("PNG 大小超限"));
    }
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(data),
        png::Limits {
            bytes: MAX_IMAGE_RGBA,
        },
    );
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(invalid)?;
    let (w, h) = (reader.info().width, reader.info().height);
    if !valid_dimensions(w, h) {
        return Err(invalid("像素大小超限"));
    }
    let len = reader
        .output_buffer_size()
        .filter(|n| *n <= MAX_IMAGE_RGBA)
        .ok_or_else(|| invalid("像素大小超限"))?;
    let mut raw = vec![0; len];
    let info = reader.next_frame(&mut raw).map_err(invalid)?;
    reader.finish().map_err(invalid)?;
    raw.truncate(info.buffer_size());
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    let channels = info.color_type.samples();
    for p in raw.chunks_exact(channels) {
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(p),
            png::ColorType::Rgb => rgba.extend_from_slice(&[p[0], p[1], p[2], 255]),
            png::ColorType::Grayscale => rgba.extend_from_slice(&[p[0], p[0], p[0], 255]),
            png::ColorType::GrayscaleAlpha => rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]),
            _ => return Err(invalid("PNG 颜色格式不支持")),
        }
    }
    Ok((w, h, rgba))
}

pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> ClipboardResult<ClipboardImage> {
    if !valid_dimensions(width, height) || rgba.len() != width as usize * height as usize * 4 {
        return Err(invalid("像素大小无效"));
    }
    let mut bytes = PngBuffer(Vec::new());
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(invalid)?;
        writer.write_image_data(rgba).map_err(invalid)?;
        writer.finish().map_err(invalid)?;
    }
    Ok(ClipboardImage {
        width,
        height,
        png: bytes.0,
    })
}

pub fn from_png(png: Vec<u8>) -> ClipboardResult<ClipboardImage> {
    let (width, height, rgba) = decode_png(&png)?;
    // Normalize palette/16-bit/APNG input to one portable, static RGBA image.
    from_rgba(width, height, &rgba)
}

fn u32_at(b: &[u8], offset: usize) -> ClipboardResult<u32> {
    Ok(u32::from_le_bytes(
        b.get(offset..offset + 4)
            .ok_or_else(|| invalid("DIB 头截断"))?
            .try_into()
            .unwrap(),
    ))
}

/// BI_RGB (24/32 bit) and standard RGBA BI_BITFIELDS. Other DIB encodings
/// are normalized by GDI by the desktop adapter before calling this function.
pub fn from_dib(b: &[u8]) -> ClipboardResult<ClipboardImage> {
    let header = u32_at(b, 0)? as usize;
    if ![40, 52, 56, 108, 124].contains(&header) || b.len() < header {
        return Err(invalid("DIB 头无效"));
    }
    let w = u32_at(b, 4)? as i32;
    let signed_h = u32_at(b, 8)? as i32;
    let h = signed_h.unsigned_abs();
    if w <= 0 || !valid_dimensions(w as u32, h) {
        return Err(invalid("DIB 尺寸无效"));
    }
    let planes = u16::from_le_bytes(b[12..14].try_into().unwrap());
    let bits = u16::from_le_bytes(b[14..16].try_into().unwrap()) as usize;
    let compression = u32_at(b, 16)?;
    if planes != 1
        || ![24, 32].contains(&bits)
        || ![0, 3].contains(&compression)
        || (compression == 3 && bits != 32)
    {
        return Err(invalid("DIB 编码不支持"));
    }
    let mut offset = header;
    if compression == 3 {
        if u32_at(b, 40)? != 0xff0000 || u32_at(b, 44)? != 0xff00 || u32_at(b, 48)? != 0xff {
            return Err(invalid("DIB 通道掩码不支持"));
        }
        if header == 40 {
            offset += 12;
        }
        if header >= 56 && ![0, 0xff000000].contains(&u32_at(b, 52)?) {
            return Err(invalid("DIB alpha 掩码不支持"));
        }
    }
    offset = offset
        .checked_add(
            (u32_at(b, 32)? as usize)
                .checked_mul(4)
                .ok_or_else(|| invalid("DIB 调色板溢出"))?,
        )
        .ok_or_else(|| invalid("DIB 偏移溢出"))?;
    let stride = (w as usize * bits).div_ceil(32) * 4;
    let end = offset
        .checked_add(stride * h as usize)
        .ok_or_else(|| invalid("DIB 长度溢出"))?;
    if end > b.len() {
        return Err(invalid("DIB 像素截断"));
    }
    let explicit_alpha = bits == 32 && header >= 56 && u32_at(b, 52)? == 0xff000000;
    // Legacy BI_RGB often leaves every reserved byte at zero (opaque), while
    // screenshot tools also use it for actual alpha. Preserve nonzero alpha.
    let alpha = explicit_alpha
        || (bits == 32
            && compression == 0
            && (0..h as usize)
                .any(|y| (0..w as usize).any(|x| b[offset + y * stride + x * 4 + 3] != 0)));
    let mut rgba = vec![0; w as usize * h as usize * 4];
    for y in 0..h as usize {
        let source_y = if signed_h < 0 { y } else { h as usize - y - 1 };
        for x in 0..w as usize {
            let p = offset + source_y * stride + x * (bits / 8);
            let d = (y * w as usize + x) * 4;
            rgba[d..d + 4].copy_from_slice(&[
                b[p + 2],
                b[p + 1],
                b[p],
                if alpha { b[p + 3] } else { 255 },
            ]);
        }
    }
    from_rgba(w as u32, h, &rgba)
}

/// Top-down BGRA DIBV5 with explicit alpha masks; legacy DIB is opaque BGR24.
pub fn to_dib(width: u32, height: u32, rgba: &[u8], v5: bool) -> ClipboardResult<Vec<u8>> {
    if !valid_dimensions(width, height) || rgba.len() != width as usize * height as usize * 4 {
        return Err(invalid("像素大小无效"));
    }
    let header = if v5 { 124 } else { 40 };
    let stride = if v5 {
        width as usize * 4
    } else {
        (width as usize * 3).div_ceil(4) * 4
    };
    let mut b = vec![0u8; header + stride * height as usize];
    for (off, n) in [
        (0, header as u32),
        (4, width),
        (8, (-(height as i32)) as u32),
        (20, (stride * height as usize) as u32),
    ] {
        b[off..off + 4].copy_from_slice(&n.to_le_bytes());
    }
    b[12..14].copy_from_slice(&1u16.to_le_bytes());
    b[14..16].copy_from_slice(&(if v5 { 32u16 } else { 24u16 }).to_le_bytes());
    if v5 {
        for (off, n) in [
            (16, 3u32),
            (40, 0xff0000),
            (44, 0xff00),
            (48, 0xff),
            (52, 0xff000000),
            (56, 0x73524742),
        ] {
            b[off..off + 4].copy_from_slice(&n.to_le_bytes());
        }
    }
    for y in 0..height as usize {
        for x in 0..width as usize {
            let p = (y * width as usize + x) * 4;
            let d = header + y * stride + x * if v5 { 4 } else { 3 };
            if v5 {
                b[d..d + 4].copy_from_slice(&[rgba[p + 2], rgba[p + 1], rgba[p], rgba[p + 3]]);
            } else {
                let a = rgba[p + 3] as u32;
                for (i, c) in [rgba[p + 2], rgba[p + 1], rgba[p]].into_iter().enumerate() {
                    b[d + i] = ((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
                }
            }
        }
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn png_and_dib_roundtrip() {
        let pixels = [255, 0, 0, 255, 0, 128, 255, 64];
        let image = from_rgba(1, 2, &pixels).unwrap();
        assert_eq!(decode_png(&image.png).unwrap().2, pixels);
        let dib = to_dib(1, 2, &pixels, true).unwrap();
        assert_eq!(decode_png(&from_dib(&dib).unwrap().png).unwrap().2, pixels);
        let mut bottom = dib.clone();
        bottom[8..12].copy_from_slice(&2i32.to_le_bytes());
        bottom[124..128].copy_from_slice(&dib[128..132]);
        bottom[128..132].copy_from_slice(&dib[124..128]);
        assert_eq!(
            decode_png(&from_dib(&bottom).unwrap().png).unwrap().2,
            pixels
        );
        let opaque = [255, 0, 0, 255, 0, 128, 255, 255];
        let dib = to_dib(1, 2, &opaque, false).unwrap();
        assert_eq!(decode_png(&from_dib(&dib).unwrap().png).unwrap().2, opaque);
    }
    #[test]
    fn malformed_images_rejected() {
        assert!(from_png(vec![0; 50]).is_err());
        assert!(from_rgba(0, 1, &[]).is_err());
        assert!(from_rgba(8192, 8192, &[]).is_err());
        for n in [0, 4, 39, 40, 123] {
            assert!(from_dib(&vec![0; n]).is_err());
        }
        let mut dib = to_dib(1, 1, &[1, 2, 3, 4], true).unwrap();
        assert!(from_dib(&dib[..127]).is_err());
        dib[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(from_dib(&dib).is_err());
    }
}
