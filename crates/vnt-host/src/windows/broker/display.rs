//! The logical network session owns admission and restoration, not capture workers.
use super::*;
use crate::windows::display::{Operation, Plan};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        OnceLock,
    },
};

#[derive(Default)]
struct Slot {
    connections: usize,
    exclusive: bool,
}
type Key = (u32, String);
static SLOTS: OnceLock<Mutex<HashMap<Key, Slot>>> = OnceLock::new();
static RESTORING: AtomicUsize = AtomicUsize::new(0);
static RESTORED: tokio::sync::Notify = tokio::sync::Notify::const_new();
fn slots() -> &'static Mutex<HashMap<Key, Slot>> {
    SLOTS.get_or_init(Default::default)
}
fn admit(map: &mut HashMap<Key, Slot>, key: &Key, wants_change: bool) -> anyhow::Result<bool> {
    let slot = map.entry(key.clone()).or_default();
    anyhow::ensure!(
        !slot.exclusive,
        "被控显示器正由其他主控修改分辨率或等待恢复，请稍后连接"
    );
    let exclusive = wants_change && slot.connections == 0;
    slot.connections += 1;
    slot.exclusive = exclusive;
    Ok(exclusive)
}
fn release(key: &Key) {
    let mut slots = slots().lock().unwrap();
    if let Some(slot) = slots.get_mut(key) {
        slot.connections -= 1;
        if slot.connections == 0 {
            slots.remove(key);
        }
    }
}
type Rpc = Arc<
    dyn Fn(Operation) -> futures_util::future::BoxFuture<'static, anyhow::Result<Option<Plan>>>
        + Send
        + Sync,
>;

