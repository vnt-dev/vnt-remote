use crate::crypto::PacketCrypto;
use crate::tls::verifier::CertValidationMode;
use crate::tunnel_core::server::transport::config::{ConnectRegConfig, ProtocolAddress};
use crate::utils::atomic64::AtomicU64;
use anyhow::{anyhow, bail};
use ipnet::Ipv4Net;
use rustp2p_core::punch::{PunchPolicy, PunchPolicySet};
use rustp2p_core::route_table::Protocol;
use rustp2p_core::socket::LocalInterface;
use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, atomic::Ordering};

pub const MAX_NETWORK_CODE_LEN: usize = 32;
pub const MAX_DEVICE_ID_LEN: usize = 64;
pub const MAX_NAME_LEN: usize = 128;
pub const MAX_VERSION_LEN: usize = 32;

const PUNCH_POLICIES: [PunchPolicy; 4] = [
    PunchPolicy::IPv4Tcp,
    PunchPolicy::IPv4Udp,
    PunchPolicy::IPv6Tcp,
    PunchPolicy::IPv6Udp,
];

#[derive(Debug, Clone)]
pub struct PunchRule {
    target: Ipv4Net,
    policies: PunchPolicySet,
}

impl PartialEq for PunchRule {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
            && PUNCH_POLICIES
                .iter()
                .all(|policy| self.policies.is_match(*policy) == other.policies.is_match(*policy))
    }
}

impl PunchRule {
    pub fn target(&self) -> Ipv4Net {
        self.target
    }

    pub fn policies(&self) -> PunchPolicySet {
        self.policies.clone()
    }

    pub fn matches(&self, ip: &Ipv4Addr) -> bool {
        self.target.contains(ip)
    }

    fn merge(&mut self, other: &Self) {
        for policy in PUNCH_POLICIES {
            if other.policies.is_match(policy) {
                self.policies.or(policy);
            }
        }
    }
}

impl FromStr for PunchRule {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parts = value.split(',').map(str::trim).collect::<Vec<_>>();
        if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
            bail!("invalid punch model rule '{value}', expected connection_id,mode[,mode...]")
        }
        let key = parts[0]
            .parse::<crate::connection_id::ConnectionId>()?
            .transport_key();
        let target = Ipv4Net::new(key, 32)?;
        let mut policies = PunchPolicySet::empty();
        for value in &parts[1..] {
            policies.or(parse_punch_policy(value)?);
        }
        Ok(Self { target, policies })
    }
}

impl Display for PunchRule {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.target.prefix_len() == 32 {
            write!(
                f,
                "{}",
                crate::connection_id::ConnectionId::from_encoded(u32::from(self.target.addr()))
                    .map_err(|_| std::fmt::Error)?
            )?;
        } else {
            write!(f, "{}", self.target)?;
        }
        for policy in PUNCH_POLICIES {
            if self.policies.is_match(policy) {
                write!(f, ",{}", punch_policy_name(policy))?;
            }
        }
        Ok(())
    }
}

fn parse_punch_policy(value: &str) -> anyhow::Result<PunchPolicy> {
    let normalized = value.trim().to_ascii_lowercase().replace(['-', '_'], "");
    match normalized.as_str() {
        "ipv4tcp" => Ok(PunchPolicy::IPv4Tcp),
        "ipv4udp" => Ok(PunchPolicy::IPv4Udp),
        "ipv6tcp" => Ok(PunchPolicy::IPv6Tcp),
        "ipv6udp" => Ok(PunchPolicy::IPv6Udp),
        _ => bail!(
            "invalid punch mode '{value}', expected one of: IPv4Tcp, IPv4Udp, IPv6Tcp, IPv6Udp"
        ),
    }
}

fn punch_policy_name(policy: PunchPolicy) -> &'static str {
    match policy {
        PunchPolicy::IPv4Tcp => "IPv4Tcp",
        PunchPolicy::IPv4Udp => "IPv4Udp",
        PunchPolicy::IPv6Tcp => "IPv6Tcp",
        PunchPolicy::IPv6Udp => "IPv6Udp",
    }
}

