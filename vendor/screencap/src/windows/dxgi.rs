use crate::windows::common::Grabber;
use crate::windows::monitor::Monitor;
use crate::Region;
use std::io;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

pub struct ScreenGrabber {
    duplication: IDXGIOutputDuplication,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    staging_key: (u32, u32, i32),
}

impl ScreenGrabber {
    pub fn new(monitor: &Monitor) -> io::Result<Self> {
        unsafe {
            // GDI monitor indices are not DXGI output indices, and the selected
            // monitor may belong to a different GPU than the default adapter.
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut selected = None;
            let mut adapter_index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
                let mut output_index = 0;
                while let Ok(output) = adapter.EnumOutputs(output_index) {
                    if output.GetDesc()?.Monitor == monitor.h_monitor() {
                        selected = Some((adapter.clone(), output.cast::<IDXGIOutput1>()?));
                        break;
                    }
                    output_index += 1;
                }
                if selected.is_some() {
                    break;
                }
                adapter_index += 1;
            }
            let (adapter, output) = selected.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Selected monitor has no DXGI output",
                )
            })?;
            let mut device = None;
            let mut context = None;

            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;

            let device = device.ok_or_else(|| io::Error::other("No D3D11 device"))?;
            let context = context.ok_or_else(|| io::Error::other("No D3D11 context"))?;

            let duplication = output.DuplicateOutput(&device)?;
            Ok(Self {
                duplication,
                device,
                context,
                staging: None,
                staging_key: (0, 0, 0),
            })
        }
    }
    fn next_texture(&mut self) -> io::Result<Option<(ID3D11Texture2D, DxgiFrameGuard)>> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut desktop_resource = None;

            if let Err(error) =
                self.duplication
                    .AcquireNextFrame(250, &mut frame_info, &mut desktop_resource)
            {
                if error.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(None); // An idle desktop is not a failed capture session.
                }
                return Err(error.into());
            }
            let guard = DxgiFrameGuard(self.duplication.clone());

            let desktop_resource = desktop_resource
                .ok_or_else(|| io::Error::other("Failed to acquire desktop resource"))?;

            let desktop_texture: ID3D11Texture2D = desktop_resource.cast()?;
            Ok(Some((desktop_texture, guard)))
        }
    }
}
impl Grabber for ScreenGrabber {
    fn get_device(&self) -> &ID3D11Device {
        &self.device
    }

    fn get_context(&self) -> &ID3D11DeviceContext {
        &self.context
    }
    fn next_frame_impl(
        &mut self,
        buf: &mut [u8],
        region: Option<Region>,
    ) -> io::Result<(usize, u32, u32)> {
        if let Some((texture, _guard)) = self.next_texture()? {
            let (desc, width, height) = self.get_desc(&texture, None)?;
            let key = (width, height, desc.Format.0);
            if self.staging.is_none() || self.staging_key != key {
                self.staging = Some(self.create_texture2d(desc)?);
                self.staging_key = key;
            }
            unsafe {
                self.context
                    .CopyResource(self.staging.as_ref().unwrap(), &texture);
            }
        }
        let staging = self.staging.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "waiting for first DXGI frame")
        })?;
        self.read_resource(staging, region, self.staging_key.0, self.staging_key.1, buf)
    }
}
struct DxgiFrameGuard(IDXGIOutputDuplication);
impl Drop for DxgiFrameGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.ReleaseFrame();
        }
    }
}