pub(super) struct Lease {
    key: Key,
    rpc: Rpc,
    applying: Option<tokio::task::JoinHandle<anyhow::Result<Option<Plan>>>>,
    status: Arc<Mutex<Status>>,
    plan: Option<Plan>,
    pub exclusive: bool,
}
impl Lease {
    pub fn acquire(
        session: u32,
        device: String,
        wants_change: bool,
        cfg: crate::config::AppConfig,
        status: Arc<Mutex<Status>>,
    ) -> anyhow::Result<Self> {
        let key = (session, device);
        let exclusive = admit(&mut slots().lock().unwrap(), &key, wants_change)?;
        let rpc: Rpc = Arc::new(move |operation| {
            let cfg = cfg.clone();
            Box::pin(async move { rpc(&cfg, session, operation).await })
        });
        Ok(Self {
            key,
            rpc,
            applying: None,
            status,
            plan: None,
            exclusive,
        })
    }
    pub async fn apply(&mut self, index: u32, width: u32, height: u32) -> anyhow::Result<()> {
        let plan = (self.rpc)(Operation::Prepare {
            monitor_index: index,
            width,
            height,
        })
        .await?
        .ok_or_else(|| anyhow::anyhow!("代理未返回原始显示模式"))?;
        // Record before awaiting a mutating RPC: cancellation may occur after Windows
        // applies the mode but before the broker receives the acknowledgement.
        self.plan = Some(plan.clone());
        self.applying = Some(tokio::spawn((self.rpc)(Operation::Apply(plan))));
        let result = self.applying.as_mut().unwrap().await;
        self.applying = None;
        result??;
        Ok(())
    }
    pub async fn restore_now(&mut self) -> anyhow::Result<()> {
        if let Some(applying) = self.applying.take() {
            let _ = applying.await;
        }
        if let Some(plan) = self.plan.clone() {
            (self.rpc)(Operation::Restore(plan)).await?;
            self.plan = None;
        }
        Ok(())
    }
    pub fn downgrade(&mut self) {
        self.exclusive = false;
        if let Some(slot) = slots().lock().unwrap().get_mut(&self.key) {
            slot.exclusive = false;
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let Some(plan) = self.plan.take() else {
            release(&self.key);
            return;
        };
        let rpc = self.rpc.clone();
        let applying = self.applying.take();
        let key = self.key.clone();
        let status = self.status.clone();
        RESTORING.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            if let Some(applying) = applying {
                let _ = applying.await;
            }
            let mut failure = String::new();
            for _ in 0..3 {
                match (rpc)(Operation::Restore(plan.clone())).await {
                    Ok(_) => {
                        release(&key);
                        failure.clear();
                        break;
                    }
                    Err(error) => failure = format!("恢复被控显示器分辨率失败：{error:#}"),
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if !failure.is_empty() {
                // Keep admission closed rather than claim the old mode was restored.
                tracing::error!(device=%key.1, "{failure}");
                status.lock().unwrap().error = Some(failure);
            }
            RESTORING.fetch_sub(1, Ordering::SeqCst);
            RESTORED.notify_waiters();
        });
    }
}
pub(super) async fn wait_restores() {
    loop {
        let notified = RESTORED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if RESTORING.load(Ordering::SeqCst) == 0 {
            break;
        }
        notified.await;
    }
}
async fn rpc(
    cfg: &crate::config::AppConfig,
    session: u32,
    operation: Operation,
) -> anyhow::Result<Option<Plan>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let name = format!(r"\\.\pipe\VntRemote.Session.{}", native::random()?);
        let mut pipe = native::pipe(&name, &agent_acl(session, false)?, true)?;
        let process = {
            let mut pool = with_root(cfg, session, false).await?;
            let root = pool
                .get_mut(&super::AgentKey::new(cfg, session, false))
                .unwrap();
            write_json(
                &mut root.control,
                &RootRequest::Open(OpenSession {
                    effects: None,
                    metadata: false,
                    available_only: false,
                    region: None,
                    pipe_name: name,
                    display: Some(operation),
                }),
            )
            .await?;
            root.process.clone()
        };
        pipe.connect().await?;
        anyhow::ensure!(
            native::client_pid(&pipe)? == process.pid,
            "显示代理身份不匹配"
        );
        read_json::<_, Result<Option<Plan>, String>>(&mut pipe)
            .await?
            .map_err(anyhow::Error::msg)
    })
    .await
    .context("显示模式操作超时")?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_is_exclusive_and_concurrent_change_falls_back() {
        let mut map = HashMap::new();
        let key = (1, "display1".to_string());
        assert!(!admit(&mut map, &key, false).unwrap());
        assert!(!admit(&mut map, &key, true).unwrap());
        assert_eq!(map[&key].connections, 2);
        map.clear();
        assert!(admit(&mut map, &key, true).unwrap());
        assert!(admit(&mut map, &key, false).is_err());
        assert!(admit(&mut map, &key, true).is_err());
        assert!(!admit(&mut map, &(1, "display2".into()), false).unwrap());
    }
    #[tokio::test]
    async fn cancelled_apply_keeps_admission_until_restored_and_retries_failures() {
        let key = (u32::MAX, "test-cancel-display".to_string());
        let events = Arc::new(Mutex::new(Vec::new()));
        let release_apply = Arc::new(tokio::sync::Notify::new());
        let applied = Arc::new(tokio::sync::Notify::new());
        let retries = Arc::new(AtomicUsize::new(0));
        let plan = Plan {
            device: key.1.clone(),
            original: Default::default(),
            desired: Default::default(),
        };
        let transport: Rpc = {
            let events = events.clone();
            let release_apply = release_apply.clone();
            let applied = applied.clone();
            let retries = retries.clone();
            Arc::new(move |operation| {
                let events = events.clone();
                let release_apply = release_apply.clone();
                let applied = applied.clone();
                let retries = retries.clone();
                let plan = plan.clone();
                Box::pin(async move {
                    match operation {
                        Operation::Prepare { .. } => Ok(Some(plan)),
                        Operation::Apply(_) => {
                            applied.notify_one();
                            release_apply.notified().await;
                            events.lock().unwrap().push("applied");
                            Ok(None)
                        }
                        Operation::Restore(_) => {
                            if retries.fetch_add(1, Ordering::SeqCst) == 0 {
                                anyhow::bail!("transient agent failure");
                            }
                            events.lock().unwrap().push("restored");
                            Ok(None)
                        }
                    }
                })
            })
        };
        assert!(admit(&mut slots().lock().unwrap(), &key, true).unwrap());
        let mut lease = Lease {
            key: key.clone(),
            rpc: transport,
            applying: None,
            status: Arc::new(Mutex::new(Status::default())),
            plan: None,
            exclusive: true,
        };
        let task = tokio::spawn(async move { lease.apply(0, 640, 360).await });
        applied.notified().await;
        task.abort();
        let _ = task.await;
        assert!(admit(&mut slots().lock().unwrap(), &key, false).is_err());
        assert!(events.lock().unwrap().is_empty());
        release_apply.notify_one();
        tokio::time::timeout(Duration::from_secs(2), wait_restores())
            .await
            .unwrap();
        assert_eq!(*events.lock().unwrap(), vec!["applied", "restored"]);
        assert_eq!(retries.load(Ordering::SeqCst), 2);
        assert!(!slots().lock().unwrap().contains_key(&key));
    }
    #[tokio::test]
    async fn failed_restoration_reports_error_and_keeps_admission_closed() {
        let key = (u32::MAX, "test-failed-restore".to_string());
        let plan = Plan {
            device: key.1.clone(),
            original: Default::default(),
            desired: Default::default(),
        };
        let status = Arc::new(Mutex::new(Status::default()));
        let rpc: Rpc = Arc::new(move |operation| {
            let plan = plan.clone();
            Box::pin(async move {
                match operation {
                    Operation::Prepare { .. } => Ok(Some(plan)),
                    Operation::Apply(_) => Ok(None),
                    Operation::Restore(_) => anyhow::bail!("agent unavailable"),
                }
            })
        });
        admit(&mut slots().lock().unwrap(), &key, true).unwrap();
        let mut lease = Lease {
            key: key.clone(),
            rpc,
            applying: None,
            status: status.clone(),
            plan: None,
            exclusive: true,
        };
        lease.apply(0, 640, 360).await.unwrap();
        drop(lease);
        tokio::time::timeout(Duration::from_secs(3), wait_restores())
            .await
            .unwrap();
        assert!(status
            .lock()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .contains("恢复被控显示器分辨率失败"));
        assert!(admit(&mut slots().lock().unwrap(), &key, false).is_err());
        release(&key);
    }
}
