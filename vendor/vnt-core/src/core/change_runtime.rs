//! Runtime policy updates and server connection reconciliation.
use super::{NetworkManager, ServerLinks};
use crate::context::NetworkRoute;
use crate::context::config::Config;
use crate::network_info::RuntimeChange;
use crate::runtime_config::RuntimePolicy;
use crate::tunnel_core::server::connection_manager::{
    InboundHandlerConfig, NewServerLink, create_server_manager,
};
use crate::tunnel_core::server::transport::config::ProtocolAddress;
use anyhow::bail;
impl NetworkManager {
    pub fn needs_instance_rebuild(&self, change: &RuntimeChange) -> bool {
        instance_rebuild_fields_differ(&self.config, &change.config)
    }
    pub fn needs_server_change(&self, change: &RuntimeChange) -> bool {
        self.config.server_addr != change.config.server_addr
    }
    pub fn apply_policy_change(&mut self, change: &RuntimeChange) {
        self.runtime_policy
            .store(RuntimePolicy::from_config(&change.config));
        self.config.turn = change.config.turn.clone();
        self.config.punch_model = change.config.punch_model.clone();
        self.config.peer_address = change.config.peer_address.clone();
        self.config.compress = change.config.compress;
        self.config.no_punch = change.config.no_punch;
        self.config.udp_stun = change.config.udp_stun.clone();
        self.config.tcp_stun = change.config.tcp_stun.clone();
        self.app_state.set_config(self.config.clone());
    }
    pub async fn apply_server_change(&mut self, change: &RuntimeChange) -> anyhow::Result<()> {
        let new_addresses = change.config.server_addr.clone();
        if new_addresses == self.config.server_addr {
            return Ok(());
        }
        if self.server_task_group.is_stopped() {
            bail!("服务端连接任务已停止，无法应用地址变更");
        }
        self.config.server_addr = new_addresses.clone();
        self.app_state.set_config(self.config.clone());
        // 阶段一（持锁、无 await）：差分出删除与新增，摘除删除项的登记，
        // 并为新增地址预先构建连接构件
        let (removals, created) = {
            let mut links = self.server_links.lock();
            let removals = links.registry.plan_removals(&new_addresses);
            let added: Vec<ProtocolAddress> = new_addresses
                .iter()
                .filter(|address| !links.registry.is_known(address))
                .cloned()
                .collect();
            let created: Vec<(u32, ProtocolAddress, NewServerLink)> = added
                .into_iter()
                .map(|address| {
                    let server_id = links.registry.next_id();
                    // 单地址配置，让 to_connect_config 以该地址构建连接参数
                    let mut connect_config = (*self.config).clone();
                    connect_config.server_addr = vec![address.clone()];
                    let link = create_server_manager(
                        server_id,
                        &connect_config,
                        links.default_interface.clone(),
                        links.identity.clone(),
                        links.client_instance_id.clone(),
                        links.network.clone(),
                    );
                    (server_id, address, link)
                })
                .collect();
            (removals, created)
        };
        // 阶段二（无锁）：停止已删除地址的连接任务
        for (server_id, address, task) in removals {
            if let Some(task) = task {
                task.stop().await;
            }
            log::info!("已停止服务端连接任务: {server_id} {address}");
        }
        // 阶段三（持锁、无 await）：启动新增连接任务并登记，统一发布
        let mut links = self.server_links.lock();
        for (server_id, address, link) in created {
            let NewServerLink {
                manager,
                sender,
                notifier,
                subscription_verified,
            } = link;
            let task = manager.data_handle_task(
                &self.server_task_group,
                self.server_handler_config(&links),
                false,
            );
            links.registry.add(
                server_id,
                address.clone(),
                sender,
                notifier,
                subscription_verified,
                task,
            );
            log::info!("已为新增服务端创建连接任务: {server_id} {address}");
        }
        self.app_state
            .server_info_collection
            .update_server(links.registry.id_address_pairs());
        self.server_rpc
            .update_server_links(links.registry.publish());
        Ok(())
    }
    fn server_handler_config(&self, links: &ServerLinks) -> Box<InboundHandlerConfig> {
        Box::new(InboundHandlerConfig {
            network_route: NetworkRoute::new(self.app_state.network.clone()),
            server_info: self.app_state.server_info_collection.clone(),
            nat_info: self.app_state.nat_info.clone(),
            peer_map: self.app_state.peer_map.clone(),
            punch_backoff: self.app_state.punch_backoff.clone(),
            puncher: links.puncher.clone(),
            packet_crypto: links.packet_crypto.clone(),
            application_inbound: links.application_inbound.clone(),
            policy: links.policy.clone(),
            basic_outbound: links.basic_outbound.clone(),
            app_state: self.app_state.clone(),
        })
    }
}
fn instance_rebuild_fields_differ(current: &Config, target: &Config) -> bool {
    current.password != target.password
        || current.cert_mode != target.cert_mode
        || current.network_code != target.network_code
        || current.device_id != target.device_id
        || current.device_name != target.device_name
        || current.managed != target.managed
        || current.outbound_interface != target.outbound_interface
        || current.tunnel_addr != target.tunnel_addr
        || current.tunnel_port != target.tunnel_port
        || current.connection_id != target.connection_id
}
