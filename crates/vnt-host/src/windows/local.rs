//! Authenticated loopback control plane. The named pipe is only a trusted bootstrap;
//! all desktop lifecycle and host commands travel over TCP.
use super::{
    broker::Broker,
    management::{self, read_json, write_json, Status},
    native, store,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

const BOOTSTRAP_PIPE: &str = r"\\.\pipe\VntRemoteService.Desktop.v1";
const VERSION: u32 = 1;
const SHORT: Duration = Duration::from_secs(5);
const LONG: Duration = Duration::from_secs(20);
static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone, Serialize, Deserialize)]
pub enum Action {
    Register,
    Status,
    StartHost,
    StopHost,
    SaveConfig(crate::config::AppConfig),
    ReadConfig,
    ConfigureLink(crate::link::LinkConfig),
    RemoveLink(String),
    LinkStatus,
    LinkStream(crate::link::LinkTarget),
    LinkMedia(crate::link::LinkTarget),
    Quit,
    ShowDesktop,
    Listen,
    PrepareUpdate,
    CancelUpdate,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Show,
    Close,
}
#[derive(Serialize, Deserialize)]
struct Notification {
    version: u32,
    event: Event,
}
#[derive(Serialize, Deserialize)]
struct Request {
    version: u32,
    id: u64,
    token: String,
    action: Action,
}
#[derive(Serialize, Deserialize)]
struct Reply<T = Status> {
    version: u32,
    id: u64,
    result: Result<T, String>,
}
#[derive(Serialize, Deserialize)]
struct Bootstrap {
    port: u16,
    token: String,
}
#[derive(Clone)]
struct Grant {
    pid: u32,
    session: u32,
    identity: String,
    desktop: bool,
    process: Arc<native::Handle>,
}
impl Grant {
    fn current(&self) -> bool {
        native::process_running(&self.process)
            && native::desktop_sessions().is_ok_and(|sessions| sessions.contains(&self.session))
            && native::session_identity(self.session).ok().as_ref() == Some(&self.identity)
            && native::process_identity(self.pid).ok()
                == Some((self.session, self.identity.clone()))
    }
}
type Grants = Arc<Mutex<HashMap<String, Grant>>>;
enum Command {
    ReadConfig {
        grant: Grant,
        reply: oneshot::Sender<Result<crate::config::AppConfig, String>>,
    },
    Bootstrap {
        pid: u32,
        reply: oneshot::Sender<Result<Bootstrap, String>>,
    },
    Request {
        grant: Grant,
        action: Box<Action>,
        reply: oneshot::Sender<Result<Status, String>>,
    },
}

/// A client never trusts a port or a command line credential. Bootstrap checks
/// the pipe's kernel server PID against SCM before receiving the TCP credential.
#[derive(Clone)]
pub struct Client {
    port: u16,
    token: String,
}
impl Client {
    pub async fn connect(expected_port: Option<u16>) -> anyhow::Result<Self> {
        tokio::time::timeout(SHORT, async {
            let mut pipe = native::connect(BOOTSTRAP_PIPE).await?;
            anyhow::ensure!(
                native::server_pid(&pipe)? == management::service_pid()?,
                "本地通信入口不属于 Windows 服务"
            );
            let bootstrap: Result<Bootstrap, String> = read_json(&mut pipe).await?;
            let bootstrap = bootstrap.map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                bootstrap.port != 0 && expected_port.is_none_or(|p| p == bootstrap.port),
                "本地服务端口与 Windows 服务不匹配"
            );
            Ok(Self {
                port: bootstrap.port,
                token: bootstrap.token,
            })
        })
        .await
        .map_err(|_| anyhow::anyhow!("连接本地服务超时"))?
    }
    async fn open<T: serde::de::DeserializeOwned>(
        &self,
        action: Action,
    ) -> anyhow::Result<(TcpStream, T)> {
        let id = NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, self.port)).await?;
        write_json(
            &mut socket,
            &Request {
                version: VERSION,
                id,
                token: self.token.clone(),
                action,
            },
        )
        .await?;
        let reply: Reply<T> = read_json(&mut socket).await?;
        anyhow::ensure!(
            reply.version == VERSION && reply.id == id,
            "本地服务响应版本或请求标识不匹配"
        );
        Ok((socket, reply.result.map_err(anyhow::Error::msg)?))
    }
    pub async fn request(&self, action: Action) -> anyhow::Result<Status> {
        let deadline = if matches!(action, Action::StartHost | Action::StopHost) {
            LONG
        } else {
            SHORT
        };
        tokio::time::timeout(deadline, self.open::<Status>(action))
            .await
            .map_err(|_| anyhow::anyhow!("本地服务操作超时"))?
            .map(|(_, status)| status)
    }
    pub async fn events(&self) -> anyhow::Result<TcpStream> {
        tokio::time::timeout(SHORT, self.open::<Status>(Action::Listen))
            .await
            .map_err(|_| anyhow::anyhow!("订阅本地服务超时"))?
            .map(|(socket, _)| socket)
    }
    pub async fn read_config(&self) -> anyhow::Result<crate::config::AppConfig> {
        tokio::time::timeout(
            SHORT,
            self.open::<crate::config::AppConfig>(Action::ReadConfig),
        )
        .await
        .map_err(|_| anyhow::anyhow!("读取服务配置超时"))?
        .map(|(_, config)| config)
    }
    pub async fn link_status(&self) -> anyhow::Result<Vec<crate::link::ProfileStatus>> {
        Ok(self
            .open::<Vec<crate::link::ProfileStatus>>(Action::LinkStatus)
            .await?
            .1)
    }
    pub async fn connect_overlay(
        &self,
        target: crate::link::LinkTarget,
    ) -> anyhow::Result<vnt_net::overlay::Prepared> {
        let target_for_stream = target.clone();
        let client = self.clone();
        let open: vnt_net::session::OpenStream = Arc::new(move || {
            let target = target_for_stream.clone();
            let client = client.clone();
            Box::pin(async move {
                let (stream, _) = client
                    .open::<Vec<crate::link::ProfileStatus>>(Action::LinkStream(target))
                    .await
                    .map_err(std::io::Error::other)?;
                Ok(Box::new(stream) as vnt_net::session::BoxStream)
            })
        });
        let (media, status) = self
            .open::<Vec<crate::link::ProfileStatus>>(Action::LinkMedia(target.clone()))
            .await?;
        let status = &status
            .iter()
            .find(|s| s.config_id == target.config_id)
            .ok_or_else(|| anyhow::anyhow!("服务端配置已变化"))?
            .status;
        let target = status
            .devices
            .iter()
            .find(|d| d.id == target.node)
            .ok_or_else(|| anyhow::anyhow!("目标设备已离线"))?;
        let peer = std::net::SocketAddr::from((
            target
                .connection_id
                .parse::<vnt_core::connection_id::ConnectionId>()?
                .transport_key(),
            29875,
        ));
        let local = std::net::SocketAddr::from((
            status
                .local_connection_id
                .parse::<vnt_core::connection_id::ConnectionId>()?
                .transport_key(),
            29875,
        ));
        let media = vnt_net::datagram::Datagram::proxy(media, local, peer);
        let channel = vnt_net::session::connect_with(vnt_net::session::Dialer::Proxy(open)).await?;
        Ok(vnt_net::overlay::Prepared {
            channel,
            media,
            peer,
        })
    }
    pub async fn next_event(socket: &mut TcpStream) -> anyhow::Result<Event> {
        let notification: Notification = read_json(socket).await?;
        anyhow::ensure!(notification.version == VERSION, "不支持的服务通知版本");
        Ok(notification.event)
    }
}

