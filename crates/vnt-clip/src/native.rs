//! OLE virtual files. The owner STA only serves metadata; agile streams perform
//! bounded remote reads on COM worker threads. No OleFlushClipboard (it downloads data).
#![allow(non_snake_case)]
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use vnt_proto::pb::ClipboardOffer;
use windows::core::{implement, Error, Result, BOOL, HRESULT};
use windows::Win32::Foundation::{
    DV_E_FORMATETC, E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, HGLOBAL, S_FALSE, S_OK,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    OleInitialize, OleSetClipboard, OleUninitialize, ReleaseStgMedium,
};
use windows::Win32::UI::Shell::*;

pub trait FileOperation: Send + Sync {
    fn read(&self, index: u32, offset: u64, length: u32) -> std::result::Result<Vec<u8>, String>;
    fn close(&self, index: u32);
    fn finish(&self, ok: bool, reason: &str);
}
pub trait FileProvider: Send + Sync {
    /// Must return promptly; a network handshake is deferred until the first read.
    fn begin(&self) -> std::result::Result<Arc<dyn FileOperation>, String>;
    fn available(&self) -> bool;
}
fn failure(reason: impl AsRef<str>) -> Error {
    let text = reason.as_ref();
    let code = if text.contains("取消") {
        HRESULT(0x800704c7u32 as i32)
    } else if text.contains("占用") {
        HRESULT(0x80070020u32 as i32)
    } else {
        E_FAIL
    };
    Error::new(code, text)
}
fn descriptor_format() -> u32 {
    unsafe { RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) }
}
fn contents_format() -> u32 {
    unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) }
}
fn preferred_format() -> u32 {
    unsafe { RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) }
}
fn format(id: u32, medium: TYMED) -> FORMATETC {
    FORMATETC {
        cfFormat: id as u16,
        dwAspect: 1,
        lindex: -1,
        tymed: medium.0 as u32,
        ..Default::default()
    }
}
fn global_bytes(bytes: &[u8]) -> Result<STGMEDIUM> {
    let global = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }?;
    let ptr = unsafe { GlobalLock(global) };
    if ptr.is_null() {
        let _ = unsafe { windows::Win32::Foundation::GlobalFree(Some(global)) };
        return Err(Error::from(E_FAIL));
    }
    // SAFETY: allocation is at least bytes.len() and both ranges are valid.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast(), bytes.len());
        let _ = GlobalUnlock(global);
    }
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: global },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}
fn descriptors(offer: &ClipboardOffer) -> Result<STGMEDIUM> {
    let header = std::mem::offset_of!(FILEGROUPDESCRIPTORW, fgd);
    let size = header + offer.items.len() * std::mem::size_of::<FILEDESCRIPTORW>();
    let mut bytes = vec![0u8; size];
    bytes[..4].copy_from_slice(&(offer.items.len() as u32).to_le_bytes());
    for (index, item) in offer.items.iter().enumerate() {
        let mut descriptor = FILEDESCRIPTORW {
            dwFlags: (FD_ATTRIBUTES.0
                | FD_FILESIZE.0
                | FD_CREATETIME.0
                | FD_WRITESTIME.0
                | FD_PROGRESSUI.0) as u32,
            dwFileAttributes: if item.directory {
                item.attributes | 0x10
            } else {
                item.attributes & !0x10
            },
            nFileSizeHigh: (item.size >> 32) as u32,
            nFileSizeLow: item.size as u32,
            ftCreationTime: windows::Win32::Foundation::FILETIME {
                dwLowDateTime: item.creation_time as u32,
                dwHighDateTime: (item.creation_time >> 32) as u32,
            },
            ftLastWriteTime: windows::Win32::Foundation::FILETIME {
                dwLowDateTime: item.write_time as u32,
                dwHighDateTime: (item.write_time >> 32) as u32,
            },
            ..Default::default()
        };
        let wide: Vec<u16> = item.rel_path.replace('/', "\\").encode_utf16().collect();
        if wide.len() >= 260 {
            return Err(Error::from(E_INVALIDARG));
        }
        let mut name = [0u16; 260];
        name[..wide.len()].copy_from_slice(&wide);
        descriptor.cFileName = name;
        // SAFETY: descriptor is a POD structure. Destination may be unaligned.
        unsafe {
            std::ptr::write_unaligned(
                bytes
                    .as_mut_ptr()
                    .add(header + index * std::mem::size_of::<FILEDESCRIPTORW>())
                    .cast::<FILEDESCRIPTORW>(),
                descriptor,
            );
        }
    }
    global_bytes(&bytes)
}
type LeaseKey = (usize, u32);
type LeaseRegistry = Mutex<HashMap<LeaseKey, std::sync::Weak<StreamLease>>>;
#[implement(IDataObject, IDataObjectAsyncCapability, Agile = false)]
struct FileObject {
    offer: Arc<ClipboardOffer>,
    provider: Arc<dyn FileProvider>,
    current: Mutex<Option<Arc<dyn FileOperation>>>,
    leases: Arc<LeaseRegistry>,
    asynchronous: AtomicBool,
    in_operation: AtomicBool,
}
impl FileObject {
    fn operation(&self) -> Result<Arc<dyn FileOperation>> {
        let mut current = self.current.lock().unwrap();
        if let Some(op) = current.as_ref() {
            return Ok(op.clone());
        }
        if !self.provider.available() {
            return Err(failure("剪贴板已替换或连接已断开"));
        }
        let op = self.provider.begin().map_err(failure)?;
        *current = Some(op.clone());
        Ok(op)
    }
    fn finish(&self, ok: bool, reason: &str) {
        let op = self.current.lock().unwrap().take();
        if let Some(op) = op {
            op.finish(ok, reason);
        }
        self.in_operation.store(false, Ordering::Release);
    }
    fn supported(&self, f: &FORMATETC) -> bool {
        if f.dwAspect != 1 || !f.ptd.is_null() {
            return false;
        }
        let id = f.cfFormat as u32;
        if id == contents_format() {
            return f.tymed & TYMED_ISTREAM.0 as u32 != 0
                && f.lindex >= 0
                && self
                    .offer
                    .items
                    .get(f.lindex as usize)
                    .is_some_and(|i| !i.directory);
        }
        f.lindex == -1
            && f.tymed & TYMED_HGLOBAL.0 as u32 != 0
            && [
                descriptor_format(),
                preferred_format(),
                crate::win::origin_format(),
            ]
            .contains(&id)
    }
}
impl Drop for FileObject {
    fn drop(&mut self) {
        // Existing streams retain the operation when the clipboard owner changes.
        self.current.lock().unwrap().take();
    }
}
impl IDataObject_Impl for FileObject_Impl {
    fn GetData(&self, ptr: *const FORMATETC) -> Result<STGMEDIUM> {
        let f = unsafe { ptr.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
        if !self.supported(f) {
            return Err(Error::from(DV_E_FORMATETC));
        }
        let id = f.cfFormat as u32;
        if id == descriptor_format() {
            return descriptors(&self.offer);
        }
        if id == preferred_format() {
            return global_bytes(&1u32.to_le_bytes());
        } // COPY only
        if id == crate::win::origin_format() {
            return global_bytes(&7u32.to_le_bytes());
        }
        let index = f.lindex as u32;
        let operation = self.operation()?;
        let key = (Arc::as_ptr(&operation) as *const () as usize, index);
        let lease = {
            let mut leases = self.leases.lock().unwrap();
            leases.retain(|_, lease| lease.strong_count() > 0);
            if let Some(lease) = leases.get(&key).and_then(std::sync::Weak::upgrade) {
                lease
            } else {
                let lease = Arc::new(StreamLease {
                    operation,
                    index,
                    key,
                    registry: Arc::downgrade(&self.leases),
                });
                leases.insert(key, Arc::downgrade(&lease));
                lease
            }
        };
        let stream: IStream = RemoteStream {
            lease,
            size: self.offer.items[index as usize].size,
            position: Mutex::new(0),
        }
        .into();
        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
        match unsafe { f.as_ref() } {
            Some(f) if self.supported(f) => S_OK,
            _ => DV_E_FORMATETC,
        }
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, output: *mut FORMATETC) -> HRESULT {
        if output.is_null() {
            return E_POINTER;
        }
        unsafe {
            (*output).ptd = std::ptr::null_mut();
        }
        HRESULT(0x00040130)
    }
    fn SetData(&self, f: *const FORMATETC, medium: *const STGMEDIUM, release: BOOL) -> Result<()> {
        let f = unsafe { f.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
        let medium_ref = unsafe { medium.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
        // Explorer reports the performed effect even for synchronous consumers.
        let succeeded = unsafe { RegisterClipboardFormatW(CFSTR_PASTESUCCEEDED) };
        let performed = unsafe { RegisterClipboardFormatW(CFSTR_PERFORMEDDROPEFFECT) };
        if [succeeded, performed].contains(&(f.cfFormat as u32))
            && medium_ref.tymed == TYMED_HGLOBAL.0 as u32
        {
            let global: HGLOBAL = unsafe { medium_ref.u.hGlobal };
            if unsafe { windows::Win32::System::Memory::GlobalSize(global) } >= 4 {
                let ptr = unsafe { GlobalLock(global) };
                if !ptr.is_null() {
                    let effect = unsafe { std::ptr::read_unaligned(ptr.cast::<u32>()) };
                    let _ = unsafe { GlobalUnlock(global) };
                    if !self.in_operation.load(Ordering::Acquire) {
                        self.finish(effect & 1 != 0, "Shell 复制结束");
                    }
                }
            }
        }
        if release.as_bool() {
            // STGMEDIUM is transferred by the caller; release the owned copy exactly once.
            let mut owned = unsafe { std::ptr::read(medium) };
            unsafe {
                ReleaseStgMedium(&mut owned);
            }
        }
        Ok(())
    }
    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(Error::from(E_NOTIMPL));
        }
        // Enum's lindex is -1; GetData requires the real descriptor index for contents.
        Ok(Formats {
            next: Mutex::new(0),
            formats: vec![
                format(descriptor_format(), TYMED_HGLOBAL),
                format(contents_format(), TYMED_ISTREAM),
                format(preferred_format(), TYMED_HGLOBAL),
                format(crate::win::origin_format(), TYMED_HGLOBAL),
            ],
        }
        .into())
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: windows::core::Ref<'_, IAdviseSink>,
    ) -> Result<u32> {
        Err(Error::from(E_NOTIMPL))
    }
    fn DUnadvise(&self, _: u32) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(Error::from(E_NOTIMPL))
    }
}
impl IDataObjectAsyncCapability_Impl for FileObject_Impl {
    fn SetAsyncMode(&self, value: BOOL) -> Result<()> {
        self.asynchronous.store(value.as_bool(), Ordering::Release);
        Ok(())
    }
    fn GetAsyncMode(&self) -> Result<BOOL> {
        Ok(self.asynchronous.load(Ordering::Acquire).into())
    }
    fn StartOperation(&self, _: windows::core::Ref<'_, IBindCtx>) -> Result<()> {
        if self.in_operation.swap(true, Ordering::AcqRel) {
            return Err(failure("该剪贴板已有粘贴操作"));
        }
        if let Err(e) = self.operation() {
            self.in_operation.store(false, Ordering::Release);
            return Err(e);
        }
        Ok(())
    }
    fn InOperation(&self) -> Result<BOOL> {
        Ok(self.in_operation.load(Ordering::Acquire).into())
    }
    fn EndOperation(
        &self,
        result: HRESULT,
        _: windows::core::Ref<'_, IBindCtx>,
        effects: u32,
    ) -> Result<()> {
        let why =
            if result == HRESULT(0x800704c7u32 as i32) || result == HRESULT(0x80004004u32 as i32) {
                "用户已取消".to_string()
            } else {
                format!("Shell: {result:?}")
            };
        self.finish(result.is_ok() && effects & 1 != 0, &why);
        Ok(())
    }
}
struct StreamLease {
    operation: Arc<dyn FileOperation>,
    index: u32,
    key: LeaseKey,
    registry: std::sync::Weak<LeaseRegistry>,
}
impl Drop for StreamLease {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let leases = registry.lock().unwrap();
            // A new GetData may have replaced our expired Weak before Drop acquired
            // this lock. Never close the handle underneath that newer stream.
            if leases
                .get(&self.key)
                .is_some_and(|lease| lease.strong_count() > 0)
            {
                return;
            }
            self.operation.close(self.index);
        } else {
            self.operation.close(self.index);
        }
    }
}
// windows-rs supplies the free-threaded marshaler. All mutable state is synchronized.
#[implement(IStream)]
struct RemoteStream {
    lease: Arc<StreamLease>,
    size: u64,
    position: Mutex<u64>,
}
impl ISequentialStream_Impl for RemoteStream_Impl {
    fn Read(&self, buffer: *mut c_void, count: u32, read: *mut u32) -> HRESULT {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if buffer.is_null() && count != 0 {
            return E_POINTER;
        }
        if count == 0 {
            return S_OK;
        }
        let mut position = self.position.lock().unwrap();
        if *position > self.size {
            return S_FALSE;
        }
        let mut done = 0u32;
        loop {
            let want = (count - done).min(vnt_proto::control::CLIPBOARD_CHUNK_BYTES as u32);
            let data = match self.lease.operation.read(self.lease.index, *position, want) {
                Ok(data)
                    if data.len() <= want as usize
                        && data.len() as u64 <= self.size.saturating_sub(*position) =>
                {
                    data
                }
                Ok(_) => return E_FAIL,
                Err(e) => {
                    self.lease.operation.finish(false, &e);
                    return failure(e).code();
                }
            };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    buffer.cast::<u8>().add(done as usize),
                    data.len(),
                );
            }
            *position += data.len() as u64;
            done += data.len() as u32;
            if !read.is_null() {
                unsafe {
                    *read = done;
                }
            }
            if data.len() < want as usize {
                return S_FALSE;
            }
            if done == count {
                return S_OK;
            }
        }
    }
    fn Write(&self, _: *const c_void, _: u32, written: *mut u32) -> HRESULT {
        if !written.is_null() {
            unsafe {
                *written = 0;
            }
        }
        E_NOTIMPL
    }
}
impl IStream_Impl for RemoteStream_Impl {
    fn Seek(&self, offset: i64, origin: STREAM_SEEK, output: *mut u64) -> Result<()> {
        let mut position = self.position.lock().unwrap();
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => *position as i128,
            STREAM_SEEK_END => self.size as i128,
            _ => return Err(Error::from(E_INVALIDARG)),
        };
        let next = base + offset as i128;
        if !(0..=u64::MAX as i128).contains(&next) {
            return Err(Error::from(E_INVALIDARG));
        }
        *position = next as u64;
        if !output.is_null() {
            unsafe {
                *output = *position;
            }
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn CopyTo(
        &self,
        target: windows::core::Ref<'_, IStream>,
        count: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> Result<()> {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if !written.is_null() {
            unsafe {
                *written = 0;
            }
        }
        let target = target.as_ref().ok_or_else(|| Error::from(E_POINTER))?;
        let mut buffer = [0u8; vnt_proto::control::CLIPBOARD_CHUNK_BYTES];
        let mut total = 0;
        while total < count {
            let want = (count - total).min(buffer.len() as u64) as u32;
            let mut got = 0;
            self.Read(buffer.as_mut_ptr().cast(), want, &mut got).ok()?;
            if !read.is_null() {
                unsafe {
                    *read += got as u64;
                }
            }
            if got == 0 {
                break;
            }
            let mut sent = 0;
            unsafe { target.Write(buffer.as_ptr().cast(), got, Some(&mut sent)) }.ok()?;
            if !written.is_null() {
                unsafe {
                    *written += sent as u64;
                }
            }
            if sent != got {
                return Err(failure("目标数据流写入不完整"));
            }
            total += got as u64;
            if got < want {
                break;
            }
        }
        Ok(())
    }
    fn Commit(&self, _: &STGC) -> Result<()> {
        Ok(())
    }
    fn Revert(&self) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn Stat(&self, stat: *mut STATSTG, _: &STATFLAG) -> Result<()> {
        if stat.is_null() {
            return Err(Error::from(E_POINTER));
        }
        unsafe {
            stat.write(STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.size,
                grfMode: STGM_READ,
                ..Default::default()
            });
        }
        Ok(())
    }
    fn Clone(&self) -> Result<IStream> {
        Ok(RemoteStream {
            lease: self.lease.clone(),
            size: self.size,
            position: Mutex::new(*self.position.lock().unwrap()),
        }
        .into())
    }
}
#[implement(IEnumFORMATETC)]
struct Formats {
    formats: Vec<FORMATETC>,
    next: Mutex<usize>,
}
impl IEnumFORMATETC_Impl for Formats_Impl {
    fn Next(&self, count: u32, output: *mut FORMATETC, fetched: *mut u32) -> HRESULT {
        if output.is_null() || (count != 1 && fetched.is_null()) {
            return E_POINTER;
        }
        let mut next = self.next.lock().unwrap();
        let n = (count as usize).min(self.formats.len().saturating_sub(*next));
        for i in 0..n {
            unsafe {
                output.add(i).write(self.formats[*next + i]);
            }
        }
        *next += n;
        if !fetched.is_null() {
            unsafe {
                *fetched = n as u32;
            }
        }
        if n == count as usize {
            S_OK
        } else {
            S_FALSE
        }
    }
    fn Skip(&self, count: u32) -> Result<()> {
        let mut next = self.next.lock().unwrap();
        let requested = next.saturating_add(count as usize);
        *next = requested.min(self.formats.len());
        if requested > self.formats.len() {
            return Err(Error::from(S_FALSE));
        }
        Ok(())
    }
    fn Reset(&self) -> Result<()> {
        *self.next.lock().unwrap() = 0;
        Ok(())
    }
    fn Clone(&self) -> Result<IEnumFORMATETC> {
        Ok(Formats {
            formats: self.formats.clone(),
            next: Mutex::new(*self.next.lock().unwrap()),
        }
        .into())
    }
}
pub fn data_object(
    offer: Arc<ClipboardOffer>,
    provider: Arc<dyn FileProvider>,
) -> std::result::Result<IDataObject, String> {
    if !vnt_proto::validation::valid_clipboardoffer(&offer) || offer.offer_id == 0 {
        return Err("文件清单无效".into());
    }
    Ok(FileObject {
        offer,
        provider,
        current: Mutex::new(None),
        leases: Arc::new(Mutex::new(HashMap::new())),
        asynchronous: AtomicBool::new(true),
        in_operation: AtomicBool::new(false),
    }
    .into())
}
/// Preserve S_FALSE: the generated Result<()> projection discards this distinction.
pub fn is_current_object(object: &IDataObject) -> bool {
    use windows::core::Interface;
    #[link(name = "ole32")]
    extern "system" {
        #[link_name = "OleIsCurrentClipboard"]
        fn raw_is_current(object: *mut c_void) -> HRESULT;
    }
    unsafe { raw_is_current(object.as_raw()) == S_OK }
}
pub fn pump() {
    use windows::Win32::UI::WindowsAndMessaging::*;
    let mut msg = MSG::default();
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
pub type ImageValidity = Arc<dyn Fn() -> bool + Send + Sync>;
enum Command {
    Image(crate::image::PreparedImage, u32, ImageValidity),
    Files(Arc<ClipboardOffer>, Arc<dyn FileProvider>, u32),
    Text(String, u32),
}
struct PasteJob {
    text: String,
    valid: ImageValidity,
    reply: std::sync::mpsc::SyncSender<std::result::Result<(),String>>,
}
/// A single latest publication slot coalesces rapid changes without retaining old manifests.
#[derive(Clone)]
pub struct NativeClipboard {
    paste: std::sync::mpsc::SyncSender<PasteJob>,
    pending: Arc<Mutex<Option<Command>>>,
    alive: Arc<AtomicBool>,
}
impl NativeClipboard {
    pub fn start() -> std::result::Result<Self, String> {
        Self::start_impl(None)
    }
    /// Subscribe before reporting readiness, so no clipboard update can fall
    /// between native initialization and watcher startup.
    pub fn start_with_updates() -> std::result::Result<(Self, std::sync::mpsc::Receiver<()>), String>
    {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        Self::start_impl(Some(tx)).map(|native| (native, rx))
    }
    fn start_impl(
        updates: Option<std::sync::mpsc::SyncSender<()>>,
    ) -> std::result::Result<Self, String> {
        let (paste,pastes)=std::sync::mpsc::sync_channel::<PasteJob>(4);
        let pending = Arc::new(Mutex::new(None));
        let work = pending.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let live = alive.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("clipboard-ole".into())
            .spawn(move || {
                if let Err(e) = unsafe { OleInitialize(None) } {
                    live.store(false, Ordering::Release);
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
                let listener = match updates
                    .map(crate::listener::ClipboardListener::new)
                    .transpose()
                {
                    Ok(listener) => listener,
                    Err(e) => {
                        live.store(false, Ordering::Release);
                        let _ = ready_tx.send(Err(format!("注册剪贴板变化通知失败: {e}")));
                        unsafe {
                            OleUninitialize();
                        }
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let mut current: Option<IDataObject> = None;
                while Arc::strong_count(&work) > 1 {
                    pump();
                    if let Ok(job)=pastes.try_recv() {
                        let result=(|| {
                            let mut error=String::new();
                            for _ in 0..20 {
                                if !(job.valid)() {return Err("粘贴请求已取消或过期".into());}
                                match crate::win::set_text(&job.text) {
                                    Ok(()) => {
                                        current=None;
                                        if !(job.valid)() {return Err("粘贴请求已取消或过期".into());}
                                        return vnt_input::paste_shortcut().map_err(|e|e.to_string());
                                    },
                                    Err(e)=>error=e.to_string(),
                                }
                                pump();std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                            Err(error)
                        })();
                        let _=job.reply.send(result);
                    }
                    let command = work.lock().unwrap().take();
                    if let Some(command) = command {
                        let sequence = match &command {
                            Command::Files(_, _, seq)
                            | Command::Text(_, seq)
                            | Command::Image(_, seq, _) => *seq,
                        };
                        let still_current = crate::win::sequence_number() == sequence
                            || crate::win::is_ours().unwrap_or(false);
                        if !still_current {
                            continue;
                        }
                        match command {
                            Command::Image(image, sequence, valid) => {
                                if let Err(e) = crate::win::set_image(&image, sequence, &*valid) {
                                    log::warn!("写入图片失败: {e}");
                                }
                                current = None;
                            }
                            Command::Files(offer, provider, _) => {
                                if !provider.available() {
                                    continue;
                                }
                                match data_object(offer, provider.clone()).and_then(|obj| {
                                    let mut error = String::new();
                                    for _ in 0..20 {
                                        if !provider.available() {
                                            return Err("剪贴板已替换".into());
                                        }
                                        if crate::win::sequence_number() != sequence
                                            && !crate::win::is_ours().unwrap_or(false)
                                        {
                                            return Err("本机剪贴板已更新".into());
                                        }
                                        match unsafe { OleSetClipboard(&obj) } {
                                            Ok(()) => return Ok(obj),
                                            Err(e) => error = e.to_string(),
                                        }
                                        pump();
                                        std::thread::sleep(std::time::Duration::from_millis(5));
                                    }
                                    Err(error)
                                }) {
                                    Ok(obj) => current = Some(obj),
                                    Err(e) => log::warn!("发布原生文件剪贴板失败: {e}"),
                                }
                            }
                            Command::Text(text, _) => {
                                if let Err(e) = crate::win::set_text(&text) {
                                    log::warn!("写入文本失败: {e}");
                                }
                                current = None;
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if let Some(obj) = current {
                    if is_current_object(&obj) {
                        let _ = unsafe { OleSetClipboard(None::<&IDataObject>) };
                    }
                }
                live.store(false, Ordering::Release);
                drop(listener);
                unsafe {
                    OleUninitialize();
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self { pending, alive, paste })
    }
    /// Runs on the clipboard's user-desktop STA, after the text write succeeds.
    pub fn paste_text(&self,text:String,valid:ImageValidity)->std::result::Result<(),String> {
        if text.len()>vnt_proto::control::MAX_CLIPBOARD_TEXT {return Err("文本超过上限".into());}
        let (reply,rx)=std::sync::mpsc::sync_channel(1);
        self.paste.try_send(PasteJob{text,valid,reply}).map_err(|_|"粘贴队列已满或关闭".to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(3)).map_err(|_|"粘贴执行超时".to_string())?
    }
    pub fn files(
        &self,
        offer: Arc<ClipboardOffer>,
        provider: Arc<dyn FileProvider>,
        sequence: u32,
    ) -> std::result::Result<(), String> {
        if !self.alive.load(Ordering::Acquire) {
            return Err("OLE 线程已停止".into());
        }
        *self.pending.lock().unwrap() = Some(Command::Files(offer, provider, sequence));
        Ok(())
    }
    pub fn image(
        &self,
        image: crate::image::PreparedImage,
        sequence: u32,
        valid: ImageValidity,
    ) -> std::result::Result<(), String> {
        if !self.alive.load(Ordering::Acquire) {
            return Err("OLE 线程已停止".into());
        }
        *self.pending.lock().unwrap() = Some(Command::Image(image, sequence, valid));
        Ok(())
    }
    pub fn text(&self, text: String, sequence: u32) -> std::result::Result<(), String> {
        if !self.alive.load(Ordering::Acquire) {
            return Err("OLE 线程已停止".into());
        }
        *self.pending.lock().unwrap() = Some(Command::Text(text, sequence));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory;
    impl FileOperation for Memory {
        fn read(&self, _: u32, offset: u64, length: u32) -> std::result::Result<Vec<u8>, String> {
            let bytes = b"abcdef";
            let end = (offset as usize + length as usize).min(bytes.len());
            Ok(bytes[offset as usize..end].to_vec())
        }
        fn close(&self, _: u32) {}
        fn finish(&self, _: bool, _: &str) {}
    }
    #[test]
    fn streams_seek_clone_and_report_short_reads() {
        let stream: IStream = RemoteStream {
            lease: Arc::new(StreamLease {
                operation: Arc::new(Memory),
                index: 0,
                key: (0, 0),
                registry: std::sync::Weak::new(),
            }),
            size: 6,
            position: Mutex::new(0),
        }
        .into();
        unsafe {
            stream.Seek(2, STREAM_SEEK_SET, None).unwrap();
            let clone = stream.Clone().unwrap();
            let mut bytes = [0u8; 8];
            let mut got = 0;
            assert_eq!(
                clone.Read(bytes.as_mut_ptr().cast(), 8, Some(&mut got)),
                S_FALSE
            );
            assert_eq!(got, 4);
            assert_eq!(&bytes[..4], b"cdef");
            assert_eq!(
                stream.Read(bytes.as_mut_ptr().cast(), 2, Some(&mut got)),
                S_OK
            );
            assert_eq!(&bytes[..2], b"cd");
            assert!(stream.Seek(-10, STREAM_SEEK_SET, None).is_err());
            stream.Seek(20, STREAM_SEEK_SET, None).unwrap();
            assert_eq!(
                stream.Read(bytes.as_mut_ptr().cast(), 1, Some(&mut got)),
                S_FALSE
            );
            assert_eq!(got, 0);
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME).unwrap();
            assert_eq!(stat.cbSize, 6);
        }
    }
    struct MemoryProvider {
        op: Arc<CountingMemory>,
    }
    struct CountingMemory {
        closed: std::sync::atomic::AtomicUsize,
        finished: Mutex<Vec<(bool, String)>>,
    }
    impl FileOperation for CountingMemory {
        fn read(&self, i: u32, offset: u64, length: u32) -> std::result::Result<Vec<u8>, String> {
            Memory.read(i, offset, length)
        }
        fn close(&self, _: u32) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
        fn finish(&self, ok: bool, why: &str) {
            self.finished.lock().unwrap().push((ok, why.into()));
        }
    }
    impl FileProvider for MemoryProvider {
        fn available(&self) -> bool {
            true
        }
        fn begin(&self) -> std::result::Result<Arc<dyn FileOperation>, String> {
            Ok(self.op.clone())
        }
    }
    #[test]
    fn repeated_getdata_shares_file_lease_and_copyto_copies_only_remaining_bytes() {
        let memory = Arc::new(CountingMemory {
            closed: std::sync::atomic::AtomicUsize::new(0),
            finished: Mutex::new(Vec::new()),
        });
        let offer = ClipboardOffer {
            offer_id: 1,
            items: vec![vnt_proto::pb::NativeFileItem {
                rel_path: "a".into(),
                size: 6,
                ..Default::default()
            }],
            total_bytes: 6,
            ..Default::default()
        };
        let object = data_object(
            Arc::new(offer),
            Arc::new(MemoryProvider { op: memory.clone() }),
        )
        .unwrap();
        unsafe {
            let mut f = format(contents_format(), TYMED_ISTREAM);
            f.lindex = 0;
            let mut first = object.GetData(&f).unwrap();
            let stream = (*first.u.pstm).as_ref().unwrap().clone();
            ReleaseStgMedium(&mut first);
            let mut second = object.GetData(&f).unwrap();
            let other = (*second.u.pstm).as_ref().unwrap().clone();
            ReleaseStgMedium(&mut second);
            f.lindex = 1;
            assert!(object.GetData(&f).is_err());
            stream.Seek(2, STREAM_SEEK_SET, None).unwrap();
            let target = windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal(
                HGLOBAL::default(),
                true,
            )
            .unwrap();
            let mut read = 0;
            let mut written = 0;
            stream
                .CopyTo(&target, 100, Some(&mut read), Some(&mut written))
                .unwrap();
            assert_eq!((read, written), (4, 4));
            target.Seek(0, STREAM_SEEK_SET, None).unwrap();
            let mut bytes = [0u8; 4];
            target
                .Read(bytes.as_mut_ptr().cast(), 4, None)
                .ok()
                .unwrap();
            assert_eq!(&bytes, b"cdef");
            drop(stream);
            assert_eq!(memory.closed.load(Ordering::SeqCst), 0);
            drop(other);
            assert_eq!(memory.closed.load(Ordering::SeqCst), 1);
            use windows::core::Interface;
            let asynchronous: IDataObjectAsyncCapability = object.cast().unwrap();
            asynchronous
                .EndOperation(HRESULT(0x800704c7u32 as i32), None::<&IBindCtx>, 0)
                .unwrap();
            assert_eq!(
                &*memory.finished.lock().unwrap(),
                &[(false, "用户已取消".to_string())]
            );
        }
    }
    #[test]
    fn a_late_stream_drop_cannot_close_a_replacement_streams_source_handle() {
        let memory = Arc::new(CountingMemory {
            closed: std::sync::atomic::AtomicUsize::new(0),
            finished: Mutex::new(Vec::new()),
        });
        let registry: Arc<LeaseRegistry> = Arc::new(Mutex::new(HashMap::new()));
        let key = (1, 0);
        let old = Arc::new(StreamLease {
            operation: memory.clone(),
            index: 0,
            key,
            registry: Arc::downgrade(&registry),
        });
        let old_weak = Arc::downgrade(&old);
        let mut entries = registry.lock().unwrap();
        entries.insert(key, old_weak.clone());
        let dropping = std::thread::spawn(move || drop(old));
        while old_weak.strong_count() != 0 {
            std::thread::yield_now();
        }
        let replacement = Arc::new(StreamLease {
            operation: memory.clone(),
            index: 0,
            key,
            registry: Arc::downgrade(&registry),
        });
        entries.insert(key, Arc::downgrade(&replacement));
        drop(entries);
        dropping.join().unwrap();
        assert_eq!(memory.closed.load(Ordering::SeqCst), 0);
        drop(replacement);
        assert_eq!(memory.closed.load(Ordering::SeqCst), 1);
    }
}
