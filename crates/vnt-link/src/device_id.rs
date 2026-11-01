use anyhow::Context;
use std::{io::Write, path::Path};

/// Publish once, so concurrent launches always converge on the same identity.
pub fn load_or_create(path: &Path) -> anyhow::Result<String> {
    fn read(path: &Path) -> anyhow::Result<String> {
        let id = std::fs::read_to_string(path)?;
        let id = id.trim();
        uuid::Uuid::parse_str(id).context("device_id 文件中的 UUID 无效")?;
        Ok(id.to_string())
    }
    match read(path) {
        Ok(id) => return Ok(id),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let parent = path.parent().context("device_id 路径无效")?;
    std::fs::create_dir_all(parent)?;
    let id = uuid::Uuid::new_v4().to_string();
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(id.as_bytes())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(path) {
        Ok(_) => Ok(id),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => read(path),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_restarts_and_is_independent_of_other_directories() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join("installed/device_id");
        let portable = root.path().join("data/portable/device_id");
        let id = load_or_create(&installed).unwrap();
        assert_eq!(uuid::Uuid::parse_str(&id).unwrap().get_version_num(), 4);
        assert_eq!(id, load_or_create(&installed).unwrap());
        assert_ne!(id, load_or_create(&portable).unwrap());
        std::fs::write(&installed, "invalid-existing-id").unwrap();
        assert!(load_or_create(&installed).is_err());
        assert_eq!(
            std::fs::read_to_string(installed).unwrap(),
            "invalid-existing-id"
        );
    }

    #[test]
    fn concurrent_first_launches_keep_one_uuid() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("device_id");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || load_or_create(&path).unwrap())
            })
            .collect();
        let ids: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(ids.iter().all(|id| id == &ids[0]));
        assert_eq!(ids[0], std::fs::read_to_string(path).unwrap());
    }
}