pub fn parse_port(args: &[String]) -> anyhow::Result<Option<u16>> {
    let mut port = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--local-service-port" {
            anyhow::ensure!(port.is_none(), "本地服务端口参数重复");
            i += 1;
            let value = args
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("缺少本地服务端口"))?
                .parse::<u16>()?;
            anyhow::ensure!(value != 0, "本地服务端口必须为 1–65535");
            port = Some(value);
        }
        i += 1;
    }
    Ok(port)
}

struct Host {
    broker: Option<Broker>,
    error: Option<String>,
}
impl Host {
    async fn start(&mut self, root: &CancellationToken) -> anyhow::Result<()> {
        if self
            .broker
            .as_ref()
            .is_some_and(|b| b.cancel.is_cancelled())
        {
            self.stop().await?;
        }
        if self.broker.is_some() {
            return Ok(());
        }
        let cfg = store::load()?;
        store::firewall(cfg.port)?;
        self.start_config(cfg, root).await
    }
    async fn start_config(
        &mut self,
        cfg: crate::config::AppConfig,
        root: &CancellationToken,
    ) -> anyhow::Result<()> {
        // A host stop must never cancel the service or the next host instance.
        self.broker = Some(Broker::start(cfg, root.child_token()).await?);
        self.error = None;
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(broker) = self.broker.take() {
            tokio::time::timeout(Duration::from_secs(15), broker.shutdown())
                .await
                .map_err(|_| anyhow::anyhow!("停止被控超时"))??;
        }
        Ok(())
    }
    async fn set(&mut self, enabled: bool, root: &CancellationToken) -> anyhow::Result<()> {
        self.set_with(enabled, root, store::save_host_enabled).await
    }
    async fn set_with(
        &mut self,
        enabled: bool,
        root: &CancellationToken,
        persist: impl FnOnce(bool) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let before = self.broker.is_some();
        if enabled {
            self.start(root).await?;
        } else {
            self.stop().await?;
        }
        if let Err(error) = persist(enabled) {
            let rollback = if before {
                self.start(root).await
            } else {
                self.stop().await
            };
            anyhow::bail!(
                "保存被控开关失败：{error:#}{}",
                rollback
                    .err()
                    .map(|e| format!("；回滚失败：{e:#}"))
                    .unwrap_or_default()
            );
        }
        self.error = None;
        Ok(())
    }
    fn status(&self) -> Status {
        let mut status = self
            .broker
            .as_ref()
            .map(|b| b.status.lock().unwrap().clone())
            .unwrap_or_else(|| {
                let cfg = store::load().unwrap_or_default();
                Status {
                    installed: true,
                    state: "stopped".into(),
                    port: cfg.port,
                    password_configured: cfg.password_configured,
                    ..Default::default()
                }
            });
        status.service_state = "running".into();
        status.error = self.error.clone().or(status.error);
        status
    }
}

pub struct Control {
    pub status: Arc<Mutex<Status>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}
impl Control {
    pub async fn wait(&mut self) -> anyhow::Result<()> {
        (&mut self.task).await?
    }
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.task.await?
    }
}

