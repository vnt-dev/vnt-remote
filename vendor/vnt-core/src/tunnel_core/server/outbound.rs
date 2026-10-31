use crate::context::ServerInfoCollection;
use crate::crypto::PacketCrypto;
use crate::protocol::ip_packet_protocol::NetPacket;
use crate::protocol::transmission::TransmissionBytes;
use anyhow::{Context, bail};
use arc_swap::ArcSwap;
use bytes::Bytes;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;

#[derive(Clone)]
pub(crate) struct ServerOutbound {
    inner: Arc<ArcSwap<ServerOutboundInner>>,
}

struct ServerOutboundInner {
    server_id_list: Arc<Vec<u32>>,
    sender: Arc<HashMap<u32, Sender<(Bytes, Instant)>>>,
    server_info_collection: ServerInfoCollection,
    packet_crypto: PacketCrypto,
}
impl ServerOutbound {
    pub fn new(
        sender: Arc<HashMap<u32, Sender<(Bytes, Instant)>>>,
        server_info_collection: ServerInfoCollection,
        packet_crypto: PacketCrypto,
    ) -> Self {
        let server_id_list = Arc::new(sender.keys().copied().collect());
        Self {
            inner: Arc::new(ArcSwap::from_pointee(ServerOutboundInner {
                server_id_list,
                sender,
                server_info_collection,
                packet_crypto,
            })),
        }
    }

    pub fn exists_route(&self, dest: &Ipv4Addr) -> bool {
        let inner = self.inner.load();
        inner
            .server_info_collection
            .find_ip_to_server(&inner.server_id_list, dest)
            .is_some()
    }

    /// 用新的发送通道表整体替换当前表（运行期服务端增删后发布）。
    pub(crate) fn update_senders(&self, sender: Arc<HashMap<u32, Sender<(Bytes, Instant)>>>) {
        let inner = self.inner.load();
        self.inner.store(Arc::new(ServerOutboundInner {
            server_id_list: Arc::new(sender.keys().copied().collect()),
            sender,
            server_info_collection: inner.server_info_collection.clone(),
            packet_crypto: inner.packet_crypto.clone(),
        }));
    }
    pub fn server_id_list(&self) -> Arc<Vec<u32>> {
        self.inner.load().server_id_list.clone()
    }
    pub fn is_server_connected(&self, server_id: u32) -> bool {
        self.inner
            .load()
            .server_info_collection
            .is_server_connected(server_id)
    }
    pub fn server_instance_id(&self, server_id: u32) -> Option<Vec<u8>> {
        self.inner
            .load()
            .server_info_collection
            .server_instance_id(server_id)
    }
    pub fn is_any_server_connected(&self) -> bool {
        let inner = self.inner.load();
        inner
            .server_info_collection
            .is_any_server_connected(Some(&inner.server_id_list))
    }

    pub fn encrypt_reserve(&self) -> usize {
        self.inner.load().packet_crypto.encrypt_reserve()
    }

    /// Sends a gateway control packet to one connected server. Callers use
    /// this for connection-scoped protocols such as verified subscription ACKs.
    pub async fn send_gateway_to_server(
        &self,
        server_id: u32,
        buf: Bytes,
        expired: Duration,
    ) -> anyhow::Result<bool> {
        let inner = self.inner.load();
        if !inner.server_info_collection.is_server_connected(server_id) {
            return Ok(false);
        }
        let sender = inner
            .sender
            .get(&server_id)
            .with_context(|| format!("未找到服务端 {server_id} 的发送通道"))?;
        match sender
            .send_timeout((buf, Instant::now() + expired), expired)
            .await
        {
            Ok(()) => Ok(true),
            Err(error) => {
                log::warn!("向服务端 {server_id} 发送控制消息失败: {error}");
                Ok(false)
            }
        }
    }

    pub async fn send_to_gateway_expired(
        &self,
        server_id: u32,
        mut buf: NetPacket<TransmissionBytes>,
        expired: Duration,
    ) -> anyhow::Result<()> {
        let inner = self.inner.load();
        if !inner.server_info_collection.is_server_connected(server_id) {
            bail!("未连接服务器")
        }
        buf.set_gateway_flag(true);
        let sender = inner
            .sender
            .get(&server_id)
            .with_context(|| format!("未找到服务端 {server_id} 的发送通道"))?;
        sender
            .send_timeout(
                (
                    buf.into_buffer().into_bytes().freeze(),
                    Instant::now() + expired,
                ),
                expired,
            )
            .await
            .context("connect server task failed")
    }

