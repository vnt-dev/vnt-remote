use crate::config::AppConfig;
use serde::{Deserialize, Serialize};
use std::{ffi::c_void, path::PathBuf};
use windows::{
    core::PCWSTR,
    Win32::{Foundation::*, Security::Cryptography::*},
};

pub fn directory() -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let exe = if current
        .file_stem()
        .is_some_and(|name| name.eq_ignore_ascii_case("vnt-service"))
        && std::env::args().nth(1).as_deref() != Some("--elevated")
    {
        current
    } else {
        super::management::service_executable()?.unwrap_or(current)
    };
    exe.parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("无法定位被控端安装目录"))
}
fn command(exe: &str, args: &[&str]) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new(super::native::system_executable(exe)?)
        .args(args)
        .creation_flags(0x08000000)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "{exe}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}
fn reject_link(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::windows::fs::MetadataExt;
    if let Ok(m) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(
            m.file_attributes() & 0x400 == 0,
            "配置目录不能是链接或重解析点"
        );
    }
    Ok(())
}
fn set_acl(path: &std::path::Path, public: bool) -> anyhow::Result<()> {
    let sddl = if public {
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)"
    } else {
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    };
    set_sddl_acl(path, sddl)
}
const CONFIG_SDDL: &str = "O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)";
fn set_config_acl(path: &std::path::Path) -> anyhow::Result<()> {
    // Both reads and writes go through the authenticated service control plane.
    set_sddl_acl(path, CONFIG_SDDL)
}
fn set_sddl_acl(path: &std::path::Path, sddl: &str) -> anyhow::Result<()> {
    use windows::Win32::Security::*;
    let sd = super::native::Descriptor::new(sddl)?;
    let ok = unsafe {
        SetFileSecurityW(
            PCWSTR(super::native::wide(path).as_ptr()),
            DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION
                | OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR(sd.attributes().lpSecurityDescriptor),
        )
    };
    if !ok.as_bool() {
        return Err(windows::core::Error::from_win32().into());
    }
    Ok(())
}
pub fn secure_directory() -> anyhow::Result<PathBuf> {
    let d = directory()?;
    reject_link(&d)?;
    std::fs::create_dir_all(&d)?;
    // The install root may be an administrator-chosen directory. Protect only
    // this application's configuration files and private state, not the root.
    reject_link(&d.join("config.json"))?;
    let state = d.join("service-state.json");
    reject_link(&state)?;
    if state.exists() {
        set_acl(&state, true)?;
    }
    let private = d.join("private");
    reject_link(&private)?;
    std::fs::create_dir_all(&private)?;
    set_acl(&private, false)?;
    Ok(d)
}

/// Resolve relative to the executable, never the service's System32 working directory.
pub fn secure_log_directory() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let folder = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("安装目录无效"))?
        .join("logs");
    reject_link(&folder)?;
    std::fs::create_dir_all(&folder)?;
    // Users may read diagnostics; only administrators and SYSTEM may write them.
    set_acl(&folder, true)?;
    for name in ["vnt-remote.log", "vnt-installer.log"] {
        for suffix in ["", ".1", ".2"] {
            let path = folder.join(format!("{name}{suffix}"));
            reject_link(&path)?;
            if path.exists() {
                set_acl(&path, true)?;
            }
        }
    }
    Ok(folder)
}
#[derive(Serialize, Deserialize)]
struct Stored {
    #[serde(flatten)]
    config: AppConfig,
    #[serde(rename = "passwordDpapi")]
    password: Vec<u8>,
    server_secrets: Vec<u8>,
}
fn crypt(bytes: &[u8], protect: bool) -> anyhow::Result<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        if protect {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
        } else {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
        }
        let data = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(out.pbData as *mut c_void)));
        Ok(data)
    }
}
pub fn load() -> anyhow::Result<AppConfig> {
    load_existing_from(&directory()?.join("config.json"))
}

