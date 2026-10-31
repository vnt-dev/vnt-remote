//! 被控配置；由安装目录中的 config.json 持久化。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CaptureTarget {
    Monitor { name: String },
    Region { id: String },
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasswordAction {
    #[default]
    Keep,
    Set,
    Clear,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub format_version: u32,
    pub servers: Vec<crate::link::LinkConfig>,
    #[serde(default)]
    pub password_action: PasswordAction,
    /// An explicit empty list means no capture target.
    pub selected_targets: Vec<CaptureTarget>,
    pub screen_regions: Vec<vnt_capture::ScreenRegion>,
    /// 被控端密码。
    pub password: String,
    /// Whether a host password has been configured.
    pub password_configured: bool,
    /// 监听端口（TCP+UDP 同端口）。
    pub port: u16,
    /// Resolved per-screen worker index, never persisted or accepted over IPC.
    #[serde(skip)]
    pub monitor_index: u32,
    /// 剪贴板同步（文本、图片与文件复制粘贴）总开关。
    pub clipboard: bool,
    /// 系统声音共享；修改后对新会话生效。
    pub audio: bool,
    /// 被控端硬件编码开关：关闭后仅协商/使用软件编码（OpenH264/Kvazaar/SVT-AV1）。
    pub hardware_encode: bool,
    /// 允许主控端操作本机，默认开启；关闭后仅观看且不同步剪贴板。
    /// 修改后对新会话生效。
    pub allow_control: bool,
    /// 显示 Windows 屏幕采集提示边框；默认关闭，实时生效。
    pub capture_border: bool,
}

pub type ServerSecrets = std::collections::BTreeMap<String, (String, String)>;
impl AppConfig {
    pub fn server_secrets_clear(&self) -> bool {
        self.servers
            .iter()
            .all(|s| s.subscription.is_empty() && s.encryption_password.is_empty())
    }
    pub fn take_server_secrets(&mut self) -> ServerSecrets {
        self.servers
            .iter_mut()
            .map(|s| {
                (
                    s.id.clone(),
                    (
                        std::mem::take(&mut s.subscription),
                        std::mem::take(&mut s.encryption_password),
                    ),
                )
            })
            .collect()
    }
    pub fn restore_server_secrets(&mut self, mut secrets: ServerSecrets) -> Result<(), String> {
        for server in &mut self.servers {
            let (subscription, password) = secrets.remove(&server.id).ok_or("服务端凭据缺失")?;
            server.subscription = subscription;
            server.encryption_password = password;
        }
        if !secrets.is_empty() {
            return Err("服务端凭据与配置不匹配".into());
        }
        Ok(())
    }
    pub fn upsert_server(&mut self, mut config: crate::link::LinkConfig) -> Result<(), String> {
        config.name = config.name.trim().to_string();
        let mut next = self.servers.clone();
        if let Some(current) = next.iter_mut().find(|s| s.id == config.id) {
            *current = config;
        } else {
            next.push(config);
        }
        crate::link::validate_profiles(&next)?;
        self.servers = next;
        Ok(())
    }

    pub fn apply_password_update(&mut self, previous: &Self) {
        match self.password_action {
            PasswordAction::Clear => self.password.clear(),
            PasswordAction::Keep if self.password.is_empty() => {
                self.password = previous.password.clone()
            }
            _ => {}
        }
        self.password_action = PasswordAction::Keep;
        self.password_configured = !self.password.trim().is_empty();
    }

    pub fn targets(&self, monitors: &[vnt_capture::MonitorInfo]) -> Vec<CaptureTarget> {
        let primary = || {
            monitors
                .iter()
                .find(|m| m.primary)
                .or(monitors.first())
                .map(|m| {
                    vec![CaptureTarget::Monitor {
                        name: m.name.clone(),
                    }]
                })
                .unwrap_or_default()
        };
        let saved = &self.selected_targets;
        let selected: Vec<_> = saved
            .iter()
            .filter(|target| match target {
                CaptureTarget::Monitor { name } => monitors.iter().any(|m| &m.name == name),
                CaptureTarget::Region { id } => self
                    .screen_regions
                    .iter()
                    .any(|r| &r.id == id && r.resolve(monitors).is_ok()),
            })
            .cloned()
            .collect();
        if selected.is_empty() && !saved.is_empty() {
            primary()
        } else {
            selected
        }
    }

