use crate::api::VntApi;
use crate::application_io::inbound::ApplicationInbound;
use crate::context::config::Config;
use crate::context::{AppState, NetworkAddr, NetworkRoute, SharedNetworkAddr};
use crate::crypto::PacketCrypto;
use crate::log_manager::InstanceLog;
use crate::protocol::client_message::{NodeIdentityTemplate, SharedNodeIdentity};
use crate::protocol::control_message::ErrorResponseMsg;
use crate::runtime_config::{RuntimePolicy, RuntimePolicyStore};
use crate::tunnel_core::outbound::{BasicOutbound, HybridOutbound};
use crate::tunnel_core::p2p::inbound::{P2pInboundConfig, P2pInboundHandler};
use crate::tunnel_core::p2p::transport::punch::{NatPuncher, PunchTaskContext, gossip_punch_task};
use crate::tunnel_core::p2p::transport::task::{
    P2pInitConfig, init_tunnel, node_announcement_task,
};
use crate::tunnel_core::server::connection_manager::{
    InboundHandlerConfig, ServerLinkRegistry, ServerTurnManager, create_server_tunnel,
    register_with_first_available, server_addresses,
};
use crate::tunnel_core::server::rpc::ServerRPC;
use crate::utils::task_control::TaskGroup;
use anyhow::{Context, bail};

use parking_lot::Mutex;
use rand::RngExt;
use std::sync::Arc;
pub mod change_runtime;

/// Context for deferred registration
struct RegistrationContext {
    server_task_group: TaskGroup,
    server_links: Arc<Mutex<ServerLinks>>,
    server_managers: Vec<ServerTurnManager>,
    puncher: NatPuncher,
    packet_crypto: PacketCrypto,
    application_inbound: ApplicationInbound,
    policy: RuntimePolicyStore,
    basic_outbound: BasicOutbound,
}

/// 运行期服务端连接登记与其按需重建所需的构件。初始服务端的连接任务
/// 由注册任务挂入，之后可通过 [`NetworkManager::apply_server_change`]
/// 按快照增删。
struct ServerLinks {
    registry: ServerLinkRegistry,
    packet_crypto: PacketCrypto,
    puncher: NatPuncher,
    application_inbound: ApplicationInbound,
    policy: RuntimePolicyStore,
    basic_outbound: BasicOutbound,
    default_interface: Option<rustp2p_core::socket::LocalInterface>,
    identity: SharedNodeIdentity,
    client_instance_id: Arc<Vec<u8>>,
    network: SharedNetworkAddr,
}

impl ServerLinks {
    #[allow(clippy::too_many_arguments)]
    fn new(
        registry: ServerLinkRegistry,
        packet_crypto: PacketCrypto,
        puncher: NatPuncher,
        application_inbound: ApplicationInbound,
        policy: RuntimePolicyStore,
        basic_outbound: BasicOutbound,
        default_interface: Option<rustp2p_core::socket::LocalInterface>,
        identity: SharedNodeIdentity,
        client_instance_id: Arc<Vec<u8>>,
        network: SharedNetworkAddr,
    ) -> Self {
        Self {
            registry,
            packet_crypto,
            puncher,
            application_inbound,
            policy,
            basic_outbound,
            default_interface,
            identity,
            client_instance_id,
            network,
        }
    }
}

pub struct NetworkManager {
    config: Box<Config>,
    app_state: AppState,
    task_group: TaskGroup,
    server_rpc: ServerRPC,
    server_task_group: TaskGroup,
    server_links: Arc<Mutex<ServerLinks>>,
    registration_status: tokio::sync::watch::Receiver<RegistrationStatus>,
    /// 运行期策略存储：apply_policy_change 通过 store() 热更新策略字段
    runtime_policy: RuntimePolicyStore,
}

pub type NetworkInstance = NetworkManager;
enum RegisterResponse {
    /// 注册成功，网段信息已写入 `app_state.network`
    Registered,
    Failed(ErrorResponseMsg),
}

/// `create_network` 启动的后台注册任务状态。注册动作对调用方透明，
/// 等待方只通过该状态拿到「注册成功」或「服务端拒绝」的结果。
enum RegistrationStatus {
    /// 注册进行中（含连接失败重试中）
    Pending,
    /// 注册成功，`app_state.network` 已写入网段信息
    Ready,
    /// 服务端明确拒绝注册，携带错误消息，不再重试
    Failed(String),
}

