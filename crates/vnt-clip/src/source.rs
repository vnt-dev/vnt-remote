//! Immutable metadata snapshot and source handles for native clipboard reads.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use vnt_proto::pb::{ClipboardOffer, NativeFileItem};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};

type Identity = (u32, u32, u32);
#[derive(Debug)]
struct Entry {
    path: PathBuf,
    identity: Identity,
}
#[derive(Debug)]
pub struct SourceOffer {
    pub manifest: ClipboardOffer,
    entries: Vec<Entry>,
}
fn identity(file: &File) -> Result<Identity, String> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file owns a live Windows handle and info is writable.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(|e| e.to_string())?;
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}
fn metadata_handle(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .access_mode(0)
        .share_mode(7)
        .custom_flags(0x02200000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}
impl SourceOffer {
    pub fn collect(offer_id: u32, roots: &[PathBuf]) -> Result<Self, String> {
        Self::collect_checked(offer_id, roots, &|| true)
    }
    pub fn collect_checked(
        offer_id: u32,
        roots: &[PathBuf],
        current: &dyn Fn() -> bool,
    ) -> Result<Self, String> {
        let mut out = Self {
            manifest: ClipboardOffer {
                offer_id,
                ..Default::default()
            },
            entries: Vec::new(),
        };
        for root in roots {
            let name = root
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("文件名不是有效的 Unicode")?;
            out.visit(root, name.to_string(), current)?;
        }
        if !vnt_proto::validation::valid_clipboardoffer(&out.manifest) {
            return Err("文件清单超限、路径冲突或文件名不受 Windows 虚拟文件格式支持".into());
        }
        Ok(out)
    }
    fn visit(
        &mut self,
        path: &Path,
        name: String,
        current: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        if !current() {
            return Err("剪贴板清单已替换".into());
        }
        if self.entries.len() >= vnt_proto::validation::MAX_CLIPBOARD_ITEMS
            || name.encode_utf16().count() >= 260
        {
            return Err("文件数量或相对路径超过原生剪贴板上限".into());
        }
        let file = metadata_handle(path)?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if meta.file_attributes() & 0x400 != 0 || (!meta.is_dir() && !meta.is_file()) {
            return Err(format!("不支持链接或特殊文件: {}", path.display()));
        }
        let directory = meta.is_dir();
        let size = if directory { 0 } else { meta.len() };
        self.manifest.total_bytes = self
            .manifest
            .total_bytes
            .checked_add(size)
            .ok_or("文件大小溢出")?;
        self.entries.push(Entry {
            path: path.to_path_buf(),
            identity: identity(&file)?,
        });
        self.manifest.items.push(NativeFileItem {
            rel_path: name.clone(),
            size,
            directory,
            attributes: meta.file_attributes() & (0x1 | 0x2 | 0x4 | 0x10 | 0x20 | 0x80),
            creation_time: meta.creation_time(),
            write_time: meta.last_write_time(),
        });
        drop(file);
        if directory {
            let mut children = std::fs::read_dir(path)
                .map_err(|e| e.to_string())?
                .map(|r| r.map(|e| e.path()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            children.sort();
            for child in children {
                let component = child
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or("文件名不是有效的 Unicode")?;
                self.visit(&child, format!("{name}/{component}"), current)?;
            }
        }
        Ok(())
    }
    fn open(&self, index: u32) -> Result<File, String> {
        let entry = self.entries.get(index as usize).ok_or("文件索引无效")?;
        let item = &self.manifest.items[index as usize];
        if item.directory {
            return Err("目录没有文件数据流".into());
        }
        // Keep a readable handle without blocking other readers, writers or rename/delete.
        // The manifest is checked once below; concurrent edits are not a content snapshot.
        let file = OpenOptions::new()
            .read(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .custom_flags(0x00200000)
            .open(&entry.path)
            .map_err(|e| format!("无法读取源文件（可能被占用）: {e}"))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if identity(&file)? != entry.identity
            || meta.len() != item.size
            || meta.last_write_time() != item.write_time
            || meta.file_attributes() & 0x400 != 0
        {
            return Err("源文件已变化，请重新复制".into());
        }
        Ok(file)
    }
}
/// An operation owns shared source handles; releasing it always closes every file.
pub struct SourceOperation {
    pub offer: std::sync::Arc<SourceOffer>,
    files: HashMap<u32, File>,
}
impl SourceOperation {
    pub fn new(offer: std::sync::Arc<SourceOffer>) -> Self {
        Self {
            offer,
            files: HashMap::new(),
        }
    }
    pub fn read(&mut self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>, String> {
        if length == 0 || length as usize > vnt_proto::control::CLIPBOARD_CHUNK_BYTES {
            return Err("读取长度无效".into());
        }
        let item = self
            .offer
            .manifest
            .items
            .get(index as usize)
            .ok_or("文件索引无效")?;
        if offset > item.size {
            return Err("读取偏移越界".into());
        }
        let count = (length as u64).min(item.size - offset) as usize;
        if !self.files.contains_key(&index) {
            self.files.insert(index, self.offer.open(index)?);
        }
        let file = self.files.get_mut(&index).unwrap();
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut bytes = vec![0; count];
        file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        Ok(bytes)
    }
    pub fn close(&mut self, index: u32) {
        self.files.remove(&index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    fn writable(path: &Path) -> bool {
        OpenOptions::new().write(true).open(path).is_ok()
    }
    fn can_open_exclusively(path: &Path) -> bool {
        OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .is_ok()
    }
    #[test]
    fn directories_empty_files_and_handle_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("资料");
        std::fs::create_dir_all(root.join("空目录")).unwrap();
        let file = root.join("a.bin");
        std::fs::write(&file, b"abcdef").unwrap();
        std::fs::write(root.join("empty"), []).unwrap();
        let offer = Arc::new(SourceOffer::collect(1, &[root]).unwrap());
        assert_eq!(offer.manifest.items.len(), 4);
        let index = offer
            .manifest
            .items
            .iter()
            .position(|i| i.rel_path.ends_with("a.bin"))
            .unwrap() as u32;
        assert!(writable(&file), "copying metadata must not lock the source");
        let mut op = SourceOperation::new(offer);
        assert_eq!(op.read(index, 3, 3).unwrap(), b"def");
        assert!(writable(&file), "reading must allow concurrent writers");
        assert!(!can_open_exclusively(&file));
        assert_eq!(op.read(index, 1, 3).unwrap(), b"bcd");
        assert!(op.read(index, 7, 1).is_err());
        assert_eq!(op.read(index, 6, 1).unwrap(), b"");
        assert!(
            !can_open_exclusively(&file),
            "EOF must not release the source before the stream ends"
        );
        op.close(index);
        assert!(can_open_exclusively(&file));
        assert_eq!(op.read(index, 0, 1).unwrap(), b"a");
        drop(op);
        assert!(can_open_exclusively(&file));
    }
    #[test]
    fn concurrent_writer_edits_append_and_truncation_are_not_blocked() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, b"abcdef").unwrap();
        // An existing writer that shares reads must not prevent the transfer from opening.
        let mut writer = OpenOptions::new().write(true).open(&path).unwrap();
        let offer = Arc::new(SourceOffer::collect(1, std::slice::from_ref(&path)).unwrap());
        let mut op = SourceOperation::new(offer);
        assert_eq!(op.read(0, 0, 3).unwrap(), b"abc");
        writer.seek(SeekFrom::Start(3)).unwrap();
        writer.write_all(b"XYZtail").unwrap();
        writer.flush().unwrap();
        assert_eq!(op.read(0, 3, 32).unwrap(), b"XYZ");
        assert_eq!(op.read(0, 6, 1).unwrap(), b"", "manifest size stays fixed");
        writer.set_len(2).unwrap();
        assert!(
            op.read(0, 0, 6).is_err(),
            "truncation must not return success"
        );
        drop(writer);
        op.close(0);
        assert!(can_open_exclusively(&path));
        assert!(op.read(0, 0, 1).unwrap_err().contains("重新复制"));
    }
    #[test]
    fn rename_and_delete_do_not_interrupt_an_open_source_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        let renamed = dir.path().join("moved.bin");
        std::fs::write(&path, b"abcdef").unwrap();
        let offer = Arc::new(SourceOffer::collect(1, std::slice::from_ref(&path)).unwrap());
        let mut op = SourceOperation::new(offer.clone());
        assert_eq!(op.read(0, 0, 1).unwrap(), b"a");
        std::fs::rename(&path, &renamed).unwrap();
        assert_eq!(op.read(0, 1, 2).unwrap(), b"bc");
        assert!(SourceOperation::new(offer).read(0, 0, 1).is_err());
        std::fs::remove_file(&renamed).unwrap();
        assert_eq!(op.read(0, 3, 3).unwrap(), b"def");
        op.close(0);
        assert!(op.read(0, 0, 1).is_err());
    }
    #[test]
    fn changed_or_replaced_source_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"abc").unwrap();
        let offer = Arc::new(SourceOffer::collect(1, std::slice::from_ref(&file)).unwrap());
        std::fs::rename(&file, dir.path().join("old.bin")).unwrap();
        std::fs::write(&file, b"abc").unwrap();
        let mut operation = SourceOperation::new(offer);
        assert!(operation.read(0, 0, 3).unwrap_err().contains("重新复制"));
        let offer = Arc::new(SourceOffer::collect(2, std::slice::from_ref(&file)).unwrap());
        std::fs::write(&file, b"different").unwrap();
        assert!(SourceOperation::new(offer).read(0, 0, 1).is_err());
    }
    #[test]
    fn unsupported_paths_and_stale_enumeration_fail_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, []).unwrap();
        assert!(SourceOffer::collect_checked(1, std::slice::from_ref(&file), &|| false).is_err());
        assert!(SourceOffer::collect(1, &[file.clone(), file]).is_err());
    }
}