pub async fn start(cancel: CancellationToken, launcher: Option<u32>) -> anyhow::Result<Control> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    tracing::info!(port, "Windows 服务本地 TCP 通道已监听");
    let pipe = native::pipe(
        BOOTSTRAP_PIPE,
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;BU)",
        true,
    )?;
    let mut host = Host {
        broker: None,
        error: None,
    };
    match store::host_enabled() {
        Ok(true) => {
            if let Err(e) = host.start(&cancel).await {
                host.error = Some(format!("恢复被控失败：{e:#}"));
            }
        }
        Ok(false) => {}
        Err(e) => host.error = Some(format!("读取被控开关失败：{e:#}")),
    }
    let link = Arc::new(crate::link::ServerLinks::new(
        &store::directory()?.join("device_id"),
    )?);
    if let Ok(config) = store::load() {
        if let Err(error) = link.configure(config.servers).await {
            tracing::warn!("恢复服务端连接失败：{error}");
        }
    }
    let grants: Grants = Default::default();
    let (tx, mut rx) = mpsc::channel::<Command>(32);
    let (events, _) = broadcast::channel::<Event>(16);
    let status = Arc::new(Mutex::new(host.status()));
    let snapshot = status.clone();
    let pipe_tx = tx.clone();
    let pipe_cancel = cancel.clone();
    let bootstrap_task = tokio::spawn(async move {
        let _cancel_on_drop = pipe_cancel.clone().drop_guard();
        let mut pipe = pipe;
        loop {
            tokio::select! { _ = pipe_cancel.cancelled() => break, result = pipe.connect() => { if result.is_err() { break; } } }
            let next = match native::pipe(
                BOOTSTRAP_PIPE,
                "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;BU)",
                false,
            ) {
                Ok(p) => p,
                Err(_) => break,
            };
            let request = async {
                let pid = native::client_pid(&pipe)?;
                let (reply, result) = oneshot::channel();
                pipe_tx.send(Command::Bootstrap { pid, reply }).await?;
                write_json(&mut pipe, &result.await?).await
            };
            let _ = tokio::time::timeout(SHORT, request).await;
            pipe = next;
        }
    });
    let tcp_grants = grants.clone();
    let tcp_cancel = cancel.clone();
    let tcp_events = events.clone();
    let tcp_link = link.clone();
    let tcp_task = tokio::spawn(async move {
        let _cancel_on_drop = tcp_cancel.clone().drop_guard();
        let permits = Arc::new(tokio::sync::Semaphore::new(32));
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = tcp_cancel.cancelled() => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => {},
                accepted = listener.accept() => {
                    let Ok((socket, _)) = accepted else { break };
                    let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                    let (tx, grants, events, cancel, link) = (tx.clone(), tcp_grants.clone(), tcp_events.clone(), tcp_cancel.clone(), tcp_link.clone());
                    connections.spawn(async move { let _permit = permit; let _ = serve(socket, tx, grants, events, cancel, link).await; });
                }
            }
        }
        // Let the actor's Close event reach the GUI before aborting sockets.
        tokio::time::sleep(Duration::from_millis(300)).await;
        connections.abort_all();
    });
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        // Unexpected task failure must wake SCM rather than leave a service
        // reporting Running with no control plane.
        let _cancel_on_drop = task_cancel.drop_guard();
        let path = std::env::current_exe()?.with_file_name("vnt-remote.exe");
        let mut desktop: Option<native::Process> = None;
        let mut supervisor = Supervisor::new();
        let mut preferred = None;
        let mut pending = launcher.and_then(|pid| native::watch_process(pid).ok());

        let mut pending_show = false;
        let mut timer = tokio::time::interval(Duration::from_millis(100));
        loop {
            *snapshot.lock().unwrap() = host.status();
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = timer.tick() => {
                    let sessions = match native::desktop_sessions() {
                        Ok(sessions) => sessions,
                        Err(error) => {
                            tracing::warn!("查询交互桌面会话失败：{error:#}");
                            continue;
                        }
                    };
                    let logins = sessions.into_iter().filter_map(|session| {
                        native::session_identity(session).ok().filter(|id| !id.is_empty()).map(|id| (session, id))
                    }).collect::<Vec<_>>();
                    let current = select_desktop(&logins, preferred.as_ref(), native::console_session());
                    if pending.as_ref().is_some_and(|p| !native::process_running(p)) { pending = None; }
                    let decision = supervisor.observe(current, desktop.as_ref().map(|p| p.running()), pending.is_some(), tokio::time::Instant::now());
                    if decision.retire {
                        grants.lock().unwrap().retain(|_, g| !g.desktop);
                        let _ = events.send(Event::Close);
                        drop(desktop.take());
                    } else if decision.dead {
                        desktop = None;
                        grants.lock().unwrap().retain(|_, g| !g.desktop);
                    }
                    if decision.launch {
                        if let Some((session, _)) = &supervisor.identity {
                            match native::spawn_desktop(&path, *session, port) {
                                Ok(process) => { tracing::info!(pid=process.pid, session, "服务已拉起桌面"); desktop = Some(process); },
                                Err(e) => {
                                    tracing::warn!("启动桌面失败：{e:#}");
                                    supervisor.failed(tokio::time::Instant::now());
                                }
                            }
                        }
                    }
                    grants.lock().unwrap().retain(|_, grant| grant.current());
                },
                incoming = link.next_incoming() => {
                    if let Some((mut channel, peer, media)) = incoming {
                        if let Some(broker) = &host.broker { broker.accept_overlay(channel, peer, media); }
                        else { let _ = channel.send(&vnt_proto::control::ControlMessage::Denied(vnt_proto::pb::Denied { reason: "目标设备未开启被控".into() })).await; }
                    }
                },
                Some(command) = rx.recv() => match command {
                    Command::ReadConfig { grant, reply } => {
                        let result = if grant.desktop && grant.current() && desktop.as_ref().is_some_and(|p| p.pid == grant.pid) { store::load() } else { Err(anyhow::anyhow!("桌面登录身份已变化")) };
                        let _ = reply.send(result.map_err(|e| format!("{e:#}")));
                    },
                    Command::Bootstrap { pid, reply } => {
                        let result = (|| -> anyhow::Result<Bootstrap> {
                            anyhow::ensure!(native::process_path(pid)?.canonicalize()? == path.canonicalize()?, "请求必须来自已安装的桌面程序");
                            let (session, user) = native::process_identity(pid)?;
                            anyhow::ensure!(!user.is_empty() && native::desktop_sessions()?.contains(&session) && native::session_identity(session)? == user, "桌面不属于当前已连接的交互登录用户");
                            let managed = desktop.as_ref().is_some_and(|p| p.pid == pid);
                            let token = native::random()?;
                            grants.lock().unwrap().retain(|_, g| g.pid != pid);
                            grants.lock().unwrap().insert(token.clone(), Grant { pid, session, identity: user, desktop: managed, process: Arc::new(native::watch_process(pid)?) });
                            Ok(Bootstrap { port, token })
                        })().map_err(|e| format!("{e:#}"));
                        let _ = reply.send(result);
                    },
                    Command::Request { grant, action, reply } => {
                        let link_action = matches!(&*action, Action::ConfigureLink(_) | Action::RemoveLink(_));
                        let result = if !grant.current() || (grant.desktop && desktop.as_ref().is_none_or(|p| p.pid != grant.pid)) { Err(anyhow::anyhow!("桌面登录身份已变化")) } else { match *action {
                            Action::StartHost => host.set(true, &cancel).await,
                            Action::StopHost => host.set(false, &cancel).await,
                            Action::ConfigureLink(server) => {
                                let result = async {
                                    let mut cfg = store::load()?;
                                    cfg.upsert_server(server).map_err(anyhow::Error::msg)?;
                                    cfg.validate().map_err(anyhow::Error::msg)?;
                                    store::save(cfg.clone())?;
                                    link.configure(cfg.servers).await.map_err(anyhow::Error::msg)
                                }.await;
                                result
                            },
                            Action::RemoveLink(id) => {
                                async {
                                    let previous = store::load()?;
                                    let mut cfg = previous.clone();
                                    cfg.servers.retain(|s| s.id != id);
                                    link.configure(cfg.servers.clone()).await.map_err(anyhow::Error::msg)?;
                                    if let Err(error) = store::save(cfg) {
                                        link.configure(previous.servers).await.map_err(anyhow::Error::msg)?;
                                        return Err(error);
                                    }
                                    Ok(())
                                }.await
                            },
                            Action::SaveConfig(mut config) => {
                                if let Ok(previous) = store::load() { config.servers = previous.servers; }
                                let border = config.capture_border;
                                let result = store::update_config(config, &host.status());
                                let result = match result {
                                    Ok(()) => super::broker::update_capture_border(border).await,
                                    Err(error) => Err(error),
                                };
                                if result.is_ok() { host.error = None; }
                                result
                            },
                            Action::ShowDesktop => { preferred = Some((grant.session, grant.identity.clone())); pending = native::watch_process(grant.pid).ok(); pending_show = true; if supervisor.identity == preferred { let _ = events.send(Event::Show); } Ok(()) },
                            Action::Listen => { if pending_show && supervisor.identity == preferred { let _ = events.send(Event::Show); pending_show = false; } Ok(()) },
                            Action::PrepareUpdate => { supervisor.paused_until = Some(tokio::time::Instant::now() + Duration::from_secs(120)); Ok(()) },
                            Action::CancelUpdate => { supervisor.paused_until = None; Ok(()) },
                            Action::Quit => { tracing::info!(pid=grant.pid, "桌面请求退出程序及 Windows 服务"); supervisor.quitting = true; Ok(()) },
                            _ => Ok(()),
                        }};
                        if let (Err(e), false) = (&result, link_action) { host.error = Some(format!("{e:#}")); }
                        let result = result.map(|_| host.status()).map_err(|e| format!("{e:#}"));
                        let _ = reply.send(result);
                    }
                }
            }
        }
        let _ = events.send(Event::Close);
        // Allow acknowledgement and cooperative GUI cleanup before cancelling sockets.
        tokio::time::sleep(Duration::from_millis(300)).await;
        link.stop().await;
        cancel.cancel();
        let stopped = host.stop().await;
        bootstrap_task.abort();
        let _ = tcp_task.await;
        if let Some(desktop) = desktop {
            desktop.shutdown().await?;
        }
        stopped
    });
    Ok(Control { status, task })
}

