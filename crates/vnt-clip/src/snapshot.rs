//! Serialize clipboard reads on one STA and retain each returned STGMEDIUM.
//! GetClipboardData returns borrowed handles which another clipboard operation
//! can invalidate. IDataObject::GetData transfers a medium with its own lifetime.
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CLIPBRD_E_CANT_OPEN, HANDLE, HGLOBAL, S_OK};
use windows::Win32::System::Com::{
    IDataObject, FORMATETC, STGMEDIUM, TYMED, TYMED_GDI, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    OleGetClipboard, OleInitialize, OleUninitialize, ReleaseStgMedium,
};

use crate::win::{LocalSnapshot, RawImage};
use crate::{ClipboardError, ClipboardResult};

const READ_TIMEOUT: Duration = Duration::from_secs(2);

struct Request {
    origin: bool,
    deadline: Instant,
    reply: mpsc::SyncSender<ClipboardResult<LocalSnapshot>>,
}

struct Reader(mpsc::SyncSender<Request>);

impl Reader {
    fn start() -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel::<Request>(1);
        let (ready, initialized) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("clipboard-reader".into())
            .spawn(move || {
                if let Err(error) = unsafe { OleInitialize(None) } {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
                let _ = ready.send(Ok(()));
                serve(rx, |origin| {
                    // Do not OpenClipboard around OLE calls: delayed renderers need
                    // to acquire it themselves while servicing COM messages.
                    let object = unsafe { OleGetClipboard() }.map_err(read_error)?;
                    read_object(&object, origin)
                });
                unsafe {
                    OleUninitialize();
                }
            })
            .map_err(|error| error.to_string())?;
        initialized.recv().map_err(|error| error.to_string())??;
        Ok(Self(tx))
    }

    fn read(&self, origin: bool) -> ClipboardResult<LocalSnapshot> {
        self.read_timeout(origin, READ_TIMEOUT)
    }

    fn read_timeout(&self, origin: bool, timeout: Duration) -> ClipboardResult<LocalSnapshot> {
        let (reply, response) = mpsc::sync_channel(1);
        let request = Request {
            origin,
            deadline: Instant::now() + timeout,
            reply,
        };
        self.0.try_send(request).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => ClipboardError::Open,
            mpsc::TrySendError::Disconnected(_) => {
                ClipboardError::Read("剪贴板读取线程已停止".into())
            }
        })?;
        response
            .recv_timeout(timeout)
            .map_err(|_| ClipboardError::Read("剪贴板读取超时或已取消".into()))?
    }
}

