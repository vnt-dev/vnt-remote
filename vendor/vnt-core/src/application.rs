//! Application QUIC carried by VNT packets, without an OS IP interface.
use crate::application_io::{
    quic_inbound::{QuicDataInbound, create_application_inbound},
    quic_outbound::create_outbound,
    socket::ExtendedQuicSocket,
};
use crate::context::SharedNetworkAddr;
use crate::tunnel_core::outbound::HybridOutbound;
use crate::utils::task_control::TaskGroup;
use anyhow::Context;
use bytes::Bytes;
use quinn::{ClientConfig, Endpoint, EndpointConfig, TransportConfig};
use sha2::{Digest, Sha256};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

pub use quinn::{Connection as ApplicationConnection, RecvStream, SendStream};

#[derive(Clone)]
pub struct ApplicationEndpoint {
    endpoint: Endpoint,
    inbound: QuicDataInbound,
    network: SharedNetworkAddr,
}

impl ApplicationEndpoint {
    pub(crate) async fn new(
        outbound: HybridOutbound,
        tasks: TaskGroup,
        password: &str,
        network: SharedNetworkAddr,
    ) -> anyhow::Result<Self> {
        // Use a domain specific to remote application channels.
        let (cert, key) =
            crate::tls::cert::generate_deterministic_cert(&format!("vnt-desktop-v1\0{password}"))?;
        let fingerprint: [u8; 32] = Sha256::digest(cert.as_ref()).into();
        let mut server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)?;
        server_tls.alpn_protocols = vec![b"vnt-desktop/1".to_vec()];
        let mut transport = TransportConfig::default();
        transport.keep_alive_interval(Some(Duration::from_secs(3)));
        transport.max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
        transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
        transport.datagram_send_buffer_size(2 * 1024 * 1024);
        transport
            .initial_mtu(1280)
            .min_mtu(1280)
            .mtu_discovery_config(None);
        let transport = Arc::new(transport);
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls)?,
        ));
        server.transport_config(transport.clone());
        let mut client_tls = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(
                crate::tls::verifier::FingerprintVerifier::new(fingerprint),
            ))
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"vnt-desktop/1".to_vec()];
        let mut client = ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_tls)?,
        ));
        client.transport_config(transport);
        let (inbound, receiver) = create_application_inbound();
        let outbound = create_outbound(tasks.clone(), outbound).await;
        let mut config = EndpointConfig::default();
        config.max_udp_payload_size(1280)?;
        let mut endpoint = Endpoint::new_with_abstract_socket(
            config,
            Some(server),
            Arc::new(ExtendedQuicSocket::new(receiver, outbound)),
            quinn::default_runtime().context("Tokio QUIC runtime unavailable")?,
        )?;
        endpoint.set_default_client_config(client);
        struct Close(Endpoint);
        impl Drop for Close {
            fn drop(&mut self) {
                self.0.close(0u32.into(), b"VNT stopped");
            }
        }
        let close = Close(endpoint.clone());
        tasks.spawn(async move {
            let _close = close;
            std::future::pending::<()>().await;
        });
        Ok(Self {
            endpoint,
            inbound,
            network,
        })
    }

    pub async fn connect(&self, peer: Ipv4Addr) -> anyhow::Result<ApplicationConnection> {
        let network = self.network.get().context("VNT 未连接")?;
        anyhow::ensure!(
            peer != network.ip
                && crate::connection_id::ConnectionId::from_encoded(u32::from(peer)).is_ok(),
            "目标设备不在当前组中"
        );
        tokio::time::timeout(
            Duration::from_secs(10),
            self.endpoint
                .connect(SocketAddr::from((peer, 10000)), "vnt-desktop")?,
        )
        .await
        .context("VNT 应用通道连接超时")?
        .context("VNT 应用通道握手失败，请检查加密密码")
    }

    pub async fn accept(&self) -> anyhow::Result<Option<ApplicationConnection>> {
        let Some(incoming) = self.endpoint.accept().await else {
            return Ok(None);
        };
        Ok(Some(incoming.await?))
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"application stopped");
    }

    pub(crate) async fn inbound(&self, data: &[u8], source: Ipv4Addr) -> anyhow::Result<()> {
        self.inbound
            .send(Bytes::copy_from_slice(data), source)
            .await
    }
}