fn retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(250 << attempt.min(3))
}

/// An explicit launch chooses its own login, including RDP. Otherwise prefer
/// the console, then a deterministic active login. Never reuse a logged-out
/// user's preference just because Windows recycled the session number.
fn select_desktop(
    logins: &[(u32, String)],
    preferred: Option<&(u32, String)>,
    console: Option<u32>,
) -> Option<(u32, String)> {
    preferred
        .and_then(|login| logins.iter().find(|candidate| *candidate == login))
        .or_else(|| logins.iter().find(|(session, _)| Some(*session) == console))
        .or_else(|| logins.first())
        .cloned()
}

struct Supervisor {
    identity: Option<(u32, String)>,
    next: tokio::time::Instant,
    failures: u32,
    paused_until: Option<tokio::time::Instant>,
    quitting: bool,
}
struct Decision {
    retire: bool,
    dead: bool,
    launch: bool,
}
impl Supervisor {
    fn new() -> Self {
        Self {
            identity: None,
            next: tokio::time::Instant::now(),
            failures: 0,
            paused_until: None,
            quitting: false,
        }
    }
    fn failed(&mut self, now: tokio::time::Instant) {
        self.next = now + retry_delay(self.failures);
        self.failures = (self.failures + 1).min(3);
    }
    fn observe(
        &mut self,
        current: Option<(u32, String)>,
        alive: Option<bool>,
        pending: bool,
        now: tokio::time::Instant,
    ) -> Decision {
        let retire = self.identity != current;
        if retire {
            self.identity = current;
            self.failures = 0;
            self.next = now;
        }
        let dead = !retire && alive == Some(false);
        if dead {
            self.failed(now);
        }
        let paused = self.paused_until.is_some_and(|until| now < until);
        let launch = !self.quitting
            && self.identity.is_some()
            && (retire || alive != Some(true))
            && !pending
            && !paused
            && now >= self.next;
        Decision {
            retire,
            dead,
            launch,
        }
    }
}

