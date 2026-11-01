//! 回环 WebSocket 桥：把视频帧从 Rust 进程送进 webview（仅绑 127.0.0.1）。
//!
//! webview 无原始 TCP/UDP socket 能力，Tauri IPC 又走 JSON 序列化不适合
//! 二进制媒体，因此用本地 WS 直连。前端 URL: `ws://127.0.0.1:{port}/?t={token}`。
//!
//! 消息：
//! - 文本帧: JSON（streamInfo / stats）
//! - 二进制帧: `[u8 keyframe][u32 BE pts_ms][u32 BE len][data]`
//!   所有视频帧前置 `[u32 BE stream_id][u32 BE generation]`，各路独立解码。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::{Bytes, Message};

/// 最新流信息（streamInfo JSON）：广播通道对无订阅者丢弃消息，
/// 前端连接前到达的 StreamReset 会丢失，故在此缓存，连接建立时重发。
pub type StreamInfoCache = Arc<Mutex<Option<String>>>;

#[derive(Debug, Clone)]
pub enum BridgeMsg {
    Text(String),
    Video {
        stream_id: u32,
        generation: u32,
        keyframe: bool,
        pts_ms: u32,
        // Takes ownership of the reassembled Vec without copying its payload;
        // broadcast clones share the allocation.
        data: Bytes,
    },
}

impl BridgeMsg {
    pub fn into_ws_message(self) -> Message {
        match self {
            BridgeMsg::Text(t) => Message::Text(t.into()),
            BridgeMsg::Video {
                stream_id,
                generation,
                keyframe,
                pts_ms,
                data,
            } => {
                let mut out = Vec::with_capacity(17 + data.len());
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.extend_from_slice(&generation.to_be_bytes());
                out.push(keyframe as u8);
                out.extend_from_slice(&pts_ms.to_be_bytes());
                out.extend_from_slice(&(data.len() as u32).to_be_bytes());
                out.extend_from_slice(&data);
                Message::Binary(out.into())
            }
        }
    }
}

#[derive(Clone)]
pub struct ScreenView {
    pub label: String,
    pub token: String,
}

/// A detached route remains reserved after its window closes until the host
/// confirms StopVideo. It must never reappear in the owner window.
pub type ScreenViews = Arc<Mutex<HashMap<u32, ScreenView>>>;

fn filter_message(
    message: BridgeMsg,
    views: &ScreenViews,
    stream: Option<u32>,
) -> Option<BridgeMsg> {
    let views = views.lock().unwrap();
    if stream.is_some_and(|id| !views.get(&id).is_some_and(|view| !view.token.is_empty())) {
        return None;
    }
    let owns = |id: u32| stream.map_or(!views.contains_key(&id), |selected| id == selected);
    match message {
        BridgeMsg::Video { stream_id, .. } if stream_id == 0 || !owns(stream_id) => None,
        BridgeMsg::Text(ref text) => {
            let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) else {
                return Some(message);
            };
            if value["type"] == "screensState" && (!views.is_empty() || stream.is_some()) {
                value["detachedScreens"] =
                    serde_json::json!(views.keys().copied().collect::<Vec<_>>());
                if let Some(id) = stream {
                    value["windowStreamId"] = id.into();
                    if let Some(screens) = value["screens"].as_array_mut() {
                        screens.retain(|screen| screen["streamId"].as_u64() == Some(id as u64));
                    }
                }
                if let Some(streams) = value["streams"].as_object_mut() {
                    streams.retain(|id, _| id.parse::<u32>().is_ok_and(owns));
                }
                Some(BridgeMsg::Text(value.to_string()))
            } else if value["streamId"]
                .as_u64()
                .is_some_and(|id| !owns(id as u32))
            {
                None
            } else {
                Some(message)
            }
        }
        _ => Some(message),
    }
}

pub struct BridgeHandle {
    pub port: u16,
    pub token: String,
    pub sender: MediaSender,
    pub views: ScreenViews,
    stream_info: StreamInfoCache,
    /// 关闭信号：watch 让阻塞在 accept() 的任务可被直接唤醒取消，
    /// 否则前端未连接时 shutdown() 只是置位标志，端口不会被释放。
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl BridgeHandle {
    pub fn refresh_views(&self) {
        if let Some(info) = self.stream_info.lock().unwrap().clone() {
            let _ = self.sender.send(BridgeMsg::Text(info));
        }
    }

    pub fn detach(&self, stream_id: u32, label: String) -> Result<String, String> {
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).map_err(|e| e.to_string())?;
        let token = u128::from_be_bytes(entropy).to_string();
        let mut views = self.views.lock().unwrap();
        if views.contains_key(&stream_id) {
            return Err("屏幕已拆分或正在关闭".into());
        }
        views.insert(
            stream_id,
            ScreenView {
                label,
                token: token.clone(),
            },
        );
        Ok(token)
    }

