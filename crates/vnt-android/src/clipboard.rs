use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, Weak,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vnt_client::clipboard::*;
use vnt_proto::{control::ControlMessage, pb, EventSink};
static NEXT: AtomicU64 = AtomicU64::new(1);
struct Entry {
    key: u64,
    enabled: bool,
    online: bool,
    cancel: CancellationToken,
    sink: Arc<dyn EventSink>,
    _spec: SessionSpec,
}
pub struct TextClipboard {
    entry: Mutex<Option<Entry>>,
    text: Mutex<Option<String>>,
    foreground: AtomicBool,
}
impl TextClipboard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entry: Mutex::new(None),
            text: Mutex::new(None),
            foreground: AtomicBool::new(true),
        })
    }
    pub fn foreground(&self, visible: bool) {
        self.foreground.store(visible, Ordering::Release);
    }
    pub fn latest(&self) -> Option<String> {
        self.text.lock().unwrap().clone()
    }
    pub fn clear(&self) {
        *self.text.lock().unwrap() = None;
    }
}
struct Registration {
    service: Weak<TextClipboard>,
    key: u64,
}
impl ClipboardRegistration for Registration {
    fn key(&self) -> u64 {
        self.key
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(service) = self.service.upgrade() {
            let mut e = service.entry.lock().unwrap();
            if e.as_ref().is_some_and(|e| e.key == self.key) {
                if let Some(e) = e.take() {
                    e.cancel.cancel();
                }
            }
        }
    }
}
impl ClipboardService for TextClipboard {
    fn capabilities(&self) -> u32 {
        vnt_proto::CAP_TEXT
    }
    fn is_enabled(&self) -> bool {
        true
    }
    fn session_enabled(&self, key: u64) -> bool {
        self.entry
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|e| e.key == key && e.enabled)
    }
    fn set_session_enabled(&self, key: u64, enabled: bool) -> bool {
        let mut e = self.entry.lock().unwrap();
        if let Some(e) = e.as_mut().filter(|e| e.key == key) {
            e.enabled = enabled;
            true
        } else {
            false
        }
    }
    fn transport_changed(&self, _: u64, _: bool) {}
    fn control_changed(&self, key: u64, available: bool) {
        if let Some(e) = self.entry.lock().unwrap().as_mut().filter(|e| e.key == key) {
            e.online = available;
        }
    }
    fn register(self: Arc<Self>, spec: SessionSpec) -> Box<dyn ClipboardRegistration> {
        self.clear();
        let key = NEXT.fetch_add(1, Ordering::Relaxed);
        *self.entry.lock().unwrap() = Some(Entry {
            key,
            enabled: spec.enabled && spec.capabilities & vnt_proto::CAP_TEXT != 0,
            online: true,
            cancel: spec.session_cancel.clone(),
            sink: spec.sink.clone(),
            _spec: spec,
        });
        Box::new(Registration {
            service: Arc::downgrade(&self),
            key,
        })
    }
    fn dispatch<'a>(
        &'a self,
        key: u64,
        _: mpsc::Sender<ControlMessage>,
        message: ControlMessage,
    ) -> Dispatch<'a> {
        Box::pin(async move {
            if let ControlMessage::ClipboardText(pb::ClipboardText { text }) = message {
                if text.len() > vnt_proto::control::MAX_CLIPBOARD_TEXT {
                    return Err("剪贴板文本过大".into());
                }
                let sink = {
                    let e = self.entry.lock().unwrap();
                    e.as_ref()
                        .filter(|e| {
                            e.key == key && e.enabled && e.online && !e.cancel.is_cancelled()
                        })
                        .map(|e| e.sink.clone())
                };
                if let Some(sink) = sink {
                    *self.text.lock().unwrap() = Some(text.clone());
                    if self.foreground.load(Ordering::Acquire) {
                        sink.emit_json(
                            "client://clipboard",
                            serde_json::json!({"text":text}).to_string(),
                        );
                    }
                }
            }
            Ok(())
        })
    }
}