impl NetworkManager {
    pub async fn stop(mut self) {
        self.task_group.stop();
        self.wait_all_stopped().await;
    }

    /// 创建网络实例，并在实例任务组内后台连接服务器注册。
    /// 建网过程中的错误会写入实例日志。
    pub async fn create_network(
        config: Box<Config>,
        task_group: TaskGroup,
        log: Arc<InstanceLog>,
    ) -> anyhow::Result<NetworkManager> {
        match Self::create_network_impl(config, task_group, log.clone()).await {
            Ok(manager) => Ok(manager),
            Err(error) => {
                log.error(format!("创建网络失败: {error:#}"));
                Err(error)
            }
        }
    }

    async fn create_network_impl(
        mut config: Box<Config>,
        task_group: TaskGroup,
        log: Arc<InstanceLog>,
    ) -> anyhow::Result<NetworkManager> {
        let app_state = AppState::default();
        // 本机 NAT 身份变化（换网/NAT 重启等）时，把所有对端的
        // 打洞退避截止时刻压缩到 10 分钟内；对称 NAT 的端口抖动
        // 不算变化（见 nat_identity_changed）
        let backoff = app_state.punch_backoff.clone();
        app_state.nat_info.set_on_change(move || backoff.cap_all());
        config.normalize()?;
        config.check()?;
        let outbound_interface_name = config
            .outbound_interface
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let resolved_interface =
            crate::utils::socket::resolve_interface(outbound_interface_name.as_deref())?;
        let default_interface = resolved_interface
            .as_ref()
            .map(|interface| interface.socket_interface.clone());
        let canonical_interface_name = resolved_interface
            .as_ref()
            .map(|interface| interface.name.clone());
        if let Some(name) = canonical_interface_name.as_deref() {
            log::info!("绑定出口网卡: {name}");
        }
        let packet_crypto = PacketCrypto::new_from_str(config.password.as_deref())?;
        let runtime_policy = RuntimePolicyStore::new(RuntimePolicy::from_config(&config));
        let node_identity = SharedNodeIdentity::new(NodeIdentityTemplate {
            name: config.device_name.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            network_code: config.network_code.clone(),
            advertised_subnets: Vec::new(),
        });
        let mut instance = vec![0_u8; 32];
        rand::rng().fill(instance.as_mut_slice());
        let client_instance_id = std::sync::Arc::new(instance);
        let (server_manager_list, tunnel_to_server, server_rpc) = create_server_tunnel(
            app_state.clone(),
            &config,
            packet_crypto.clone(),
            default_interface.clone(),
            node_identity.clone(),
            client_instance_id.clone(),
        );
        app_state
            .server_info_collection
            .update_server(server_addresses(&config));
        let server_task_group = task_group.child_scope();
        // Keep the listener resident so no_punch/peer/turn can be changed
        // without replacing the application network instance.
        let (puncher, p2p_socket_manager, p2p_task) = init_tunnel(
            task_group.clone(),
            app_state.clone(),
            tunnel_to_server.clone(),
            packet_crypto.clone(),
            P2pInitConfig {
                tunnel_addr: config.tunnel_addr.clone(),
                tunnel_port: config.tunnel_port,
                automatic_punch: true,
                policy: runtime_policy.clone(),
                default_interface: default_interface.clone(),
                identity: node_identity.clone(),
            },
        )
        .await?;
        let p2p_socket = Some(p2p_socket_manager);
        let p2p_task = Some(p2p_task);
        let puncher = NatPuncher::new(
            app_state.network.clone(),
            app_state.punch_backoff.clone(),
            Some(puncher),
            packet_crypto.clone(),
            runtime_policy.clone(),
            node_identity.get(),
        );

        let basic_outbound = BasicOutbound::new(
            tunnel_to_server.clone(),
            p2p_socket.clone(),
            packet_crypto.clone(),
            runtime_policy.clone(),
        );
        if p2p_socket.is_some() {
            task_group.spawn(node_announcement_task(
                app_state.network.clone(),
                basic_outbound.clone(),
                app_state.route_table.clone(),
                node_identity.clone(),
            ));
        }
        {
            let punch_state = app_state.clone();
            let punch_ctx = PunchTaskContext {
                network: app_state.network.clone(),
                server_info: app_state.server_info_collection.clone(),
                punch_backoff: app_state.punch_backoff.clone(),
                punch_info_getter: std::sync::Arc::new(move |target| {
                    punch_state.get_punch_info(target)
                }),
                policy: runtime_policy.clone(),
                node_info_map: app_state.node_info_map.clone(),
            };
            task_group.spawn(gossip_punch_task(
                basic_outbound.clone(),
                app_state.route_table.clone(),
                punch_ctx,
            ));
        }
        let hybrid_outbound = HybridOutbound::new(
            app_state.network.clone(),
            app_state.traffic_stats.clone(),
            basic_outbound.clone(),
        );
        let endpoint = crate::application::ApplicationEndpoint::new(
            hybrid_outbound,
            task_group.clone(),
            config.password.as_deref().unwrap_or(""),
            app_state.network.clone(),
        )
        .await?;
        let _ = app_state.application.set(endpoint);
        let application_inbound = ApplicationInbound::new(
            app_state.application.clone(),
            app_state.traffic_stats.clone(),
        );

        if let Some(p2p_task) = p2p_task {
            let handler = P2pInboundHandler::new(P2pInboundConfig {
                network_route: NetworkRoute::new(app_state.network.clone()),
                route_table: app_state.route_table.clone(),
                node_info_map: app_state.node_info_map.clone(),
                packet_loss_stats: app_state.packet_loss_stats.clone(),
                packet_crypto: packet_crypto.clone(),
                application_inbound: application_inbound.clone(),
                basic_outbound: basic_outbound.clone(),
                punch_backoff: app_state.punch_backoff.clone(),
                identity: node_identity.clone(),
                policy: runtime_policy.clone(),
                puncher: puncher.clone(),
                punch_info_getter: {
                    let state = app_state.clone();
                    std::sync::Arc::new(move |target| state.get_punch_info(target))
                },
                peer_map: app_state.peer_map.clone(),
            });
            p2p_task.start(handler);
        }

        // 运行期服务端连接登记：初始 server_id 与地址按配置顺序登记，
        // 连接任务由注册任务挂入；之后可通过 apply_server_change 增删。
        let server_link_registry = {
            let mut registry = ServerLinkRegistry::new();
            for (index, address) in config.server_addr.iter().enumerate() {
                registry.insert_initial(index as u32, address.clone());
            }
            registry
        };
        let server_links = Arc::new(Mutex::new(ServerLinks::new(
            server_link_registry,
            packet_crypto.clone(),
            puncher.clone(),
            application_inbound.clone(),
            runtime_policy.clone(),
            basic_outbound.clone(),
            default_interface.clone(),
            node_identity.clone(),
            client_instance_id.clone(),
            app_state.network.clone(),
        )));

        let registration_context = Box::new(RegistrationContext {
            server_task_group: server_task_group.clone(),
            server_links: server_links.clone(),
            server_managers: server_manager_list,
            puncher,
            packet_crypto,
            application_inbound,
            policy: runtime_policy.clone(),
            basic_outbound: basic_outbound.clone(),
        });

        app_state.set_config(config.clone());
        let fixed_ip = config.connection_id;
        let (registration_status, registration_status_rx) =
            tokio::sync::watch::channel(RegistrationStatus::Pending);
        let manager = Self {
            config,
            app_state: app_state.clone(),
            task_group: task_group.clone(),
            server_rpc,
            server_task_group,
            server_links,
            registration_status: registration_status_rx,
            runtime_policy: runtime_policy.clone(),
        };
        // 连接服务器并注册：注册在实例任务组内后台进行，调用方无需关心注册
        // 动作，通过 current_network 获取网段信息即可
        manager.task_group.spawn(async move {
            Self::registration_task(
                app_state,
                registration_context,
                fixed_ip,
                registration_status,
                log,
            )
            .await;
        });
        Ok(manager)
    }

