//! One server connection runtime, owned by the Windows service or portable app.
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use vnt_core::{
    api::VntApi,
    application::ApplicationEndpoint,
    context::config::Config,
    log_manager::InstanceLog,
    managed_config::Subscription,
    network_info::{RuntimeChangeManager, RuntimeEvent},
};
use vnt_net::{
    datagram::{Datagram, OverlayDatagrams},
    overlay::Peer,
    session::SessionChannel,
};

pub fn default_device_name() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "VNT Desk".into())
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LinkConfig {
    pub id: String,
    pub name: String,
    pub device_name: String,
    pub enabled: bool,
    pub mode: String,
    pub subscription: String,
    pub servers: Vec<String>,
    pub network_code: String,
    pub connection_id: String,
    pub encryption_password: String,
}
impl LinkConfig {
    fn core_config(&self, device_id: &str) -> anyhow::Result<Config> {
        let subscription = self.mode == "subscription";
        let config = Config {
            server_addr: if subscription {
                Vec::new()
            } else {
                self.servers
                    .iter()
                    .map(|s| s.parse())
                    .collect::<Result<_, _>>()
                    .map_err(anyhow::Error::msg)?
            },
            network_code: if subscription {
                String::new()
            } else {
                self.network_code.clone()
            },
            device_id: if subscription {
                String::new()
            } else {
                device_id.to_string()
            },
            connection_id: if subscription {
                None
            } else {
                Some(parse_connection_id(&self.connection_id).map_err(anyhow::Error::msg)?)
            },
            device_name: if !subscription && !self.device_name.trim().is_empty() {
                self.device_name.trim().to_string()
            } else {
                default_device_name()
            },
            password: (!subscription && !self.encryption_password.is_empty())
                .then(|| self.encryption_password.clone()),
            ..Default::default()
        };
        Ok(config)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.device_name.chars().count() > 64 {
            return Err("设备名称最多 64 个字符".into());
        }
        if !matches!(self.mode.as_str(), "" | "manual" | "subscription") {
            return Err("未知服务端配置模式".into());
        }
        if self.subscription.len() > 32768 || self.encryption_password.len() > 1024 {
            return Err("服务端凭据过长".into());
        }
        if self.mode == "subscription" {
            if self.subscription.trim().is_empty() {
                return Err("请填写订阅地址".into());
            }
            if self.enabled {
                Subscription::parse(&self.subscription)
                    .map_err(|_| "服务订阅地址无效".to_string())?;
            }
        } else {
            if self.servers.is_empty() {
                return Err("请填写服务器地址".into());
            }
            if self.network_code.trim().is_empty() {
                return Err("请填写房间编号".into());
            }
            if self.connection_id.trim().is_empty() {
                return Err("请填写连接 ID".into());
            }
            parse_connection_id(&self.connection_id)?;
            if self.servers.len() > 32 {
                return Err("请填写 1–32 个服务器地址".into());
            }
            for server in &self.servers {
                server
                    .parse::<vnt_core::tunnel_core::server::transport::config::ProtocolAddress>()
                    .map_err(|_| "服务器地址无效".to_string())?;
            }
            if self.network_code.len() > 32 {
                return Err("房间编号无效".into());
            }
        }
        Ok(())
    }
}
fn parse_connection_id(value: &str) -> Result<vnt_core::connection_id::ConnectionId, String> {
    value
        .parse()
        .map_err(|error: anyhow::Error| error.to_string())
}
fn display_connection_id(key: std::net::Ipv4Addr) -> String {
    vnt_core::connection_id::ConnectionId::from_encoded(u32::from(key))
        .map(|id| id.to_string())
        .unwrap_or_default()
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkStatus {
    pub phase: String,
    pub error: Option<String>,
    pub network_code: String,
    pub device_id: String,
    pub local_connection_id: String,
    pub devices: Vec<OnlineDevice>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineDevice {
    pub id: String,
    pub name: String,
    pub connection_id: String,
    pub direct: bool,
}
pub type Incoming = (SessionChannel, SocketAddr, Arc<Datagram>);
struct Active {
    api: VntApi,
    endpoint: ApplicationEndpoint,
    datagrams: Arc<OverlayDatagrams>,
    cancel: CancellationToken,
    dialing: Arc<Mutex<()>>,
}
pub struct ServerLink {
    device_id: String,
    operation: Mutex<()>,
    worker: Mutex<Option<(CancellationToken, tokio::task::JoinHandle<()>)>>,
    active: RwLock<Option<Active>>,
    status: RwLock<LinkStatus>,
    incoming: Mutex<mpsc::Receiver<Incoming>>,
    accepted: mpsc::Sender<Incoming>,
}
impl Default for ServerLink {
    fn default() -> Self {
        let (accepted, incoming) = mpsc::channel(16);
        let mut link = Self::with_sender(accepted, uuid::Uuid::new_v4().to_string());
        link.incoming = Mutex::new(incoming);
        link
    }
}
impl ServerLink {
    fn with_sender(accepted: mpsc::Sender<Incoming>, device_id: String) -> Self {
        let (_, incoming) = mpsc::channel(16);
        Self {
            device_id,
            operation: Default::default(),
            worker: Default::default(),
            active: Default::default(),
            status: Default::default(),
            incoming: Mutex::new(incoming),
            accepted,
        }
    }
}
impl ServerLink {
    pub async fn snapshot(&self) -> LinkStatus {
        self.status.read().await.clone()
    }
    pub async fn next_incoming(&self) -> Option<Incoming> {
        self.incoming.lock().await.recv().await
    }
    pub async fn stop(&self) {
        let _operation = self.operation.lock().await;
        self.stop_inner().await;
    }
    async fn stop_inner(&self) {
        if let Some((cancel, mut task)) = self.worker.lock().await.take() {
            cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(3), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        if let Some(active) = self.active.write().await.take() {
            active.cancel.cancel();
            active.endpoint.close();
        }
        *self.status.write().await = LinkStatus {
            phase: "stopped".into(),
            ..Default::default()
        };
    }
    pub async fn configure(self: &Arc<Self>, config: LinkConfig) -> Result<(), String> {
        config.validate()?;
        let _operation = self.operation.lock().await;
        self.stop_inner().await;
        if !config.enabled {
            return Ok(());
        }
        let cancel = CancellationToken::new();
        let tx = self.accepted.clone();
        *self.status.write().await = LinkStatus {
            phase: "connecting".into(),
            ..Default::default()
        };
        let this = self.clone();
        let stopped = cancel.clone();
        let task = tokio::spawn(async move {
            let (connection_error, mut errors) = tokio::sync::watch::channel(None);
            let run = this.run(config, stopped.clone(), tx, connection_error);
            tokio::pin!(run);
            let result = loop {
                tokio::select! {
                    _ = stopped.cancelled() => break Ok(()),
                    result = &mut run => break result,
                    Ok(()) = errors.changed() => {
                        let error = errors.borrow_and_update().clone();
                        let active = this.active.read().await.is_some();
                        let mut status = this.status.write().await;
                        status.phase = if error.is_some() {
                            "reconnecting"
                        } else if active {
                            "connected"
                        } else {
                            "connecting"
                        }.into();
                        status.error = error;
                    }
                }
            };
            if let Some(active) = this.active.write().await.take() {
                active.cancel.cancel();
                active.endpoint.close();
            }
            if let Err(error) = result {
                let mut status = this.status.write().await;
                status.phase = "error".into();
                status.error = Some(format!("{error:#}"));
                status.devices.clear();
            }
        });
        *self.worker.lock().await = Some((cancel, task));
        Ok(())
    }
    async fn run(
        &self,
        config: LinkConfig,
        cancel: CancellationToken,
        incoming: mpsc::Sender<Incoming>,
        connection_error: tokio::sync::watch::Sender<Option<String>>,
    ) -> anyhow::Result<()> {
        let subscription = if config.mode == "subscription" {
            Some(Subscription::parse(&config.subscription)?)
        } else {
            None
        };
        let core = config.core_config(&self.device_id)?;
        let mut manager = RuntimeChangeManager::new_with_subscription_status(
            core,
            subscription,
            Arc::new(InstanceLog::new("remote-desktop")),
            connection_error,
        )
        .await?;
        let result = self.run_manager(&mut manager, cancel, incoming).await;
        manager.stop().await;
        result
    }
    async fn run_manager(
        &self,
        manager: &mut RuntimeChangeManager,
        cancel: CancellationToken,
        incoming: mpsc::Sender<Incoming>,
    ) -> anyhow::Result<()> {
        let mut timer = tokio::time::interval(Duration::from_secs(3));
        loop {
            let network = manager.wait_network_ready().await?;
            let api = manager
                .api()
                .ok_or_else(|| anyhow::anyhow!("核心实例不可用"))?;
            let endpoint = api
                .application_endpoint()
                .ok_or_else(|| anyhow::anyhow!("核心应用通道不可用"))?;
            let datagrams = Arc::new(OverlayDatagrams::default());
            datagrams.set_local(SocketAddr::from((network.ip, 29875)));
            let scope = cancel.child_token();
            let (accepted, mut rx) = mpsc::channel(16);
            vnt_net::overlay::listen(endpoint.clone(), datagrams.clone(), scope.clone(), accepted);
            let media = Arc::new(Datagram::overlay(datagrams.clone()));
            let out = incoming.clone();
            let forward_cancel = scope.clone();
            tokio::spawn(async move {
                loop {
                    let item = tokio::select! {
                        _ = forward_cancel.cancelled() => break,
                        item = rx.recv() => item,
                    };
                    let Some((channel, peer)) = item else {
                        break;
                    };
                    tokio::select! {
                        _ = forward_cancel.cancelled() => break,
                        result = out.send((channel, peer, media.clone())) => {
                            if result.is_err() { break; }
                        }
                    }
                }
            });
            *self.active.write().await = Some(Active {
                api: api.clone(),
                endpoint: endpoint.clone(),
                datagrams,
                cancel: scope.clone(),
                dialing: Default::default(),
            });
            let cfg = api
                .get_config()
                .ok_or_else(|| anyhow::anyhow!("核心配置不可用"))?;
            *self.status.write().await = LinkStatus {
                phase: "connected".into(),
                network_code: cfg.network_code,
                device_id: cfg.device_id.clone(),
                local_connection_id: display_connection_id(network.ip),
                ..Default::default()
            };
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => { scope.cancel(); endpoint.close(); return Ok(()); },
                    _ = timer.tick() => { self.refresh(&api, &cfg.device_id).await; },
                    event = manager.next_event() => {
                        match event? {
                            RuntimeEvent::Changed(change) => {
                                let outcome = manager.apply_change(&change).await?;
                                if matches!(outcome, vnt_core::network_info::ChangeOutcome::Rebuilt) {
                                    scope.cancel(); endpoint.close(); break;
                                }
                            },
                            RuntimeEvent::InstanceStopped => { scope.cancel(); endpoint.close(); anyhow::bail!("VNT 核心连接已停止"); }
                        }
                    }
                }
            }
        }
    }
    async fn refresh(&self, api: &VntApi, own_id: &str) {
        let connected = api.server_node_list().iter().any(|s| s.connected);
        let list =
            tokio::time::timeout(Duration::from_secs(2), api.server_rpc().client_list()).await;
        let mut status = self.status.write().await;
        status.phase = if connected && status.error.is_none() {
            "connected"
        } else {
            "reconnecting"
        }
        .into();
        status.devices = match list {
            Ok(Ok(list)) if connected => list
                .list
                .into_iter()
                .filter(|d| d.online && d.id != own_id && d.client_type == 3)
                .map(|d| {
                    let ip = std::net::Ipv4Addr::from(d.ip);
                    OnlineDevice {
                        direct: api.is_direct(&ip),
                        id: d.id,
                        name: d.name,
                        connection_id: display_connection_id(ip),
                    }
                })
                .collect(),
            _ => Vec::new(),
        };
        status
            .devices
            .sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    }
    pub async fn peer(&self, node: &str) -> anyhow::Result<Peer> {
        let active = self.active.read().await;
        let active = active
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("请先启动服务端连接"))?;
        let list = tokio::time::timeout(
            Duration::from_secs(3),
            active.api.server_rpc().client_list(),
        )
        .await??;
        let device = list
            .list
            .into_iter()
            .find(|d| d.id == node && d.online && d.client_type == 3)
            .ok_or_else(|| anyhow::anyhow!("目标设备已离线"))?;
        {
            let mut status = self.status.write().await;
            status.devices.retain(|d| d.id != device.id);
            status.devices.push(OnlineDevice {
                id: device.id.clone(),
                name: device.name,
                connection_id: display_connection_id(device.ip.into()),
                direct: active.api.is_direct(&device.ip.into()),
            });
        }
        Ok(Peer {
            endpoint: active.endpoint.clone(),
            ip: device.ip.into(),
            datagrams: active.datagrams.clone(),
            cancel: active.cancel.clone(),
            dialing: active.dialing.clone(),
        })
    }
}

