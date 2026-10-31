pub mod clipboard;
mod color_depth;
pub mod config;
mod device_id;
pub mod link;
pub mod logging;
pub mod region;
pub mod resolution;
pub mod screens;
pub mod server;
#[cfg(windows)]
pub mod windows;
pub trait EventSink: Send + Sync + 'static {
    fn emit_json(&self, event: &str, payload_json: String);
}
pub struct NoopEventSink;
impl EventSink for NoopEventSink {
    fn emit_json(&self, _: &str, _: String) {}
}

pub mod status;
#[cfg(windows)]
pub use windows::store;
#[cfg(target_os = "macos")]
pub mod store;
