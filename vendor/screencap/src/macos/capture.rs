use crate::common::{convert_bgra_to_bgr, convert_bgra_to_rgb, convert_bgra_to_rgba};
use crate::macos::Monitor;
use crate::{PixelFormat, Region};
use core_graphics::display::{CGPoint, CGRect, CGSize};
use core_graphics::image::CGImage;
use std::io;

#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum CaptureType {
    #[default]
    Quartz,
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

    fn next_frame_region_inner(
        &mut self,
        buf: &mut [u8],
        region: Option<Region>,
    ) -> io::Result<(usize, u32, u32)> {
        let image = if let Some(region) = region {
            let (full_width, scale_x, full_height, scale_y) = self.monitor.scale_size()?;
            region.check(full_width, full_height)?;
            let rect = CGRect {
                origin: CGPoint {
                    x: region.left as f64 / scale_x,
                    y: region.top as f64 / scale_y,
                },
                size: CGSize {
                    width: region.width as f64 / scale_x,
                    height: region.height as f64 / scale_y,
                },
            };
            self.monitor
                .display()
                .image_for_rect(rect)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no data"))?
        } else {
            self.monitor
                .display()
                .image()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no data"))?
        };
        let len = copy_image_data(&image, buf)?;
        Ok((len, image.width() as u32, image.height() as u32))
    }

    pub fn next_frame_region(
        &mut self,
        buf: &mut [u8],
        region: Region,
    ) -> io::Result<(usize, u32, u32)> {
        self.next_frame_region_inner(buf, Some(region))
    }

    pub fn next_frame_region_format(
        &mut self,
        buf: &mut [u8],
        region: Option<Region>,
        pixel_format: PixelFormat,
    ) -> io::Result<(usize, u32, u32)> {
        let (len, width, height) = self.next_frame_region_inner(buf, region)?;
        let len = match pixel_format {
            PixelFormat::RGB => convert_bgra_to_rgb(&mut buf[..len], width, height),
            PixelFormat::RGBA => {
                convert_bgra_to_rgba(&mut buf[..len], width, height);
                len
            }
            PixelFormat::BGR => convert_bgra_to_bgr(&mut buf[..len], width, height),
            PixelFormat::BGRA => len,
        };
        Ok((len, width, height))
    }
}

/// 复制CGImage的像素数据到buf。返回拷贝字节数。
fn copy_image_data(image: &CGImage, buf: &mut [u8]) -> io::Result<usize> {
    let data = image.data();
    let bytes = data.bytes();
    let len = bytes.len();
    if buf.len() < len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Buffer({}) too small: need {} bytes", buf.len(), len),
        ));
    }
    buf[..len].copy_from_slice(bytes);
    Ok(len)
}
