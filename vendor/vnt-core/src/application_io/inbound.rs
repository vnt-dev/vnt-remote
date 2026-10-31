use crate::context::{NetworkAddr, TrafficStats};
use crate::protocol::ip_packet_protocol::{MsgType, NetPacket};
use crate::protocol::transmission::TransmissionBytes;
use std::{
    net::Ipv4Addr,
    sync::{Arc, OnceLock},
};

#[derive(Clone)]
pub(crate) struct ApplicationInbound {
    endpoint: Arc<OnceLock<crate::application::ApplicationEndpoint>>,
    traffic: TrafficStats,
}
impl ApplicationInbound {
    pub fn new(
        endpoint: Arc<OnceLock<crate::application::ApplicationEndpoint>>,
        traffic: TrafficStats,
    ) -> Self {
        Self { endpoint, traffic }
    }
    pub async fn inbound(
        &self,
        network: &NetworkAddr,
        kind: MsgType,
        source: Ipv4Addr,
        packet: NetPacket<TransmissionBytes>,
    ) -> anyhow::Result<()> {
        let Some(endpoint) = self.endpoint.get() else {
            return Ok(());
        };
        if kind == MsgType::Application
            && packet.dest_id() == u32::from(network.ip)
            && crate::connection_id::ConnectionId::from_encoded(u32::from(source)).is_ok()
            && source != network.ip
        {
            self.traffic.record_rx(source, packet.buffer().len() as u64);
            endpoint.inbound(packet.payload(), source).await?;
        }
        Ok(())
    }
}