    pub async fn send(
        &self,
        dest_ip: Ipv4Addr,
        buf: NetPacket<TransmissionBytes>,
    ) -> anyhow::Result<()> {
        self.send_expired(dest_ip, buf, Duration::from_secs(5))
            .await
    }
    pub async fn send_expired(
        &self,
        dest_ip: Ipv4Addr,
        buf: NetPacket<TransmissionBytes>,
        expired: Duration,
    ) -> anyhow::Result<()> {
        let inner = self.inner.load();
        let server_ids = inner
            .server_info_collection
            .servers_for_ip_by_latency(&inner.server_id_list, &dest_ip);
        if server_ids.is_empty() {
            bail!("not found ip route: {dest_ip}")
        }
        Self::send_expired_impl(&inner, &server_ids, buf, expired)
    }
    fn send_expired_impl(
        inner: &ServerOutboundInner,
        server_ids: &[u32],
        mut buf: NetPacket<TransmissionBytes>,
        expired: Duration,
    ) -> anyhow::Result<()> {
        if !buf.is_gateway() {
            inner.packet_crypto.encrypt_in_place(&mut buf)?;
        }
        let bytes = buf.into_buffer().into_bytes().freeze();
        if try_send_server_candidates(&inner.sender, server_ids, bytes, expired) {
            return Ok(());
        }
        bail!("connect server task failed")
    }

    pub async fn send_raw(&self, dest_ip: Ipv4Addr, buf: NetPacket<Bytes>) -> anyhow::Result<()> {
        let inner = self.inner.load();
        let server_ids = inner
            .server_info_collection
            .servers_for_ip_by_latency(&inner.server_id_list, &dest_ip);
        if server_ids.is_empty() {
            bail!("not found ip route: {dest_ip}")
        }
        let expired = Duration::from_secs(5);
        let bytes = buf.into_buffer();
        if try_send_server_candidates(&inner.sender, &server_ids, bytes, expired) {
            return Ok(());
        }
        bail!("connect server task failed")
    }
}

fn try_send_server_candidates(
    senders: &HashMap<u32, Sender<(Bytes, Instant)>>,
    server_ids: &[u32],
    bytes: Bytes,
    expired: Duration,
) -> bool {
    server_ids.iter().any(|server_id| {
        senders.get(server_id).is_some_and(|sender| {
            sender
                .try_send((bytes.clone(), Instant::now() + expired))
                .is_ok()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn packet_send_falls_back_without_duplicate_delivery() {
        let (closed_sender, closed_receiver) = tokio::sync::mpsc::channel(1);
        let (healthy_sender, mut healthy_receiver) = tokio::sync::mpsc::channel(1);
        drop(closed_receiver);
        let senders = HashMap::from([(1, closed_sender), (2, healthy_sender)]);

        assert!(try_send_server_candidates(
            &senders,
            &[1, 2],
            Bytes::from_static(b"one-packet"),
            Duration::from_secs(1),
        ));
        assert_eq!(
            healthy_receiver.recv().await.map(|(bytes, _)| bytes),
            Some(Bytes::from_static(b"one-packet"))
        );
        assert!(healthy_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn packet_send_falls_back_when_the_preferred_queue_is_full() {
        let (busy_sender, mut busy_receiver) = tokio::sync::mpsc::channel(1);
        let (healthy_sender, mut healthy_receiver) = tokio::sync::mpsc::channel(1);
        busy_sender
            .try_send((Bytes::from_static(b"occupied"), Instant::now()))
            .unwrap();
        let senders = HashMap::from([(1, busy_sender), (2, healthy_sender)]);

        assert!(try_send_server_candidates(
            &senders,
            &[1, 2],
            Bytes::from_static(b"fallback"),
            Duration::from_secs(1),
        ));
        assert_eq!(
            busy_receiver.recv().await.map(|(bytes, _)| bytes),
            Some(Bytes::from_static(b"occupied"))
        );
        assert_eq!(
            healthy_receiver.recv().await.map(|(bytes, _)| bytes),
            Some(Bytes::from_static(b"fallback"))
        );
    }
}
