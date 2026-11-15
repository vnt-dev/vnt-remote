//! Shared Linux desktop session checks. No native dependencies on other targets.
#![cfg(target_os = "linux")]
pub use x11rb;
pub mod session;