    pub fn close_view(&self, stream_id: u32) {
        if let Some(view) = self.views.lock().unwrap().get_mut(&stream_id) {
            view.token.clear();
            view.label.clear();
        }
        self.refresh_views();
    }

    /// 关闭桥（立即取消 accept 等待并断开前端 WS）。
    pub fn shutdown_sender(&self) -> tokio::sync::watch::Sender<bool> {
        self.shutdown.clone()
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

/// Keep the listener alive across accept errors. Descriptor/resource exhaustion
/// may outlast a single retry, so back off up to 1s instead of killing the bridge
/// or spinning. Cancellation also interrupts the backoff immediately.
async fn accept_with_retry<T, F, Fut>(
    mut accept: F,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let mut consecutive_errors = 0u32;
    loop {
        if *shutdown.borrow() {
            return None;
        }
        let accepted = tokio::select! {
            biased;
            _ = shutdown.changed() => return None,
            result = accept() => result,
        };
        match accepted {
            Ok(connection) => {
                if consecutive_errors > 0 {
                    tracing::info!("视频桥 accept 已恢复（此前连续失败 {consecutive_errors} 次）");
                }
                return Some(connection);
            }
            Err(error) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                let delay_ms = (50u64 << (consecutive_errors - 1).min(5)).min(1000);
                // Log the first error and powers of two to retain diagnostics
                // without flooding logs during prolonged resource exhaustion.
                if consecutive_errors.is_power_of_two() {
                    tracing::warn!(
                        "视频桥 accept 连续失败 {consecutive_errors} 次，{delay_ms}ms 后重试: {error}"
                    );
                }
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => return None,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                }
            }
        }
    }
}

