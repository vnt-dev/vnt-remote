use crate::MonitorInfo;
use screencap::{PixelFormat, Region};
use std::io;
pub struct Monitor {
    pub info: MonitorInfo,
}
impl Monitor {
    pub fn all() -> io::Result<Vec<Self>> {
        let value = vnt_macos::query(0).map_err(io::Error::other)?;
        let monitors: Vec<MonitorInfo> = serde_json::from_value(value).map_err(io::Error::other)?;
        Ok(monitors.into_iter().map(|info| Self { info }).collect())
    }
    pub fn primary() -> io::Result<Self> {
        Self::all()?
            .into_iter()
            .find(|m| m.info.primary)
            .ok_or(io::ErrorKind::NotFound.into())
    }
    pub fn from_index(index: u32) -> io::Result<Self> {
        Self::all()?
            .into_iter()
            .find(|m| m.info.index == index)
            .ok_or(io::ErrorKind::NotFound.into())
    }
    pub fn index(&self) -> u32 {
        self.info.index
    }
    pub fn size(&self) -> io::Result<(u32, u32)> {
        let m = Self::from_index(self.index())?;
        Ok((m.info.width, m.info.height))
    }
    pub fn device_name(&self) -> io::Result<String> {
        Ok(self.info.name.clone())
    }
}
pub struct ScreenGrabber(vnt_macos::Stream);
impl ScreenGrabber {
    pub fn new(monitor: &Monitor) -> io::Result<Self> {
        vnt_macos::Stream::open(monitor.index(), false)
            .map(Self)
            .map_err(io::Error::other)
    }
    pub fn next_frame_region_format(
        &mut self,
        pixels: &mut [u8],
        _: Option<Region>,
        _: PixelFormat,
    ) -> io::Result<(usize, u32, u32)> {
        let (n, w, h) = self.0.read(pixels).map_err(io::Error::other)?;
        if n == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok((n, w, h))
    }
}
