//! Platform clipboard adapter. Desktop implementations retain image/file handling.
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vnt_proto::{control::ControlMessage, EventSink};
pub const OUT_QUEUE: usize = 8;
pub const INBOUND_QUEUE: usize = 16;
pub type InboundClipboard = ControlMessage;
pub type Dispatch<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
pub struct SessionSpec {
    pub capabilities: u32,
    pub session_id: u32,
    pub file_tx: mpsc::Sender<ControlMessage>,
    pub control_tx: mpsc::Sender<ControlMessage>,
    pub in_rx: mpsc::Receiver<ControlMessage>,
    pub session_cancel: CancellationToken,
    pub sink: Arc<dyn EventSink>,
    pub enabled: bool,
}
pub trait ClipboardRegistration: Send + Sync {
    fn key(&self) -> u64;
}
pub trait ClipboardService: Send + Sync + 'static {
    fn capabilities(&self) -> u32;
    fn is_enabled(&self) -> bool;
    fn session_enabled(&self, key: u64) -> bool;
    fn set_session_enabled(&self, key: u64, enabled: bool) -> bool;
    fn transport_changed(&self, key: u64, available: bool);
    fn control_changed(&self, key: u64, available: bool);
    fn register(self: Arc<Self>, spec: SessionSpec) -> Box<dyn ClipboardRegistration>;
    fn dispatch<'a>(
        &'a self,
        key: u64,
        tx: mpsc::Sender<ControlMessage>,
        message: ControlMessage,
    ) -> Dispatch<'a>;
}