    /// 后台注册任务：连接服务器并注册，把结果写入共享状态与 watch 通道。
    /// 连接级错误按 5 秒间隔重试；服务端明确拒绝时记录错误并不再重试。
    async fn registration_task(
        app_state: AppState,
        mut ctx: Box<RegistrationContext>,
        fixed_ip: Option<crate::connection_id::ConnectionId>,
        status: tokio::sync::watch::Sender<RegistrationStatus>,
        log: Arc<InstanceLog>,
    ) {
        loop {
            match Self::register_impl(&app_state, &mut ctx, fixed_ip).await {
                Ok(RegisterResponse::Registered) => {
                    if let Some(addr) = app_state.network.get() {
                        log.info(format!(
                            "注册成功，连接 ID {}",
                            crate::connection_id::ConnectionId::from_encoded(u32::from(addr.ip))
                                .map(|id| id.to_string())
                                .unwrap_or_default()
                        ));
                    }
                    let _ = status.send(RegistrationStatus::Ready);
                    return;
                }
                Ok(RegisterResponse::Failed(e)) => {
                    log::error!("注册失败: {}", e.message);
                    log.error(format!("注册失败: {}", e.message));
                    let _ = status.send(RegistrationStatus::Failed(e.message));
                    return;
                }
                Err(e) => {
                    log::error!("Register failed: {e:?}, 5 秒后重试");
                    log.warn(format!("连接服务器失败，5 秒后重试: {e:#}"));
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// 获取当前网络（网段信息）。已配置时立即返回；
    /// 否则等待 create_network 启动的后台注册结果。
    pub async fn current_network(&self) -> anyhow::Result<NetworkAddr> {
        if let Some(addr) = self.app_state.network.get() {
            return Ok(addr);
        }
        let mut status = self.registration_status.clone();
        loop {
            match &*status.borrow_and_update() {
                RegistrationStatus::Ready => break,
                RegistrationStatus::Failed(message) => bail!("{message}"),
                RegistrationStatus::Pending => {}
            }
            if status.changed().await.is_err() {
                bail!("网络注册在完成前被停止");
            }
        }
        self.app_state
            .network
            .get()
            .context("network is not registered")
    }

    async fn register_impl(
        app_state: &AppState,
        ctx: &mut RegistrationContext,
        fixed_ip: Option<crate::connection_id::ConnectionId>,
    ) -> anyhow::Result<RegisterResponse> {
        let mut initially_connected_server = None;
        if let Some(fixed_ip) = fixed_ip {
            let addr = NetworkAddr {
                gateway: None,
                broadcast: std::net::Ipv4Addr::UNSPECIFIED,
                ip: fixed_ip.transport_key(),
                prefix_len: 0,
            };
            app_state.network.set(addr);
            log::info!("Local fixed network activated: {fixed_ip}");
        } else {
            let is_multi_server = ctx.server_managers.len() > 1;
            let (server_index, response) = if is_multi_server {
                // A fixed IP lets each server register independently. Start as soon
                // as one server is ready and reconnect the rest in the background.
                log::info!(
                    "Multi-server mode: registering concurrently with {} servers",
                    ctx.server_managers.len()
                );
                register_with_first_available(&mut ctx.server_managers).await?
            } else {
                log::info!("Single-server mode: performing normal registration");
                (
                    0,
                    ctx.server_managers[0]
                        .connect_and_reg(crate::protocol::control_message::RegistrationMode::Normal)
                        .await?,
                )
            };
            let reg_response = match response {
                crate::protocol::control_message::ResponseMessage::Reg(reg) => reg,
                crate::protocol::control_message::ResponseMessage::Error(e) => {
                    return Ok(RegisterResponse::Failed(e));
                }
                crate::protocol::control_message::ResponseMessage::ConfirmReg(_) => {
                    bail!("Unexpected ConfirmReg response");
                }
                crate::protocol::control_message::ResponseMessage::FastReg(_) => {
                    bail!("Unexpected FastReg response");
                }
                crate::protocol::control_message::ResponseMessage::SubscriptionConfig(_) => {
                    bail!("Unexpected subscription configuration response during registration");
                }
                crate::protocol::control_message::ResponseMessage::SubscriptionRegister(_)
                | crate::protocol::control_message::ResponseMessage::SubscriptionPush(_)
                | crate::protocol::control_message::ResponseMessage::SubscriptionPong(_) => {
                    bail!("Unexpected subscription control response during traffic registration");
                }
            };
            let addr = NetworkAddr {
                gateway: None,
                broadcast: std::net::Ipv4Addr::UNSPECIFIED,
                ip: reg_response.ip,
                prefix_len: reg_response.prefix_len,
            };
            app_state.network.set(addr);
            initially_connected_server = Some(server_index);
            if !reg_response.server_version.is_empty() {
                app_state
                    .server_info_collection
                    .set_server_version(server_index as u32, reg_response.server_version);
            }
            app_state.server_info_collection.set_server_identity(
                server_index as u32,
                reg_response.server_instance_id,
                reg_response.multi_link_supported,
            );
        }

        // Start data handling tasks for all servers
        for (server_id, turn_manager) in ctx.server_managers.drain(..).enumerate() {
            let handler_config = Box::new(InboundHandlerConfig {
                network_route: NetworkRoute::new(app_state.network.clone()),
                server_info: app_state.server_info_collection.clone(),
                nat_info: app_state.nat_info.clone(),
                peer_map: app_state.peer_map.clone(),
                punch_backoff: app_state.punch_backoff.clone(),
                puncher: ctx.puncher.clone(),
                packet_crypto: ctx.packet_crypto.clone(),
                application_inbound: ctx.application_inbound.clone(),
                policy: ctx.policy.clone(),
                basic_outbound: ctx.basic_outbound.clone(),
                app_state: app_state.clone(),
            });
            let task = turn_manager.data_handle_task(
                &ctx.server_task_group,
                handler_config,
                initially_connected_server == Some(server_id),
            );
            ctx.server_links
                .lock()
                .registry
                .attach_task(server_id as u32, task);
        }

        Ok(RegisterResponse::Registered)
    }

    fn stop_network(&mut self) {
        self.task_group.stop();
        self.app_state.stop_network();
    }
    pub async fn wait_all_stopped(&mut self) {
        self.task_group.wait_all_stopped().await;
    }
    pub fn vnt_api(&self) -> VntApi {
        VntApi::new(self.app_state.clone(), self.server_rpc.clone())
    }
}

impl Drop for NetworkManager {
    fn drop(&mut self) {
        self.stop_network();
    }
}

#[cfg(test)]
mod decentralized_loopback_tests {
    use super::InstanceLog;
    use crate::context::config::Config;
    use crate::network_info::RuntimeChangeManager;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
    use std::time::Duration;

    fn reserve_loopback_ports(count: usize) -> Vec<u16> {
        let mut listeners = Vec::with_capacity(count);
        while listeners.len() < count {
            let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = udp.local_addr().unwrap().port();
            if let Ok(tcp) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                listeners.push((tcp, udp));
            }
        }
        let ports = listeners
            .iter()
            .map(|(_, udp)| udp.local_addr().unwrap().port())
            .collect();
        drop(listeners);
        ports
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn four_serverless_nodes_form_a_multihop_loopback_graph() {
        let ports = reserve_loopback_ports(4);
        let peers = [ports[1], ports[2], ports[3], ports[2]];
        let mut managers = Vec::new();

        for index in 0..4 {
            let id = format!("00{}", index + 1)
                .parse::<crate::connection_id::ConnectionId>()
                .unwrap();
            let config = Config {
                network_code: "loopback-gossip-test".to_string(),
                device_id: format!("loopback-device-{index}"),
                device_name: format!("node-{}", (b'A' + index as u8) as char),
                connection_id: Some(id),
                password: Some("loopback-gossip-password".to_string()),
                no_punch: true,
                tunnel_addr: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, ports[index]))],
                peer_address: vec![format!("tcp://127.0.0.1:{}", peers[index]).parse().unwrap()],
                ..Config::default()
            };
            // 配置管理器创建并持有组网实例（含路由通道与任务组）
            let manager = RuntimeChangeManager::new(
                config,
                None,
                std::sync::Arc::new(InstanceLog::new("loopback-test")),
            )
            .await
            .unwrap();
            manager.current_network().await.unwrap();
            managers.push(manager);
        }

        // Passive graph discovery waits for the first 25-35 second periodic
        // announcement; route creation no longer triggers an immediate one.
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let a = managers[0].api().unwrap();
                let d = managers[3].api().unwrap();
                let a_to_d = a.find_route(
                    &"004"
                        .parse::<crate::connection_id::ConnectionId>()
                        .unwrap()
                        .transport_key(),
                );
                let d_to_a = d.find_route(
                    &"001"
                        .parse::<crate::connection_id::ConnectionId>()
                        .unwrap()
                        .transport_key(),
                );
                let a_knows_d = a.gossip_node_list().iter().any(|node| {
                    node.ip
                        == "004"
                            .parse::<crate::connection_id::ConnectionId>()
                            .unwrap()
                            .transport_key()
                        && node.name == "node-D"
                });
                if a_to_d.is_some_and(|route| route.metric() == 3)
                    && d_to_a.is_some_and(|route| route.metric() == 3)
                    && a_knows_d
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "loopback graph did not converge: A={:?}, D={:?}",
                managers[0].api().unwrap().route_table(),
                managers[3].api().unwrap().route_table()
            )
        });

        // 管理器 drop 即停止组网实例
        drop(managers);
    }
}
