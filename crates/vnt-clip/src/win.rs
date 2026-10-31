//! Windows 剪贴板实现：文本、文件、PNG / DIB 位图。
//!
//! 关键约束：
//! - `OpenClipboard` 可能被其他进程占用（资源管理器拖拽、Office 等），
//!   需短重试；失败按"本次变化跳过"处理，不能崩监听循环。
//! - `SetClipboardData` 成功后句柄归系统所有，**不得**再释放；
//!   失败才需要自己 `GlobalFree`。
//! - `EmptyClipboard` 之后再 `SetClipboardData`，中间不要 await/yield
//!   （本实现全程同步，天然满足）。

use std::path::PathBuf;
use std::time::Duration;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, POINT};
#[cfg(test)]
use windows::Win32::System::DataExchange::GetClipboardData;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use crate::{ClipboardContent, ClipboardError, ClipboardResult};

const CF_HDROP_FORMAT: u32 = CF_HDROP.0 as u32;
const CF_UNICODETEXT_FORMAT: u32 = CF_UNICODETEXT.0 as u32;

/// 打开剪贴板（带短重试）。
pub fn open() -> ClipboardResult<()> {
    for attempt in 0..20 {
        // SAFETY: 传入 None 表示与当前任务关联；成功时下方必然 CloseClipboard。
        if unsafe { OpenClipboard(None) }.is_ok() {
            return Ok(());
        }
        // 占用方通常只持有几毫秒：首次让出 2ms，之后退避到 5ms
        std::thread::sleep(Duration::from_millis(if attempt == 0 { 2 } else { 5 }));
    }
    Err(ClipboardError::Open)
}

/// 当前剪贴板内容序列号：每次内容变化递增（含我们自己写入的）。
pub fn sequence_number() -> u32 {
    // SAFETY: 纯查询，无副作用
    unsafe { GetClipboardSequenceNumber() }
}

/// 读取剪贴板；文件优先于图片，图片优先于文本。
pub fn read() -> ClipboardResult<Option<ClipboardContent>> {
    match crate::snapshot::snapshot(false)? {
        LocalSnapshot::Image(raw) => raw
            .decode()
            .map(|image| Some(ClipboardContent::Image(image))),
        LocalSnapshot::Text(text) => Ok(Some(ClipboardContent::Text(text))),
        LocalSnapshot::Files(paths) => Ok(Some(ClipboardContent::Files(paths))),
        LocalSnapshot::Other | LocalSnapshot::Ours => Ok(None),
    }
}

/// 关闭剪贴板（`open()` 成功后必须调用）。
pub fn close() {
    // SAFETY: 与 open() 配对
    let _ = unsafe { CloseClipboard() };
}

/// 从 `CF_HDROP` 读出顶层文件路径。
pub(crate) fn read_drop_files(handle: HANDLE) -> ClipboardResult<Vec<PathBuf>> {
    if handle.0.is_null() {
        return Err(ClipboardError::Read("剪贴板文件列表为空".into()));
    }
    let drop = HDROP(handle.0);
    // SAFETY: 0xFFFFFFFF 下标 = 查询条目数
    let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
    if count as usize > crate::MAX_CLIPBOARD_ITEMS {
        return Err(ClipboardError::TooLarge("文件数量超过剪贴板上限".into()));
    }
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: None 缓冲区 = 查询所需字符数
        let needed = unsafe { DragQueryFileW(drop, index, None) } as usize;
        if needed > 32767 {
            return Err(ClipboardError::TooLarge(
                "文件绝对路径超过 Windows 上限".into(),
            ));
        }
        let mut buffer = vec![0u16; needed + 1];
        // SAFETY: 缓冲区容量已按查询结果 +1 分配
        let written = unsafe { DragQueryFileW(drop, index, Some(&mut buffer)) } as usize;
        buffer.truncate(written);
        paths.push(PathBuf::from(
            <std::ffi::OsString as std::os::windows::ffi::OsStringExt>::from_wide(&buffer),
        ));
    }
    Ok(paths)
}

/// DROPFILES 结构（windows crate 未导出）：头 + UTF-16 路径列表。
#[repr(C)]
#[derive(Default)]
struct DropFiles {
    /// 头之后到路径列表的字节偏移。
    p_files: u32,
    /// 放下点（剪贴板用途固定为 0,0）。
    pt: POINT,
    /// 是否允许非客户区放下（固定 FALSE）。
    f_nc: i32,
    /// 路径是否为宽字符（固定 TRUE）。
    f_wide: i32,
}

