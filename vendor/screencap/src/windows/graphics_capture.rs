use crate::windows::common::Grabber;
use crate::windows::monitor::Monitor;
use crate::Region;
use std::io;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use windows::core::IInspectable;
use windows::Graphics::Capture::Direct3D11CaptureFrame;
use windows::Graphics::SizeInt32;

static CAPTURE_BORDER: AtomicBool = AtomicBool::new(false);

/// Applied by every active WGC source on its next frame acquisition.
pub fn set_capture_border(enabled: bool) {
    CAPTURE_BORDER.store(enabled, Ordering::Release);
}

fn configure_border(session: &GraphicsCaptureSession, enabled: bool) -> io::Result<()> {
    if !enabled {
        use windows::Graphics::Capture::{GraphicsCaptureAccess, GraphicsCaptureAccessKind};
        let access =
            GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)?
                .join()?;
        use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
        if access != AppCapabilityAccessStatus::Allowed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Borderless capture access denied",
            ));
        }
    }
    session.SetIsBorderRequired(enabled)?;
    Ok(())
}
use windows::{
    core::Interface,
    Foundation::TypedEventHandler,
    Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
    Graphics::DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::*,
            Dxgi::IDXGIDevice,
        },
        System::WinRT::{
            Direct3D11::CreateDirect3D11DeviceFromDXGIDevice,
            Graphics::Capture::IGraphicsCaptureItemInterop, RoInitialize, RoUninitialize,
            RO_INIT_MULTITHREADED,
        },
    },
};

pub struct ScreenGrabber {
    id: i64,
    device: ID3D11Device,
    receiver: Receiver<()>,

    session: GraphicsCaptureSession,
    border: bool,
    frame_pool: Direct3D11CaptureFramePool,
    context: ID3D11DeviceContext,
    size: SizeInt32,
    /// 暂存纹理缓存（键：宽/高/格式）。尺寸不变则复用——4K 一帧 33MB，
    /// 每帧新建暂存纹理是纯浪费，还可能引入多余的同步点。
    staging: Option<ID3D11Texture2D>,
    staging_key: (u32, u32, i32),
    // Drop after all WinRT/COM objects, including on construction failure.
    _runtime: WinRtApartment,
}

struct WinRtApartment;
impl Drop for WinRtApartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

// WGC only signals new desktop content. Periodically repeat our own staging
// copy so an idle desktop still services encoder output and keyframe recovery.
const IDLE_FRAME_INTERVAL: Duration = Duration::from_millis(250);

impl Drop for ScreenGrabber {
    fn drop(&mut self) {
        if let Err(e) = self.session.Close() {
            eprintln!("Error closing session: {}", e);
        }

        if let Err(e) = self.frame_pool.RemoveFrameArrived(self.id) {
            eprintln!("Error removing frame arrived handler: {}", e);
        }

        if let Err(e) = self.frame_pool.Close() {
            eprintln!("Error closing frame pool: {}", e);
        }
    }
}

// WGC can leave a native callback winding down after Close/RoUninitialize.
// Keep its code mapped for the process lifetime; do not substitute a timed sleep.
// Observed failure: GraphicsCapture.dll_unloaded / STATUS_ACCESS_VIOLATION.
// https://github.com/robmikh/Win32CaptureSample/issues/99
fn pin_capture_module() -> io::Result<()> {
    use std::sync::OnceLock;
    use windows::{
        core::w,
        Win32::System::LibraryLoader::{GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_PIN},
    };
    static PINNED: OnceLock<Result<(), String>> = OnceLock::new();
    PINNED
        .get_or_init(|| unsafe {
            let mut module = HMODULE::default();
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_PIN,
                w!("GraphicsCapture.dll"),
                &mut module,
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))?;
    Ok(())
}

impl ScreenGrabber {
    pub fn is_supported() -> io::Result<bool> {
        Ok(GraphicsCaptureSession::IsSupported()?)
    }

