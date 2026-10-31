mod dxgi;

mod gdi;

mod capture;
pub use capture::*;
mod graphics_capture;
pub use graphics_capture::set_capture_border;

mod monitor;
pub use monitor::*;

pub(crate) mod common;
