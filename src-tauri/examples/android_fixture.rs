//! Synthetic LAN fixture: never captures the screen, injects input, or changes the clipboard.
use std::{sync::Arc, time::Duration};
#[tokio::main]
async fn main() {
    let clipboard = vnt_host::clipboard::ClipboardService::with_text_paster(
        Arc::new(|_, _| {}),
        Arc::new(|text, valid| {
            if !valid() {
                return Err("synthetic cancelled".into());
            }
            if text == "reject-paste" {
                Err("synthetic paste failure".into())
            } else {
                Ok(())
            }
        }),
    );
    let server = vnt_host::server::start(
        Arc::new(vnt_host::NoopEventSink),
        vnt_host::server::ServerConfig {
            port: 39875,
            password: "android-test".into(),
            monitor_index: 0,
            region: None,
            fps: 60,
            bitrate_kbps: 8000,
            hardware_encode: false,
            audio: true,
            view_only: false,
            clipboard,
            synthetic: true,
        },
    )
    .await
    .expect("synthetic server");
    println!(
        "Synthetic Android fixture listening on port {}; password android-test",
        server.port
    );
    tokio::time::sleep(Duration::from_secs(600)).await;
    server.stop().await;
}
