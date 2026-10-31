use crate::application_io::inbound::ApplicationInbound;
use crate::context::config::{allow_punch, punch_model_for};
use crate::context::nat::{MyNatInfo, PunchBackoff};
use crate::context::{NetworkAddr, NetworkRoute, PeerInfoMap, ServerInfoCollection};
use crate::crypto::PacketCrypto;
use crate::protocol::client_message::PunchInfo;
use crate::protocol::control_message::{
    ClientSimpleInfoList, ResponseMessage, SubscriptionConfigEnvelope,
};
use crate::protocol::ip_packet_protocol::{HEAD_LENGTH, MsgType, NetPacket};
use crate::protocol::rpc_message::RpcMessageResponse;
use crate::protocol::transmission::TransmissionBytes;
use crate::runtime_config::RuntimePolicyStore;
use crate::tunnel_core::p2p::transport::punch::NatPuncher;
use crate::tunnel_core::server::rpc::RpcNotifier;
use crate::tunnel_core::server::transport::TransportClient;
use crate::utils::atomic64::AtomicI64;
use anyhow::bail;
use prost::Message;
use rustp2p_core::nat::NatInfo;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct ServerTurnInboundHandler {
    server_id: u32,
    network_route: NetworkRoute,
    server_info: ServerInfoCollection,
    nat_info: MyNatInfo,
    peer_map: PeerInfoMap,
    punch_backoff: PunchBackoff,
    puncher: NatPuncher,
    packet_crypto: PacketCrypto,
    application_inbound: ApplicationInbound,
    policy: RuntimePolicyStore,
    basic_outbound: crate::tunnel_core::outbound::BasicOutbound,
    app_state: crate::context::AppState,
    subscription_verified: Arc<AtomicBool>,
    subscription_identity: Option<(String, String)>,
    last_unverified_push_log: Arc<AtomicI64>,
}

impl ServerTurnInboundHandler {
    pub fn reconcile_server_network(
        &self,
        ip: Ipv4Addr,
        prefix_len: u8,
        gateway: Ipv4Addr,
    ) -> bool {
        self.network_route
            .network
            .reconcile_server(ip, prefix_len, gateway)
    }
    pub fn new(
        server_id: u32,
        config: Box<super::connection_manager::InboundHandlerConfig>,
        subscription_verified: Arc<AtomicBool>,
        subscription_identity: Option<(String, String)>,
    ) -> Self {
        let config = *config;
        Self {
            server_id,
            network_route: config.network_route,
            server_info: config.server_info,
            nat_info: config.nat_info,
            peer_map: config.peer_map,
            punch_backoff: config.punch_backoff,
            puncher: config.puncher,
            packet_crypto: config.packet_crypto,
            application_inbound: config.application_inbound,
            policy: config.policy,
            basic_outbound: config.basic_outbound,
            app_state: config.app_state,
            subscription_verified,
            subscription_identity,
            last_unverified_push_log: Arc::new(AtomicI64::new(0)),
        }
    }
    fn filter_ip(&self, info: NatInfo) -> NatInfo {
        info
    }
    fn get_punch_info(&self, target: Ipv4Addr) -> Option<PunchInfo> {
        self.nat_info.get().map(|info| PunchInfo {
            nat_info: self.filter_ip(info),
            punch_model: punch_model_for(&self.policy.load().punch_model, &target),
        })
    }
    fn update_peer_nat_info(&self, ip: Ipv4Addr, nat_info: NatInfo) {
        if self.peer_map.update_nat_info(ip, nat_info) {
            // 对端 NAT 变化时，把该对端的退避截止时刻压缩到 10 分钟内
            self.punch_backoff.cap(ip);
        }
    }