pub fn host_enabled() -> anyhow::Result<bool> {
    let d = secure_directory()?;
    host_enabled_at(&d)
}
fn host_enabled_at(directory: &std::path::Path) -> anyhow::Result<bool> {
    let path = directory.join("private/host-enabled.json");
    match std::fs::read(&path) {
        Ok(data) => Ok(serde_json::from_slice(&data)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            save_host_enabled_at(directory, false)?;
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

pub fn save_host_enabled(enabled: bool) -> anyhow::Result<()> {
    save_host_enabled_at(&secure_directory()?, enabled)
}
fn save_host_enabled_at(directory: &std::path::Path, enabled: bool) -> anyhow::Result<()> {
    use std::io::Write;
    let directory = directory.join("private");
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    file.write_all(&serde_json::to_vec(&enabled)?)?;
    file.as_file().sync_all()?;
    file.persist(directory.join("host-enabled.json"))?;
    Ok(())
}

#[cfg(test)]
mod host_state_tests {
    use super::*;
    #[test]
    fn missing_state_is_disabled_and_config_saves_preserve_choice() {
        for password in ["", "configured"] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::create_dir(directory.path().join("private")).unwrap();
            let config = AppConfig {
                password: password.into(),
                ..Default::default()
            };
            save_at(&directory.path().join("config.json"), &config).unwrap();
            assert_eq!(host_enabled_at(directory.path()).unwrap(), false);
            save_host_enabled_at(directory.path(), false).unwrap();
            save_at(
                &directory.path().join("config.json"),
                &AppConfig {
                    password: "changed".into(),
                    ..config
                },
            )
            .unwrap();
            assert!(!host_enabled_at(directory.path()).unwrap());
            save_host_enabled_at(directory.path(), true).unwrap();
            assert!(host_enabled_at(directory.path()).unwrap());
        }
    }
    #[test]
    fn corrupt_state_fails_closed_and_failed_replace_preserves_original() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("private")).unwrap();
        save_host_enabled_at(directory.path(), true).unwrap();
        let path = directory.path().join("private/host-enabled.json");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert!(save_host_enabled_at(directory.path(), false).is_err());
        drop(lock);
        assert!(host_enabled_at(directory.path()).unwrap());
        std::fs::write(path, b"broken").unwrap();
        assert!(host_enabled_at(directory.path()).is_err());
    }
}
/// 每次新连接认证时读盘；禁止使用启动时缓存的密码或接受空密码。
pub fn check_response(nonce: &[u8; 32], proof: &[u8]) -> anyhow::Result<bool> {
    check_response_from(&directory()?.join("config.json"), nonce, proof)
}
fn check_response_from(
    path: &std::path::Path,
    nonce: &[u8; 32],
    proof: &[u8],
) -> anyhow::Result<bool> {
    let cfg = load_from(path)?;
    Ok(vnt_proto::auth::verify(&cfg.password, nonce, proof))
}
#[cfg(test)]
fn check_password_from(path: &std::path::Path, password: &str) -> anyhow::Result<bool> {
    let nonce = vnt_proto::auth::new_nonce().unwrap();
    check_response_from(path, &nonce, &vnt_proto::auth::response(password, &nonce))
}
pub fn load_from(path: &std::path::Path) -> anyhow::Result<AppConfig> {
    if !path.exists() {
        let monitors = vnt_capture::list_available_monitors().unwrap_or_default();
        let config = AppConfig::initial(&monitors);
        save_at(path, &config)?;
        return Ok(config);
    }
    load_existing_from(path)
}
pub fn load_existing_from(path: &std::path::Path) -> anyhow::Result<AppConfig> {
    use std::io::Read;
    let mut bytes = Vec::new();
    open_config(path, false)?.read_to_end(&mut bytes)?;
    let stored: Stored = serde_json::from_slice(&bytes)?;
    let mut cfg = stored.config;
    cfg.password = String::from_utf8(crypt(&stored.password, false)?)?;
    anyhow::ensure!(cfg.server_secrets_clear(), "服务端配置不允许存储明文凭据");
    let secrets = serde_json::from_slice(&crypt(&stored.server_secrets, false)?)?;
    cfg.restore_server_secrets(secrets)
        .map_err(anyhow::Error::msg)?;
    cfg.password_configured = !cfg.password.trim().is_empty();
    cfg.validate().map_err(anyhow::Error::msg)?;
    Ok(cfg)
}
fn atomic(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut f = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    f.write_all(bytes)?;
    f.as_file().sync_all()?;
    f.persist(path)?;
    Ok(())
}
pub fn save(cfg: AppConfig) -> anyhow::Result<()> {
    write_config(&directory()?.join("config.json"), &encode_config(&cfg)?)
}