/// 写入文本剪贴板。
pub fn set_text(text: &str) -> ClipboardResult<()> {
    if text.len() > vnt_proto::control::MAX_CLIPBOARD_TEXT {
        return Err(ClipboardError::TooLarge("文本超过 4 MiB".into()));
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();
    let global = alloc_global(bytes)?;
    {
        // SAFETY: 容量按字节数精确分配
        let ptr = unsafe { GlobalLock(global) } as *mut u16;
        if ptr.is_null() {
            free_global(global);
            return Err(ClipboardError::Write("锁定文本内存失败".into()));
        }
        // SAFETY: 目标容量来自同一 wide 向量
        unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len()) };
        // SAFETY: 配对 GlobalLock
        let _ = unsafe { GlobalUnlock(global) };
    }
    set_clipboard(CF_UNICODETEXT_FORMAT, global)
}

/// 写入文件剪贴板：把 `paths` 组成一个 `CF_HDROP`（可含多个文件）。
pub fn set_files(paths: &[PathBuf]) -> ClipboardResult<()> {
    if paths.is_empty() {
        return Err(ClipboardError::Write("没有要写入剪贴板的文件".into()));
    }
    // 双 NUL 结尾的路径列表
    let mut text: Vec<u16> = Vec::new();
    for path in paths {
        let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().collect();
        text.extend(wide);
        text.push(0);
    }
    text.push(0);
    let total = std::mem::size_of::<DropFiles>() + text.len() * std::mem::size_of::<u16>();
    let global = alloc_global(total)?;
    {
        // SAFETY: 容量按结构体 + 路径列表精确分配
        let base = unsafe { GlobalLock(global) };
        if base.is_null() {
            free_global(global);
            return Err(ClipboardError::Write("锁定文件列表内存失败".into()));
        }
        let header = DropFiles {
            p_files: std::mem::size_of::<DropFiles>() as u32,
            pt: POINT { x: 0, y: 0 },
            f_nc: 0,
            f_wide: 1,
        };
        // SAFETY: base 指向足够容纳头部的可写内存
        unsafe { std::ptr::write(base as *mut DropFiles, header) };
        // SAFETY: 头部之后紧接着足够容纳路径列表的空间
        unsafe {
            std::ptr::copy_nonoverlapping(
                text.as_ptr(),
                (base as *mut u8).add(std::mem::size_of::<DropFiles>()) as *mut u16,
                text.len(),
            )
        };
        // SAFETY: 配对 GlobalLock
        let _ = unsafe { GlobalUnlock(global) };
    }
    set_clipboard(CF_HDROP_FORMAT, global)
}

fn alloc_global(bytes: usize) -> ClipboardResult<HGLOBAL> {
    // SAFETY: GMEM_MOVEABLE 可移动内存，大小由调用方按内容精确计算
    unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes) }
        .map_err(|e| ClipboardError::Write(format!("分配剪贴板内存失败: {e}")))
}

/// 打开剪贴板 → 清空 → 写入一个格式的数据。
fn set_clipboard(format: u32, global: HGLOBAL) -> ClipboardResult<()> {
    if let Err(error) = open() {
        free_global(global);
        return Err(error);
    }
    // SAFETY: 剪贴板已打开
    if unsafe { EmptyClipboard() }.is_err() {
        // SAFETY: 配对 open()
        let _ = unsafe { CloseClipboard() };
        free_global(global);
        return Err(ClipboardError::Write("清空剪贴板失败".into()));
    }
    // SAFETY: global 是刚分配的 GMEM_MOVEABLE 内存；成功后归系统所有，
    // 失败时由 free_global 释放（SetClipboardData 失败不取得所有权）。
    let written = unsafe { SetClipboardData(format, Some(HANDLE(global.0))) };
    let result = match written {
        Ok(_) => {
            // A private marker lets the listener ignore our own writes without hashing contents.
            if let Ok(marker) = alloc_global(4) {
                if unsafe { SetClipboardData(origin_format(), Some(HANDLE(marker.0))) }.is_err() {
                    free_global(marker);
                }
            }
            Ok(())
        }
        Err(e) => {
            free_global(global);
            Err(ClipboardError::Write(format!("写入剪贴板失败: {e}")))
        }
    };
    // SAFETY: 配对 open()
    let _ = unsafe { CloseClipboard() };
    result
}

