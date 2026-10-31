//! File output for tracing: 2 MiB per file, current file plus two backups.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn init(data_directory: &Path) -> anyhow::Result<()> {
    init_file(&data_directory.join("logs/vnt-remote.log"))
}

pub fn init_file(path: &Path) -> anyhow::Result<()> {
    let writer = RollingFile::open_file(path)?;
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(Mutex::new(writer))
        .try_init()
        .map_err(|error| anyhow::anyhow!("初始化日志失败: {error}"))?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "应用启动");
    Ok(())
}

// tracing-appender rotates by time only. This writer supplies the size limit
// without a logging worker, queue, or additional rotation dependency.
struct RollingFile {
    directory: PathBuf,
    name: String,
    file: Option<File>,
    bytes: u64,
}

impl RollingFile {
    fn open_file(path: &Path) -> io::Result<Self> {
        let directory = path
            .parent()
            .ok_or_else(|| io::Error::other("日志目录无效"))?
            .to_path_buf();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::other("日志文件名无效"))?
            .to_owned();
        fs::create_dir_all(&directory)?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            directory,
            name,
            file: Some(file),
            bytes,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.take(); // Windows requires closing the file before rename.
        let current = self.directory.join(&self.name);
        let previous = self.directory.join(format!("{}.1", self.name));
        let oldest = self.directory.join(format!("{}.2", self.name));
        ignore_missing(fs::remove_file(&oldest))?;
        ignore_missing(fs::rename(&previous, &oldest))?;
        ignore_missing(fs::rename(&current, &previous))?;
        self.file = Some(OpenOptions::new().create(true).append(true).open(current)?);
        self.bytes = 0;
        Ok(())
    }
}

fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

impl Write for RollingFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        // Move complete normal records to the next file before reaching 2 MiB.
        if self.bytes > 0 && bytes.len() as u64 > MAX_BYTES.saturating_sub(self.bytes) {
            self.rotate()?;
        }
        // Also cap an unusually large single record; write_all continues it.
        let count = bytes.len().min(MAX_BYTES as usize);
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("日志文件不可用"))?;
        let written = file.write(&bytes[..count])?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_logs_rotate_without_modifying_other_process_logs() {
        let directory = tempfile::tempdir().unwrap();
        let service = directory.path().join("vnt-remote.log");
        fs::write(&service, b"service log").unwrap();
        let installer = directory.path().join("vnt-installer.log");
        {
            let mut writer = RollingFile::open_file(&installer).unwrap();
            writer.write_all(&vec![b'x'; MAX_BYTES as usize]).unwrap();
            writer.write_all(b"installer log").unwrap();
            writer.flush().unwrap();
        }
        assert_eq!(fs::read(&service).unwrap(), b"service log");
        assert_eq!(fs::read(&installer).unwrap(), b"installer log");
        assert_eq!(
            fs::metadata(directory.path().join("vnt-installer.log.1"))
                .unwrap()
                .len(),
            MAX_BYTES
        );
        assert!(!directory.path().join("vnt-remote.log.1").exists());
    }

    #[test]
    fn tracing_formats_events_and_rotation_stays_bounded_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        for _ in 0..2 {
            let writer = RollingFile::open_file(&directory.path().join("vnt-remote.log")).unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(Mutex::new(writer))
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                // Enough real records to rotate more than three files.
                let message = "测试日志".repeat(1000);
                for i in 0..800 {
                    tracing::info!(iteration = i, "{message}");
                }
                tracing::warn!("最终日志");
            });
            let paths = fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            assert_eq!(paths.len(), 3);
            for path in paths {
                assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES);
                assert!(fs::read_to_string(path).unwrap().contains("INFO"));
            }
            assert!(fs::read_to_string(directory.path().join("vnt-remote.log"))
                .unwrap()
                .contains("最终日志"));
        }
    }
}
