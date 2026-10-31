pub use vnt_host::config::AppConfig;

pub const INSTALLED: bool = cfg!(feature = "installed");

pub struct ConfigStore {
    portable_path: std::path::PathBuf,
}

impl ConfigStore {
    pub fn new(app_data: &std::path::Path) -> Self {
        Self {
            portable_path: app_data.join("portable").join("config.json"),
        }
    }

    pub fn portable_path(&self) -> std::path::PathBuf {
        self.portable_path.clone()
    }

    pub fn load(&self) -> Result<AppConfig, String> {
        if INSTALLED {
            return Err("安装版配置必须通过本地服务读取".into());
        }
        vnt_host::store::load_from(&self.portable_path).map_err(|e| format!("{e:#}"))
    }

    pub fn save(&self, cfg: &AppConfig) -> Result<(), String> {
        if INSTALLED {
            return Err("安装版配置必须通过本地服务保存".into());
        }
        vnt_host::store::save_at(&self.portable_path, cfg).map_err(|e| format!("{e:#}"))
    }
}

#[cfg(all(test, feature = "installed"))]
mod installed_tests {
    use super::*;
    #[test]
    fn installed_store_never_falls_back_to_direct_config_access() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(directory.path());
        assert!(store
            .load()
            .err()
            .expect("direct installed reads must fail")
            .contains("通过本地服务读取"));
        assert!(store
            .save(&AppConfig::default())
            .unwrap_err()
            .contains("通过本地服务保存"));
        assert!(!store.portable_path().exists());
    }
}

#[cfg(all(test, windows, not(feature = "installed")))]
mod tests {
    use super::*;

    #[test]
    fn portable_store_creates_user_config_without_resolving_installed_service() {
        let user = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(user.path());
        assert!(store.load().unwrap().password.is_empty());
        let cfg = AppConfig {
            password: "portable-user-password".into(),
            port: 30123,
            ..Default::default()
        };
        store.save(&cfg).unwrap();
        let reloaded = ConfigStore::new(user.path()).load().unwrap();
        assert_eq!(reloaded.password, cfg.password);
        assert_eq!(reloaded.port, cfg.port);
        assert!(reloaded.password_configured);
        let data = std::fs::read_to_string(store.portable_path()).unwrap();
        assert!(!data.contains(&cfg.password));
        let another_user = tempfile::tempdir().unwrap();
        assert!(ConfigStore::new(another_user.path())
            .load()
            .unwrap()
            .password
            .is_empty());
        std::fs::write(store.portable_path(), "corrupted config").unwrap();
        assert!(store.load().is_err());
    }
}
