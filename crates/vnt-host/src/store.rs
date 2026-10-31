//! User configuration with an opaque Keychain reference. All writes are atomic.
use crate::config::AppConfig;
use serde::{Deserialize, Serialize};
use std::{io::Write, os::unix::fs::PermissionsExt, path::Path};
#[derive(Serialize, Deserialize)]
struct Stored {
    #[serde(flatten)]
    config: AppConfig,
    #[serde(rename = "passwordKeychain")]
    account: String,
}
pub fn load_from(path: &Path) -> anyhow::Result<AppConfig> {
    match load_existing_from(path) {
        Ok(config) => Ok(config),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            let config = AppConfig::initial(&vnt_capture::list_monitors().unwrap_or_default());
            save_at(path, &config)?;
            Ok(config)
        }
        Err(error) => Err(error),
    }
}
pub fn load_existing_from(path: &Path) -> anyhow::Result<AppConfig> {
    let data = std::fs::read(path)?;
    let mut saved: Stored = serde_json::from_slice(&data)?;
    anyhow::ensure!(
        saved.config.password.is_empty() && saved.config.server_secrets_clear(),
        "配置文件不允许存储明文密码"
    );
    if !saved.account.is_empty() {
        let result =
            secret(&serde_json::json!({"account":saved.account})).map_err(anyhow::Error::msg)?;
        let protected = result["password"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Keychain 密码无效"))?
            .to_string();
        let value = protected
            .strip_prefix("vnt-secrets-v1:")
            .ok_or_else(|| anyhow::anyhow!("Invalid Keychain credentials"))?;
        let (password, secrets): (String, crate::config::ServerSecrets) =
            serde_json::from_str(value)?;
        saved.config.password = password;
        saved
            .config
            .restore_server_secrets(secrets)
            .map_err(anyhow::Error::msg)?;
    }
    saved.config.password_configured = !saved.config.password.trim().is_empty();
    saved.config.validate().map_err(anyhow::Error::msg)?;
    Ok(saved.config)
}
pub fn save_at(path: &Path, config: &AppConfig) -> anyhow::Result<()> {
    config.validate().map_err(anyhow::Error::msg)?;
    let mut public = config.clone();
    let server_secrets = public.take_server_secrets();
    let old = std::fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Stored>(&data).ok())
        .map(|s| s.account);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("配置路径无效"))?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    // A fresh account keeps the old config usable if atomic replacement fails.
    let account = if config.password.is_empty() && config.server_secrets_clear() {
        String::new()
    } else {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("随机数失败：{e}"))?;
        let account = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        secret(&serde_json::json!({"account":account,"password":format!("vnt-secrets-v1:{}", serde_json::to_string(&(&config.password, &server_secrets))?)}))
            .map_err(anyhow::Error::msg)?;
        account
    };
    public.password.clear();
    public.password_configured = !config.password.trim().is_empty();
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(&Stored {
        config: public,
        account,
    })?)?;
    file.as_file().sync_all()?;
    let new_account: Stored = serde_json::from_slice(&std::fs::read(file.path())?)?;
    if let Err(e) = file.persist(path) {
        if !new_account.account.is_empty() {
            let _ = secret(&serde_json::json!({"account":new_account.account,"delete":true}));
        }
        return Err(e.into());
    }
    if let Some(account) = old.filter(|a| !a.is_empty()) {
        let _ = secret(&serde_json::json!({"account":account,"delete":true}));
    }
    Ok(())
}

#[cfg(not(test))]
fn secret(value: &serde_json::Value) -> Result<serde_json::Value, String> {
    vnt_macos::json(8, value)
}
#[cfg(test)]
fn secret(value: &serde_json::Value) -> Result<serde_json::Value, String> {
    use std::sync::{Mutex, OnceLock};
    static VALUES: OnceLock<Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();
    let mut values = VALUES.get_or_init(Default::default).lock().unwrap();
    let account = value["account"].as_str().ok_or("account missing")?;
    if value["delete"] == true {
        values.remove(account);
        return Ok(serde_json::json!({}));
    }
    if let Some(password) = value["password"].as_str() {
        values.insert(account.into(), password.into());
        return Ok(serde_json::json!({}));
    }
    values
        .get(account)
        .map(|p| serde_json::json!({"password":p}))
        .ok_or("Keychain entry missing".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiple_profiles_keep_keychain_secrets_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut cfg = AppConfig::default();
        for id in ["a", "b"] {
            cfg.upsert_server(crate::link::LinkConfig {
                id: id.into(),
                name: id.into(),
                mode: "subscription".into(),
                subscription: format!("subscription-{id}"),
                encryption_password: format!("password-{id}"),
                ..Default::default()
            })
            .unwrap();
        }
        save_at(&path, &cfg).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        for secret in [
            "subscription-a",
            "subscription-b",
            "password-a",
            "password-b",
        ] {
            assert!(!raw.contains(secret));
        }
        let mut saved = load_from(&path).unwrap();
        assert!(saved == cfg);
        saved.servers.reverse();
        save_at(&path, &saved).unwrap();
        assert!(load_from(&path).unwrap() == saved);
    }
    #[test]
    fn password_reference_roundtrip_and_corruption_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private/config.json");
        assert!(load_from(&path).unwrap().password.is_empty());
        let cfg = AppConfig {
            password: "secret-password".into(),
            ..Default::default()
        };
        save_at(&path, &cfg).unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(!data.contains("secret-password"));
        assert!(data.contains("passwordKeychain"));
        assert_eq!(load_from(&path).unwrap().password, "secret-password");
        let mut saved: Stored = serde_json::from_str(&data).unwrap();
        saved.account = "missing".into();
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        assert!(load_from(&path).is_err());
        std::fs::write(&path, b"invalid").unwrap();
        assert!(load_from(&path).is_err());
    }
}
