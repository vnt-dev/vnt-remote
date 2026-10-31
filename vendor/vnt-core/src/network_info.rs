//! Subscription configuration and application-network lifecycle.
use crate::{
    api::VntApi,
    context::{NetworkAddr, config::Config},
    core::NetworkManager,
    log_manager::InstanceLog,
    managed_config::{Subscription, SubscriptionListener},
    utils::task_control::{TaskGroupGuard, TaskGroupManager},
};
use anyhow::Context;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct RuntimeChange {
    pub config: Config,
}
pub fn merge_present_config(
    mut local: Config,
    base: &Config,
    present: &std::collections::HashSet<String>,
) -> Config {
    local.network_code = base.network_code.clone();
    local.device_id = base.device_id.clone();
    local.device_name = base.device_name.clone();
    local.connection_id = base.connection_id;
    if !base.server_addr.is_empty() {
        local.server_addr = base.server_addr.clone();
    }
    macro_rules! overlay {
        ($key:literal, $field:ident) => {
            if present.contains($key) {
                local.$field = base.$field.clone();
            }
        };
    }
    // 策略类（apply_policy_change 热应用）
    overlay!("peer_address", peer_address);
    overlay!("turn", turn);
    overlay!("punch_model", punch_model);
    overlay!("compress", compress);
    overlay!("no_punch", no_punch);
    overlay!("udp_stun", udp_stun);
    overlay!("tcp_stun", tcp_stun);
    // 重建类（触发实例重建后生效）
    overlay!("password", password);
    overlay!("cert_mode", cert_mode);
    overlay!("outbound_interface", outbound_interface);
    overlay!("tunnel_addr", tunnel_addr);
    overlay!("tunnel_port", tunnel_port);
    local
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeOutcome {
    Applied,
    Rebuilt,
}
#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    InstanceStopped,
    Changed(Box<RuntimeChange>),
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangePlan {
    pub instance_rebuild: bool,
}

pub struct RuntimeChangeManager {
    local_config: Config,
    subscription_listener: Option<SubscriptionListener>,
    groups: TaskGroupManager,
    log: Arc<InstanceLog>,
    instance: Option<ManagedInstance>,
}
struct ManagedInstance {
    manager: NetworkManager,
    _guard: TaskGroupGuard,
}
impl RuntimeChangeManager {
    pub async fn new(
        local_config: Config,
        subscription: Option<Subscription>,
        log: Arc<InstanceLog>,
    ) -> anyhow::Result<Self> {
        let (connection_error, _) = tokio::sync::watch::channel(None);
        Self::new_with_subscription_status(local_config, subscription, log, connection_error).await
    }
    /// Reports subscription failures while waiting for the first config and
    /// during reconnects. `None` clears the error after authentication succeeds.
    pub async fn new_with_subscription_status(
        local_config: Config,
        subscription: Option<Subscription>,
        log: Arc<InstanceLog>,
        connection_error: tokio::sync::watch::Sender<Option<String>>,
    ) -> anyhow::Result<Self> {
        let mut manager = Self {
            local_config,
            subscription_listener: subscription
                .map(|s| SubscriptionListener::new(s, connection_error)),
            groups: TaskGroupManager::new(),
            log,
            instance: None,
        };
        let config = manager.effective_config().await?;
        manager.start_instance(config).await?;
        Ok(manager)
    }
    pub fn managed_identity(&self) -> Option<(String, String)> {
        let envelope = self.subscription_listener.as_ref()?.current_config_now()?;
        Some((envelope.config.network_code, envelope.config.device_id))
    }
    pub async fn current_config(&mut self) -> anyhow::Result<Config> {
        self.effective_config().await
    }
    /// Waits for registration; no operating-system network interface is created.
    pub async fn wait_network_ready(&self) -> anyhow::Result<NetworkAddr> {
        self.current_network().await
    }
    pub async fn current_network(&self) -> anyhow::Result<NetworkAddr> {
        self.instance
            .as_ref()
            .context("组网实例未启动")?
            .manager
            .current_network()
            .await
    }
    pub fn task_groups(&self) -> TaskGroupManager {
        self.groups.clone()
    }
    pub fn api(&self) -> Option<VntApi> {
        self.instance.as_ref().map(|i| i.manager.vnt_api())
    }
    pub async fn stop(&mut self) {
        if let Some(instance) = self.instance.take() {
            instance.manager.stop().await;
        }
        self.groups.stop_and_wait().await;
        self.subscription_listener = None;
    }
    pub async fn wait_instance_stopped(&mut self) {
        match self.instance.as_mut() {
            Some(instance) => instance.manager.wait_all_stopped().await,
            None => std::future::pending().await,
        }
    }
    async fn effective_config(&mut self) -> anyhow::Result<Config> {
        match self.subscription_listener.as_mut() {
            Some(listener) => {
                let envelope = listener.current_config().await?;
                Ok(merge_present_config(
                    self.local_config.clone(),
                    &envelope.config,
                    &envelope.present,
                ))
            }
            None => Ok(self.local_config.clone()),
        }
    }
    async fn start_instance(&mut self, config: Config) -> anyhow::Result<()> {
        let (tasks, guard) = self.groups.create_task().context("创建任务组失败")?;
        let manager =
            NetworkManager::create_network(Box::new(config), tasks, self.log.clone()).await?;
        self.instance = Some(ManagedInstance {
            manager,
            _guard: guard,
        });
        Ok(())
    }
    async fn rebuild_with(&mut self, config: Config) -> anyhow::Result<()> {
        if let Some(instance) = self.instance.take() {
            instance.manager.stop().await;
        }
        self.groups.stop_and_wait().await;
        self.log.info("重建组网实例");
        self.start_instance(config).await?;
        self.wait_network_ready().await?;
        Ok(())
    }
    pub async fn rebuild(&mut self) -> anyhow::Result<()> {
        let config = self.effective_config().await?;
        self.rebuild_with(config).await
    }
    pub fn inspect_change(&self, change: &RuntimeChange) -> ChangePlan {
        ChangePlan {
            instance_rebuild: self
                .instance
                .as_ref()
                .is_some_and(|i| i.manager.needs_instance_rebuild(change)),
        }
    }
    pub async fn apply_change(&mut self, change: &RuntimeChange) -> anyhow::Result<ChangeOutcome> {
        let mut config = change.config.clone();
        config.normalize()?;
        config.check()?;
        let change = RuntimeChange { config };
        if self.inspect_change(&change).instance_rebuild {
            self.rebuild_with(change.config.clone()).await?;
            if self.subscription_listener.is_none() {
                self.local_config = change.config;
            }
            return Ok(ChangeOutcome::Rebuilt);
        }
        let manager = &mut self.instance.as_mut().context("组网实例未启动")?.manager;
        manager.apply_policy_change(&change);
        if manager.needs_server_change(&change) {
            manager.apply_server_change(&change).await?;
        }
        if self.subscription_listener.is_none() {
            self.local_config = change.config;
        }
        Ok(ChangeOutcome::Applied)
    }
    pub async fn changed(&mut self) -> anyhow::Result<RuntimeChange> {
        Self::changed_with(&mut self.subscription_listener, &self.local_config).await
    }
    async fn changed_with(
        listener: &mut Option<SubscriptionListener>,
        local: &Config,
    ) -> anyhow::Result<RuntimeChange> {
        let Some(listener) = listener.as_mut() else {
            return std::future::pending().await;
        };
        let envelope = listener.changed().await?;
        Ok(RuntimeChange {
            config: merge_present_config(local.clone(), &envelope.config, &envelope.present),
        })
    }
    pub async fn next_event(&mut self) -> anyhow::Result<RuntimeEvent> {
        let stopped = async {
            match self.instance.as_mut() {
                Some(instance) => instance.manager.wait_all_stopped().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = stopped => Ok(RuntimeEvent::InstanceStopped),
            result = Self::changed_with(&mut self.subscription_listener, &self.local_config) => result.map(|c| RuntimeEvent::Changed(Box::new(c))),
        }
    }
}

#[cfg(test)]
mod merge_present_tests {
    use super::merge_present_config;
    use crate::context::config::Config;
    use std::collections::HashSet;

    fn present(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    fn base_config() -> Config {
        Config {
            network_code: "managed-net".to_string(),
            device_id: "managed-dev".to_string(),
            device_name: "managed-name".to_string(),
            ..Config::default()
        }
    }

    #[test]
    fn identity_and_ip_always_come_from_the_envelope() {
        let local = Config {
            network_code: "local-net".to_string(),
            device_id: "local-dev".to_string(),
            device_name: "local-name".to_string(),
            ..Config::default()
        };
        let base = base_config();
        let merged = merge_present_config(local, &base, &present(&[]));
        assert_eq!(merged.network_code, "managed-net");
        assert_eq!(merged.device_id, "managed-dev");
        assert_eq!(merged.device_name, "managed-name");
    }

    #[test]
    fn only_keys_present_in_the_envelope_override_local() {
        let local = Config {
            compress: false,
            no_punch: true,
            password: Some("local-pass".to_string()),
            ..Config::default()
        };
        let mut base = base_config();
        base.compress = true;
        base.password = Some("managed-pass".to_string());
        // 信封只设置了 compress：no_punch 与 password 保持本地值
        let merged = merge_present_config(local.clone(), &base, &present(&["compress"]));
        assert!(merged.compress);
        assert!(merged.no_punch);
        assert_eq!(merged.password.as_deref(), Some("local-pass"));

        // 信封同时设置 password：重建类字段同样覆盖本地
        let merged = merge_present_config(local, &base, &present(&["compress", "password"]));
        assert!(merged.compress);
        assert_eq!(merged.password.as_deref(), Some("managed-pass"));
    }

    #[test]
    fn non_empty_server_list_from_the_envelope_wins() {
        let local = Config {
            server_addr: vec!["tcp://127.0.0.1:29872".parse().unwrap()],
            ..Config::default()
        };
        let mut base = base_config();
        base.server_addr = vec![
            "tcp://127.0.0.1:29872".parse().unwrap(),
            "tcp://127.0.0.1:29873".parse().unwrap(),
        ];
        let merged = merge_present_config(local, &base, &present(&[]));
        assert_eq!(merged.server_addr.len(), 2);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::{net::Ipv4Addr, time::Duration};

    fn config() -> Config {
        Config {
            network_code: "desktop-lifecycle".into(),
            device_id: "test-device".into(),
            connection_id: Some("001".parse().unwrap()),
            no_punch: true,
            tunnel_addr: vec![(Ipv4Addr::LOCALHOST, 0).into()],
            ..Config::default()
        }
    }
    #[tokio::test]
    async fn policy_and_server_updates_keep_the_application_instance() {
        let mut manager =
            RuntimeChangeManager::new(config(), None, Arc::new(InstanceLog::new("runtime-test")))
                .await
                .unwrap();
        assert_eq!(
            manager.wait_network_ready().await.unwrap().ip,
            "001"
                .parse::<crate::connection_id::ConnectionId>()
                .unwrap()
                .transport_key()
        );
        let old_api = manager.api().unwrap();
        let mut config = *old_api.get_config().unwrap();
        config.compress = true;
        config.no_punch = false;
        config.punch_model = vec!["001,IPv4Tcp".parse().unwrap()];
        config.udp_stun = vec!["127.0.0.1:9".into()];
        config.server_addr = vec!["tcp://127.0.0.1:9".parse().unwrap()];
        let change = RuntimeChange { config };
        assert!(!manager.inspect_change(&change).instance_rebuild);
        assert_eq!(
            manager.apply_change(&change).await.unwrap(),
            ChangeOutcome::Applied
        );
        let live = old_api.get_config().unwrap();
        assert!(live.compress && !live.no_punch);
        assert_eq!(live.punch_model, change.config.punch_model);
        assert_eq!(live.udp_stun, change.config.udp_stun);
        assert_eq!(old_api.server_node_list().len(), 1);
        assert_eq!(
            manager.current_config().await.unwrap().server_addr,
            change.config.server_addr
        );
        manager.stop().await;
        assert!(old_api.network().is_none());
    }
    #[tokio::test]
    async fn password_change_rebuilds_once_with_the_requested_snapshot() {
        let mut manager =
            RuntimeChangeManager::new(config(), None, Arc::new(InstanceLog::new("rebuild-test")))
                .await
                .unwrap();
        manager.wait_network_ready().await.unwrap();
        let old_api = manager.api().unwrap();
        let old_endpoint = old_api.application_endpoint().unwrap();
        let mut config = *old_api.get_config().unwrap();
        config.password = Some("new-password".into());
        let change = RuntimeChange { config };
        assert!(manager.inspect_change(&change).instance_rebuild);
        assert_eq!(
            manager.apply_change(&change).await.unwrap(),
            ChangeOutcome::Rebuilt
        );
        assert!(old_api.network().is_none());
        assert!(old_endpoint.accept().await.unwrap().is_none());
        assert_eq!(
            manager.api().unwrap().get_config().unwrap().password,
            change.config.password
        );
        assert!(!manager.inspect_change(&change).instance_rebuild);
        assert_eq!(
            manager.apply_change(&change).await.unwrap(),
            ChangeOutcome::Applied
        );
        let mut invalid = change;
        invalid.config.network_code = "x".repeat(33);
        assert!(manager.apply_change(&invalid).await.is_err());
        assert_eq!(
            manager.api().unwrap().get_config().unwrap().network_code,
            "desktop-lifecycle"
        );
        manager.stop().await;
    }
    #[tokio::test]
    async fn standalone_change_listener_waits_without_a_route_channel() {
        let mut manager =
            RuntimeChangeManager::new(config(), None, Arc::new(InstanceLog::new("listener-test")))
                .await
                .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), manager.changed())
                .await
                .is_err()
        );
        manager.stop().await;
    }
}
