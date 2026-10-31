//! Shared VNT application endpoint and independently framed reliable lanes.
use crate::{
    datagram::OverlayDatagrams,
    session::{BoxStream, Dialer, SessionChannel, SessionListener},
};
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;
use vnt_core::application::ApplicationEndpoint;

#[derive(Clone)]
pub struct Peer {
    pub endpoint: ApplicationEndpoint,
    pub ip: Ipv4Addr,
    pub datagrams: Arc<OverlayDatagrams>,
    pub cancel: CancellationToken,
    pub dialing: Arc<tokio::sync::Mutex<()>>,
}
impl Peer {
    pub async fn open_stream(&self) -> io::Result<BoxStream> {
        let _guard = self.dialing.lock().await;
        if self.cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "VNT stopped"));
        }
        let existing = self
            .datagrams
            .outbound
            .read()
            .await
            .get(&self.ip.into())
            .filter(|c| c.close_reason().is_none())
            .cloned();
        let connection = if let Some(connection) = existing {
            connection
        } else {
            let connection = self
                .endpoint
                .connect(self.ip)
                .await
                .map_err(io::Error::other)?;
            self.datagrams
                .register(connection.clone(), self.cancel.clone())
                .await;
            self.datagrams
                .outbound
                .write()
                .await
                .insert(self.ip.into(), connection.clone());
            connection
        };
        let (send, recv) = connection.open_bi().await.map_err(io::Error::other)?;
        Ok(Box::new(tokio::io::join(recv, send)))
    }
}
pub async fn connect(peer: Peer) -> io::Result<SessionChannel> {
    crate::session::connect_with(Dialer::Overlay(peer)).await
}

pub fn listen(
    endpoint: ApplicationEndpoint,
    datagrams: Arc<OverlayDatagrams>,
    cancel: CancellationToken,
    accepted: tokio::sync::mpsc::Sender<(SessionChannel, SocketAddr)>,
) {
    tokio::spawn(async move {
        let listener = SessionListener::scoped(cancel.clone());
        loop {
            let connection = tokio::select! { _ = cancel.cancelled() => break, result = endpoint.accept() => match result { Ok(Some(connection)) => connection, Ok(None) => break, Err(_) => continue } };
            datagrams.register(connection.clone(), cancel.clone()).await;
            let listener = listener.clone();
            let cancel = cancel.clone();
            let accepted = accepted.clone();
            tokio::spawn(async move {
                let peer = SocketAddr::new(connection.remote_address().ip(), 29875);
                loop {
                    let stream = tokio::select! { _ = cancel.cancelled() => break, result = connection.accept_bi() => match result { Ok(stream) => stream, Err(_) => break } };
                    let listener = listener.clone();
                    let accepted = accepted.clone();
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        tokio::select! { _ = cancel.cancelled() => {}, result = listener.accept_stream(Box::new(tokio::io::join(stream.1, stream.0)), peer) => {
                            if let Ok(Some(channel)) = result { let _ = accepted.send((channel, peer)).await; }
                        } }
                    });
                }
            });
        }
    });
}

pub struct Prepared {
    pub channel: SessionChannel,
    pub media: crate::datagram::Datagram,
    pub peer: SocketAddr,
}
pub async fn prepare(peer: Peer) -> io::Result<Prepared> {
    let address = SocketAddr::from((peer.ip, 29875));
    let media = crate::datagram::Datagram::overlay_peer(peer.datagrams.clone(), address);
    let channel = connect(peer).await?;
    Ok(Prepared {
        channel,
        media,
        peer: address,
    })
}
