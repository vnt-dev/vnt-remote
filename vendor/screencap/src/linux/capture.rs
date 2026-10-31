use crate::common::{convert_bgra_to_bgr, convert_bgra_to_rgb, convert_bgra_to_rgba};
use crate::linux::monitor::{to_io_error, Monitor};
use crate::{PixelFormat, Region};
use std::io;
use xcb::x;

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

        let data = reply.data();
        if data.len() < expected_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "X11 returned {} bytes, expected at least {} bytes",
                    data.len(),
                    expected_len
                ),
            ));
        }

        buf[..expected_len].copy_from_slice(&data[..expected_len]);
        for pixel in buf[..expected_len].chunks_exact_mut(4) {
            pixel[3] = u8::MAX;
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
