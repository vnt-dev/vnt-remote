//! A dedicated authenticated user-agent pipe follows the logical connection,
//! including control-lane recovery and capture desktop transitions.
use super::*;
use crate::windows::desktop_effects::Settings;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncWriteExt;

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static RELEASED: tokio::sync::Notify = tokio::sync::Notify::const_new();
struct Running;
impl Drop for Running {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
        RELEASED.notify_waiters();
    }
}
pub(super) struct Lease(CancellationToken);
impl Lease {
    pub fn start(
        cfg: crate::config::AppConfig,
        identity: (u32, String),
        settings: Settings,
    ) -> Self {
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let _running = Running;
            loop {
                if stop.is_cancelled()
                    || native::session_identity(identity.0).ok().as_ref() != Some(&identity.1)
                {
                    break;
                }
                // Finish a submitted acquire even if the owner is cancelled:
                // the agent may already have changed Windows before its reply.
                // Shutdown keeps the agent pool alive until this task releases.
                let result =
                    tokio::time::timeout(Duration::from_secs(10), open(&cfg, identity.0, settings))
                        .await;
                if let Ok(Ok((mut pipe, _process))) = result {
                    tokio::select! {
                        _ = stop.cancelled() => {
                            // Wait for the user agent's restore acknowledgement
                            // before host shutdown terminates pooled processes.
                            let _ = tokio::time::timeout(Duration::from_secs(6), async {
                                pipe.write_all(&[1]).await?;
                                read_json::<_, bool>(&mut pipe).await
                            }).await;
                            break;
                        }
                        _ = read_json::<_, bool>(&mut pipe) => {}
                    }
                }
                tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
            }
        });
        Self(cancel)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
async fn open(
    cfg: &crate::config::AppConfig,
    session: u32,
    settings: Settings,
) -> anyhow::Result<(
    tokio::net::windows::named_pipe::NamedPipeServer,
    Arc<native::Process>,
)> {
    let name = format!(r"\\.\pipe\VntRemote.Session.{}", native::random()?);
    let mut pipe = native::pipe(&name, &agent_acl(session, true)?, true)?;
    let process = {
        let mut pool = with_root(cfg, session, true).await?;
        let root = pool.get_mut(&AgentKey::new(cfg, session, true)).unwrap();
        write_json(
            &mut root.control,
            &RootRequest::Open(OpenSession {
                effects: Some(settings),
                pipe_name: name,
                region: None,
                display: None,
                metadata: false,
                available_only: false,
            }),
        )
        .await?;
        root.process.clone()
    };
    pipe.connect().await?;
    anyhow::ensure!(
        native::client_pid(&pipe)? == process.pid,
        "桌面效果代理身份不匹配"
    );
    // Unsupported effects do not fail the remote session. Keep the pipe alive
    // in either case; reconnect only if the user agent itself exits.
    let _supported = read_json::<_, bool>(&mut pipe).await?;
    Ok((pipe, process))
}
pub(super) async fn wait_releases() {
    loop {
        let notified = RELEASED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if ACTIVE.load(Ordering::SeqCst) == 0 {
            break;
        }
        notified.await;
    }
}
