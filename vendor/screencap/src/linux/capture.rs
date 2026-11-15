use crate::common::{convert_bgra_to_bgr, convert_bgra_to_rgb, convert_bgra_to_rgba};
use crate::linux::monitor::{to_io_error, Monitor};
use crate::{PixelFormat, Region};
use std::io;
use xcb::{x, xfixes};

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum CaptureType {
    #[default]
    Xcb,
}

pub struct ScreenGrabber {
    monitor: Monitor,
}

impl ScreenGrabber {
    pub fn new(monitor: &Monitor, _capture_type: CaptureType) -> io::Result<Self> {
        monitor
            .conn()
            .wait_for_reply(monitor.conn().send_request(&xfixes::QueryVersion {
                client_major_version: 5,
                client_minor_version: 0,
            }))
            .map_err(to_io_error)?;
        Ok(Self {
            monitor: monitor.clone(),
        })
    }

    pub fn next_frame(&mut self, buf: &mut [u8]) -> io::Result<(usize, u32, u32)> {
        self.next_frame_region_inner(buf, None)
    }

    pub fn next_frame_region(
        &mut self,
        buf: &mut [u8],
        region: Region,
    ) -> io::Result<(usize, u32, u32)> {
        self.next_frame_region_inner(buf, Some(region))
    }

    fn next_frame_region_inner(
        &mut self,
        buf: &mut [u8],
        region: Option<Region>,
    ) -> io::Result<(usize, u32, u32)> {
        let (monitor_width, monitor_height) = self.monitor.size()?;
        let region = region.unwrap_or_else(|| Region::new(0, 0, monitor_width, monitor_height));
        region.check(monitor_width, monitor_height)?;

        let expected_len = region.width as usize * region.height as usize * 4;
        if buf.len() < expected_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Buffer({}) too small: need {} bytes",
                    buf.len(),
                    expected_len
                ),
            ));
        }

        let (origin_x, origin_y) = self.monitor.origin();
        let x = checked_i16_add_u32(origin_x, region.left, "x")?;
        let y = checked_i16_add_u32(origin_y, region.top, "y")?;
        let width = u16::try_from(region.width).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Region width exceeds X11 limit",
            )
        })?;
        let height = u16::try_from(region.height).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Region height exceeds X11 limit",
            )
        })?;

        let reply = self
            .monitor
            .conn()
            .wait_for_reply(self.monitor.conn().send_request(&x::GetImage {
                format: x::ImageFormat::ZPixmap,
                drawable: x::Drawable::Window(self.monitor.root()),
                x,
                y,
                width,
                height,
                plane_mask: u32::MAX,
            }))
            .map_err(to_io_error)?;

        let setup = self.monitor.conn().get_setup();
        let format = setup
            .pixmap_formats()
            .iter()
            .find(|f| f.depth() == reply.depth())
            .ok_or_else(|| io::Error::other("Unknown X11 pixmap depth"))?;
        let visual = setup
            .roots()
            .flat_map(|s| s.allowed_depths())
            .flat_map(|d| d.visuals())
            .find(|v| v.visual_id() == reply.visual())
            .ok_or_else(|| io::Error::other("Unknown X11 visual"))?;
        unpack_pixels(
            reply.data(),
            &mut buf[..expected_len],
            region.width as usize,
            region.height as usize,
            format.bits_per_pixel(),
            format.scanline_pad(),
            setup.image_byte_order() == x::ImageOrder::LsbFirst,
            [visual.red_mask(), visual.green_mask(), visual.blue_mask()],
        )?;
        if let Ok(cursor) = self
            .monitor
            .conn()
            .wait_for_reply(self.monitor.conn().send_request(&xfixes::GetCursorImage {}))
        {
            let left = i32::from(cursor.x()) - i32::from(cursor.xhot()) - i32::from(x);
            let top = i32::from(cursor.y()) - i32::from(cursor.yhot()) - i32::from(y);
            for (i, pixel) in cursor.cursor_image().iter().enumerate() {
                let cx = left + (i % usize::from(cursor.width())) as i32;
                let cy = top + (i / usize::from(cursor.width())) as i32;
                if cx < 0 || cy < 0 || cx >= region.width as i32 || cy >= region.height as i32 {
                    continue;
                }
                let at = (cy as usize * region.width as usize + cx as usize) * 4;
                let alpha = pixel >> 24;
                for channel in 0..3 {
                    let source = (pixel >> (channel * 8)) & 255;
                    buf[at + channel] = (source
                        + (u32::from(buf[at + channel]) * (255 - alpha) + 127) / 255)
                        .min(255) as u8;
                }
            }
        }
        Ok((expected_len, region.width, region.height))
    }

    pub fn next_frame_region_format(
        &mut self,
        buf: &mut [u8],
        region: Option<Region>,
        pixel_format: PixelFormat,
    ) -> io::Result<(usize, u32, u32)> {
        let (mut len, width, height) = self.next_frame_region_inner(buf, region)?;
        match pixel_format {
            PixelFormat::RGB => len = convert_bgra_to_rgb(&mut buf[..len], width, height),
            PixelFormat::RGBA => convert_bgra_to_rgba(&mut buf[..len], width, height),
            PixelFormat::BGR => len = convert_bgra_to_bgr(&mut buf[..len], width, height),
            PixelFormat::BGRA => {}
        }
        Ok((len, width, height))
    }
}