    pub async fn handle_server_data(
        &self,
        transport_client: &mut TransportClient,
        network_addr: NetworkAddr,
        data: TransmissionBytes,
        rpc_notifier: &RpcNotifier,
        now: i64,
    ) -> anyhow::Result<()> {
        let net_packet = NetPacket::new(data)?;
        let src = Ipv4Addr::from(net_packet.src_id());
        let msg_type = net_packet.msg_type()?;
        let policy = self.policy.load();
        let mut net_packet = policy.packet_compression.decompress(net_packet)?;

        match msg_type {
            MsgType::Ping => {
                net_packet.set_ttl(2);
                net_packet.set_msg_type(MsgType::Pong);
                net_packet.set_src_id(network_addr.ip.into());
                net_packet.set_dest_id(src.into());
                transport_client.send_turn(net_packet).await?;
            }
            MsgType::PongTurn => {
                // 服务端ping 回复，记录延迟
                if net_packet.payload().len() == 8 + 8 {
                    let time = i64::from_be_bytes(net_packet.payload()[..8].try_into()?);
                    // let data_version = u64::from_be_bytes(net_packet.payload()[8..].try_into()?);
                    if now >= time {
                        self.server_info
                            .set_server_rtt(self.server_id, (now - time) as u32);
                    }
                }
            }
            MsgType::PushClientIps => {
                let list = ClientSimpleInfoList::from_slice(net_packet.payload())?;
                let changed = self.server_info.update_client_simple_list(
                    self.server_id,
                    network_addr.ip,
                    list,
                    now,
                );
                for ip in changed {
                    self.punch_backoff.reset(ip);
                }
            }
            MsgType::RpcRes => {
                // 设置rpc响应
                let response = RpcMessageResponse::decode(net_packet.payload())?;
                rpc_notifier.notify_response(response);
            }
            MsgType::FastReg => match ResponseMessage::from_slice(net_packet.payload())? {
                ResponseMessage::FastReg(response) if response.success => {
                    log::info!("服务端 {} 快速注册成功", self.server_id);
                }
                ResponseMessage::FastReg(_) => {
                    log::warn!("服务端 {} 快速注册返回失败", self.server_id);
                }
                ResponseMessage::Error(error) => {
                    log::warn!(
                        "服务端 {} 快速注册失败: {} ({})",
                        self.server_id,
                        error.message,
                        error.code
                    );
                }
                response => {
                    log::warn!(
                        "服务端 {} 返回了非预期的快速注册响应: {response:?}",
                        self.server_id
                    );
                }
            },
            MsgType::SubscriptionConfigPush => {
                if !self.subscription_verified.load(Ordering::Acquire) {
                    let now = crate::utils::time::now_ts_ms();
                    let last = self.last_unverified_push_log.load(Ordering::Relaxed);
                    if now.saturating_sub(last) >= 10_000
                        && self
                            .last_unverified_push_log
                            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        log::warn!(
                            "服务器 {} 未通过订阅链接凭据校验，已忽略其下发的配置",
                            self.server_id
                        );
                    }
                    return Ok(());
                }
                let config = SubscriptionConfigEnvelope::from_slice(net_packet.payload())?;
                if self.subscription_identity.as_ref().is_none_or(|identity| {
                    identity.0 != config.network_code || identity.1 != config.device_id
                }) {
                    log::warn!(
                        "服务器 {} 下发了不匹配的订阅链接管理身份，已忽略",
                        self.server_id
                    );
                    return Ok(());
                }
                self.app_state.push_subscription_config(config);
            }
            _ => {}
        }
        Ok(())
    }
    pub async fn handle_client_data(
        &self,
        network_addr: NetworkAddr,
        transport_client: &mut TransportClient,
        data: TransmissionBytes,
    ) -> anyhow::Result<()> {
        let net_packet = NetPacket::new(data)?;
        let msg_type = net_packet.msg_type()?;
        let graph_raw = if msg_type == MsgType::NodeAnnouncement {
            let mut raw = NetPacket::new(net_packet.source_buf().clone())?;
            raw.decr_ttl();
            Some(raw.into_bytes())
        } else {
            None
        };

        // FEC 外层只做认证，解码后再按每个内层包的类型决定是否 AEAD 解密。

        self.process_inner_packet(network_addr, transport_client, net_packet, graph_raw)
            .await
    }

    async fn process_inner_packet(
        &self,
        network_addr: NetworkAddr,
        transport_client: &mut TransportClient,
        mut net_packet: NetPacket<TransmissionBytes>,
        graph_raw: Option<NetPacket<bytes::Bytes>>,
    ) -> anyhow::Result<()> {
        let msg_type = net_packet.msg_type()?;
        if let Err(e) = self.packet_crypto.decrypt_in_place(&mut net_packet) {
            log::error!(
                "{},msg_type={msg_type:?},src={},dst={}",
                e,
                Ipv4Addr::from(net_packet.src_id()),
                Ipv4Addr::from(net_packet.dest_id())
            );
            return Ok(());
        }
        if let Some(raw) = graph_raw {
            let source = Ipv4Addr::from(net_packet.src_id());
            if msg_type == MsgType::NodeAnnouncement {
                crate::protocol::client_message::NodeAnnouncement::from_slice(
                    net_packet.payload(),
                    source,
                )?;
                if !crate::connection_id::ConnectionId::from_encoded(u32::from(source)).is_ok() {
                    return Ok(());
                }
            }
            if !self
                .basic_outbound
                .graph_first_seen(msg_type, source, net_packet.seq())
            {
                return Ok(());
            }
            if raw.ttl() >= 1 {
                self.basic_outbound.flood_direct_p2p(&raw, None);
            }
        }
        self.process_plain_packet(network_addr, transport_client, net_packet)
            .await
    }

    /// 处理已经完成普通包解密/FEC 解码的原始 NetPacket。
    async fn process_plain_packet(
        &self,
        network_addr: NetworkAddr,
        transport_client: &mut TransportClient,
        net_packet: NetPacket<TransmissionBytes>,
    ) -> anyhow::Result<()> {
        let msg_type = net_packet.msg_type()?;
        let src = Ipv4Addr::from(net_packet.src_id());
        let dest = Ipv4Addr::from(net_packet.dest_id());

        if msg_type == MsgType::Application {
            return self
                .application_inbound
                .inbound(&network_addr, msg_type, src, net_packet)
                .await;
        }

        let net_packet = self
            .policy
            .load()
            .packet_compression
            .decompress(net_packet)?;
        self.process_decompressed_packet(
            network_addr,
            transport_client,
            net_packet,
            msg_type,
            src,
            dest,
        )
        .await
    }

    async fn process_decompressed_packet(
        &self,
        _network_addr: NetworkAddr,
        transport_client: &mut TransportClient,
        net_packet: NetPacket<TransmissionBytes>,
        msg_type: MsgType,
        src: Ipv4Addr,
        dest: Ipv4Addr,
    ) -> anyhow::Result<()> {
        match msg_type {
            MsgType::PunchStart1 => {
                if !allow_punch(&self.policy.load().turn, &src) {
                    log::debug!("ignore configured turn target PunchStart1 from {src}");
                    return Ok(());
                }
                // 对方发起打洞
                let peer_punch_info = PunchInfo::from_slice(net_packet.payload())?;
                let Some(mut self_punch_info) = self.get_punch_info(src) else {
                    return Ok(());
                };
                log::info!(
                    "对方主动发起打洞 对方nat信息={peer_punch_info:?}，自己nat信息={self_punch_info:?} {src}->{dest}"
                );
                self.update_peer_nat_info(src, peer_punch_info.nat_info.clone());
                let effective_policies = self.puncher.punch(src, peer_punch_info)?;
                if let Some(effective_policies) = effective_policies {
                    // 回传双方都支持的请求策略，确保发起端也只打当前缺失的路由类型。
                    self_punch_info.punch_model = effective_policies;
                    let bytes_mut = self_punch_info.encode();
                    let mut net_packet = NetPacket::new(TransmissionBytes::zeroed_size(
                        HEAD_LENGTH + bytes_mut.len(),
                        self.packet_crypto.encrypt_reserve(),
                    ))?;
                    net_packet.set_msg_type(MsgType::PunchStart2);
                    net_packet.set_ttl(2);
                    net_packet.set_src_id(dest.into());
                    net_packet.set_dest_id(src.into());
                    net_packet.set_payload(&bytes_mut)?;
                    self.packet_crypto.encrypt_in_place(&mut net_packet)?;
                    transport_client.send_turn(net_packet).await?;
                } else {
                    log::info!("限制打洞频率")
                }
            }
            MsgType::PunchStart2 => {
                if !allow_punch(&self.policy.load().turn, &src) {
                    log::debug!("ignore configured turn target PunchStart2 from {src}");
                    return Ok(());
                }
                // 对方回复开始打洞
                let peer_punch_info = PunchInfo::from_slice(net_packet.payload())?;
                self.update_peer_nat_info(src, peer_punch_info.nat_info.clone());
                log::info!("对方回复开始打洞 {:?} {src}->{dest}", peer_punch_info);
                self.puncher.punch_uncheck(src, peer_punch_info)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn handle(
        &self,
        transport_client: &mut TransportClient,
        data: TransmissionBytes,
        rpc_notifier: &RpcNotifier,
        now: i64,
    ) -> anyhow::Result<()> {
        let net_packet = NetPacket::new(&data)?;
        if net_packet.is_fec()
            || net_packet.is_ethernet()
            || matches!(
                net_packet.msg_type()?,
                MsgType::Turn
                    | MsgType::Broadcast
                    | MsgType::ExcludeBroadcast
                    | MsgType::TargetBroadcast
                    | MsgType::Quic
                    | MsgType::Ikev2Relay
                    | MsgType::WireGuardRelay
                    | MsgType::SubnetSyncReq
                    | MsgType::SubnetSyncRes
            )
        {
            return Ok(());
        }

        let Some(network_addr) = self.network_route.network.get() else {
            bail!("未找到自身IP")
        };

        if net_packet.is_gateway() {
            // 服务端数据
            return self
                .handle_server_data(transport_client, network_addr, data, rpc_notifier, now)
                .await;
        }
        let dest = Ipv4Addr::from(net_packet.dest_id());
        let graph_message = net_packet.msg_type()? == MsgType::NodeAnnouncement;
        if !graph_message
            && !dest.is_broadcast()
            && !dest.is_unspecified()
            && network_addr.ip != dest
        {
            return Ok(());
        }
        self.handle_client_data(network_addr, transport_client, data)
            .await
    }
    pub async fn handle_ping(
        &self,
        transport_client: &mut TransportClient,
        now: i64,
    ) -> anyhow::Result<()> {
        let mut ping_packet = NetPacket::new(TransmissionBytes::zeroed(HEAD_LENGTH + 8 + 8))?;
        ping_packet.set_ttl(1);
        ping_packet.set_msg_type(MsgType::PingTurn);
        ping_packet.set_gateway_flag(true);
        ping_packet.set_payload(&now.to_be_bytes())?;
        ping_packet.payload_mut()[0..8].copy_from_slice(&now.to_be_bytes());
        ping_packet.payload_mut()[8..]
            .copy_from_slice(&self.server_info.data_version(self.server_id).to_be_bytes());
        transport_client
            .send(ping_packet.into_buffer().into_bytes().freeze())
            .await?;
        Ok(())
    }

    pub fn handle_connected(&self) {
        self.server_info.set_server_connected(self.server_id, true);
        self.server_info
            .set_last_connected_time(self.server_id, Some(crate::utils::time::now_ts_ms()));
        self.server_info.set_disconnected_time(self.server_id, None);
    }
    pub fn set_server_version(&self, version: String) {
        self.server_info.set_server_version(self.server_id, version);
    }
    pub fn set_server_identity(&self, instance_id: Vec<u8>, supported: bool) {
        self.server_info
            .set_server_identity(self.server_id, instance_id, supported);
    }

    pub fn network_addr(&self) -> Option<NetworkAddr> {
        self.network_route.network.get()
    }
    pub fn handle_disconnected(&self) {
        if self.server_info.set_server_connected(self.server_id, false) {
            self.server_info
                .set_disconnected_time(self.server_id, Some(crate::utils::time::now_ts_ms()));
        }
    }
}