pub fn punch_model_for(rules: &[PunchRule], target: &Ipv4Addr) -> PunchPolicySet {
    rules
        .iter()
        .filter(|rule| rule.matches(target))
        .max_by_key(|rule| rule.target.prefix_len())
        .map(PunchRule::policies)
        .unwrap_or_else(PunchPolicySet::all)
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct TurnRule {
    target: Ipv4Net,
    turn_ip: Ipv4Addr,
}

impl TurnRule {
    pub fn target(&self) -> Ipv4Net {
        self.target
    }

    pub fn turn_ip(&self) -> Ipv4Addr {
        self.turn_ip
    }

    pub fn matches(&self, ip: &Ipv4Addr) -> bool {
        self.target.contains(ip)
    }
}

impl FromStr for TurnRule {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parts = value.split(',').map(str::trim).collect::<Vec<_>>();
        if parts.len() != 2 {
            bail!("invalid turn rule '{value}', expected connection_id,relay_connection_id")
        }

        let key = parts[0]
            .parse::<crate::connection_id::ConnectionId>()?
            .transport_key();
        let target = Ipv4Net::new(key, 32)?;
        let turn_ip = parts[1]
            .parse::<crate::connection_id::ConnectionId>()
            .map(|id| id.transport_key())
            .map_err(|error| anyhow::anyhow!("invalid turn IP '{}': {error}", parts[1]))?;

        Ok(Self { target, turn_ip })
    }
}

impl Display for TurnRule {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.target.prefix_len() == 32 {
            write!(
                f,
                "{},{}",
                crate::connection_id::ConnectionId::from_encoded(u32::from(self.target.addr()))
                    .map_err(|_| std::fmt::Error)?,
                crate::connection_id::ConnectionId::from_encoded(u32::from(self.turn_ip))
                    .map_err(|_| std::fmt::Error)?
            )
        } else {
            write!(f, "{},{}", self.target, self.turn_ip)
        }
    }
}

pub fn turn_ip_for(rules: &[TurnRule], target: &Ipv4Addr) -> Option<Ipv4Addr> {
    rules
        .iter()
        .filter(|rule| rule.matches(target))
        .max_by_key(|rule| rule.target.prefix_len())
        .map(TurnRule::turn_ip)
}

pub fn is_turn_ip(rules: &[TurnRule], ip: &Ipv4Addr) -> bool {
    rules.iter().any(|rule| rule.turn_ip == *ip)
}