/// Identity is a configuration ID, never a display name, node ID or virtual IP.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkTarget {
    pub config_id: String,
    pub network_code: Option<String>,
    pub node: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileStatus {
    pub config_id: String,
    pub name: String,
    pub enabled: bool,
    #[serde(flatten)]
    pub status: LinkStatus,
}

pub fn validate_profiles(configs: &[LinkConfig]) -> Result<(), String> {
    let mut ids = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for config in configs {
        if config.id.is_empty() || config.id.len() > 128 || !ids.insert(&config.id) {
            return Err("服务端配置标识无效或重复".into());
        }
        let name = config.name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            return Err("请填写 1–64 个字符的配置名称".into());
        }
        if !names.insert(name) {
            return Err("配置名称已存在".into());
        }
        config.validate()?;
    }
    Ok(())
}

/// Shared incoming queue, with an independent lifecycle and address space per profile.
pub struct ServerLinks {
    device_id: String,
    operation: Mutex<()>,
    profiles: RwLock<Vec<(LinkConfig, Arc<ServerLink>)>>,
    accepted: mpsc::Sender<Incoming>,
    incoming: Mutex<mpsc::Receiver<Incoming>>,
}
impl Default for ServerLinks {
    fn default() -> Self {
        let (accepted, incoming) = mpsc::channel(32);
        Self {
            device_id: uuid::Uuid::new_v4().to_string(),
            operation: Mutex::new(()),
            profiles: RwLock::new(Vec::new()),
            accepted,
            incoming: Mutex::new(incoming),
        }
    }
}
impl ServerLinks {
    pub fn new(device_id_path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self {
            device_id: crate::device_id::load_or_create(device_id_path)?,
            ..Self::default()
        })
    }
    pub async fn configure(&self, configs: Vec<LinkConfig>) -> Result<(), String> {
        validate_profiles(&configs)?;
        let _operation = self.operation.lock().await;
        let previous = self.profiles.read().await.clone();
        for (config, link) in &previous {
            if !configs.iter().any(|next| next.id == config.id) {
                link.stop().await;
            }
        }
        let mut profiles = Vec::new();
        for config in configs {
            let prior = previous.iter().find(|(old, _)| old.id == config.id);
            let link = prior.map(|(_, link)| link.clone()).unwrap_or_else(|| {
                Arc::new(ServerLink::with_sender(
                    self.accepted.clone(),
                    self.device_id.clone(),
                ))
            });
            let changed = prior.is_none_or(|(old, _)| {
                let mut old = old.clone();
                old.name = config.name.clone();
                old != config
            });
            if changed {
                link.configure(config.clone()).await?;
            }
            profiles.push((config, link));
        }
        *self.profiles.write().await = profiles;
        Ok(())
    }
    pub async fn snapshot(&self) -> Vec<ProfileStatus> {
        let profiles = self.profiles.read().await.clone();
        let mut statuses = Vec::new();
        for (config, link) in profiles {
            let mut status = link.snapshot().await;
            if status.phase.is_empty() {
                status.phase = "stopped".into();
            }
            if status.network_code.is_empty() && config.mode != "subscription" {
                status.network_code = config.network_code;
            }
            statuses.push(ProfileStatus {
                config_id: config.id,
                name: config.name,
                enabled: config.enabled,
                status,
            });
        }
        statuses
    }
    pub async fn select(&self, target: &LinkTarget) -> anyhow::Result<Arc<ServerLink>> {
        let profiles = self.profiles.read().await;
        let (config, link) = profiles
            .iter()
            .find(|(config, _)| config.id == target.config_id)
            .ok_or_else(|| anyhow::anyhow!("服务端配置不存在，请重新选择设备"))?;
        anyhow::ensure!(config.enabled, "请先启动设备所属的服务端配置");
        if let Some(expected) = &target.network_code {
            anyhow::ensure!(
                link.snapshot().await.network_code == *expected,
                "当前连接组与保存设备所属组不一致"
            );
        }
        Ok(link.clone())
    }
    pub async fn next_incoming(&self) -> Option<Incoming> {
        self.incoming.lock().await.recv().await
    }
    pub async fn stop(&self) {
        let _operation = self.operation.lock().await;
        for (_, link) in self.profiles.read().await.iter() {
            link.stop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn subscription_failures_reach_status_and_stop_cancels_retry() {
        // A syntactically valid subscription pointing at an unusable local port.
        let subscription = concat!(
            "vnt2://join/2/",
            "eyJ2IjoyLCJzZXJ2ZXIiOiJ0Y3A6Ly8xMjcuMC4wLjE6MCIsImNlcnRfbW9kZSI6ImZpbmdlcjpiMjI1Y2YyOGU5MjFlM2RkMzBmNDQ1MGEwZDdmZDRjMzk5M2E1ZWExZGEzN2I5OTRiYjY3NWY3YjljZDk2MjJmIiwiam9pbl9pZCI6InRlc3QiLCJjcmVkZW50aWFsX2tleSI6IkFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUEifQ"
        );
        let link = Arc::new(ServerLink::default());
        link.configure(LinkConfig {
            enabled: true,
            mode: "subscription".into(),
            subscription: subscription.into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = link.snapshot().await;
                if status.error.is_some() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("subscription failure must become visible before the next retry");
        assert_eq!(status.phase, "reconnecting");
        let error = status.error.unwrap();
        assert!(error.contains("tcp://127.0.0.1:0"), "{error}");
        assert!(error.contains("正在重试"), "{error}");
        assert!(
            !error.contains("AAAAAAAA"),
            "credentials must not appear in errors"
        );
        link.stop().await;
        let status = link.snapshot().await;
        assert_eq!(status.phase, "stopped");
        assert!(status.error.is_none());
    }
    #[test]
    fn connection_ids_preserve_length_and_device_identity() {
        let mut config = profile("a", "A");
        for node in [
            "0",
            "00",
            "001",
            "0001",
            "000000000",
            "000000001",
            "123456789",
            "123456768",
            "123456767",
            "999999999",
        ] {
            config.connection_id = node.into();
            config.validate().unwrap();
            let core = config.core_config("persistent-uuid").unwrap();
            assert_eq!(core.connection_id.unwrap().to_string(), node);
            assert_eq!(core.device_id, "persistent-uuid");
        }
        for node in [
            "1234567890",
            "12345678a",
            " 123456789",
            "１２３４５６７８９",
            "7.91.205.21",
        ] {
            config.connection_id = node.into();
            assert!(config.validate().is_err(), "{node}");
        }
    }

    #[tokio::test]
    async fn profiles_and_restarts_reuse_the_persisted_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_id");
        let links = ServerLinks::new(&path).unwrap();
        links
            .configure(vec![profile("a", "A"), profile("b", "B")])
            .await
            .unwrap();
        for (_, link) in links.profiles.read().await.iter() {
            assert_eq!(link.device_id, links.device_id);
        }
        assert_eq!(links.device_id, ServerLinks::new(&path).unwrap().device_id);
    }
    fn profile(id: &str, name: &str) -> LinkConfig {
        LinkConfig {
            id: id.into(),
            name: name.into(),
            mode: "manual".into(),
            network_code: "group".into(),
            servers: vec!["tcp://127.0.0.1:29872".into()],
            connection_id: "123456789".into(),
            ..Default::default()
        }
    }
    #[test]
    fn connection_fields_are_required_even_when_stopped() {
        let valid = profile("a", "A");
        assert!(valid.validate().is_ok());
        assert_eq!(
            LinkConfig {
                servers: vec![],
                ..valid.clone()
            }
            .validate()
            .unwrap_err(),
            "请填写服务器地址"
        );
        assert_eq!(
            LinkConfig {
                network_code: " ".into(),
                ..valid.clone()
            }
            .validate()
            .unwrap_err(),
            "请填写房间编号"
        );
        assert_eq!(
            LinkConfig {
                connection_id: " ".into(),
                ..valid.clone()
            }
            .validate()
            .unwrap_err(),
            "请填写连接 ID"
        );
        assert_eq!(
            LinkConfig {
                mode: "subscription".into(),
                subscription: " ".into(),
                ..valid
            }
            .validate()
            .unwrap_err(),
            "请填写订阅地址"
        );
    }
    #[test]
    fn names_are_required_unique_and_trimmed_for_comparison_even_when_stopped() {
        assert!(validate_profiles(&[profile("a", " ")]).is_err());
        assert_eq!(
            validate_profiles(&[profile("a", "办公室"), profile("b", " 办公室 ")]).unwrap_err(),
            "配置名称已存在"
        );
        assert!(validate_profiles(&[profile("a", "办公室")]).is_ok());
        assert!(validate_profiles(&[profile("a", "一"), profile("a", "二")]).is_err());
    }
    #[test]
    fn manual_device_name_overrides_hostname_and_blank_uses_hostname() {
        let mut config = profile("a", "一");
        config.device_name = " 自定义设备 ".into();
        assert_eq!(
            config.core_config("test-device-uuid").unwrap().device_name,
            "自定义设备"
        );
        config.device_name = " ".into();
        assert!(!config
            .core_config("test-device-uuid")
            .unwrap()
            .device_name
            .trim()
            .is_empty());
    }
    #[tokio::test]
    async fn profiles_are_independent_and_renaming_preserves_runtime() {
        let links = ServerLinks::default();
        let a = profile("a", "一");
        let b = profile("b", "二");
        links.configure(vec![a.clone(), b.clone()]).await.unwrap();
        let original = links.profiles.read().await[0].1.clone();
        original.status.write().await.phase = "test-marker".into();
        let mut renamed = a.clone();
        renamed.name = "新名称".into();
        links
            .configure(vec![renamed.clone(), b.clone()])
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&original, &links.profiles.read().await[0].1));
        assert_eq!(links.snapshot().await[0].status.phase, "test-marker");
        assert_eq!(links.snapshot().await[0].name, "新名称");
        let mut duplicate = b.clone();
        duplicate.name = renamed.name.clone();
        assert!(links.configure(vec![renamed, duplicate]).await.is_err());
        assert_eq!(links.snapshot().await.len(), 2);
        links.configure(vec![b]).await.unwrap();
        assert_eq!(original.snapshot().await.phase, "stopped");
        assert_eq!(links.snapshot().await[0].config_id, "b");
        links.stop().await;
    }
    #[tokio::test]
    async fn equal_connection_ids_and_addresses_are_routed_by_profile_and_group() {
        let links = ServerLinks::default();
        let a = Arc::new(ServerLink::default());
        let b = Arc::new(ServerLink::default());
        for link in [&a, &b] {
            *link.status.write().await = LinkStatus {
                phase: "connected".into(),
                network_code: "same-group".into(),
                local_connection_id: "001".into(),
                devices: vec![OnlineDevice {
                    id: "same-node".into(),
                    name: "Remote".into(),
                    connection_id: "0001".into(),
                    direct: true,
                }],
                ..Default::default()
            };
        }
        // Inject established runtimes so selection can be checked without a vnts server.
        *links.profiles.write().await = vec![
            (
                LinkConfig {
                    enabled: true,
                    ..profile("a", "A")
                },
                a.clone(),
            ),
            (
                LinkConfig {
                    enabled: true,
                    ..profile("b", "B")
                },
                b.clone(),
            ),
        ];
        let mut target = LinkTarget {
            config_id: "b".into(),
            network_code: Some("same-group".into()),
            node: "same-node".into(),
        };
        assert!(Arc::ptr_eq(&links.select(&target).await.unwrap(), &b));
        a.status.write().await.phase = "error".into();
        assert!(Arc::ptr_eq(&links.select(&target).await.unwrap(), &b));
        target.network_code = Some("wrong-group".into());
        assert!(links.select(&target).await.is_err());
        target.config_id = "deleted".into();
        assert!(links.select(&target).await.is_err());
        links.stop().await;
    }
    #[test]
    fn subscription_mode_does_not_read_inactive_manual_settings() {
        let config = LinkConfig {
            mode: "subscription".into(),
            servers: vec!["unsupported://manual-draft".into()],
            network_code: "manual-group".into(),
            connection_id: "manual-node".into(),
            encryption_password: "manual-secret".into(),
            ..Default::default()
        };
        let core = config.core_config("test-device-uuid").unwrap();
        assert!(
            core.password.is_none()
                && core.server_addr.is_empty()
                && core.network_code.is_empty()
                && core.device_id.is_empty()
        );
    }
}
