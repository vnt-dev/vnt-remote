pub mod clipboard;
mod color_depth;
pub mod config;
pub mod link;
pub mod logging;
pub mod region;
pub mod resolution;
pub mod screens;
pub mod server;
#[cfg(windows)]
pub mod windows;
pub use vnt_proto::{EventSink, NoopEventSink};

pub mod status;
#[cfg(windows)]
pub use windows::store;
#[cfg(target_os = "macos")]
pub mod store;