/// A configured relay remains punchable even when it is covered by a target rule.
pub fn allow_punch(rules: &[TurnRule], ip: &Ipv4Addr) -> bool {
    is_turn_ip(rules, ip) || turn_ip_for(rules, ip).is_none()
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum PeerProtocol {
    Both,
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct PeerAddress {
    kind: PeerAddressKind,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
enum PeerAddressKind {
    Static {
        protocol: PeerProtocol,
        /// host 为 IP 字面量或域名；IPv6 存储时不含方括号。
        host: String,
        port: u16,
    },
    /// `dynamic://` 后的 DNS 域名或 HTTP(S) 地址列表 URL。
    Dynamic { source: String },
}

impl PeerAddress {
    pub fn protocol(&self) -> PeerProtocol {
        match &self.kind {
            PeerAddressKind::Static { protocol, .. } => *protocol,
            // 动态地址的实际协议由解析结果决定。
            PeerAddressKind::Dynamic { .. } => PeerProtocol::Both,
        }
    }

    pub fn is_dynamic(&self) -> bool {
        matches!(self.kind, PeerAddressKind::Dynamic { .. })
    }

    /// 解析动态地址列表。DNS TXT 或 HTTP(S) 响应的每个非空行均为一条普通
    /// peer_address；无效条目仅告警，不影响同一列表中的其他地址。
    pub async fn resolve_dynamic(
        &self,
        default_interface: &Option<LocalInterface>,
    ) -> anyhow::Result<Vec<Self>> {
        let PeerAddressKind::Dynamic { source } = &self.kind else {
            return Ok(vec![self.clone()]);
        };
        let lower_source = source.to_ascii_lowercase();
        let entries: Vec<String> =
            if lower_source.starts_with("http://") || lower_source.starts_with("https://") {
                crate::utils::http_get::http_get_text(source)
                    .await?
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect()
            } else {
                crate::utils::dns_query::dns_query_txt(source, vec![], default_interface)
                    .await?
                    .into_iter()
                    .flat_map(|entry| {
                        entry
                            .lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .collect()
            };

        let mut peers = Vec::new();
        for entry in entries {
            match entry.parse::<PeerAddress>() {
                Ok(peer) if !peer.is_dynamic() => peers.push(peer),
                Ok(_) => log::warn!(
                    "dynamic peer address {self} contains nested dynamic entry {entry:?}; skipping"
                ),
                Err(error) => log::warn!(
                    "invalid dynamic peer address entry {entry:?} from {self}: {error}; skipping"
                ),
            }
        }
        if peers.is_empty() {
            bail!("no valid peer addresses resolved for {self}")
        }
        Ok(peers)
    }

    /// 在使用时解析出实际地址：
    /// - IP 字面量直接使用
    /// - 域名通过 DNS 查询解析（A/AAAA），解析失败返回错误
    pub(crate) async fn endpoints(
        &self,
        default_interface: &Option<LocalInterface>,
    ) -> anyhow::Result<Vec<(Protocol, SocketAddr)>> {
        let PeerAddressKind::Static {
            protocol,
            host,
            port,
        } = &self.kind
        else {
            bail!("dynamic peer address {self} must be resolved before probing")
        };
        let addrs = if let Ok(ip) = host.parse::<IpAddr>() {
            vec![SocketAddr::new(ip, *port)]
        } else {
            crate::utils::dns_query::dns_query_all(host, &vec![], default_interface)
                .await?
                .into_iter()
                .map(|ip| SocketAddr::new(ip, *port))
                .collect::<Vec<_>>()
        };
        let mut endpoints = Vec::with_capacity(addrs.len() * 2);
        for addr in addrs {
            match protocol {
                PeerProtocol::Both => {
                    endpoints.push((Protocol::TCP, addr));
                    endpoints.push((Protocol::UDP, addr));
                }
                PeerProtocol::Tcp => endpoints.push((Protocol::TCP, addr)),
                PeerProtocol::Udp => endpoints.push((Protocol::UDP, addr)),
            }
        }
        Ok(endpoints)
    }
}

impl FromStr for PeerAddress {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let lower = value.to_ascii_lowercase();
        if lower.starts_with("dynamic://") {
            let source = value[10..].trim();
            if source.is_empty() {
                bail!("dynamic peer address source must not be empty")
            }
            return Ok(Self {
                kind: PeerAddressKind::Dynamic {
                    source: source.to_owned(),
                },
            });
        }
        let (protocol, address) = if lower.starts_with("tcp://") {
            (PeerProtocol::Tcp, &value[6..])
        } else if lower.starts_with("udp://") {
            (PeerProtocol::Udp, &value[6..])
        } else if value.contains("://") {
            bail!("invalid peer protocol in '{value}', expected tcp://, udp://, or dynamic://")
        } else {
            (PeerProtocol::Both, value)
        };
        let (host, port) = crate::utils::addr::split_host_port(address)
            .map_err(|error| anyhow!("invalid peer address '{value}': {error}"))?;
        if port == 0 {
            bail!("invalid peer address '{value}': port must not be 0")
        }
        Ok(Self {
            kind: PeerAddressKind::Static {
                protocol,
                host: host.to_string(),
                port,
            },
        })
    }
}

impl Display for PeerAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            PeerAddressKind::Dynamic { source } => write!(f, "dynamic://{source}"),
            PeerAddressKind::Static {
                protocol,
                host,
                port,
            } => {
                let address = if host.contains(':') {
                    format!("[{host}]:{port}")
                } else {
                    format!("{host}:{port}")
                };
                match protocol {
                    PeerProtocol::Both => write!(f, "{address}"),
                    PeerProtocol::Tcp => write!(f, "tcp://{address}"),
                    PeerProtocol::Udp => write!(f, "udp://{address}"),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub server_addr: Vec<ProtocolAddress>,
    pub peer_address: Vec<PeerAddress>,
    pub turn: Vec<TurnRule>,
    pub punch_model: Vec<PunchRule>,
    pub cert_mode: CertValidationMode,
    pub network_code: String,
    pub device_id: String,
    pub device_name: String,
    /// 绑定 VNT 对外通信 Socket 的物理网卡名称。
    pub outbound_interface: Option<String>,
    pub connection_id: Option<crate::connection_id::ConnectionId>,
    pub password: Option<String>,
    pub no_punch: bool,
    pub compress: bool,
    pub udp_stun: Vec<String>,
    pub tcp_stun: Vec<String>,
    /// P2P 隧道监听地址；每个地址族最多一个地址，且必须使用同一端口。
    pub tunnel_addr: Vec<SocketAddr>,
    /// 监听所有地址的指定端口；与 tunnel_addr 互斥。
    pub tunnel_port: Option<u16>,
    /// Opaque server-management credential. Its Debug implementation is
    /// deliberately redacted.
    pub managed: Option<ManagedRegistration>,
}

#[derive(Clone)]
pub struct ManagedRegistration {
    credential_key: Vec<u8>,
    pub network_code: String,
    pub device_id: String,
    pub instance_id: Vec<u8>,
    revision: Arc<AtomicU64>,
}

impl PartialEq for ManagedRegistration {
    fn eq(&self, other: &Self) -> bool {
        // revision 是运行期回执状态而非配置，比较时刻意排除
        self.credential_key == other.credential_key
            && self.network_code == other.network_code
            && self.device_id == other.device_id
            && self.instance_id == other.instance_id
    }
}

impl ManagedRegistration {
    pub fn new(
        credential_key: Vec<u8>,
        network_code: String,
        device_id: String,
        instance_id: Vec<u8>,
        revision: u64,
    ) -> Self {
        Self {
            credential_key,
            network_code,
            device_id,
            instance_id,
            revision: Arc::new(AtomicU64::new(revision)),
        }
    }

    pub(crate) fn create_registration(
        &self,
        _runtime_network_code: &str,
        _runtime_device_id: &str,
    ) -> crate::protocol::control_message::SubscriptionRegistration {
        let client_nonce = crate::managed_config::random_nonce();
        let client_proof = crate::managed_config::client_proof(&self.credential_key, &client_nonce);
        crate::protocol::control_message::SubscriptionRegistration {
            network_code: self.network_code.clone(),
            device_id: self.device_id.clone(),
            client_nonce,
            client_proof,
            instance_id: self.instance_id.clone(),
            applied_revision: self.revision.load(Ordering::Acquire),
        }
    }

    /// 已应用的服务端配置 revision（0 表示尚未应用）。
    pub(crate) fn applied_revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub(crate) fn verify_server_proof(
        &self,
        registration: &crate::protocol::control_message::SubscriptionRegistration,
        proof: &crate::protocol::control_message::SubscriptionServerProof,
    ) -> bool {
        crate::managed_config::verify_server_proof(
            &self.credential_key,
            &registration.client_nonce,
            &proof.server_nonce,
            &proof.server_proof,
        )
    }

    pub(crate) fn mark_applied(&self, revision: u64) {
        self.revision.fetch_max(revision, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for ManagedRegistration {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRegistration")
            .field("credential_key", &"[REDACTED]")
            .field("network_code", &self.network_code)
            .field("device_id", &self.device_id)
            .field("instance_id", &"[REDACTED]")
            .field("revision", &self.revision.load(Ordering::Acquire))
            .finish()
    }
}
impl Config {
    /// 序列化为配置文件（TOML）文本，用于查看实例当前生效的配置。
    ///
    /// 只输出与默认值不同的字段（布尔项仅在为 true 时输出），身份字段
    /// （network_code/device_id）始终输出；订阅托管时把已应用的 revision
    /// 以注释形式标注在头部。键名与配置文件一致（如 server 对应
    /// server_addr）。
    pub fn to_toml_string(&self) -> String {
        let mut table = toml::Table::new();
        table.insert("network_code".into(), self.network_code.clone().into());
        table.insert("device_id".into(), self.device_id.clone().into());
        if !self.device_name.is_empty() {
            table.insert("device_name".into(), self.device_name.clone().into());
        }
        if !self.server_addr.is_empty() {
            table.insert(
                "server".into(),
                toml_string_array(self.server_addr.iter().map(ToString::to_string)),
            );
        }
        if let Some(id) = self.connection_id {
            table.insert("connection_id".into(), id.to_string().into());
        }
        insert_string_array(&mut table, "peer_address", &self.peer_address);
        insert_string_array(&mut table, "turn", &self.turn);
        insert_string_array(&mut table, "punch_model", &self.punch_model);
        if self.cert_mode != CertValidationMode::default() {
            table.insert("cert_mode".into(), self.cert_mode.to_string().into());
        }
        if let Some(password) = &self.password {
            table.insert("password".into(), password.clone().into());
        }
        if let Some(outbound_interface) = &self.outbound_interface {
            table.insert(
                "outbound_interface".into(),
                outbound_interface.clone().into(),
            );
        }

        if let Some(tunnel_port) = self.tunnel_port {
            table.insert("tunnel_port".into(), i64::from(tunnel_port).into());
        }
        insert_string_array(&mut table, "tunnel_addr", &self.tunnel_addr);
        insert_string_array(&mut table, "udp_stun", &self.udp_stun);
        insert_string_array(&mut table, "tcp_stun", &self.tcp_stun);
        for (key, value) in [("no_punch", self.no_punch), ("compress", self.compress)] {
            if value {
                table.insert(key.into(), value.into());
            }
        }

        let mut text = toml::to_string(&table).unwrap_or_default();
        let mut header = String::from(
            "# 当前生效配置（本地配置与服务端下发合并后的结果）
",
        );
        if let Some(managed) = &self.managed {
            header.push_str(&format!(
                "# 服务端管理: 已应用 revision {}
",
                managed.applied_revision()
            ));
        }
        text.insert_str(0, &header);
        text
    }

    pub fn normalize(&mut self) -> anyhow::Result<()> {
        self.check_turn_rules()?;
        self.check_tunnel_addr()?;
        let mut seen = HashSet::new();
        self.turn.retain(|rule| seen.insert(rule.clone()));
        let mut merged = Vec::<PunchRule>::new();
        for rule in self.punch_model.drain(..) {
            if let Some(existing) = merged.iter_mut().find(|item| item.target == rule.target) {
                existing.merge(&rule);
            } else {
                merged.push(rule);
            }
        }
        self.punch_model = merged;
        Ok(())
    }

    fn check_tunnel_addr(&self) -> anyhow::Result<()> {
        if !self.tunnel_addr.is_empty() && self.tunnel_port.is_some() {
            bail!("tunnel_addr and tunnel_port cannot be configured together")
        }
        let mut ipv4 = false;
        let mut ipv6 = false;
        let mut port = None;
        for addr in &self.tunnel_addr {
            let seen = match addr {
                SocketAddr::V4(_) => &mut ipv4,
                SocketAddr::V6(_) => &mut ipv6,
            };
            if *seen {
                bail!("tunnel_addr supports at most one address per IP family")
            }
            *seen = true;
            if let Some(expected) = port
                && expected != addr.port()
            {
                bail!("all tunnel_addr entries must use the same port")
            }
            port = Some(addr.port());
        }
        Ok(())
    }

    fn check_turn_rules(&self) -> anyhow::Result<()> {
        for (index, rule) in self.turn.iter().enumerate() {
            if let Some(conflict) = self.turn[index + 1..]
                .iter()
                .find(|other| other.target == rule.target && other.turn_ip != rule.turn_ip)
            {
                bail!(
                    "conflicting turn rules for {}: {} and {}",
                    rule.target,
                    rule.turn_ip,
                    conflict.turn_ip
                )
            }
        }
        Ok(())
    }

    pub fn check(&self) -> anyhow::Result<()> {
        self.check_tunnel_addr()?;
        if self.connection_id.is_none() {
            bail!("必须指定连接 ID");
        }

        self.check_turn_rules()?;

        if self.network_code.len() > MAX_NETWORK_CODE_LEN {
            bail!(
                "network_code length exceeds {} characters (current: {})",
                MAX_NETWORK_CODE_LEN,
                self.network_code.len()
            )
        }

        if self.device_id.len() > MAX_DEVICE_ID_LEN {
            bail!(
                "device_id length exceeds {} characters (current: {})",
                MAX_DEVICE_ID_LEN,
                self.device_id.len()
            )
        }

        if self.device_name.len() > MAX_NAME_LEN {
            bail!(
                "name length exceeds {} characters (current: {})",
                MAX_NAME_LEN,
                self.device_name.len()
            )
        }

        Ok(())
    }
    pub fn key_sign(&self) -> Option<String> {
        self.password.as_ref().map(|p| PacketCrypto::key_sign(p))
    }
    pub(crate) fn to_connect_config(
        &self,
        index: usize,
        default_interface: Option<rustp2p_core::socket::LocalInterface>,
        network: crate::context::SharedNetworkAddr,
        identity: crate::protocol::client_message::SharedNodeIdentity,
        client_instance_id: std::sync::Arc<Vec<u8>>,
    ) -> ConnectRegConfig {
        ConnectRegConfig {
            server_addr: self.server_addr[index].clone(),
            cert_mode: self.cert_mode.clone(),
            network_code: self.network_code.clone(),
            device_id: self.device_id.clone(),
            identity,
            ip: network,
            key_sign: self.key_sign(),
            ip_variable: false,
            default_interface,
            managed: self.managed.clone(),
            client_instance_id,
        }
    }
}

/// 把实现了 Display 的集合序列化为 TOML 字符串数组。
fn insert_string_array<T: std::fmt::Display>(table: &mut toml::Table, key: &str, values: &[T]) {
    if values.is_empty() {
        return;
    }
    table.insert(
        key.into(),
        toml::Value::Array(
            values
                .iter()
                .map(|value| toml::Value::String(value.to_string()))
                .collect(),
        ),
    );
}

fn toml_string_array<I: IntoIterator<Item = String>>(values: I) -> toml::Value {
    toml::Value::Array(values.into_iter().map(toml::Value::String).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn spawn_http_server(response: String) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        address
    }

    #[test]
    fn connection_ids_reject_ip_and_cidr_configuration() {
        assert!(
            "10.26.0.2"
                .parse::<crate::connection_id::ConnectionId>()
                .is_err()
        );
        assert!(
            "001/24"
                .parse::<crate::connection_id::ConnectionId>()
                .is_err()
        );
    }

    #[test]
    fn managed_registration_reports_latest_locally_applied_revision() {
        let managed = ManagedRegistration::new(
            vec![7; 32],
            "managed-network".into(),
            "managed-device".into(),
            vec![9; 32],
            3,
        );
        assert_eq!(
            managed
                .create_registration("managed-network", "managed-device")
                .applied_revision,
            3
        );

        managed.mark_applied(5);
        assert_eq!(
            managed
                .create_registration("managed-network", "managed-device")
                .applied_revision,
            5
        );

        // Late completion of an older update must never move registration
        // backwards and trigger a stale server catch-up on reconnect.
        managed.mark_applied(4);
        assert_eq!(
            managed
                .create_registration("managed-network", "managed-device")
                .applied_revision,
            5
        );
    }

    #[test]
    fn serverless_requires_a_fixed_virtual_ip() {
        assert!(Config::default().check().is_err());
        let config = Config {
            connection_id: Some("001".parse().unwrap()),
            ..Default::default()
        };
        assert!(config.check().is_ok());
    }

    #[test]
    fn tunnel_addr_validation_accepts_dual_stack_with_one_port() {
        let mut config = Config {
            tunnel_addr: vec![
                "192.168.1.10:29873".parse().unwrap(),
                "[2001:db8::10]:29873".parse().unwrap(),
            ],
            ..Default::default()
        };
        config.normalize().unwrap();
    }

    #[test]
    fn tunnel_addr_validation_rejects_ambiguous_bindings() {
        for addrs in [
            vec![
                "192.168.1.10:29873".parse().unwrap(),
                "192.168.1.11:29873".parse().unwrap(),
            ],
            vec![
                "192.168.1.10:29873".parse().unwrap(),
                "[2001:db8::10]:29874".parse().unwrap(),
            ],
        ] {
            let mut config = Config {
                tunnel_addr: addrs,
                ..Default::default()
            };
            assert!(config.normalize().is_err());
        }

        let mut config = Config {
            tunnel_addr: vec!["192.168.1.10:29873".parse().unwrap()],
            tunnel_port: Some(29873),
            ..Default::default()
        };
        assert!(config.normalize().is_err());
    }

    #[test]
    fn multiple_servers_still_require_a_connection_id() {
        let mut config = Config {
            server_addr: vec!["tcp://127.0.0.1:29872".parse().unwrap()],
            ..Default::default()
        };
        assert!(config.check().is_err());
        config.connection_id = Some("001".parse().unwrap());
        assert!(config.check().is_ok());
    }

    #[tokio::test]
    async fn peer_address_parse_and_display() {
        let both = " 127.0.0.1:29872 ".parse::<PeerAddress>().unwrap();
        assert_eq!(both.protocol(), PeerProtocol::Both);
        assert_eq!(both.to_string(), "127.0.0.1:29872");
        // IP 字面量在使用时直接构建 endpoint，无需 DNS
        let endpoints = both.endpoints(&None).await.unwrap();
        assert_eq!(endpoints.len(), 2);
        assert!(endpoints.iter().any(|(protocol, _)| protocol.is_tcp()));
        assert!(endpoints.iter().any(|(protocol, _)| protocol.is_udp()));

        let tcp = "TCP://127.0.0.1:29872".parse::<PeerAddress>().unwrap();
        assert_eq!(tcp.protocol(), PeerProtocol::Tcp);
        assert_eq!(tcp.to_string(), "tcp://127.0.0.1:29872");

        let udp = "udp://[::1]:29872".parse::<PeerAddress>().unwrap();
        assert_eq!(udp.protocol(), PeerProtocol::Udp);
        assert_eq!(udp.to_string(), "udp://[::1]:29872");

        let dynamic = "DYNAMIC://https://Example.com/Peers?token=AbC"
            .parse::<PeerAddress>()
            .unwrap();
        assert!(dynamic.is_dynamic());
        assert_eq!(dynamic.protocol(), PeerProtocol::Both);
        assert_eq!(
            dynamic.to_string(),
            "dynamic://https://Example.com/Peers?token=AbC"
        );
    }

    #[tokio::test]
    async fn dynamic_peer_address_http_skips_invalid_entries() {
        let body = "127.0.0.1:30001\ntcp://127.0.0.1:30002\nudp://[::1]:30003\nquic://127.0.0.1:30004\ndynamic://nested.example\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let address = spawn_http_server(response).await;
        let dynamic: PeerAddress = format!("dynamic://http://{address}/peers").parse().unwrap();

        let peers = dynamic.resolve_dynamic(&None).await.unwrap();
        assert_eq!(
            peers.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![
                "127.0.0.1:30001",
                "tcp://127.0.0.1:30002",
                "udp://[::1]:30003",
            ]
        );
    }

    #[test]
    fn peer_address_supports_domain_without_resolving() {
        // 域名只做 host:port 拆分，解析时不做 DNS
        let domain = "peer.example.com:29872".parse::<PeerAddress>().unwrap();
        assert_eq!(domain.protocol(), PeerProtocol::Both);
        assert_eq!(domain.to_string(), "peer.example.com:29872");

        let tcp = "tcp://peer.example.com:29873"
            .parse::<PeerAddress>()
            .unwrap();
        assert_eq!(tcp.to_string(), "tcp://peer.example.com:29873");
    }

    #[test]
    fn peer_address_rejects_invalid_values() {
        for value in [
            "quic://127.0.0.1:29872",
            "dynamic://",
            "127.0.0.1",
            "example.com",
            "127.0.0.1:0",
            "example.com:0",
        ] {
            assert!(
                value.parse::<PeerAddress>().is_err(),
                "{value} must be rejected"
            );
        }
    }

    #[test]
    fn punch_rule_parses_id_targets_and_rejects_ip_ranges() {
        let host: PunchRule = "001,ipv4-udp".parse().unwrap();
        assert_eq!(host.to_string(), "001,IPv4Udp");
        assert!(host.policies().is_match(PunchPolicy::IPv4Udp));
        for value in [
            "001",
            "001,",
            "bad,IPv4Udp",
            "001,quic",
            "10.0.0.2,IPv4Udp",
            "001/24,IPv4Tcp",
        ] {
            assert!(value.parse::<PunchRule>().is_err());
        }
    }

    #[test]
    fn punch_rules_merge_only_for_the_same_connection_id() {
        let mut config = Config {
            punch_model: vec![
                "001,IPv4Tcp".parse().unwrap(),
                "0001,IPv4Udp".parse().unwrap(),
                "0001,IPv6Tcp".parse().unwrap(),
            ],
            ..Default::default()
        };
        config.normalize().unwrap();
        assert_eq!(config.punch_model.len(), 2);
        let key = |id: &str| {
            id.parse::<crate::connection_id::ConnectionId>()
                .unwrap()
                .transport_key()
        };
        let policy = punch_model_for(&config.punch_model, &key("0001"));
        assert!(policy.is_match(PunchPolicy::IPv4Udp) && policy.is_match(PunchPolicy::IPv6Tcp));
        assert!(!policy.is_match(PunchPolicy::IPv4Tcp));
        assert!(!punch_model_for(&config.punch_model, &key("001")).is_match(PunchPolicy::IPv4Udp));
        assert!(punch_model_for(&config.punch_model, &key("0")).is_match(PunchPolicy::IPv6Udp));
    }

    #[test]
    fn turn_rules_match_exact_connection_ids_preserving_zeroes() {
        let rules = vec!["001,0001".parse().unwrap(), "0001,0".parse().unwrap()];
        let key = |id: &str| {
            id.parse::<crate::connection_id::ConnectionId>()
                .unwrap()
                .transport_key()
        };
        assert_eq!(turn_ip_for(&rules, &key("001")), Some(key("0001")));
        assert_eq!(turn_ip_for(&rules, &key("0001")), Some(key("0")));
        assert_eq!(turn_ip_for(&rules, &key("00")), None);
        assert_eq!(rules[0].to_string(), "001,0001");
    }

    #[test]
    fn turn_rule_rejects_invalid_values_and_conflicts() {
        for value in ["10.26.0.0/24", "bad,10.26.0.2", "10.26.0.1,bad"] {
            assert!(value.parse::<TurnRule>().is_err(), "{value} must fail");
        }

        let mut config = Config {
            turn: vec!["001,0001".parse().unwrap(), "001,0".parse().unwrap()],
            ..Default::default()
        };
        assert!(config.normalize().is_err());

        config.turn = vec!["001,0001".parse().unwrap(), "001,0001".parse().unwrap()];
        config.normalize().unwrap();
        assert_eq!(config.turn.len(), 1);
    }

    #[test]
    fn configured_relay_remains_punchable() {
        let rules = vec!["001,0001".parse().unwrap()];
        let key = |id: &str| {
            id.parse::<crate::connection_id::ConnectionId>()
                .unwrap()
                .transport_key()
        };
        assert!(!allow_punch(&rules, &key("001")));
        assert!(allow_punch(&rules, &key("0001")));
        assert!(allow_punch(&rules, &key("0")));
    }

    #[test]
    fn to_toml_string_emits_non_default_fields_and_identity() {
        let config = Config {
            network_code: "net".to_string(),
            device_id: "dev".to_string(),
            device_name: "node".to_string(),
            compress: true,
            server_addr: vec!["tcp://127.0.0.1:29872".parse().unwrap()],
            ..Config::default()
        };
        let text = config.to_toml_string();
        assert!(text.contains("network_code = \"net\""), "{text}");
        assert!(text.contains("device_id = \"dev\""), "{text}");
        assert!(text.contains("device_name = \"node\""), "{text}");
        assert!(
            text.contains("server = [\"tcp://127.0.0.1:29872\"]"),
            "{text}"
        );
        assert!(text.contains("compress = true"), "{text}");
        // 默认值字段不输出
        assert!(!text.contains("rtx"), "{text}");
        assert!(!text.contains("no_punch"), "{text}");
        assert!(!text.contains("password"), "{text}");
        // 输出本身是合法 TOML
        assert!(toml::from_str::<toml::Table>(&text).is_ok(), "{text}");
    }
}