    pub fn new(monitor: &Monitor) -> io::Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        let runtime = WinRtApartment;
        if !Self::is_supported()? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unsupported GraphicsCapture",
            ));
        }
        pin_capture_module()?;
        {
            // Create the D3D11 device and context
            let (device, context) = create_d3d11_device()?;

            // Create Direct3D device
            let d3d_device = create_direct3d_device(&device)?;

            // Create capture item for monitor
            let item = create_capture_item_for_monitor(monitor)?;
            let size = item.Size()?;

            // Create the capture frame pool
            let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &d3d_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                3,
                size,
            )?;
            d3d_device.Close()?;
            // Create the capture session and start capture
            let session = frame_pool.CreateCaptureSession(&item)?;
            let border = CAPTURE_BORDER.load(Ordering::Acquire);
            if let Err(error) = configure_border(&session, border) {
                let _ = session.Close();
                let _ = frame_pool.Close();
                return Err(error);
            }

            let (tx, rx) = mpsc::sync_channel(1);
            let frame_pool_clone = frame_pool.clone();

            // Register event handler for frame arrival
            let id = frame_pool_clone.FrameArrived(&TypedEventHandler::<
                Direct3D11CaptureFramePool,
                IInspectable,
            >::new(move |_, _| {
                _ = tx.try_send(());
                Ok(())
            }))?;
            session.StartCapture()?;

            Ok(Self {
                id,
                device,
                context,
                receiver: rx,
                session,
                border,
                frame_pool,
                size,
                staging: None,
                staging_key: (0, 0, 0),
                _runtime: runtime,
            })
        }
    }
    fn next_texture(
        &mut self,
    ) -> io::Result<Option<(ID3D11Texture2D, OwnedDirect3D11CaptureFrame)>> {
        let border = CAPTURE_BORDER.load(Ordering::Acquire);
        if border != self.border {
            // A system-policy failure must not interrupt an established capture.
            if let Err(error) = configure_border(&self.session, border) {
                eprintln!("Unable to update capture border: {error}");
            }
            self.border = border;
        }
        let deadline = Instant::now() + IDLE_FRAME_INTERVAL;
        loop {
            match self.frame_pool.TryGetNextFrame() {
                Ok(frame) => unsafe {
                    let mut frame = OwnedDirect3D11CaptureFrame(frame);
                    // The compositor can run faster than the requested stream.
                    // Read back the newest queued pixels, not up to three old
                    // monitor frames. Bound draining to the frame-pool depth so
                    // a continuously updating desktop cannot starve delivery.
                    for _ in 1..3 {
                        match self.frame_pool.TryGetNextFrame() {
                            Ok(newer) => frame = OwnedDirect3D11CaptureFrame(newer),
                            Err(error) => {
                                error.code().ok()?;
                                break;
                            }
                        }
                    }
                    let content_size = frame.ContentSize()?;

                    if content_size != self.size {
                        let d3d_device = create_direct3d_device(&self.device)?;
                        self.frame_pool.Recreate(
                            &d3d_device,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            3,
                            content_size,
                        )?;
                        d3d_device.Close()?;
                        self.size = content_size;
                    }

                    let surface = frame.Surface()?;
                    let access: windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess = surface.cast()?;
                    let texture: ID3D11Texture2D = access.GetInterface()?;
                    return Ok(Some((texture, frame)));
                },
                Err(e) => {
                    e.code().ok()?;
                    match self
                        .receiver
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(()) => {}
                        Err(RecvTimeoutError::Timeout) => return Ok(None),
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err(io::Error::other("channel closed"))
                        }
                    }
                }
            }
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
        // Keep the WGC frame open through Map/readback, which waits for the
        // asynchronous CopyResource to finish before its pool slot is recycled.
        let captured = self.next_texture()?;
        if let Some((texture, _guard)) = captured.as_ref() {
            // Keep our own full-desktop copy. WGC frame-pool textures must not
            // be reused after their frame is closed, and callers may change ROI.
            let (desc, _, _) = self.get_desc(texture, None)?;
            let key = (desc.Width, desc.Height, desc.Format.0);
            if self.staging.is_none() || self.staging_key != key {
                self.staging = Some(self.create_texture2d(desc)?);
                self.staging_key = key;
            }
            unsafe {
                self.context
                    .CopyResource(self.staging.as_ref().unwrap(), texture);
            }
        }
        let staging = self.staging.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "waiting for first capture frame")
        })?;
        self.read_resource(staging, region, self.staging_key.0, self.staging_key.1, buf)
    }
}
struct OwnedDirect3D11CaptureFrame(Direct3D11CaptureFrame);
impl Drop for OwnedDirect3D11CaptureFrame {
    fn drop(&mut self) {
        _ = self.0.Close();
    }
}
impl Deref for OwnedDirect3D11CaptureFrame {
    type Target = Direct3D11CaptureFrame;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn create_d3d11_device() -> io::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }

    Ok((
        device.ok_or_else(|| io::Error::other("Failed to create device"))?,
        context.ok_or_else(|| io::Error::other("Failed to create context"))?,
    ))
}

fn create_direct3d_device(device: &ID3D11Device) -> io::Result<IDirect3DDevice> {
    unsafe {
        let dxgi_device: IDXGIDevice = device.cast()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;
        Ok(inspectable.cast()?)
    }
}

fn create_capture_item_for_monitor(monitor: &Monitor) -> io::Result<GraphicsCaptureItem> {
    unsafe {
        let h_monitor = monitor.h_monitor();
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        Ok(interop.CreateForMonitor(h_monitor)?)
    }
}