    pub fn selected_region(&self) -> Result<Option<vnt_capture::ScreenRegion>, String> {
        match self.selected_targets.first() {
            Some(CaptureTarget::Region { id }) => self
                .screen_regions
                .iter()
                .find(|r| &r.id == id)
                .cloned()
                .map(Some)
                .ok_or_else(|| "所选屏幕区域不存在".to_string()),
            _ => Ok(None),
        }
    }

    /// Only called when creating a new configuration, never for a saved empty selection.
    pub fn initial(monitors: &[vnt_capture::MonitorInfo]) -> Self {
        let mut config = Self::default();
        if let Some(monitor) = monitors.iter().find(|m| m.primary).or(monitors.first()) {
            config.selected_targets.push(CaptureTarget::Monitor {
                name: monitor.name.clone(),
            });
        }
        config
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != 1 {
            return Err("配置格式版本无效".into());
        }
        crate::link::validate_profiles(&self.servers)?;
        {
            let targets = &self.selected_targets;
            let mut unique = std::collections::HashSet::new();
            if targets.len() > 128 || targets.iter().any(|t| !unique.insert(t)) {
                return Err("采集目标重复或数量超过 128".into());
            }
            for target in targets {
                match target {
                    CaptureTarget::Monitor { name } if name.is_empty() || name.len() > 256 => {
                        return Err("显示器标识无效".into())
                    }
                    CaptureTarget::Region { id }
                        if !self.screen_regions.iter().any(|r| &r.id == id) =>
                    {
                        return Err("所选屏幕区域不存在".into())
                    }
                    _ => {}
                }
            }
        }
        if self.screen_regions.len() > 100 {
            return Err("最多保存 100 个屏幕区域".into());
        }
        let mut ids = std::collections::HashSet::new();
        for region in &self.screen_regions {
            region.validate()?;
            if region.id.is_empty()
                || region.id.len() > 128
                || !ids.insert(&region.id)
                || region.name.trim().is_empty()
                || region.name.chars().count() > 64
            {
                return Err("屏幕区域名称或标识无效".into());
            }
        }
        if self.port == 0 {
            return Err("服务端口必须在 1–65535 之间".into());
        }
        if self.password.len() > 1024 {
            return Err("密码过长".into());
        }
        Ok(())
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            format_version: 1,
            servers: Vec::new(),
            password_action: Default::default(),
            selected_targets: Vec::new(),
            screen_regions: Vec::new(),
            password: String::new(),
            password_configured: false,
            port: vnt_proto::DEFAULT_PORT,
            monitor_index: 0,
            clipboard: true,
            audio: true,
            hardware_encode: true,
            allow_control: true,
            capture_border: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initial_selection_and_explicit_empty_are_distinct() {
        let mut secondary = vnt_capture::synthetic_monitor(0, 100, 100);
        secondary.primary = false;
        secondary.name = "secondary".into();
        let mut primary = secondary.clone();
        primary.primary = true;
        primary.name = "primary".into();
        let monitors = [secondary, primary];
        let mut config = AppConfig::initial(&monitors);
        assert_eq!(
            config.selected_targets,
            vec![CaptureTarget::Monitor {
                name: "primary".into()
            }]
        );
        assert_eq!(
            AppConfig::initial(&monitors[..1]).selected_targets,
            vec![CaptureTarget::Monitor {
                name: "secondary".into()
            }]
        );
        config.selected_targets.clear();
        assert!(config.targets(&monitors).is_empty());
        assert!(AppConfig::initial(&[]).selected_targets.is_empty());
    }
    #[test]
    fn stale_targets_recover_without_rewriting_selection() {
        let monitor = vnt_capture::synthetic_monitor(0, 100, 100);
        let mut config = AppConfig {
            selected_targets: vec![CaptureTarget::Monitor {
                name: "offline".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            config.targets(std::slice::from_ref(&monitor)),
            vec![CaptureTarget::Monitor { name: monitor.name }]
        );
        assert!(config.targets(&[]).is_empty());
        assert_eq!(config.selected_targets.len(), 1);
        config
            .selected_targets
            .push(config.selected_targets[0].clone());
        assert!(config.validate().is_err());
    }
    #[test]
    fn region_selection_roundtrips_and_session_snapshots_are_independent() {
        let monitor = vnt_capture::synthetic_monitor(3, 1920, 1080);
        let region = vnt_capture::ScreenRegion {
            id: "r".into(),
            name: "Region".into(),
            monitor_name: monitor.name.clone(),
            screen_width: 1920,
            screen_height: 1080,
            orientation: monitor.orientation,
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        let mut config = AppConfig {
            screen_regions: vec![region],
            selected_targets: vec![CaptureTarget::Region { id: "r".into() }],
            ..Default::default()
        };
        config.validate().unwrap();
        let restored: AppConfig =
            serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(restored == config);
        let snapshot = config.selected_region().unwrap().unwrap();
        config.screen_regions[0].x = 0;
        assert_eq!(snapshot.x, 100);
        assert_eq!(
            config.targets(std::slice::from_ref(&monitor)),
            config.selected_targets
        );
        config.screen_regions[0].screen_width = 1280;
        assert_eq!(
            config.targets(std::slice::from_ref(&monitor)),
            vec![CaptureTarget::Monitor { name: monitor.name }]
        );
        assert!(config.targets(&[]).is_empty());
    }

    #[test]
    fn first_release_format_requires_all_fields_and_skips_runtime_index() {
        let config = AppConfig::default();
        let value = serde_json::to_value(&config).unwrap();
        assert!(value.get("monitorIndex").is_none());
        assert!(value.get("selectedRegionId").is_none());
        for field in [
            "formatVersion",
            "selectedTargets",
            "screenRegions",
            "captureBorder",
            "servers",
        ] {
            let mut incomplete = value.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<AppConfig>(incomplete).is_err(),
                "{field}"
            );
        }
        let mut invalid = config.clone();
        invalid.format_version = 2;
        assert!(invalid.validate().is_err());
        let restored: AppConfig = serde_json::from_value(value).unwrap();
        assert!(restored.selected_targets.is_empty());
    }
}

#[cfg(test)]
mod server_profile_tests {
    use super::*;
    #[test]
    fn profile_secrets_follow_ids_not_array_order() {
        let mut config = AppConfig::default();
        for (id, secret) in [("a", "secret-a"), ("b", "secret-b")] {
            config
                .upsert_server(crate::link::LinkConfig {
                    id: id.into(),
                    name: id.into(),
                    mode: "subscription".into(),
                    subscription: secret.into(),
                    ..Default::default()
                })
                .unwrap();
        }
        let secrets = config.take_server_secrets();
        assert!(config.server_secrets_clear());
        config.servers.reverse();
        config.restore_server_secrets(secrets).unwrap();
        assert_eq!(config.servers[0].subscription, "secret-b");
        assert_eq!(config.servers[1].subscription, "secret-a");
    }
    #[test]
    fn upsert_trims_and_rejects_duplicate_names() {
        let mut config = AppConfig::default();
        let server = crate::link::LinkConfig {
            id: "a".into(),
            name: " 测试 ".into(),
            mode: "subscription".into(),
            subscription: "test-subscription".into(),
            ..Default::default()
        };
        config.upsert_server(server.clone()).unwrap();
        assert_eq!(config.servers[0].name, "测试");
        config.upsert_server(server.clone()).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(
            config
                .upsert_server(crate::link::LinkConfig {
                    id: "b".into(),
                    ..server
                })
                .unwrap_err(),
            "配置名称已存在"
        );
    }
}