fn free_global(global: HGLOBAL) {
    // SAFETY: 仅在 SetClipboardData 失败后调用（成功时系统已取得所有权）
    let _ = unsafe { GlobalFree(Some(global)) };
}

/// Owned clipboard snapshot; decoding and directory enumeration happen later.
pub enum LocalSnapshot {
    Image(RawImage),
    Text(String),
    Files(Vec<PathBuf>),
    Other,
    Ours,
}
pub fn origin_format() -> u32 {
    unsafe {
        windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!(
            "vnt-remote.clipboard.v1.origin"
        ))
    }
}
pub fn snapshot() -> ClipboardResult<LocalSnapshot> {
    crate::snapshot::snapshot(true)
}

/// Metadata only: the publishing STA must keep pumping messages instead of
/// waiting for the reader STA to call back into its own delayed data object.
pub(crate) fn is_ours() -> ClipboardResult<bool> {
    open()?;
    let ours = unsafe { IsClipboardFormatAvailable(origin_format()) }.is_ok();
    close();
    Ok(ours)
}

pub enum RawImage {
    Png(Vec<u8>),
    Dib(Vec<u8>),
}
impl RawImage {
    pub fn decode(self) -> ClipboardResult<crate::image::ClipboardImage> {
        match self {
            Self::Png(bytes) => crate::image::from_png(bytes),
            Self::Dib(bytes) => crate::image::from_dib(&bytes),
        }
    }
}
pub(crate) fn png_format() -> u32 {
    unsafe {
        windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!("PNG"))
    }
}
/// Convert a bitmap while its owning STGMEDIUM is retained by the caller.
pub(crate) fn read_bitmap(handle: HANDLE) -> ClipboardResult<RawImage> {
    use windows::Win32::Graphics::Gdi::{
        GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
        DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
    };
    let mut bitmap = BITMAP::default();
    if unsafe {
        GetObjectW(
            HGDIOBJ(handle.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bitmap as *mut BITMAP).cast()),
        )
    } == 0
    {
        return Err(ClipboardError::Read("无法读取位图尺寸".into()));
    }
    let (w, h) = (bitmap.bmWidth, bitmap.bmHeight.unsigned_abs());
    if w <= 0 || !vnt_proto::clipboard::valid_dimensions(w as u32, h) {
        return Err(ClipboardError::TooLarge("位图像素大小超限".into()));
    }
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: 40,
            biWidth: w,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bytes = vec![0u8; 40 + w as usize * h as usize * 4];
    let dc = unsafe { GetDC(None) };
    if dc.is_invalid() {
        return Err(ClipboardError::Read("无法读取位图设备".into()));
    }
    let lines = unsafe {
        GetDIBits(
            dc,
            HBITMAP(handle.0),
            0,
            h,
            Some(bytes.as_mut_ptr().add(40).cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        ReleaseDC(None, dc);
    }
    if lines != h as i32 {
        return Err(ClipboardError::Read("位图读取不完整".into()));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&info.bmiHeader as *const BITMAPINFOHEADER).cast::<u8>(),
            bytes.as_mut_ptr(),
            40,
        );
    }
    // GDI's RGB conversion has no defined alpha; make its pixels opaque.
    for pixel in bytes[40..].as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
    Ok(RawImage::Dib(bytes))
}

/// All conversion and allocation precedes EmptyClipboard. The validity guard is
/// checked again with the clipboard open, so a local copy cannot be overwritten.
pub fn set_image(
    image: &crate::image::PreparedImage,
    sequence: u32,
    valid: &dyn Fn() -> bool,
) -> ClipboardResult<()> {
    let mut handles = Vec::new();
    for (format, bytes) in [
        (png_format(), image.png.as_slice()),
        (17, image.dib_v5.as_slice()),
        (8, image.dib.as_slice()),
        (origin_format(), &[0u8; 4][..]),
    ] {
        let allocation = (|| {
            let global = alloc_global(bytes.len())?;
            let ptr = unsafe { GlobalLock(global) } as *mut u8;
            if ptr.is_null() {
                free_global(global);
                return Err(ClipboardError::Write("锁定图片内存失败".into()));
            }
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            }
            let _ = unsafe { GlobalUnlock(global) };
            Ok(global)
        })();
        match allocation {
            Ok(global) => handles.push((format, global)),
            Err(e) => {
                for (_, g) in handles {
                    free_global(g);
                }
                return Err(e);
            }
        }
    }
    let result = (|| {
        open()?;
        let result = (|| {
            if sequence_number() != sequence || !valid() {
                return Err(ClipboardError::Write("图片剪贴板已替换".into()));
            }
            unsafe { EmptyClipboard() }.map_err(|e| ClipboardError::Write(e.to_string()))?;
            for (format, global) in &mut handles {
                unsafe { SetClipboardData(*format, Some(HANDLE(global.0))) }
                    .map_err(|e| ClipboardError::Write(e.to_string()))?;
                *global = HGLOBAL::default(); // Ownership transferred to Windows.
            }
            Ok(())
        })();
        close();
        result
    })();
    for (_, global) in handles {
        if !global.0.is_null() {
            free_global(global);
        }
    }
    result
}