/// Called by the serialized service actor, so saving and host startup/stop
/// cannot race the authoritative listener-port check.
pub fn update_config(config: AppConfig, status: &super::management::Status) -> anyhow::Result<()> {
    update_config_at(&directory()?.join("config.json"), config, status)
}

pub(super) fn update_config_at(
    path: &std::path::Path,
    mut config: AppConfig,
    status: &super::management::Status,
) -> anyhow::Result<()> {
    config.validate().map_err(anyhow::Error::msg)?;
    status
        .validate_port_change(config.port)
        .map_err(anyhow::Error::msg)?;
    let previous = load_from(path)?;
    config.apply_password_update(&previous);
    if config != previous {
        write_config(path, &encode_config(&config)?)?;
    }
    Ok(())
}
/// User-owned portable configuration; never resolve through SCM or change ACLs.
pub fn save_at(path: &std::path::Path, cfg: &AppConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("配置目录无效"))?,
    )?;
    if !path.exists() {
        return atomic(path, &encode_config(cfg)?);
    }
    write_config(path, &encode_config(cfg)?)
}
fn encode_config(cfg: &AppConfig) -> anyhow::Result<Vec<u8>> {
    let mut cfg = cfg.clone();
    cfg.validate().map_err(anyhow::Error::msg)?;
    cfg.password_configured = !cfg.password.trim().is_empty();
    let password = crypt(cfg.password.as_bytes(), true)?;
    cfg.password.clear();
    let server_secrets = crypt(&serde_json::to_vec(&cfg.take_server_secrets())?, true)?;
    Ok(serde_json::to_vec_pretty(&Stored {
        config: cfg,
        password,
        server_secrets,
    })?)
}
fn save_to(d: &std::path::Path, cfg: &AppConfig) -> anyhow::Result<()> {
    atomic(&d.join("config.json"), &encode_config(cfg)?)
}
pub fn initialize_config() -> anyhow::Result<()> {
    let directory = secure_directory()?;
    let path = directory.join("config.json");
    if !path.exists() {
        let monitors = vnt_capture::list_available_monitors().unwrap_or_default();
        save_to(&directory, &AppConfig::initial(&monitors))?;
    }
    set_config_acl(&path)
}
fn open_config(path: &std::path::Path, write: bool) -> anyhow::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(write)
            .share_mode(if write { 0 } else { 1 })
            .open(path)
        {
            Ok(file) => return Ok(file),
            Err(error)
                if matches!(error.raw_os_error(), Some(32 | 33))
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "{}配置文件 {} 失败：{error}；请确认已安装最新版本",
                    if write { "写入" } else { "读取" },
                    path.display()
                ))
            }
        }
    }
}
fn write_config(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    // Replacing via a temporary file would require write permission over
    // Program Files itself. Keep the existing file/ACL and hold an exclusive
    // Windows share lock until all bytes are flushed; readers use the same gate.
    let mut file = open_config(path, true)?;
    let mut previous = Vec::new();
    file.read_to_end(&mut previous)?;
    let replace = |file: &mut std::fs::File, data: &[u8]| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(data)?;
        file.set_len(data.len() as u64)?;
        file.sync_all()
    };
    if let Err(error) = replace(&mut file, bytes) {
        if let Err(rollback) = replace(&mut file, &previous) {
            anyhow::bail!("保存配置失败：{error}；恢复原配置失败：{rollback}");
        }
        return Err(error.into());
    }
    Ok(())
}

pub fn service_error() -> anyhow::Result<Option<String>> {
    let path = directory()?.join("service-state.json");
    service_error_from(&path)
}
pub(super) fn service_error_from(path: &std::path::Path) -> anyhow::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(serde_json::from_slice::<Option<String>>(&std::fs::read(
        path,
    )?)?)
}
pub fn save_service_error(error: Option<&str>) -> anyhow::Result<()> {
    let d = secure_directory()?;
    let path = d.join("service-state.json");
    atomic(&path, &serde_json::to_vec(&error)?)?;
    set_acl(&path, true)
}

