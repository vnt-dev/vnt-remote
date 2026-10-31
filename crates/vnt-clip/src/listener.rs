//! Message-only window owned by the native clipboard STA. Window callbacks only
//! coalesce notifications; clipboard reads and network work happen elsewhere.
use std::sync::{mpsc::SyncSender, OnceLock};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, RemoveClipboardFormatListener,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub(crate) struct ClipboardListener {
    hwnd: HWND,
    // The allocation must outlive the window's GWLP_USERDATA pointer.
    _notifications: Box<SyncSender<()>>,
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: WM_NCCREATE supplies CREATESTRUCTW for this CreateWindowExW.
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }
    if message == WM_CLIPBOARDUPDATE {
        let sender = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const SyncSender<()>;
        // SAFETY: ClipboardListener retains the Box until after DestroyWindow.
        if let Some(sender) = unsafe { sender.as_ref() } {
            // A full slot already represents a pending read of the latest clipboard.
            let _ = sender.try_send(());
        }
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

impl ClipboardListener {
    /// Create and drop on the same thread, whose message loop must be pumped.
    pub(crate) fn new(notifications: SyncSender<()>) -> Result<Self, String> {
        let module = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
        let instance = HINSTANCE(module.0);
        static CLASS: OnceLock<Result<u16, String>> = OnceLock::new();
        CLASS
            .get_or_init(|| {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    hInstance: instance,
                    lpszClassName: w!("vnt-remote.clipboard.listener"),
                    ..Default::default()
                };
                let atom = unsafe { RegisterClassW(&class) };
                if atom == 0 {
                    Err(windows::core::Error::from_win32().to_string())
                } else {
                    Ok(atom)
                }
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let notifications = Box::new(notifications);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("vnt-remote.clipboard.listener"),
                w!(""),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                Some((&*notifications as *const SyncSender<()>).cast()),
            )
        }
        .map_err(|e| e.to_string())?;
        if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
            let _ = unsafe { DestroyWindow(hwnd) };
            return Err(e.to_string());
        }
        Ok(Self {
            hwnd,
            _notifications: notifications,
        })
    }
}

impl Drop for ClipboardListener {
    fn drop(&mut self) {
        // The owner STA destroys its window before releasing the callback state.
        let _ = unsafe { RemoveClipboardFormatListener(self.hwnd) };
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{sync_channel, TryRecvError};

    #[test]
    fn notifications_coalesce_and_window_cleanup_disconnects_receiver() {
        let (tx, rx) = sync_channel(1);
        let listener = ClipboardListener::new(tx).unwrap();
        crate::native::pump();
        while rx.try_recv().is_ok() {}
        for _ in 0..10 {
            unsafe {
                PostMessageW(
                    Some(listener.hwnd),
                    WM_CLIPBOARDUPDATE,
                    WPARAM(0),
                    LPARAM(0),
                )
            }
            .unwrap();
        }
        crate::native::pump();
        assert_eq!(rx.try_recv(), Ok(()));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(listener);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
        // The window class is reusable after an instance has been removed.
        let (tx, _rx) = sync_channel(1);
        drop(ClipboardListener::new(tx).unwrap());
    }

    #[test]
    #[ignore = "uses the real Windows clipboard; run explicitly and serially"]
    fn native_updates_follow_clipboard_changes_and_stop_on_shutdown() {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::{Duration, Instant};
        let _guard = crate::CLIPBOARD_LOCK.lock().unwrap();
        let (native, updates) = crate::native::NativeClipboard::start_with_updates().unwrap();
        while updates.try_recv().is_ok() {}
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match crate::win::set_text("clipboard listener notification") {
                Ok(()) => break,
                Err(crate::ClipboardError::Open) if Instant::now() < deadline => {}
                Err(e) => panic!("cannot publish clipboard test text: {e}"),
            }
        }
        updates
            .recv_timeout(Duration::from_secs(2))
            .expect("WM_CLIPBOARDUPDATE should wake the watcher");
        // No callback owns the native publisher: dropping it unregisters the
        // listener and disconnects the worker even while no copies occur.
        drop(native);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match updates.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => {}
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("clipboard listener did not shut down"),
            }
        }
    }
}