fn checked_i16_add_u32(base: i16, offset: u32, axis: &str) -> io::Result<i16> {
    let value = i32::from(base)
        .checked_add(i32::try_from(offset).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Region {axis} offset exceeds X11 coordinate limit"),
            )
        })?)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Region {axis} coordinate overflow"),
            )
        })?;

    i16::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Region {axis} coordinate exceeds X11 coordinate limit"),
        )
    })
}

// X11 depth is not bits-per-pixel: 24-bit visuals commonly use 32-bit storage.
fn unpack_pixels(
    data: &[u8],
    out: &mut [u8],
    width: usize,
    height: usize,
    bpp: u8,
    pad: u8,
    little: bool,
    masks: [u32; 3],
) -> io::Result<()> {
    if ![16, 24, 32].contains(&bpp) || ![8, 16, 32].contains(&pad) || masks.contains(&0) {
        return Err(io::Error::other("Unsupported X11 pixel format"));
    }
    let stride = (width * usize::from(bpp)).div_ceil(usize::from(pad)) * usize::from(pad) / 8;
    if data.len() < stride * height || out.len() < width * height * 4 {
        return Err(io::Error::other("Truncated X11 image"));
    }
    let bytes = usize::from(bpp / 8);
    for y in 0..height {
        for x in 0..width {
            let source = &data[y * stride + x * bytes..][..bytes];
            let mut value = 0u32;
            for (i, byte) in source.iter().enumerate() {
                value |= u32::from(*byte) << (if little { i } else { bytes - 1 - i } * 8);
            }
            let destination = &mut out[(y * width + x) * 4..][..4];
            for (channel, mask) in masks.iter().rev().enumerate() {
                let max = mask >> mask.trailing_zeros();
                destination[channel] =
                    ((((value & mask) >> mask.trailing_zeros()) * 255 + max / 2) / max) as u8;
            }
            destination[3] = 255;
        }
    }
    Ok(())
}

#[cfg(test)]
mod pixel_tests {
    use super::*;
    #[test]
    fn padded_rgb565_and_big_endian_rgb24() {
        let mut out = [0; 8];
        unpack_pixels(
            &[0, 248, 0, 0, 224, 7, 0, 0],
            &mut out,
            1,
            2,
            16,
            32,
            true,
            [0xf800, 0x07e0, 0x001f],
        )
        .unwrap();
        assert_eq!(out, [0, 0, 255, 255, 0, 255, 0, 255]);
        unpack_pixels(
            &[255, 0, 0, 0, 0, 255, 0, 0],
            &mut out,
            1,
            2,
            24,
            32,
            false,
            [0xff0000, 0xff00, 0xff],
        )
        .unwrap();
        assert_eq!(out, [0, 0, 255, 255, 0, 255, 0, 255]);
        assert!(
            unpack_pixels(&[], &mut out, 1, 2, 32, 32, true, [0xff0000, 0xff00, 0xff]).is_err()
        );
    }
}
