use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
pub trait FileOperation: Send + Sync {
    fn read(&self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>, String>;
    fn close(&self, index: u32);
    fn finish(&self, ok: bool, reason: &str);
}
pub trait FileProvider: Send + Sync {
    fn begin(&self) -> Result<Arc<dyn FileOperation>, String>;
    fn available(&self) -> bool;
}
pub type ImageValidity = Arc<dyn Fn() -> bool + Send + Sync>;
struct ListenerStop(AtomicBool);
impl Drop for ListenerStop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
#[derive(Clone)]
pub struct NativeClipboard {
    _stop: Arc<ListenerStop>,
}
impl NativeClipboard {
    pub fn start() -> Result<Self, String> {
        Self::start_with_updates().map(|(s, _)| s)
    }
    pub fn start_with_updates() -> Result<(Self, mpsc::Receiver<()>), String> {
        let (tx, rx) = mpsc::sync_channel(1);
        let stop = Arc::new(ListenerStop(AtomicBool::new(false)));
        let done = Arc::downgrade(&stop);
        std::thread::Builder::new()
            .name("pasteboard-listener".into())
            .spawn(move || {
                let mut previous = crate::win::sequence_number();
                while done.upgrade().is_some_and(|s| !s.0.load(Ordering::Acquire)) {
                    let sequence = crate::win::sequence_number();
                    if previous != sequence {
                        previous = sequence;
                        if matches!(tx.try_send(()), Err(mpsc::TrySendError::Disconnected(_))) {
                            break;
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok((Self { _stop: stop }, rx))
    }
    pub fn files(
        &self,
        _: Arc<vnt_proto::pb::ClipboardOffer>,
        _: Arc<dyn FileProvider>,
        _: u32,
    ) -> Result<(), String> {
        Err("macOS 首版不支持文件剪贴板".into())
    }
    pub fn image(
        &self,
        image: crate::image::PreparedImage,
        sequence: u32,
        valid: ImageValidity,
    ) -> Result<(), String> {
        vnt_macos::clipboard_image(&image.png, sequence, || valid())
    }

    pub fn text(&self, text: String, sequence: u32) -> Result<(), String> {
        if text.len() > vnt_proto::control::MAX_CLIPBOARD_TEXT {
            return Err("文本超过上限".into());
        }
        vnt_macos::json(6, &serde_json::json!({"sequence":sequence,"text":text}))?;
        Ok(())
    }
}