async fn serve(
    mut socket: TcpStream,
    tx: mpsc::Sender<Command>,
    grants: Grants,
    events: broadcast::Sender<Event>,
    cancel: CancellationToken,
    link: Arc<crate::link::ServerLinks>,
) -> anyhow::Result<()> {
    let request: Request = tokio::time::timeout(SHORT, read_json(&mut socket)).await??;
    let grant = match authorize(&request, &grants, Grant::current) {
        Ok(grant) => grant,
        Err(error) => {
            tokio::time::timeout(
                SHORT,
                write_json(
                    &mut socket,
                    &Reply::<Status> {
                        version: VERSION,
                        id: request.id,
                        result: Err(format!("{error:#}")),
                    },
                ),
            )
            .await??;
            return Ok(());
        }
    };
    if matches!(
        request.action,
        Action::LinkStatus | Action::LinkStream(_) | Action::LinkMedia(_)
    ) {
        let result: anyhow::Result<(
            Vec<crate::link::ProfileStatus>,
            Option<vnt_net::session::BoxStream>,
            Option<vnt_net::datagram::Datagram>,
        )> = async {
            anyhow::ensure!(grant.desktop && grant.current(), "桌面登录身份已变化");
            match &request.action {
                Action::LinkStream(node) => {
                    let selected = link.select(node).await?;
                    let peer = selected.peer(&node.node).await?;
                    let stream = peer.open_stream().await?;
                    Ok((link.snapshot().await, Some(stream), None))
                }
                Action::LinkMedia(node) => {
                    let selected = link.select(node).await?;
                    let peer = selected.peer(&node.node).await?;
                    let media = vnt_net::datagram::Datagram::overlay_peer(
                        peer.datagrams,
                        (peer.ip, 29875).into(),
                    );
                    Ok((link.snapshot().await, None, Some(media)))
                }
                _ => Ok((link.snapshot().await, None, None)),
            }
        }
        .await;
        let (status, stream, media) = match result {
            Ok(value) => value,
            Err(error) => {
                write_json(
                    &mut socket,
                    &Reply::<Vec<crate::link::ProfileStatus>> {
                        version: VERSION,
                        id: request.id,
                        result: Err(format!("{error:#}")),
                    },
                )
                .await?;
                return Ok(());
            }
        };
        write_json(
            &mut socket,
            &Reply {
                version: VERSION,
                id: request.id,
                result: Ok(status),
            },
        )
        .await?;
        if let Some(mut remote) = stream {
            tokio::select! { _ = cancel.cancelled() => {}, _ = tokio::io::copy_bidirectional(&mut socket, &mut remote) => {} }
        } else if let Some(media) = media {
            use futures_util::{SinkExt, StreamExt};
            use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
            let peer = match request.action {
                Action::LinkMedia(node) => link.select(&node).await?.peer(&node.node).await?.ip,
                _ => unreachable!(),
            };
            let (read, write) = socket.into_split();
            let codec = || {
                LengthDelimitedCodec::builder()
                    .max_frame_length(65536)
                    .new_codec()
            };
            let mut reader = FramedRead::new(read, codec());
            let mut writer = FramedWrite::new(write, codec());
            let mut buf = vec![0; 65536];
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    data = reader.next() => { let Some(data) = data else { break }; media.send_to(&data?, (peer, 29875).into()).await?; },
                    packet = media.recv_from(&mut buf) => { let (len, _) = packet?; writer.send(bytes::Bytes::copy_from_slice(&buf[..len])).await?; }
                }
            }
        }
        return Ok(());
    }
    if matches!(request.action, Action::ReadConfig) {
        let (reply, result) = oneshot::channel();
        tx.send(Command::ReadConfig { grant, reply }).await?;
        let result = tokio::time::timeout(SHORT, result).await??;
        tokio::time::timeout(
            SHORT,
            write_json(
                &mut socket,
                &Reply {
                    version: VERSION,
                    id: request.id,
                    result,
                },
            ),
        )
        .await??;
        return Ok(());
    }
    let listen = matches!(request.action, Action::Listen);
    let quitting = matches!(request.action, Action::Quit);
    let mut subscription = events.subscribe();
    let (reply, result) = oneshot::channel();
    tx.send(Command::Request {
        grant: grant.clone(),
        action: Box::new(request.action),
        reply,
    })
    .await?;
    let result = tokio::time::timeout(LONG, result).await??;
    let accepted_quit = quitting && result.is_ok();
    let written = tokio::time::timeout(
        SHORT,
        write_json(
            &mut socket,
            &Reply {
                version: VERSION,
                id: request.id,
                result,
            },
        ),
    )
    .await;
    if accepted_quit {
        cancel.cancel();
    }
    written??;
    if listen {
        notify(&mut socket, &mut subscription, &cancel, || grant.current()).await?;
    }
    Ok(())
}