/// 启动回环 WS 桥，返回端口与会话 token。
/// `stream_info` 为最新流信息缓存：每个新连接建立后立即重发。
pub async fn start(stream_info: StreamInfoCache) -> std::io::Result<BridgeHandle> {
    start_with_capacity(stream_info, 2).await
}
pub async fn start_with_capacity(
    stream_info: StreamInfoCache,
    capacity: usize,
) -> std::io::Result<BridgeHandle> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let mut entropy = [0u8; 16];
    getrandom::fill(&mut entropy).map_err(|e| std::io::Error::other(e.to_string()))?;
    let token = u128::from_be_bytes(entropy).to_string();
    // Each encoded frame is capped at 8 MiB by the transport.
    // 容量 2（不是更大的数）：视频是按画面年龄管理延迟的，缓冲越深只是
    // 把旧画面堆在 webview 前面（30fps 下 8 帧约 267ms）。接收端跟不上
    // 时 broadcast 会报 Lagged：清空旧队列，重放流信息让前端重建解码器并请求 IDR。
    // 不能直接续传 delta：丢失视频参考帧或 StreamReset 会破坏解码状态。
    // 2 是下限：1 会在接收端尚未 recv 时就挤掉它本该看到的帧。
    let (tx, _rx) = broadcast::channel::<BridgeMsg>(capacity.max(2));
    let tx_handle = tx.clone();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let expected = token.clone();
    let views: ScreenViews = Default::default();
    let accept_views = views.clone();
    let handle_cache = stream_info.clone();

    tokio::spawn(async move {
        loop {
            let Some((stream, _)) = accept_with_retry(|| listener.accept(), &mut shutdown_rx).await
            else {
                break;
            };
            let expected = expected.clone();
            let views = accept_views.clone();
            let stream_info = stream_info.clone();
            let tx = tx.clone();

            let mut rx = tx.subscribe();
            let mut conn_shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                if *conn_shutdown.borrow() {
                    return;
                }
                let route = Arc::new(Mutex::new(None));
                let accepted_route = route.clone();
                let accepted_views = views.clone();
                #[allow(clippy::result_large_err)]
                // tungstenite requires an HTTP response in this callback.
                let callback = move |req: &Request, resp: Response| {
                    let supplied = req
                        .uri()
                        .query()
                        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("t=")));
                    let child = supplied.and_then(|token| {
                        accepted_views
                            .lock()
                            .unwrap()
                            .iter()
                            .find(|(_, view)| !view.token.is_empty() && view.token == token)
                            .map(|(id, _)| *id)
                    });
                    *accepted_route.lock().unwrap() = child;
                    let ok = supplied == Some(expected.as_str()) || child.is_some();
                    if ok {
                        Ok(resp)
                    } else {
                        let reject = Response::builder()
                            .status(403)
                            .body(Some("forbidden".to_string()))
                            .expect("build 403");
                        Err(reject)
                    }
                };
                // 握手阶段同样必须可取消：TCP 连入但未完成 WS 握手时，
                // shutdown() 应能立即中断并释放端口；5s 超时限制半开连接。
                let handshake = tokio::select! {
                    r = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        tokio_tungstenite::accept_hdr_async(stream, callback),
                    ) => r,
                    _ = conn_shutdown.changed() => return,
                };
                let mut ws = match handshake {
                    Ok(Ok(w)) => w,
                    _ => return, // 握手失败/超时：丢弃该连接
                };
                let route = *route.lock().unwrap();
                // 连接建立：先重发最新流信息（分辨率/编码），再开始转发。
                // 否则连接前到达的 StreamReset 已被广播丢弃，前端沿用旧尺寸
                // （注意：MutexGuard 不得跨 await，先取克隆再发送）
                let cached = { stream_info.lock().unwrap().clone() };
                if let Some(BridgeMsg::Text(info)) =
                    cached.and_then(|info| filter_message(BridgeMsg::Text(info), &views, route))
                {
                    let sent = tokio::select! {
                        result = tokio::time::timeout(std::time::Duration::from_secs(3), ws.send(Message::Text(info.into()))) => matches!(result, Ok(Ok(()))),
                        _ = conn_shutdown.changed() => false,
                    };
                    if !sent {
                        return;
                    }
                }
                let connected_at = tokio::time::Instant::now();
                let mut resets = 0u64;
                tracing::info!(port, "本地视频桥已连接");
                // 连接建立：转发直到断开
                loop {
                    tokio::select! {
                        msg = rx.recv() => {
                            match msg {
                                Ok(m) => {
                                    let Some(m) = filter_message(m, &views, route) else { continue; };
                                    // 发送同样必须可取消：前端停止读取时 TCP 缓冲
                                    // 填满，ws.send() 会永久阻塞导致 shutdown 无效
                                    // （端口不释放）。超时（3s 无进展）视为僵死连接。
                                    tokio::select! {
                                        r = ws.send(m.into_ws_message()) => {
                                            if let Err(error) = r {
                                                tracing::warn!(port, %error, "本地视频桥发送失败");
                                                break;
                                            }
                                        }
                                        _ = conn_shutdown.changed() => break,
                                        _ = tokio::time::sleep(std::time::Duration::from_secs(3)) => {
                                            tracing::warn!(port, "本地视频桥发送停滞超过 3s，断开前端连接");
                                            break;
                                        }
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                    resets += 1;
                                    // Scheduling jitter or a burst of reassembled UDP frames can
                                    // overflow this deliberately shallow queue on a healthy LAN.
                                    // Discard stale frames, then reset the reference chain on the
                                    // same socket. Replaying cached info also recovers a lost reset.
                                    let queued = rx.len();
                                    rx = tx.subscribe();
                                    let reset = stream_info.lock().unwrap().clone()
                                        .unwrap_or_else(|| r#"{"type":"videoReset"}"#.to_string());
                                    let reset=if let Ok(mut value)=serde_json::from_str::<serde_json::Value>(&reset){if value["type"]=="screensState"{value["reset"]=true.into();value.to_string()}else{reset}}else{reset};
                                    if resets.is_power_of_two() {
                                        tracing::warn!(port, resets, skipped, queued,
                                            "本地视频桥积压，原连接内请求关键帧恢复（远程会话未断开）");
                                    }
                                    let Some(BridgeMsg::Text(reset)) = filter_message(BridgeMsg::Text(reset), &views, route) else { continue; };
                                    let sent = tokio::select! {
                                        result = tokio::time::timeout(
                                            std::time::Duration::from_secs(3),
                                            ws.send(Message::Text(reset.into())),
                                        ) => matches!(result, Ok(Ok(()))),
                                        _ = conn_shutdown.changed() => false,
                                    };
                                    if !sent {
                                        tracing::warn!(port, "本地视频桥恢复消息发送失败或取消");
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                        message = ws.next() => {
                            if matches!(message, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) {
                                tracing::info!(port, ?message, "本地视频桥接收端关闭");
                                break;
                            }
                        }
                        _ = conn_shutdown.changed() => {
                            let _ = tokio::time::timeout(std::time::Duration::from_millis(200), ws.send(Message::Close(None))).await;
                            break;
                        }
                    }
                }
                tracing::info!(
                    port,
                    resets,
                    elapsed_ms = connected_at.elapsed().as_millis() as u64,
                    "本地视频桥连接结束"
                );
            });
        }
    });

    Ok(BridgeHandle {
        port,
        token,
        sender: MediaSender {
            broadcast: tx_handle,
            native: None,
        },
        views,
        stream_info: handle_cache,
        shutdown: shutdown_tx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_frame(id: u32) -> BridgeMsg {
        BridgeMsg::Video {
            stream_id: id,
            generation: 1,
            keyframe: true,
            pts_ms: id,
            data: Bytes::from_static(&[42]),
        }
    }

    #[tokio::test]
    async fn concurrent_views_filter_frames_and_survive_an_unfinished_handshake() {
        let cache = serde_json::json!({"type":"screensState","revision":1,"activeStreamId":2,
            "screens":[{"streamId":1},{"streamId":2}],"streams":{"1":{"config":{"streamId":1}},"2":{"config":{"streamId":2}}}}).to_string();
        let bridge = start_with_capacity(Arc::new(Mutex::new(Some(cache))), 16)
            .await
            .unwrap();
        let child_token = bridge.detach(2, "remote-screen-1".into()).unwrap();
        // This bare TCP client used to block every subsequent window.
        let _half_open = tokio::net::TcpStream::connect(("127.0.0.1", bridge.port))
            .await
            .unwrap();
        let url = |token: &str| format!("ws://127.0.0.1:{}/?t={token}", bridge.port);
        let connect = async {
            let (owner, _) = tokio_tungstenite::connect_async(url(&bridge.token))
                .await
                .unwrap();
            let (child, _) = tokio_tungstenite::connect_async(url(&child_token))
                .await
                .unwrap();
            (owner, child)
        };
        let (mut owner, mut child) =
            tokio::time::timeout(std::time::Duration::from_secs(2), connect)
                .await
                .expect("a stalled handshake must not block other views");
        let next = |message: Message| -> serde_json::Value {
            serde_json::from_str(message.to_text().unwrap()).unwrap()
        };
        let owner_state = next(owner.next().await.unwrap().unwrap());
        let child_state = next(child.next().await.unwrap().unwrap());
        assert_eq!(owner_state["detachedScreens"], serde_json::json!([2]));
        assert!(owner_state["streams"].get("2").is_none());
        assert_eq!(child_state["screens"], serde_json::json!([{"streamId":2}]));
        assert!(child_state["streams"].get("1").is_none());
        assert_eq!(child_state["activeStreamId"], 2);
        for id in [2, 1] {
            bridge.sender.send(screen_frame(id)).unwrap();
        }
        for (socket, id) in [(&mut owner, 1u32), (&mut child, 2u32)] {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let Message::Binary(data) = frame else {
                panic!("expected video");
            };
            assert_eq!(u32::from_be_bytes(data[..4].try_into().unwrap()), id);
        }
        child.close(None).await.unwrap();
        bridge.close_view(2);
        // Reserved route is never reintroduced into the owner's media stream.
        assert!(filter_message(screen_frame(2), &bridge.views, None).is_none());
        assert!(filter_message(screen_frame(2), &bridge.views, Some(2)).is_none());
        assert!(bridge.detach(2, "duplicate".into()).is_err());
        assert!(tokio_tungstenite::connect_async(url(&child_token))
            .await
            .is_err());
        bridge.sender.send(screen_frame(1)).unwrap();
        let live = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(Ok(Message::Binary(_))) = owner.next().await {
                    break;
                }
            }
        })
        .await;
        assert!(
            live.is_ok(),
            "closing a child must leave the owner bridge alive"
        );
        bridge.shutdown();
    }

    #[test]
    fn each_view_replays_only_its_own_configuration_after_lag() {
        let views: ScreenViews = Default::default();
        views.lock().unwrap().insert(
            2,
            ScreenView {
                label: "child".into(),
                token: "token".into(),
            },
        );
        let reset = serde_json::json!({"type":"screensState","revision":5,"activeStreamId":1,"reset":true,
            "screens":[{"streamId":1},{"streamId":2}],"streams":{"1":{"config":{"streamId":1}},"2":{"config":{"streamId":2}}}});
        let Some(BridgeMsg::Text(text)) =
            filter_message(BridgeMsg::Text(reset.to_string()), &views, Some(2))
        else {
            panic!("missing reset");
        };
        let replay: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(replay["reset"], true);
        assert_eq!(
            replay["activeStreamId"], 1,
            "input ownership is independent of the child view"
        );
        assert_eq!(replay["windowStreamId"], 2);
        assert_eq!(replay["screens"].as_array().unwrap().len(), 1);
        assert!(replay["streams"].get("1").is_none());
        assert!(filter_message(
            BridgeMsg::Text(r#"{"type":"streamInfo","streamId":2}"#.into()),
            &views,
            None
        )
        .is_none());
        assert!(filter_message(
            BridgeMsg::Text(r#"{"type":"screenStats","streamId":1}"#.into()),
            &views,
            Some(2)
        )
        .is_none());
    }

    #[test]
    fn video_payload_owns_reassembled_allocation_and_broadcast_clones_share_it() {
        let data = vec![7u8; 8 * 1024 * 1024];
        let allocation = data.as_ptr();
        let message = BridgeMsg::Video {
            stream_id: 1,
            generation: 1,
            keyframe: true,
            pts_ms: 0x01020304,
            data: data.into(),
        };
        let cloned = message.clone();
        for value in [&message, &cloned] {
            let BridgeMsg::Video { data, .. } = value else {
                panic!("expected video");
            };
            assert_eq!(data.as_ptr(), allocation);
        }
        let Message::Binary(wire) = cloned.into_ws_message() else {
            panic!("expected binary message");
        };
        assert_eq!(&wire[..8], &[0, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(&wire[8..17], &[1, 1, 2, 3, 4, 0, 128, 0, 0]);
        assert_eq!(wire.len(), 17 + 8 * 1024 * 1024);
        assert!(wire[17..].iter().all(|byte| *byte == 7));
    }

    #[tokio::test(start_paused = true)]
    async fn accept_recovers_from_errors_on_the_same_listener_and_resets_backoff() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let _first_client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (_shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let mut outcomes = std::collections::VecDeque::from([false, false, true, false, true]);
        let mut accept = || {
            let succeed = outcomes.pop_front().expect("unexpected accept attempt");
            let listener = &listener;
            async move {
                if succeed {
                    listener.accept().await
                } else {
                    // Fault injection leaves the real listener and port bound.
                    assert!(tokio::net::TcpListener::bind(address).await.is_err());
                    Err(std::io::Error::other("simulated descriptor exhaustion"))
                }
            }
        };
        let start = tokio::time::Instant::now();
        let (stream, _) = accept_with_retry(&mut accept, &mut shutdown_rx)
            .await
            .unwrap();
        assert_eq!(stream.local_addr().unwrap(), address);
        assert_eq!(start.elapsed(), std::time::Duration::from_millis(150));
        let _second_client = tokio::net::TcpStream::connect(address).await.unwrap();
        let start = tokio::time::Instant::now();
        let (stream, _) = accept_with_retry(&mut accept, &mut shutdown_rx)
            .await
            .unwrap();
        assert_eq!(stream.local_addr().unwrap(), address);
        assert_eq!(start.elapsed(), std::time::Duration::from_millis(50));
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_accept_errors_back_off_with_a_cap_and_shutdown_interrupts_the_wait() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let attempts = Arc::new(AtomicU32::new(0));
        let count = attempts.clone();
        let task = tokio::spawn(async move {
            let result = accept_with_retry(
                || {
                    count.fetch_add(1, Ordering::Relaxed);
                    std::future::ready(Err::<(), _>(std::io::Error::other("resources unavailable")))
                },
                &mut shutdown_rx,
            )
            .await;
            drop(listener);
            result
        });
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        for (index, delay_ms) in [50, 100, 200, 400, 800, 1000, 1000].into_iter().enumerate() {
            tokio::time::advance(std::time::Duration::from_millis(delay_ms - 1)).await;
            assert_eq!(attempts.load(Ordering::Relaxed), index as u32 + 1);
            tokio::time::advance(std::time::Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
            assert_eq!(attempts.load(Ordering::Relaxed), index as u32 + 2);
            assert!(!task.is_finished());
        }
        let now = tokio::time::Instant::now();
        shutdown.send(true).unwrap();
        assert!(task.await.unwrap().is_none());
        assert_eq!(now.elapsed(), std::time::Duration::ZERO);
        assert!(tokio::net::TcpListener::bind(address).await.is_ok());
    }

    #[tokio::test]
    async fn already_shutdown_bridge_does_not_attempt_accept() {
        let (_shutdown, mut shutdown_rx) = tokio::sync::watch::channel(true);
        let attempts = std::cell::Cell::new(0);
        assert!(accept_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                std::future::ready(Ok::<(), std::io::Error>(()))
            },
            &mut shutdown_rx
        )
        .await
        .is_none());
        assert_eq!(attempts.get(), 0);
    }

    #[tokio::test]
    async fn lost_broadcast_frames_reset_on_the_same_socket_and_replay_latest_info() {
        check_backlog_recovery(Some(r#"{"type":"streamInfo","width":64}"#.to_string())).await;
    }

    #[tokio::test]
    async fn lost_broadcast_frames_reset_without_cached_info() {
        check_backlog_recovery(None).await;
    }

    async fn check_backlog_recovery(cached: Option<String>) {
        let cache = Arc::new(Mutex::new(cached.clone()));
        let bridge = start(cache.clone()).await.unwrap();
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", bridge.port))
            .await
            .unwrap();
        // Wait for subscription before finishing the WebSocket handshake, so the
        // broadcast queue can be overflowed deterministically without sleeps.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while bridge.sender.receiver_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for pts_ms in 0..4 {
            bridge
                .sender
                .send(BridgeMsg::Video {
                    stream_id: 1,
                    generation: 1,
                    keyframe: pts_ms == 0,
                    pts_ms,
                    data: Bytes::from_static(&[7u8]),
                })
                .unwrap();
        }
        let url = format!("ws://127.0.0.1:{}/?t={}", bridge.port, bridge.token);
        let (mut ws, _) = tokio_tungstenite::client_async(url, stream).await.unwrap();
        if let Some(info) = &cached {
            let next = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(next, Message::Text(info.clone().into()));
        }
        let reset = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            reset,
            Message::Text(
                cached
                    .unwrap_or_else(|| r#"{"type":"videoReset"}"#.to_string())
                    .into()
            )
        );
        // No stale delta frames may precede the recovery keyframe. A new
        // StreamReset (e.g. monitor resize) must still reach this same socket.
        bridge
            .sender
            .send(BridgeMsg::Text(
                r#"{"type":"streamInfo","width":128}"#.into(),
            ))
            .unwrap();
        let info = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            info,
            Message::Text(r#"{"type":"streamInfo","width":128}"#.into())
        );
        bridge
            .sender
            .send(BridgeMsg::Video {
                stream_id: 1,
                generation: 1,
                keyframe: true,
                pts_ms: 100,
                data: Bytes::from_static(&[42]),
            })
            .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(frame, Message::Binary(data) if data[8] == 1 && data[17] == 42));
        bridge.shutdown();
    }

    /// 回归：前端从未连接时 shutdown()，监听端口必须释放
    #[tokio::test]
    async fn shutdown_without_client_releases_port() {
        let bridge = start(std::sync::Arc::new(std::sync::Mutex::new(None)))
            .await
            .expect("启动桥");
        let port = bridge.port;
        bridge.shutdown();
        // 给任务一瞬处理取消；随后端口应可重新绑定
        for _ in 0..50 {
            if tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("shutdown 后端口 {port} 未释放");
    }

    /// 回归：TCP 连入但未完成 WS 握手时 shutdown()，端口必须释放
    #[tokio::test]
    async fn shutdown_during_handshake_releases_port() {
        let bridge = start(std::sync::Arc::new(std::sync::Mutex::new(None)))
            .await
            .expect("启动桥");
        let port = bridge.port;
        // 裸 TCP 连入，不做任何 WS 握手
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("TCP 连接");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        bridge.shutdown();
        for _ in 0..50 {
            if tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .is_ok()
            {
                drop(stream);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        drop(stream);
        panic!("半握手连接下 shutdown 后端口 {port} 未释放");
    }

    /// 回归：连接前到达的 StreamReset（广播无订阅者被丢弃）不得丢失——
    /// 新连接建立后必须收到缓存的最新流信息。
    #[tokio::test(flavor = "multi_thread")]
    async fn stream_info_replayed_on_connect() {
        let cache: StreamInfoCache = std::sync::Arc::new(std::sync::Mutex::new(Some(
            r#"{"type":"streamInfo","width":64,"height":48}"#.to_string(),
        )));
        let bridge = start(cache.clone()).await.expect("启动桥");
        let url = format!("ws://127.0.0.1:{}/?t={}", bridge.port, bridge.token);

        // 无订阅者期间更新（模拟 StreamReset 先于前端连接到达）
        *cache.lock().unwrap() =
            Some(r#"{"type":"streamInfo","width":128,"height":96}"#.to_string());

        use futures_util::StreamExt;
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("连接桥");
        let first = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
            .await
            .expect("应收到重发的流信息")
            .expect("流错误")
            .expect("连接关闭");
        match first {
            Message::Text(t) => {
                assert!(t.contains("128"), "应重发最新尺寸 128x96，实际: {t}");
            }
            other => panic!("期望 Text 流信息，得到 {other:?}"),
        }
        let _ = ws.close(None).await;
        bridge.shutdown();
    }

    /// 回归：前端连接后停止读取，发送缓冲填满、任务阻塞在 ws.send()，
    /// 此时 shutdown() 必须仍能生效（端口释放）。
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_with_stalled_reader() {
        let bridge = start(std::sync::Arc::new(std::sync::Mutex::new(None)))
            .await
            .expect("启动桥");
        let port = bridge.port;
        let url = format!("ws://127.0.0.1:{}/?t={}", port, bridge.token);
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("连接桥");
        // 连接后完全不读取，持续灌数据把 TCP 缓冲填满
        let sender = bridge.sender.clone();
        let filler = tokio::spawn(async move {
            let big = "x".repeat(256 * 1024);
            for _ in 0..200 {
                if sender.send(BridgeMsg::Text(big.clone())).is_err() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        bridge.shutdown();
        for _ in 0..100 {
            if tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .is_ok()
            {
                filler.abort();
                let _ = ws.close(None).await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        filler.abort();
        let _ = ws.close(None).await;
        panic!("停滞读端下 shutdown 后端口 {port} 未释放");
    }

    #[tokio::test]
    async fn shutdown_with_connected_client() {
        let bridge = start(std::sync::Arc::new(std::sync::Mutex::new(None)))
            .await
            .expect("启动桥");
        let url = format!("ws://127.0.0.1:{}/?t={}", bridge.port, bridge.token);
        let (ws, _) = tokio_tungstenite::connect_async(url).await.expect("连接桥");
        let mut ws = ws;
        bridge.shutdown(); // 先关闭，再等关闭帧
        let closed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            use futures_util::StreamExt;
            while let Some(msg) = ws.next().await {
                if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(closed, "shutdown 后已连接的 WS 应收到关闭");
    }
}

/// Called synchronously by the receiver; implementations must enqueue without blocking.
pub trait MediaSink: Send + Sync + 'static {
    fn send(&self, message: BridgeMsg);
}
#[derive(Clone)]
pub struct MediaSender {
    broadcast: broadcast::Sender<BridgeMsg>,
    native: Option<Arc<dyn MediaSink>>,
}
impl MediaSender {
    pub fn send(
        &self,
        message: BridgeMsg,
    ) -> Result<usize, broadcast::error::SendError<BridgeMsg>> {
        if let Some(native) = &self.native {
            native.send(message);
            Ok(1)
        } else {
            self.broadcast.send(message)
        }
    }
}
impl std::ops::Deref for MediaSender {
    type Target = broadcast::Sender<BridgeMsg>;
    fn deref(&self) -> &Self::Target {
        &self.broadcast
    }
}
pub fn native(stream_info: StreamInfoCache, sink: Arc<dyn MediaSink>) -> BridgeHandle {
    let (tx, _) = broadcast::channel(2);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    if let Some(info) = stream_info.lock().unwrap().clone() {
        sink.send(BridgeMsg::Text(info));
    }
    BridgeHandle {
        port: 0,
        token: String::new(),
        sender: MediaSender {
            broadcast: tx,
            native: Some(sink),
        },
        views: Default::default(),
        stream_info,
        shutdown,
    }
}
