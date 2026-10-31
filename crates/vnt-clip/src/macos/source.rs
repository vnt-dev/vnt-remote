//! File clipboard is intentionally unavailable on macOS v1.
use std::{path::PathBuf, sync::Arc};
pub struct SourceOffer {
    pub manifest: vnt_proto::pb::ClipboardOffer,
}
impl SourceOffer {
    pub fn collect(_: u32, _: &[PathBuf]) -> Result<Self, String> {
        Err("macOS 首版不支持文件剪贴板".into())
    }
    pub fn collect_checked(_: u32, _: &[PathBuf], _: &dyn Fn() -> bool) -> Result<Self, String> {
        Err("macOS 首版不支持文件剪贴板".into())
    }
}
pub struct SourceOperation {
    pub offer: Arc<SourceOffer>,
}
impl SourceOperation {
    pub fn new(offer: Arc<SourceOffer>) -> Self {
        Self { offer }
    }
    pub fn read(&mut self, _: u32, _: u64, _: u32) -> Result<Vec<u8>, String> {
        Err("macOS 首版不支持文件剪贴板".into())
    }
    pub fn close(&mut self, _: u32) {}
}
