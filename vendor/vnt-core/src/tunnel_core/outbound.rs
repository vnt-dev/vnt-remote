use crate::context::config::{TurnRule, is_turn_ip, turn_ip_for};
use crate::context::{NetworkAddr, SharedNetworkAddr, TrafficStats};
use crate::crypto::PacketCrypto;
use crate::protocol::ip_packet_protocol::{MsgType, NetPacket};
use crate::protocol::transmission::TransmissionBytes;
use crate::runtime_config::RuntimePolicyStore;
use crate::tunnel_core::p2p::outbound::P2pOutbound;
use crate::tunnel_core::server::outbound::ServerOutbound;
use anyhow::bail;
use bytes::Bytes;
use parking_lot::Mutex;
use rustp2p_core::route_table::RouteKey;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

const GRAPH_DEDUP_TTL: Duration = Duration::from_secs(60);
const GRAPH_DEDUP_CAPACITY: usize = 8192;
type GraphMessageKey = (u8, Ipv4Addr, u32);
type GraphSeen = Arc<Mutex<HashMap<GraphMessageKey, Instant>>>;
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum PreferredTurn {
    Server,
    Peer(Ipv4Addr),
}

fn preferred_turn(net: NetworkAddr, rules: &[TurnRule], dest: &Ipv4Addr) -> Option<PreferredTurn> {
    if is_turn_ip(rules, dest) {
        return None;
    }
    let turn_ip = turn_ip_for(rules, dest)?;
    if net.gateway == Some(turn_ip) {
        Some(PreferredTurn::Server)
    } else {
        Some(PreferredTurn::Peer(turn_ip))
    }
}

#[derive(Clone)]
pub(crate) struct BasicOutbound {
    server_outbound: ServerOutbound,
    p2p_outbound: Option<P2pOutbound>,
    packet_crypto: PacketCrypto,
    policy: RuntimePolicyStore,
    graph_seen: GraphSeen,
}

