//! One challenge and one response per connection, before any session resources.
use crate::tcp::ControlChannel;
use std::{io, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite};
use vnt_proto::{auth, control::ControlMessage, pb};

pub async fn respond<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut ControlChannel<S>,
    password: &str,
) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        match channel.recv().await? {
            Some(ControlMessage::AuthChallenge(challenge)) => {
                let nonce = challenge
                    .nonce
                    .as_slice()
                    .try_into()
                    .map_err(|_| io::Error::other("认证 nonce 长度无效"))?;
                channel
                    .send(&ControlMessage::AuthResponse(pb::AuthResponse {
                        proof: auth::response(password, nonce),
                    }))
                    .await
            }
            Some(ControlMessage::Denied(d)) => Err(io::Error::other(d.reason)),
            _ => Err(io::Error::other("缺少认证挑战")),
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "认证挑战超时"))?
}

pub async fn challenge<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut ControlChannel<S>,
) -> io::Result<([u8; auth::NONCE_LEN], Vec<u8>)> {
    let nonce = auth::new_nonce().map_err(|e| io::Error::other(e.to_string()))?;
    tokio::time::timeout(Duration::from_secs(10), async {
        channel
            .send(&ControlMessage::AuthChallenge(pb::AuthChallenge {
                nonce: nonce.to_vec(),
            }))
            .await?;
        match channel.recv().await? {
            Some(ControlMessage::AuthResponse(response)) => Ok((nonce, response.proof)),
            _ => Err(io::Error::other("缺少认证响应")),
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "认证响应超时"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authenticates_and_rejects_replayed_response() {
        let (a, b) = tokio::io::duplex(4096);
        let mut host = ControlChannel::new(a);
        let mut client = ControlChannel::new(b);
        let (received, sent) = tokio::join!(challenge(&mut host), respond(&mut client, "密码"));
        sent.unwrap();
        let (nonce, proof) = received.unwrap();
        assert!(auth::verify("密码", &nonce, &proof));
        assert!(!auth::verify("wrong", &nonce, &proof));

        let replay = async {
            assert!(matches!(
                client.recv().await.unwrap(),
                Some(ControlMessage::AuthChallenge(_))
            ));
            client
                .send(&ControlMessage::AuthResponse(pb::AuthResponse { proof }))
                .await
                .unwrap();
        };
        let (received, _) = tokio::join!(challenge(&mut host), replay);
        let (fresh_nonce, replayed) = received.unwrap();
        assert_ne!(nonce, fresh_nonce);
        assert!(!auth::verify("密码", &fresh_nonce, &replayed));
    }

    #[tokio::test]
    async fn rejects_messages_out_of_order_and_malformed_challenge() {
        let (a, b) = tokio::io::duplex(4096);
        let mut host = ControlChannel::new(a);
        let mut client = ControlChannel::new(b);
        host.send(&ControlMessage::Welcome(pb::Welcome::default()))
            .await
            .unwrap();
        assert!(respond(&mut client, "pw").await.is_err());
        host.send(&ControlMessage::AuthChallenge(pb::AuthChallenge {
            nonce: vec![1; 31],
        }))
        .await
        .unwrap();
        assert!(respond(&mut client, "pw").await.is_err());

        let (a, b) = tokio::io::duplex(4096);
        let mut host = ControlChannel::new(a);
        let mut client = ControlChannel::new(b);
        let wrong = async {
            client.recv().await.unwrap();
            client
                .send(&ControlMessage::Hello(pb::Hello::default()))
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(challenge(&mut host), wrong);
        assert!(result.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn unresponsive_peers_time_out() {
        let (a, b) = tokio::io::duplex(4096);
        let mut host = ControlChannel::new(a);
        let mut client = ControlChannel::new(b);
        assert_eq!(
            challenge(&mut host).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        client.recv().await.unwrap();
        assert_eq!(
            respond(&mut client, "pw").await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