#[cfg(test)]
mod image_tests {
    use super::*;
    #[test]
    #[ignore = "uses the real Windows clipboard; run explicitly and serially"]
    fn native_images_publish_all_formats_and_reject_stale_writes() {
        let _guard = crate::CLIPBOARD_LOCK.lock().unwrap();
        let pixels = [255, 0, 0, 255, 0, 128, 255, 64];
        let image = crate::image::from_rgba(2, 1, &pixels).unwrap();
        let owner = crate::native::NativeClipboard::start().unwrap();
        owner
            .image(
                crate::image::PreparedImage::new(image.clone()).unwrap(),
                sequence_number(),
                std::sync::Arc::new(|| true),
            )
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if matches!(read(), Ok(Some(ClipboardContent::Image(ref actual))) if actual == &image) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native image publication timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(snapshot().unwrap(), LocalSnapshot::Ours));
        open().unwrap();
        for format in [png_format(), 17, 8, origin_format()] {
            assert!(unsafe { IsClipboardFormatAvailable(format) }.is_ok());
        }
        close();
        // Read both bitmap representations, not just the preferred PNG.
        for (v5, format) in [(true, 17), (false, 8)] {
            open().unwrap();
            let handle = unsafe { GetClipboardData(format) }.unwrap();
            let global = HGLOBAL(handle.0);
            let size = unsafe { windows::Win32::System::Memory::GlobalSize(global) };
            let ptr = unsafe { GlobalLock(global) } as *const u8;
            assert!(!ptr.is_null());
            let bytes = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
            let _ = unsafe { GlobalUnlock(global) };
            close();
            let decoded =
                crate::image::decode_png(&crate::image::from_dib(&bytes).unwrap().png).unwrap();
            assert_eq!((decoded.0, decoded.1), (2, 1));
            if v5 {
                assert_eq!(decoded.2, pixels);
            }
        }
        // Simulate an external copier publishing only a DIB (no PNG or marker).
        let prepared = crate::image::PreparedImage::new(image.clone()).unwrap();
        let mut dib16 = vec![0u8; 44];
        dib16[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib16[4..8].copy_from_slice(&1i32.to_le_bytes());
        dib16[8..12].copy_from_slice(&1i32.to_le_bytes());
        dib16[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib16[14..16].copy_from_slice(&16u16.to_le_bytes());
        dib16[40..42].copy_from_slice(&0x7c00u16.to_le_bytes());
        for (format, bytes, dimensions) in [
            (17, prepared.dib_v5, (2, 1)),
            (8, prepared.dib, (2, 1)),
            (8, dib16, (1, 1)),
        ] {
            let global = alloc_global(bytes.len()).unwrap();
            let ptr = unsafe { GlobalLock(global) } as *mut u8;
            assert!(!ptr.is_null());
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            }
            let _ = unsafe { GlobalUnlock(global) };
            open().unwrap();
            unsafe { EmptyClipboard() }.unwrap();
            unsafe { SetClipboardData(format, Some(HANDLE(global.0))) }.unwrap();
            close();
            let LocalSnapshot::Image(raw) = snapshot().unwrap() else {
                panic!("DIB source must be detected");
            };
            let decoded = raw.decode().unwrap();
            assert_eq!((decoded.width, decoded.height), dimensions);
            if dimensions == (1, 1) {
                assert_eq!(
                    crate::image::decode_png(&decoded.png).unwrap().2,
                    [255, 0, 0, 255]
                );
            }
        }
        let old_sequence = sequence_number();
        set_text("new local copy").unwrap();
        assert!(set_image(
            &crate::image::PreparedImage::new(image).unwrap(),
            old_sequence,
            &|| true
        )
        .is_err());
        assert!(
            matches!(read().unwrap(), Some(ClipboardContent::Text(t)) if t == "new local copy")
        );
    }
}
