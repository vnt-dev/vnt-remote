use crate::Region;
use std::io;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

pub trait Grabber {
    fn get_device(&self) -> &ID3D11Device;
    fn get_context(&self) -> &ID3D11DeviceContext;
    fn get_desc(
        &self,
        texture: &ID3D11Texture2D,
        region: Option<Region>,
    ) -> io::Result<(D3D11_TEXTURE2D_DESC, u32, u32)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        let full_width = desc.Width;
        let full_height = desc.Height;

        desc.Usage = D3D11_USAGE_STAGING;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        if let Some(region) = region {
            region.check(full_width, full_height)?;
            desc.Width = region.width;
            desc.Height = region.height;
        }
        Ok((desc, full_width, full_height))
    }
    fn create_texture2d(&self, desc: D3D11_TEXTURE2D_DESC) -> io::Result<ID3D11Texture2D> {
        unsafe {
            let mut tex = None;
            self.get_device()
                .CreateTexture2D(&desc, None, Some(&mut tex))?;
            tex.ok_or_else(|| io::Error::other("Failed to create staging texture"))
        }
    }

    fn next_frame(&mut self, buf: &mut [u8]) -> io::Result<(usize, u32, u32)> {
        self.next_frame_impl(buf, None)
    }
    fn next_frame_impl(
        &mut self,
        buf: &mut [u8],
        region: Option<Region>,
    ) -> io::Result<(usize, u32, u32)>;
    /// Read an already populated staging texture without another GPU copy.
    fn read_resource(
        &self,
        staging: &ID3D11Texture2D,
        region: Option<Region>,
        full_width: u32,
        full_height: u32,
        buf: &mut [u8],
    ) -> io::Result<(usize, u32, u32)> {
        const PIXEL_WIDTH: usize = 4;
        let (left, top, target_width, target_height) = if let Some(region) = region {
            region.check(full_width, full_height)?;
            (region.left, region.top, region.width, region.height)
        } else {
            (0, 0, full_width, full_height)
        };
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.get_context()
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

            let row_pitch = mapped.RowPitch as usize;
            let expected_size = target_height as usize * target_width as usize * PIXEL_WIDTH;

            if buf.len() < expected_size {
                self.get_context().Unmap(staging, 0);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Buffer is too small to hold the frame data",
                ));
            }
            if left == 0 && top == 0 && row_pitch == target_width as usize * PIXEL_WIDTH {
                std::ptr::copy_nonoverlapping(
                    mapped.pData as *const u8,
                    buf.as_mut_ptr(),
                    expected_size,
                )
            } else {
                // 拷贝每一行（确保跳过 row_pitch 的填充字节）
                let bytes_per_row = target_width as usize * PIXEL_WIDTH;
                for y in 0..target_height as usize {
                    let src = (mapped.pData as *const u8)
                        .add((top as usize + y) * row_pitch + left as usize * PIXEL_WIDTH);
                    let dst = buf.as_mut_ptr().add(y * bytes_per_row);
                    std::ptr::copy_nonoverlapping(src, dst, bytes_per_row);
                }
            }

            self.get_context().Unmap(staging, 0);
            Ok((expected_size, target_width, target_height))
        }
    }
}
