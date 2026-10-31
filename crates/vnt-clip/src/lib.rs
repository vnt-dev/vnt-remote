//! 本机剪贴板的文本、无损 PNG 图片及文件来源路径。
//! Windows 使用 OLE 按需文件读取；macOS 支持文本和图片。
//! 原生快照只取得顶层路径，文件枚举与读取在剪贴板锁外执行。

pub mod image;
#[cfg(windows)]
mod listener;
#[cfg(windows)]
pub mod native;
#[cfg(target_os = "macos")]
#[path = "macos/native.rs"]
pub mod native;
#[cfg(windows)]
mod snapshot;
#[cfg(windows)]
pub mod source;
#[cfg(target_os = "macos")]
#[path = "macos/source.rs"]
pub mod source;
#[cfg(windows)]
pub mod win;
#[cfg(target_os = "macos")]
#[path = "macos/win.rs"]
pub mod win;

use std::path::PathBuf;

pub use vnt_proto::validation::MAX_CLIPBOARD_ITEMS;

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("打开剪贴板失败（可能被其他程序占用）")]
    Open,
    #[error("剪贴板数据读取失败: {0}")]
    Read(String),
    #[error("剪贴板数据写入失败: {0}")]
    Write(String),
    #[error("剪贴板内容过大: {0}")]
    TooLarge(String),
}

pub type ClipboardResult<T> = Result<T, ClipboardError>;

/// 本机剪贴板内容。文件只保存来源路径，正文由粘贴操作按需读取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Image(image::ClipboardImage),
    Text(String),
    Files(Vec<PathBuf>),
}

/// 真实剪贴板是进程级共享资源：多个测试（含集成测试）同时写会互相
/// 冲掉，因此所有碰剪贴板的测试都必须先拿这把锁。
///
/// 不加 `cfg(test)`：集成测试链接的是未开 test 特性的 crate。
pub static CLIPBOARD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, windows))]
mod windows_clipboard_tests {
    use super::*;
    use std::time::Duration;

    /// 真实剪贴板回读：验证 CF_UNICODETEXT / CF_HDROP 的写入布局。
    ///
    /// 无人值守桌面可能拿不到剪贴板（其他进程占用、无交互会话），
    /// 此时跳过而不是失败——布局错误在正常桌面才会暴露。
    #[test]
    fn windows_clipboard_roundtrips_text_and_files() {
        // 真实剪贴板与其他测试共享：串行化
        let _guard = crate::CLIPBOARD_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.bin");
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&second, b"two").unwrap();

        if win::set_text("剪贴板回读 ✅").is_err() {
            eprintln!("跳过：本机剪贴板不可写");
            return;
        }
        // 读取可能被其他进程抢到剪贴板：重试几次
        let mut text = None;
        for _ in 0..20 {
            if let Ok(Some(ClipboardContent::Text(value))) = win::read() {
                text = Some(value);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(text.as_deref(), Some("剪贴板回读 ✅"));
        let maximum = "x".repeat(vnt_proto::control::MAX_CLIPBOARD_TEXT);
        win::set_text(&maximum).unwrap();
        assert!(
            matches!(win::read().unwrap(), Some(ClipboardContent::Text(value)) if value == maximum),
            "4 MiB ASCII text must not be truncated to a UTF-16 byte bound"
        );

        if win::set_files(&[first.clone(), second.clone()]).is_err() {
            eprintln!("跳过：本机剪贴板不可写（文件）");
            return;
        }
        let mut files = None;
        for _ in 0..20 {
            if let Ok(Some(ClipboardContent::Files(paths))) = win::read() {
                files = Some(paths);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let paths = files.expect("写入 CF_HDROP 后应能读回文件列表");
        // 多文件列表：顺序与路径都必须原样回来
        assert_eq!(paths, vec![first, second]);
        // CF_HDROP 保留顶层目录，文件枚举由按需读取源执行
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("c.txt"), b"three").unwrap();
        assert!(win::set_files(std::slice::from_ref(&nested)).is_ok());
        let mut back = None;
        for _ in 0..20 {
            if let Ok(Some(ClipboardContent::Files(paths))) = win::read() {
                back = Some(paths);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let paths = back.expect("目录写入后应能读回");
        assert_eq!(paths, vec![nested]);
    }
}
