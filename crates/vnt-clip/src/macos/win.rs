//! NSPasteboard implementation of the shared native clipboard interface.
use crate::{ClipboardContent, ClipboardError, ClipboardResult};
use base64::{engine::general_purpose::STANDARD, Engine};
pub enum LocalSnapshot {
    Image(RawImage),
    Text(String),
    Files(Vec<std::path::PathBuf>),
    Other,
    Ours,
}
pub enum RawImage {
    Png(Vec<u8>),
    Dib(Vec<u8>),
}
impl RawImage {
    pub fn decode(self) -> ClipboardResult<crate::image::ClipboardImage> {
        match self {
            Self::Png(b) => crate::image::from_png(b),
            Self::Dib(b) => crate::image::from_dib(&b),
        }
    }
}
pub fn sequence_number() -> u32 {
    vnt_macos::query(4)
        .ok()
        .and_then(|v| v.as_u64())
        .unwrap_or_default() as u32
}
pub fn snapshot() -> ClipboardResult<LocalSnapshot> {
    let value = vnt_macos::query(5).map_err(ClipboardError::Read)?;
    Ok(match value["kind"].as_str() {
        Some("ours") => LocalSnapshot::Ours,
        Some("text") => LocalSnapshot::Text(value["text"].as_str().unwrap_or_default().into()),
        Some("image") => LocalSnapshot::Image(RawImage::Png(
            STANDARD
                .decode(value["data"].as_str().unwrap_or_default())
                .map_err(|e| ClipboardError::Read(e.to_string()))?,
        )),
        _ => LocalSnapshot::Other,
    })
}
pub fn read() -> ClipboardResult<Option<ClipboardContent>> {
    Ok(match snapshot()? {
        LocalSnapshot::Text(t) => Some(ClipboardContent::Text(t)),
        LocalSnapshot::Image(i) => Some(ClipboardContent::Image(i.decode()?)),
        _ => None,
    })
}
pub fn set_text(text: &str) -> ClipboardResult<()> {
    if text.len() > vnt_proto::control::MAX_CLIPBOARD_TEXT {
        return Err(ClipboardError::TooLarge("文本超过 4 MiB".into()));
    }
    vnt_macos::json(
        6,
        &serde_json::json!({"sequence":sequence_number(),"text":text}),
    )
    .map_err(ClipboardError::Write)?;
    Ok(())
}
