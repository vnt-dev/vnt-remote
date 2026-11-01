//! Output contracts keep desktop and Android media ownership outside the session.
use crate::bridge::MediaSink;
use std::sync::Arc;
use vnt_proto::pb;

pub struct Outputs {
    pub client_name: String,
    pub media: Option<Arc<dyn MediaSink>>,
    pub audio: Arc<dyn AudioFactory>,
}
#[derive(Clone, Debug)]
pub struct PlaybackStatus {
    pub available: bool,
    pub reason: String,
    pub buffer_ms: u32,
}
pub trait AudioFactory: Send + Sync + 'static {
    fn start(&self, device: Option<String>) -> Result<Box<dyn AudioPlayback>, String>;
}
pub trait AudioPlayback: Send {
    fn sender(&self) -> Arc<dyn AudioSender>;
}
pub trait AudioSender: Send + Sync {
    fn set_state(&self, state: pb::AudioState);
    fn send(&self, frame: pb::AudioFrame);
    fn status(&self) -> PlaybackStatus;
}