async fn notify(
    socket: &mut TcpStream,
    subscription: &mut broadcast::Receiver<Event>,
    cancel: &CancellationToken,
    current: impl Fn() -> bool,
) -> anyhow::Result<()> {
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    loop {
        let mut probe = [0u8; 1];
        let event = tokio::select! {
            _ = cancel.cancelled() => Event::Close,
            _ = timer.tick() => { if current() { continue; } Event::Close },
            event = subscription.recv() => event.unwrap_or(Event::Close),
            incoming = socket.peek(&mut probe) => {
                anyhow::ensure!(incoming? == 0, "通知连接收到意外数据");
                return Ok(());
            },
        };
        let close = matches!(event, Event::Close);
        tokio::time::timeout(
            SHORT,
            write_json(
                socket,
                &Notification {
                    version: VERSION,
                    event,
                },
            ),
        )
        .await??;
        if close {
            return Ok(());
        }
    }
}

fn authorize(
    request: &Request,
    grants: &Grants,
    current: impl FnOnce(&Grant) -> bool,
) -> anyhow::Result<Grant> {
    anyhow::ensure!(request.version == VERSION, "不支持的本地协议版本");
    let grant = grants
        .lock()
        .unwrap()
        .get(&request.token)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("本地通信认证失败"))?;
    anyhow::ensure!(current(&grant), "本地通信凭证已失效");
    anyhow::ensure!(
        grant.desktop || matches!(request.action, Action::ShowDesktop),
        "此进程仅允许交接桌面启动"
    );
    Ok(grant)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_port_arguments() {
        for args in [
            vec!["--local-service-port"],
            vec!["--local-service-port", "0"],
            vec!["--local-service-port", "65536"],
            vec!["--local-service-port", "abc"],
            vec!["--local-service-port", "1", "--local-service-port", "2"],
        ] {
            assert!(parse_port(&args.into_iter().map(str::to_string).collect::<Vec<_>>()).is_err());
        }
        assert_eq!(
            parse_port(&["--local-service-port".into(), "65535".into()]).unwrap(),
            Some(65535)
        );
        assert_eq!(parse_port(&[]).unwrap(), None);
    }
    #[test]
    fn retries_are_bounded() {
        assert_eq!(
            (0..6).map(retry_delay).collect::<Vec<_>>(),
            [250, 500, 1000, 2000, 2000, 2000].map(Duration::from_millis)
        );
    }
    #[test]
    fn desktop_selection_supports_rdp_without_changing_capture_console() {
        let console = (1, "console:login-1".into());
        let rdp = (4, "rdp:login-2".into());
        let logins = [console.clone(), rdp.clone()];
        assert_eq!(
            select_desktop(&logins, None, Some(1)),
            Some(console.clone())
        );
        assert_eq!(
            select_desktop(&logins, Some(&rdp), Some(1)),
            Some(rdp.clone())
        );
        assert_eq!(
            select_desktop(std::slice::from_ref(&rdp), None, Some(1)),
            Some(rdp.clone())
        );
        assert_eq!(
            select_desktop(std::slice::from_ref(&console), Some(&rdp), Some(1)),
            Some(console)
        );
        assert_eq!(select_desktop(&[], Some(&rdp), None), None);
        let recycled = (4, "different:login-3".into());
        assert_eq!(
            select_desktop(&[recycled.clone(), logins[0].clone()], Some(&rdp), Some(1)),
            Some(logins[0].clone())
        );
    }

    #[test]
    fn switching_gui_to_rdp_retires_console_and_waits_for_launcher_exit() {
        let mut supervisor = Supervisor::new();
        let now = tokio::time::Instant::now();
        let console = Some((1, "console:login-1".into()));
        let rdp = Some((4, "rdp:login-2".into()));
        assert!(supervisor.observe(console.clone(), None, false, now).launch);
        let handoff = supervisor.observe(rdp.clone(), Some(true), true, now);
        assert!(handoff.retire);
        assert!(!handoff.launch);
        assert!(supervisor.observe(rdp.clone(), None, false, now).launch);
        assert!(!supervisor.observe(rdp, Some(true), false, now).launch);
        let disconnected = supervisor.observe(console, Some(true), false, now);
        assert!(disconnected.retire && disconnected.launch);
    }
    #[test]
    fn desktop_handoff_login_changes_crashes_and_shutdown_are_distinct() {
        let mut s = Supervisor::new();
        let now = tokio::time::Instant::now();
        let login = Some((1, "user-a:login-1".into()));
        assert!(!s.observe(None, None, false, now).launch, "wait for login");
        assert!(
            !s.observe(login.clone(), None, true, now).launch,
            "wait for launcher exit"
        );
        assert!(s.observe(login.clone(), None, false, now).launch);
        assert!(
            !s.observe(login.clone(), Some(true), false, now).launch,
            "reuse existing desktop"
        );
        let death = s.observe(login.clone(), Some(false), false, now);
        assert!(death.dead);
        assert!(!death.launch);
        assert!(
            !s.observe(login.clone(), None, false, now + Duration::from_millis(249))
                .launch
        );
        assert!(
            s.observe(login.clone(), None, false, now + Duration::from_millis(250))
                .launch
        );
        let logout = s.observe(None, Some(true), false, now);
        assert!(logout.retire);
        assert!(!logout.launch);
        let relogin = s.observe(Some((1, "user-a:login-2".into())), None, false, now);
        assert!(
            relogin.retire && relogin.launch,
            "new login generation gets a new desktop"
        );
        let switch = s.observe(Some((2, "user-b:login-3".into())), Some(true), false, now);
        assert!(switch.retire && switch.launch);
        s.paused_until = Some(now + Duration::from_secs(120));
        assert!(
            !s.observe(s.identity.clone(), None, false, now).launch,
            "installer owns handoff"
        );
        assert!(
            s.observe(
                s.identity.clone(),
                None,
                false,
                now + Duration::from_secs(120)
            )
            .launch
        );
        s.quitting = true;
        assert!(
            !s.observe(
                s.identity.clone(),
                None,
                false,
                now + Duration::from_secs(121)
            )
            .launch,
            "intentional quit never respawns"
        );
    }

    #[test]
    fn authentication_checks_version_credential_generation_and_launcher_permissions() {
        let grants: Grants = Default::default();
        grants.lock().unwrap().insert(
            "secret".into(),
            Grant {
                pid: 1,
                session: 1,
                identity: "login-a".into(),
                desktop: false,
                process: Arc::new(native::watch_process(std::process::id()).unwrap()),
            },
        );
        let mut request = Request {
            version: VERSION,
            id: 42,
            token: "secret".into(),
            action: Action::ShowDesktop,
        };
        assert!(authorize(&request, &grants, |_| true).is_ok());
        for action in [
            Action::Quit,
            Action::Register,
            Action::Status,
            Action::StartHost,
            Action::StopHost,
            Action::SaveConfig(crate::config::AppConfig::default()),
            Action::ConfigureLink(crate::link::LinkConfig::default()),
            Action::RemoveLink("profile".into()),
            Action::LinkStatus,
            Action::LinkStream(crate::link::LinkTarget {
                config_id: "profile".into(),
                network_code: None,
                node: "node".into(),
            }),
            Action::LinkMedia(crate::link::LinkTarget {
                config_id: "profile".into(),
                network_code: None,
                node: "node".into(),
            }),
            Action::ReadConfig,
            Action::Listen,
            Action::PrepareUpdate,
        ] {
            request.action = action;
            assert!(authorize(&request, &grants, |_| true).is_err());
        }
        grants.lock().unwrap().get_mut("secret").unwrap().desktop = true;
        assert!(authorize(&request, &grants, |_| true).is_ok());
        assert!(authorize(&request, &grants, |_| false).is_err());
        request.version += 1;
        assert!(authorize(&request, &grants, |_| true).is_err());
        request.version = VERSION;
        request.token = "wrong".into();
        assert!(authorize(&request, &grants, |_| true).is_err());
        request.token = "secret".into();
        grants.lock().unwrap().clear();
        assert!(authorize(&request, &grants, |_| true).is_err());
    }

    #[tokio::test]
    async fn tcp_rejects_unauthenticated_commands_without_dispatch() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let client = Client {
            port: listener.local_addr().unwrap().port(),
            token: "forged".into(),
        };
        let (tx, mut rx) = mpsc::channel(1);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            serve(
                socket,
                tx,
                Default::default(),
                broadcast::channel(1).0,
                CancellationToken::new(),
                Arc::new(crate::link::ServerLinks::default()),
            )
            .await
            .unwrap();
        });
        assert!(client
            .request(Action::Quit)
            .await
            .unwrap_err()
            .to_string()
            .contains("认证失败"));
        server.await.unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn tcp_round_trip_matches_ids_and_delivers_unsolicited_events() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let client = Client {
            port: listener.local_addr().unwrap().port(),
            token: "test".into(),
        };
        let server = tokio::spawn(async move {
            let mut ids = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request: Request = read_json(&mut socket).await.unwrap();
                assert_eq!(request.token, "test");
                assert_eq!(request.version, VERSION);
                ids.push(request.id);
                write_json(
                    &mut socket,
                    &Reply {
                        version: VERSION,
                        id: request.id,
                        result: Ok(Status {
                            service_state: "running".into(),
                            state: "stopped".into(),
                            ..Default::default()
                        }),
                    },
                )
                .await
                .unwrap();
                if matches!(request.action, Action::Listen) {
                    write_json(
                        &mut socket,
                        &Notification {
                            version: VERSION,
                            event: Event::Show,
                        },
                    )
                    .await
                    .unwrap();
                    write_json(
                        &mut socket,
                        &Notification {
                            version: VERSION,
                            event: Event::Close,
                        },
                    )
                    .await
                    .unwrap();
                }
            }
            assert_ne!(ids[0], ids[1]);
        });
        let status = client.request(Action::Status).await.unwrap();
        assert_eq!(status.service_state, "running");
        assert!(!status.running);
        let mut stream = client.events().await.unwrap();
        assert!(matches!(
            Client::next_event(&mut stream).await.unwrap(),
            Event::Show
        ));
        assert!(matches!(
            Client::next_event(&mut stream).await.unwrap(),
            Event::Close
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tcp_config_save_persists_settings_and_returns_service_validation_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        store::save_at(&path, &crate::config::AppConfig::default()).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let client = Client {
            port: listener.local_addr().unwrap().port(),
            token: "test".into(),
        };
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request: Request = read_json(&mut socket).await.unwrap();
                assert_eq!(request.token, "test");
                if matches!(request.action, Action::ReadConfig) {
                    let result = store::load_from(&server_path).map_err(|e| e.to_string());
                    write_json(
                        &mut socket,
                        &Reply {
                            version: VERSION,
                            id: request.id,
                            result,
                        },
                    )
                    .await
                    .unwrap();
                    continue;
                }
                let Action::SaveConfig(config) = request.action else {
                    panic!("expected config save");
                };
                let status = Status {
                    running: true,
                    state: "running".into(),
                    port: vnt_proto::DEFAULT_PORT,
                    ..Default::default()
                };
                let result = store::update_config_at(&server_path, config, &status)
                    .map(|_| status)
                    .map_err(|e| e.to_string());
                write_json(
                    &mut socket,
                    &Reply {
                        version: VERSION,
                        id: request.id,
                        result,
                    },
                )
                .await
                .unwrap();
            }
        });
        let config = crate::config::AppConfig {
            password: "tcp-saved-password".into(),
            ..Default::default()
        };
        client
            .request(Action::SaveConfig(config.clone()))
            .await
            .unwrap();
        assert_eq!(store::load_from(&path).unwrap().password, config.password);
        let returned = client.read_config().await.unwrap();
        assert_eq!(returned.password, config.password);
        assert_eq!(returned.port, config.port);
        assert!(returned.password_configured);
        let changed = crate::config::AppConfig {
            port: config.port + 1,
            ..config
        };
        assert!(client
            .request(Action::SaveConfig(changed))
            .await
            .unwrap_err()
            .to_string()
            .contains("不能修改端口"));
        assert_eq!(
            store::load_from(&path).unwrap().port,
            vnt_proto::DEFAULT_PORT
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn config_read_rejects_forged_credentials_and_propagates_service_errors() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let client = Client {
            port: listener.local_addr().unwrap().port(),
            token: "forged".into(),
        };
        let (tx, mut rx) = mpsc::channel(1);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            serve(
                socket,
                tx,
                Default::default(),
                broadcast::channel(1).0,
                CancellationToken::new(),
                Arc::new(crate::link::ServerLinks::default()),
            )
            .await
            .unwrap();
            let (mut socket, _) = listener.accept().await.unwrap();
            let request: Request = read_json(&mut socket).await.unwrap();
            assert!(matches!(request.action, Action::ReadConfig));
            write_json(
                &mut socket,
                &Reply::<crate::config::AppConfig> {
                    version: VERSION,
                    id: request.id,
                    result: Err("配置文件损坏".into()),
                },
            )
            .await
            .unwrap();
        });
        assert!(client
            .read_config()
            .await
            .err()
            .expect("forged credentials must fail")
            .to_string()
            .contains("认证失败"));
        assert!(rx.try_recv().is_err());
        assert!(client
            .read_config()
            .await
            .err()
            .expect("service read errors must propagate")
            .to_string()
            .contains("配置文件损坏"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tcp_rejects_oversized_frames_before_allocating_payload() {
        use tokio::io::AsyncWriteExt;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        client.write_u32_le(65537).await.unwrap();
        let (tx, _) = mpsc::channel(1);
        let error = serve(
            socket,
            tx,
            Default::default(),
            broadcast::channel(1).0,
            CancellationToken::new(),
            Arc::new(crate::link::ServerLinks::default()),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("过长"));
    }

    #[tokio::test]
    async fn client_rejects_mismatched_reply_ids() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let client = Client {
            port: listener.local_addr().unwrap().port(),
            token: "test".into(),
        };
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request: Request = read_json(&mut socket).await.unwrap();
            write_json(
                &mut socket,
                &Reply {
                    version: VERSION,
                    id: request.id + 1,
                    result: Ok(Status::default()),
                },
            )
            .await
            .unwrap();
        });
        assert!(client.request(Action::Status).await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn notification_disconnect_releases_connection_and_session_change_closes_gui() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        let (events, mut subscription) = broadcast::channel(1);
        let cancel = CancellationToken::new();
        drop(client);
        tokio::time::timeout(
            SHORT,
            notify(&mut socket, &mut subscription, &cancel, || true),
        )
        .await
        .unwrap()
        .unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        let task = tokio::spawn(async move {
            notify(&mut socket, &mut subscription, &cancel, || false)
                .await
                .unwrap();
        });
        assert!(matches!(
            Client::next_event(&mut client).await.unwrap(),
            Event::Close
        ));
        task.await.unwrap();
        drop(events);
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_tcp_handshake_times_out() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let (tx, _) = mpsc::channel(1);
        let task = tokio::spawn(serve(
            socket,
            tx,
            Default::default(),
            broadcast::channel(1).0,
            CancellationToken::new(),
            Arc::new(crate::link::ServerLinks::default()),
        ));
        tokio::time::advance(SHORT + Duration::from_secs(1)).await;
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn host_stop_preserves_root_and_control_listener_and_releases_remote_ports() {
        let root = CancellationToken::new();
        let control = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut host = Host {
            broker: None,
            error: None,
        };
        // No session probes run before cancellation on this single-threaded test
        // runtime. No firewall or real installation state is modified.
        for _ in 0..3 {
            // Windows can reserve different port ranges for TCP and UDP. A
            // free ephemeral TCP port alone does not establish UDP availability.
            let mut selected = None;
            for _ in 0..64 {
                let reserved = TcpListener::bind(("0.0.0.0", 0)).await.unwrap();
                let port = reserved.local_addr().unwrap().port();
                if let Ok(udp) = tokio::net::UdpSocket::bind(("0.0.0.0", port)).await {
                    drop(udp);
                    selected = Some(port);
                    break;
                }
            }
            let port = selected.expect("a port available to both TCP and UDP");
            host.start_config(
                crate::config::AppConfig {
                    port,
                    password: "test".into(),
                    ..Default::default()
                },
                &root,
            )
            .await
            .unwrap();
            assert!(host.broker.is_some());
            assert!(host
                .set_with(true, &root, |_| anyhow::bail!("disk failure"))
                .await
                .is_err());
            assert!(
                host.broker.is_some(),
                "failed persistence preserves previous runtime state"
            );
            host.set_with(false, &root, |enabled| {
                assert!(!enabled);
                Ok(())
            })
            .await
            .unwrap();
            assert!(!root.is_cancelled());
            assert!(host.broker.is_none());
            let _tcp = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
            let _udp = tokio::net::UdpSocket::bind(("0.0.0.0", port))
                .await
                .unwrap();
            let _client = TcpStream::connect(control.local_addr().unwrap())
                .await
                .unwrap();
            control.accept().await.unwrap();
        }
    }
}