impl BasicOutbound {
    pub fn new(
        server_outbound: ServerOutbound,
        p2p_outbound: Option<P2pOutbound>,
        packet_crypto: PacketCrypto,
        policy: RuntimePolicyStore,
    ) -> Self {
        Self {
            server_outbound,
            p2p_outbound,
            packet_crypto,
            policy,
            graph_seen: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 获取加密保留空间大小
    pub fn encrypt_reserve(&self) -> usize {
        self.packet_crypto.encrypt_reserve()
    }

    /// 加密数据包
    pub fn encrypt_in_place(
        &self,
        packet: &mut NetPacket<TransmissionBytes>,
    ) -> anyhow::Result<()> {
        Ok(self.packet_crypto.encrypt_in_place(packet)?)
    }

    /// 发送原始数据包到指定目标（通过P2P或服务器）
    pub async fn send_raw(
        &self,
        net: NetworkAddr,
        dest: Ipv4Addr,
        packet: NetPacket<TransmissionBytes>,
    ) -> anyhow::Result<()> {
        if !self.try_send_raw(net, dest, packet, None).await? {
            bail!("no route to {dest}")
        }
        Ok(())
    }

    /// Sends one packet using a normal P2P route, a server which currently
    /// advertises the destination, or one deterministic direct peer as an
    /// opportunity-forwarding fallback. `exclude` is the physical ingress
    /// route of a relayed packet; all RouteKeys owned by that peer are skipped.
    pub async fn try_send_raw(
        &self,
        net: NetworkAddr,
        dest: Ipv4Addr,
        packet: NetPacket<TransmissionBytes>,
        exclude: Option<&RouteKey>,
    ) -> anyhow::Result<bool> {
        let policy = self.policy.load();
        let preferred = preferred_turn(net, &policy.turn, &dest);
        let selected_p2p = self.p2p_outbound.as_ref().and_then(|p2p| match preferred {
            Some(PreferredTurn::Peer(turn_ip)) => {
                p2p.get_direct_route_to_peer(dest, turn_ip, exclude)
            }
            Some(PreferredTurn::Server) => None,
            None => p2p.get_route_by_id_excluding(&dest, exclude),
        });

        let packet = packet.into_bytes();
        if let Some(p2p) = self.p2p_outbound.as_ref() {
            if preferred == Some(PreferredTurn::Server) {
                if self.server_outbound.exists_route(&dest) {
                    self.server_outbound.send_raw(dest, packet).await?;
                    return Ok(true);
                }
                return Ok(false);
            }
            if let Some(route) = selected_p2p {
                p2p.send_raw_to(packet, &route.route_key()).await?;
                return Ok(true);
            }
        }
        if self.server_outbound.exists_route(&dest) {
            self.server_outbound.send_raw(dest, packet).await?;
            return Ok(true);
        }
        if let Some(p2p) = self.p2p_outbound.as_ref()
            && let Some((_peer, route)) = p2p.direct_candidate(dest, exclude)
        {
            p2p.send_raw_to(packet, &route.route_key()).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Floods an already encrypted graph-control packet over all direct
    /// tunnels. The ingress tunnel is excluded to prevent immediate
    /// reflection; sequence based deduplication handles graph cycles.
    pub fn flood_direct_p2p(&self, packet: &NetPacket<Bytes>, exclude: Option<&RouteKey>) -> usize {
        self.p2p_outbound
            .as_ref()
            .map(|p2p| p2p.flood_direct(packet, exclude))
            .unwrap_or(0)
    }

    pub fn is_any_server_connected(&self) -> bool {
        self.server_outbound.is_any_server_connected()
    }

    pub fn graph_first_seen(&self, msg_type: MsgType, source: Ipv4Addr, seq: u32) -> bool {
        let now = Instant::now();
        let mut seen = self.graph_seen.lock();
        seen.retain(|_, time| now.duration_since(*time) < GRAPH_DEDUP_TTL);
        let key = (msg_type as u8, source, seq);
        if seen.contains_key(&key) {
            return false;
        }
        if seen.len() >= GRAPH_DEDUP_CAPACITY
            && let Some(oldest) = seen
                .iter()
                .min_by_key(|(_, time)| **time)
                .map(|(key, _)| *key)
        {
            seen.remove(&oldest);
        }
        seen.insert(key, now);
        true
    }

    pub async fn send_encrypted_packet(
        &self,
        net: NetworkAddr,
        dest: Ipv4Addr,
        mut packet: NetPacket<TransmissionBytes>,
    ) -> anyhow::Result<()> {
        if packet.msg_type()? != MsgType::Application {
            packet = self
                .policy
                .load()
                .packet_compression
                .compress(packet, self.encrypt_reserve())?;
        }
        self.packet_crypto.encrypt_in_place(&mut packet)?;
        self.send_raw(net, dest, packet).await
    }
}

#[derive(Clone)]
pub(crate) struct HybridOutbound {
    network: SharedNetworkAddr,
    traffic_stats: TrafficStats,
    basic_outbound: BasicOutbound,
}
impl HybridOutbound {
    pub fn new(
        network: SharedNetworkAddr,
        traffic_stats: TrafficStats,
        basic_outbound: BasicOutbound,
    ) -> Self {
        Self {
            network,
            traffic_stats,
            basic_outbound,
        }
    }
    pub async fn outbound_raw(
        &self,
        dest: Ipv4Addr,
        mut packet: NetPacket<TransmissionBytes>,
    ) -> anyhow::Result<()> {
        let net = self
            .network
            .get()
            .ok_or_else(|| anyhow::anyhow!("Not src ip"))?;
        if packet.src_id() == 0 {
            packet.set_src_id(net.ip.into());
        }
        let len = packet.buffer().len() as u64;
        self.basic_outbound
            .send_encrypted_packet(net, dest, packet)
            .await?;
        self.traffic_stats.record_tx(dest, len);
        Ok(())
    }
}