pub fn firewall(port: u16) -> anyhow::Result<()> {
    remove_firewall()?;
    for protocol in ["TCP", "UDP"] {
        command(
            "netsh.exe",
            &[
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name=VntRemoteService-{protocol}"),
                "dir=in",
                "action=allow",
                &format!("protocol={protocol}"),
                &format!("localport={port}"),
                "profile=private",
                "remoteip=LocalSubnet",
            ],
        )?;
    }
    Ok(())
}
pub fn remove_firewall() -> anyhow::Result<()> {
    for protocol in ["TCP", "UDP"] {
        let _ = command(
            "netsh.exe",
            &[
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name=VntRemoteService-{protocol}"),
            ],
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn password_clear_is_explicit_and_server_credentials_are_protected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        assert!(load_existing_from(&path).is_err());
        let cfg = AppConfig {
            password: "remote-access".into(),
            servers: vec![crate::link::LinkConfig {
                id: "private".into(),
                name: "Private".into(),
                mode: "subscription".into(),
                subscription: "private-subscription".into(),
                encryption_password: "private-encryption".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        save_at(&path, &cfg).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        for secret in [
            "remote-access",
            "private-subscription",
            "private-encryption",
        ] {
            assert!(!raw.contains(secret));
        }
        let mut keep = load_from(&path).unwrap();
        keep.password.clear();
        update_config_at(&path, keep, &super::super::management::Status::default()).unwrap();
        assert_eq!(load_from(&path).unwrap().password, "remote-access");
        let mut clear = load_from(&path).unwrap();
        clear.password.clear();
        clear.password_action = crate::config::PasswordAction::Clear;
        update_config_at(&path, clear, &super::super::management::Status::default()).unwrap();
        let saved = load_from(&path).unwrap();
        assert!(saved.password.is_empty() && !saved.password_configured);
        assert_eq!(saved.servers[0].encryption_password, "private-encryption");
        assert!(vnt_proto::auth::verify_remote(
            &saved.password,
            &[0; 32],
            &[0; 32]
        ));
    }
    #[test]
    fn managed_config_validates_port_and_password_before_writing_and_preserves_host_choice() {
        use super::super::management::Status;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let cfg = AppConfig {
            password: "original".into(),
            password_configured: true,
            port: 31007,
            ..Default::default()
        };
        save_at(&path, &cfg).unwrap();
        std::fs::create_dir(directory.path().join("private")).unwrap();
        save_host_enabled_at(directory.path(), true).unwrap();
        let running = Status {
            running: true,
            state: "running".into(),
            service_state: "running".into(),
            port: cfg.port,
            ..Default::default()
        };
        let changed_port = AppConfig {
            port: cfg.port + 1,
            ..cfg.clone()
        };
        assert!(update_config_at(&path, changed_port.clone(), &running).is_err());
        assert!(load_from(&path).unwrap() == cfg);
        let invalid = AppConfig {
            password: "x".repeat(1025),
            ..cfg.clone()
        };
        assert!(update_config_at(&path, invalid, &running).is_err());
        assert!(load_from(&path).unwrap() == cfg);
        let blank = AppConfig {
            password: String::new(),
            password_configured: false,
            audio: false,
            clipboard: false,
            allow_control: false,
            hardware_encode: false,
            capture_border: true,
            ..cfg.clone()
        };
        update_config_at(&path, blank, &running).unwrap();
        let saved = load_from(&path).unwrap();
        assert_eq!(saved.password, "original");
        assert!(saved.password_configured);
        assert!(!saved.audio);
        assert!(!saved.clipboard);
        assert!(!saved.allow_control);
        assert!(!saved.hardware_encode);
        assert_eq!(saved.monitor_index, 0);
        assert!(saved.capture_border);
        let replacement = AppConfig {
            password: "replacement".into(),
            ..saved.clone()
        };
        update_config_at(&path, replacement, &running).unwrap();
        assert!(check_password_from(&path, "replacement").unwrap());
        assert!(!check_password_from(&path, "original").unwrap());
        let stopped = Status {
            state: "stopped".into(),
            running: false,
            ..running
        };
        update_config_at(&path, changed_port.clone(), &stopped).unwrap();
        assert!(load_from(&path).unwrap() == changed_port);
        assert!(host_enabled_at(directory.path()).unwrap());
        assert!(!std::fs::read_to_string(path).unwrap().contains("original"));
    }

    #[test]
    fn managed_config_write_failure_returns_error_without_losing_saved_values() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let original = AppConfig::default();
        save_at(&path, &original).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0)
            .open(&path)
            .unwrap();
        let changed = AppConfig {
            password: "not-saved".into(),
            ..original.clone()
        };
        assert!(
            update_config_at(&path, changed, &super::super::management::Status::default()).is_err()
        );
        drop(lock);
        assert!(load_from(&path).unwrap() == original);
    }
    #[test]
    fn multiple_profile_credentials_survive_reordering_and_deletion_without_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut cfg = AppConfig::default();
        for id in ["a", "b"] {
            cfg.upsert_server(crate::link::LinkConfig {
                id: id.into(),
                name: id.into(),
                mode: "subscription".into(),
                subscription: format!("subscription-{id}"),
                encryption_password: format!("encryption-{id}"),
                ..Default::default()
            })
            .unwrap();
        }
        save_at(&path, &cfg).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        for secret in [
            "subscription-a",
            "subscription-b",
            "encryption-a",
            "encryption-b",
        ] {
            assert!(!raw.contains(secret));
        }
        let mut saved = load_from(&path).unwrap();
        assert!(saved == cfg);
        saved.servers.reverse();
        save_at(&path, &saved).unwrap();
        let mut saved = load_from(&path).unwrap();
        assert_eq!(saved.servers[0].id, "b");
        assert_eq!(saved.servers[0].subscription, "subscription-b");
        assert_eq!(saved.servers[1].encryption_password, "encryption-a");
        saved.servers.retain(|s| s.id == "b");
        save_at(&path, &saved).unwrap();
        assert!(load_from(&path).unwrap() == saved);
    }
    #[test]
    fn machine_dpapi_roundtrip_and_tamper_rejection() {
        let secret = b"test-only Windows host password";
        let mut protected = crypt(secret, true).unwrap();
        assert!(!protected.windows(secret.len()).any(|w| w == secret));
        assert_eq!(crypt(&protected, false).unwrap(), secret);
        let index = protected.len() / 2;
        protected[index] ^= 0x80;
        assert!(crypt(&protected, false).is_err());
    }
    #[test]
    fn single_install_config_roundtrips_password_and_settings_without_plaintext() {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            password: "local-display-password".into(),
            password_configured: true,
            port: 31001,
            audio: false,
            allow_control: false,
            ..Default::default()
        };
        save_to(directory.path(), &config).unwrap();
        let path = directory.path().join("config.json");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("local-display-password"));
        assert!(text.contains("passwordDpapi"));
        assert!(load_from(&path).unwrap() == config);
        let mut incomplete: serde_json::Value = serde_json::from_str(&text).unwrap();
        incomplete.as_object_mut().unwrap().remove("allowControl");
        std::fs::write(&path, serde_json::to_vec(&incomplete).unwrap()).unwrap();
        assert!(load_from(&path).is_err());
        let mut changed = config;
        changed.password = "replacement-password".into();
        changed.port = 31002;
        save_to(directory.path(), &changed).unwrap();
        assert!(load_from(&path).unwrap() == changed);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    #[test]
    fn new_connections_read_updated_password_and_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        assert!(!check_password_from(&path, "").unwrap());
        let mut cfg = AppConfig {
            password: "first-password".into(),
            ..Default::default()
        };
        save_to(directory.path(), &cfg).unwrap();
        assert!(check_password_from(&path, "first-password").unwrap());
        cfg.password = "second-password".into();
        write_config(&path, &encode_config(&cfg).unwrap()).unwrap();
        assert!(!check_password_from(&path, "first-password").unwrap());
        assert!(check_password_from(&path, "second-password").unwrap());
        cfg.password.clear();
        write_config(&path, &encode_config(&cfg).unwrap()).unwrap();
        assert!(!check_password_from(&path, "").unwrap());
        assert!(!check_password_from(&path, "second-password").unwrap());
        write_config(&path, b"{").unwrap();
        assert!(check_password_from(&path, "second-password").is_err());
    }
    #[test]
    fn service_saves_to_the_existing_file_and_authentication_reads_the_new_settings() {
        let directory = tempfile::tempdir().unwrap();
        save_to(directory.path(), &AppConfig::default()).unwrap();
        let path = directory.path().join("config.json");
        let config = AppConfig {
            password: "direct-save-test".into(),
            password_configured: true,
            port: 31004,
            clipboard: false,
            ..Default::default()
        };
        write_config(&path, &encode_config(&config).unwrap()).unwrap();
        assert!(load_from(&path).unwrap() == config);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("direct-save-test"));
    }

    #[test]
    fn service_read_waits_for_an_in_progress_config_write() {
        use std::io::{Seek, SeekFrom, Write};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        save_to(directory.path(), &AppConfig::default()).unwrap();
        let config = AppConfig {
            password: "concurrent-save-test".into(),
            password_configured: true,
            port: 31005,
            ..Default::default()
        };
        let bytes = encode_config(&config).unwrap();
        let writing_path = path.clone();
        let (started, receive) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut file = open_config(&writing_path, true).unwrap();
            file.set_len(1).unwrap();
            file.write_all(b"{").unwrap();
            started.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&bytes).unwrap();
            file.set_len(bytes.len() as u64).unwrap();
            file.sync_all().unwrap();
        });
        receive.recv().unwrap();
        assert!(load_from(&path).unwrap() == config);
        writer.join().unwrap();
    }

    #[test]
    fn non_admin_cannot_read_or_write_config_or_replace_service_files() {
        use windows::Win32::{Security::*, System::Threading::*};
        fn dacl(path: &std::path::Path, sddl: &str) {
            let descriptor = super::super::native::Descriptor::new(sddl).unwrap();
            assert!(unsafe {
                SetFileSecurityW(
                    PCWSTR(super::super::native::wide(path).as_ptr()),
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    PSECURITY_DESCRIPTOR(descriptor.attributes().lpSecurityDescriptor),
                )
            }
            .as_bool());
        }
        struct Restore(std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                dacl(
                    &self.0,
                    "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)",
                );
            }
        }
        struct Revert;
        impl Drop for Revert {
            fn drop(&mut self) {
                unsafe {
                    RevertToSelf().unwrap();
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let executable = directory.path().join("vnt-service.exe");
        save_to(directory.path(), &AppConfig::default()).unwrap();
        std::fs::write(&executable, b"protected binary").unwrap();
        let _restore = Restore(directory.path().to_path_buf());
        dacl(
            directory.path(),
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;BU)",
        );
        dacl(&executable, "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;BU)");
        dacl(&path, CONFIG_SDDL);
        // Deny-only Administrators SID reproduces an unelevated desktop token,
        // even if the test runner itself was launched with administrator rights.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE,
                &mut token,
            )
            .unwrap();
            let token = super::super::native::Handle(token);
            let mut admin = [0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];
            let mut size = std::mem::size_of_val(&admin) as u32;
            let sid = PSID(admin.as_mut_ptr().cast());
            CreateWellKnownSid(WinBuiltinAdministratorsSid, None, Some(sid), &mut size).unwrap();
            let disabled = [SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: 0,
            }];
            let mut restricted = HANDLE::default();
            CreateRestrictedToken(
                token.0,
                DISABLE_MAX_PRIVILEGE,
                Some(&disabled),
                None,
                None,
                &mut restricted,
            )
            .unwrap();
            let restricted = super::super::native::Handle(restricted);
            ImpersonateLoggedOnUser(restricted.0).unwrap();
            let _revert = Revert;
            assert!(std::fs::write(directory.path().join("temporary-file"), b"must fail").is_err());
            assert!(std::fs::write(&executable, b"must fail").is_err());
            let config = AppConfig {
                password: "unelevated-test".into(),
                password_configured: true,
                port: 31006,
                ..Default::default()
            };
            assert!(write_config(&path, &encode_config(&config).unwrap()).is_err());
            assert!(load_from(&path).is_err());
        }
    }
}