// One active read and one queued request across all connections. Timeout in a
// caller never spawns another native reader or retains an unbounded snapshot.
fn serve(
    rx: mpsc::Receiver<Request>,
    mut read: impl FnMut(bool) -> ClipboardResult<LocalSnapshot>,
) {
    loop {
        crate::native::pump();
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(request) if Instant::now() < request.deadline => {
                let result = read(request.origin);
                let _ = request.reply.send(result);
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

pub(crate) fn snapshot(origin: bool) -> ClipboardResult<LocalSnapshot> {
    static READER: OnceLock<Result<Reader, String>> = OnceLock::new();
    READER
        .get_or_init(Reader::start)
        .as_ref()
        .map_err(|error| ClipboardError::Read(error.clone()))?
        .read(origin)
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

struct Medium(STGMEDIUM);
impl Medium {
    fn get(object: &IDataObject, id: u32, kind: TYMED) -> ClipboardResult<Self> {
        let medium = Self(unsafe { object.GetData(&format(id, kind)) }.map_err(read_error)?);
        if medium.0.tymed != kind.0 as u32 {
            return Err(ClipboardError::Read("剪贴板返回了错误的数据介质".into()));
        }
        Ok(medium)
    }
    fn global(&self) -> HGLOBAL {
        unsafe { self.0.u.hGlobal }
    }
    fn bytes(&self, limit: usize) -> ClipboardResult<Vec<u8>> {
        // The medium is still owned here, and the lock pins its memory until
        // after the copy. Never release a medium with an outstanding lock.
        let global = self.global();
        let locked = Locked::new(global)?;
        let size = unsafe { GlobalSize(global) };
        if size == 0 || size > limit {
            return Err(ClipboardError::TooLarge("剪贴板数据大小无效或超限".into()));
        }
        Ok(unsafe { std::slice::from_raw_parts(locked.ptr, size) }.to_vec())
    }
}
impl Drop for Medium {
    fn drop(&mut self) {
        unsafe {
            ReleaseStgMedium(&mut self.0);
        }
    }
}

struct Locked {
    global: HGLOBAL,
    ptr: *const u8,
}
impl Locked {
    fn new(global: HGLOBAL) -> ClipboardResult<Self> {
        if global.0.is_null() {
            return Err(ClipboardError::Read("剪贴板数据为空".into()));
        }
        let ptr = unsafe { GlobalLock(global) }.cast::<u8>();
        if ptr.is_null() {
            return Err(ClipboardError::Read("无法锁定剪贴板数据".into()));
        }
        Ok(Self { global, ptr })
    }
}
impl Drop for Locked {
    fn drop(&mut self) {
        let _ = unsafe { GlobalUnlock(self.global) };
    }
}

fn read_error(error: windows::core::Error) -> ClipboardError {
    if error.code() == CLIPBRD_E_CANT_OPEN {
        ClipboardError::Open
    } else {
        ClipboardError::Read(error.to_string())
    }
}

fn available(object: &IDataObject, id: u32, kind: TYMED) -> ClipboardResult<bool> {
    match unsafe { object.QueryGetData(&format(id, kind)) } {
        CLIPBRD_E_CANT_OPEN => Err(ClipboardError::Open),
        status => Ok(status == S_OK),
    }
}

fn bitmap(object: &IDataObject) -> ClipboardResult<RawImage> {
    let medium = Medium::get(object, 2, TYMED_GDI)?;
    crate::win::read_bitmap(HANDLE(unsafe { medium.0.u.hBitmap }.0))
}

fn read_object(object: &IDataObject, origin: bool) -> ClipboardResult<LocalSnapshot> {
    if origin && available(object, crate::win::origin_format(), TYMED_HGLOBAL)? {
        return Ok(LocalSnapshot::Ours);
    }
    if available(object, 15, TYMED_HGLOBAL)? {
        let medium = Medium::get(object, 15, TYMED_HGLOBAL)?;
        return crate::win::read_drop_files(HANDLE(medium.global().0)).map(LocalSnapshot::Files);
    }
    let png = crate::win::png_format();
    for (id, limit) in [
        (png, vnt_proto::clipboard::MAX_IMAGE_BYTES),
        (17, vnt_proto::clipboard::MAX_IMAGE_RGBA + 1024 * 1024),
        (8, vnt_proto::clipboard::MAX_IMAGE_RGBA + 1024 * 1024),
    ] {
        if !available(object, id, TYMED_HGLOBAL)? {
            continue;
        }
        let bytes = Medium::get(object, id, TYMED_HGLOBAL)?.bytes(limit)?;
        if id != png && bytes.len() >= 20 {
            let bits = u16::from_le_bytes(bytes[14..16].try_into().unwrap());
            let compression = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
            if ![24, 32].contains(&bits) || ![0, 3].contains(&compression) {
                return bitmap(object).map(LocalSnapshot::Image);
            }
        }
        return Ok(LocalSnapshot::Image(if id == png {
            RawImage::Png(bytes)
        } else {
            RawImage::Dib(bytes)
        }));
    }
    if available(object, 2, TYMED_GDI)? {
        return bitmap(object).map(LocalSnapshot::Image);
    }
    if available(object, 13, TYMED_HGLOBAL)? {
        let bytes = Medium::get(object, 13, TYMED_HGLOBAL)?
            .bytes(vnt_proto::control::MAX_CLIPBOARD_TEXT * 2 + 2)?;
        if bytes.len() % 2 != 0 {
            return Err(ClipboardError::Read("剪贴板文本长度无效".into()));
        }
        let words: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let end = words
            .iter()
            .position(|&word| word == 0)
            .ok_or_else(|| ClipboardError::Read("剪贴板文本未终止".into()))?;
        let text = String::from_utf16_lossy(&words[..end]);
        if text.len() > vnt_proto::control::MAX_CLIPBOARD_TEXT {
            return Err(ClipboardError::TooLarge("文本超过 4 MiB".into()));
        }
        return Ok(LocalSnapshot::Text(text));
    }
    Ok(LocalSnapshot::Other)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use std::mem::ManuallyDrop;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use windows::core::{implement, Error, Interface, Ref, Result, BOOL, HRESULT};
    use windows::Win32::Foundation::{GlobalFree, DV_E_FORMATETC, E_NOTIMPL, E_POINTER};
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalFlags, GMEM_MOVEABLE, GMEM_ZEROINIT};

    // A genuine COM release owner lets the tests check ordering and exactly-once
    // release without dereferencing a freed handle or touching the OS clipboard.
    #[implement(ISequentialStream)]
    struct ReleaseOwner {
        global: usize,
        released: Arc<AtomicUsize>,
        released_locked: Arc<AtomicUsize>,
    }
    impl Drop for ReleaseOwner {
        fn drop(&mut self) {
            let global = HGLOBAL(self.global as *mut c_void);
            if unsafe { GlobalFlags(global) } & 0xff != 0 {
                self.released_locked.fetch_add(1, Ordering::SeqCst);
            }
            // GlobalFree returns NULL on success; windows 0.61 maps that to
            // Err, so its Result::is_ok() cannot be used as a success check.
            if unsafe { GlobalFree(Some(global)) }.is_ok() {
                self.released_locked.fetch_add(1, Ordering::SeqCst);
            }
            self.released.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl ISequentialStream_Impl for ReleaseOwner_Impl {
        fn Read(&self, _: *mut c_void, _: u32, _: *mut u32) -> HRESULT {
            E_NOTIMPL
        }
        fn Write(&self, _: *const c_void, _: u32, _: *mut u32) -> HRESULT {
            E_NOTIMPL
        }
    }

    type Formats = Vec<(u32, Vec<u8>)>;

    #[implement(IDataObject)]
    struct Source {
        data: Arc<Mutex<Formats>>,
        reads: Arc<AtomicUsize>,
        failure: Option<HRESULT>,
        released: Arc<AtomicUsize>,
        released_locked: Arc<AtomicUsize>,
    }
    impl IDataObject_Impl for Source_Impl {
        fn GetData(&self, f: *const FORMATETC) -> Result<STGMEDIUM> {
            if let Some(failure) = self.failure {
                return Err(failure.into());
            }
            let f = unsafe { f.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
            let data = self.data.lock().unwrap();
            let (_, bytes) = data
                .iter()
                .find(|(id, _)| *id == f.cfFormat as u32)
                .ok_or_else(|| Error::from(DV_E_FORMATETC))?;
            self.reads.fetch_add(1, Ordering::SeqCst);
            let global = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1)) }?;
            let ptr = unsafe { GlobalLock(global) };
            assert!(!ptr.is_null());
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast(), bytes.len());
                let _ = GlobalUnlock(global);
            }
            let owner: ISequentialStream = ReleaseOwner {
                global: global.0 as usize,
                released: self.released.clone(),
                released_locked: self.released_locked.clone(),
            }
            .into();
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: global },
                pUnkForRelease: ManuallyDrop::new(Some(owner.cast()?)),
            })
        }
        fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
            if let Some(failure) = self.failure {
                return failure;
            }
            match unsafe { f.as_ref() } {
                Some(f)
                    if f.tymed == TYMED_HGLOBAL.0 as u32
                        && self
                            .data
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|(id, _)| *id == f.cfFormat as u32) =>
                {
                    S_OK
                }
                _ => DV_E_FORMATETC,
            }
        }
        fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
            E_NOTIMPL
        }
        fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn EnumFormatEtc(&self, _: u32) -> Result<IEnumFORMATETC> {
            Err(E_NOTIMPL.into())
        }
        fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<'_, IAdviseSink>) -> Result<u32> {
            Err(E_NOTIMPL.into())
        }
        fn DUnadvise(&self, _: u32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
        fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
            Err(E_NOTIMPL.into())
        }
    }
    fn source(data: Formats) -> Source {
        Source {
            data: Arc::new(Mutex::new(data)),
            reads: Arc::new(AtomicUsize::new(0)),
            failure: None,
            released: Arc::new(AtomicUsize::new(0)),
            released_locked: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn text(value: &str) -> Vec<u8> {
        value
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[test]
    fn busy_data_object_requests_retry_for_queries_and_reads() {
        let mut src = source(vec![]);
        src.failure = Some(CLIPBRD_E_CANT_OPEN);
        let object: IDataObject = src.into();
        assert!(matches!(
            read_object(&object, true),
            Err(ClipboardError::Open)
        ));
        assert!(matches!(
            Medium::get(&object, 17, TYMED_HGLOBAL),
            Err(ClipboardError::Open)
        ));
    }

    #[test]
    fn owned_medium_survives_source_replacement_and_releases_after_unlock() {
        for _ in 0..200 {
            let src = source(vec![(17, vec![0x42; 64])]);
            let data = src.data.clone();
            let released = src.released.clone();
            let locked = src.released_locked.clone();
            let object: IDataObject = src.into();
            let medium = Medium::get(&object, 17, TYMED_HGLOBAL).unwrap();
            // Replace the original source on another thread and release it.
            std::thread::spawn(move || data.lock().unwrap().clear())
                .join()
                .unwrap();
            drop(object);
            assert_eq!(released.load(Ordering::SeqCst), 0);
            assert_eq!(&medium.bytes(1024).unwrap()[..64], &[0x42; 64]);
            drop(medium);
            assert_eq!(released.load(Ordering::SeqCst), 1);
            assert_eq!(locked.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn oversize_and_wrong_media_release_owned_storage_once() {
        for wrong in [false, true] {
            let src = source(vec![(17, vec![1; 128])]);
            let released = src.released.clone();
            let locked = src.released_locked.clone();
            let object: IDataObject = src.into();
            if wrong {
                assert!(matches!(
                    Medium::get(&object, 17, TYMED_GDI),
                    Err(ClipboardError::Read(_))
                ));
            } else {
                let medium = Medium::get(&object, 17, TYMED_HGLOBAL).unwrap();
                assert!(matches!(medium.bytes(1), Err(ClipboardError::TooLarge(_))));
            }
            assert_eq!(released.load(Ordering::SeqCst), 1);
            assert_eq!(locked.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn origin_marker_skips_owned_content_unless_explicitly_read() {
        let src = source(vec![
            (crate::win::origin_format(), vec![0; 4]),
            (13, text("截图 🖼️")),
        ]);
        let reads = src.reads.clone();
        let released = src.released.clone();
        let object: IDataObject = src.into();
        assert!(matches!(
            read_object(&object, true),
            Ok(LocalSnapshot::Ours)
        ));
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(
            matches!(read_object(&object, false), Ok(LocalSnapshot::Text(value)) if value == "截图 🖼️")
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn png_and_dib_snapshots_own_their_bytes_after_source_dropped() {
        let image = crate::image::from_rgba(2, 1, &[255, 0, 0, 255, 0, 128, 255, 64]).unwrap();
        let prepared = crate::image::PreparedImage::new(image.clone()).unwrap();
        for (id, bytes) in [
            (crate::win::png_format(), prepared.png),
            (17, prepared.dib_v5),
            (8, prepared.dib),
        ] {
            let src = source(vec![(13, text("fallback")), (id, bytes)]);
            let released = src.released.clone();
            let object: IDataObject = src.into();
            let LocalSnapshot::Image(raw) = read_object(&object, true).unwrap() else {
                panic!("expected image");
            };
            assert_eq!(released.load(Ordering::SeqCst), 1);
            drop(object);
            let decoded = raw.decode().unwrap();
            assert_eq!((decoded.width, decoded.height), (2, 1));
            if id != 8 {
                assert_eq!(decoded, image);
            }
        }
    }

    #[test]
    fn unicode_text_requires_terminator_and_obeys_byte_limit() {
        for bytes in [vec![0x61, 0x61], vec![1; 3]] {
            let object: IDataObject = source(vec![(13, bytes)]).into();
            assert!(matches!(
                read_object(&object, true),
                Err(ClipboardError::Read(_))
            ));
        }
        let maximum = "x".repeat(vnt_proto::control::MAX_CLIPBOARD_TEXT);
        let object: IDataObject = source(vec![(13, text(&maximum))]).into();
        assert!(
            matches!(read_object(&object, true), Ok(LocalSnapshot::Text(value)) if value == maximum)
        );
        let object: IDataObject = source(vec![(
            13,
            text(&"界".repeat(vnt_proto::control::MAX_CLIPBOARD_TEXT / 3 + 1)),
        )])
        .into();
        assert!(matches!(
            read_object(&object, true),
            Err(ClipboardError::TooLarge(_))
        ));
    }

    #[test]
    fn files_take_priority_without_rendering_image_or_text() {
        let paths = [r"C:\screenshots\一.png", r"C:\screenshots\two.png"];
        let mut drop = vec![0u8; 20];
        drop[..4].copy_from_slice(&20u32.to_le_bytes());
        drop[16..20].copy_from_slice(&1u32.to_le_bytes());
        for path in paths {
            drop.extend(text(path));
        }
        drop.extend([0, 0]);
        let src = source(vec![
            (15, drop),
            (crate::win::png_format(), vec![1]),
            (13, text("fallback")),
        ]);
        let reads = src.reads.clone();
        let released = src.released.clone();
        let object: IDataObject = src.into();
        let LocalSnapshot::Files(actual) = read_object(&object, true).unwrap() else {
            panic!("expected paths");
        };
        assert_eq!(actual, paths.map(std::path::PathBuf::from));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(released.load(Ordering::SeqCst), 1);
        let empty: IDataObject = source(vec![]).into();
        assert!(matches!(
            read_object(&empty, true),
            Ok(LocalSnapshot::Other)
        ));
    }

    #[test]
    fn stopped_reader_returns_error_without_waiting_for_deadline() {
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        assert!(matches!(
            Reader(tx).read(true),
            Err(ClipboardError::Read(_))
        ));
    }

    #[test]
    fn blocked_reader_has_bounded_queue_and_skips_timed_out_requests() {
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = Reader(tx);
        let (entered, started) = mpsc::sync_channel(1);
        let (resume, wait) = mpsc::sync_channel(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let worker = std::thread::spawn(move || {
            serve(rx, |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                entered.send(()).unwrap();
                wait.recv().unwrap();
                Ok(LocalSnapshot::Other)
            })
        });
        let (reply, response) = mpsc::sync_channel(1);
        reader
            .0
            .send(Request {
                origin: true,
                deadline: Instant::now() + Duration::from_secs(5),
                reply,
            })
            .unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            reader.read_timeout(true, Duration::from_millis(20)),
            Err(ClipboardError::Read(_))
        ));
        assert!(matches!(
            reader.read_timeout(true, Duration::from_millis(20)),
            Err(ClipboardError::Open)
        ));
        resume.send(()).unwrap();
        assert!(matches!(
            response.recv_timeout(Duration::from_secs(5)).unwrap(),
            Ok(LocalSnapshot::Other)
        ));
        drop(reader);
        worker.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
