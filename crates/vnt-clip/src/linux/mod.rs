//! Bounded ICCCM CLIPBOARD selections. PRIMARY is never read or changed.
use crate::{ClipboardContent, ClipboardError, ClipboardResult};
use std::{
    collections::HashMap,
    sync::{mpsc, Arc, OnceLock},
    time::{Duration, Instant},
};
use vnt_linux::x11rb::{
    self,
    connection::Connection,
    protocol::{
        xfixes,
        xproto::{self, ConnectionExt},
        Event,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};
thread_local! { static OWNER_SEQUENCE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) }; }
const TIMEOUT: Duration = Duration::from_secs(3);
const CHUNK: usize = 64 * 1024;
const MAX_TRANSFERS: usize = 16;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
type Reply<T> = mpsc::SyncSender<Result<T, String>>;
pub type ImageValidity = Arc<dyn Fn() -> bool + Send + Sync>;
#[derive(Clone)]
enum Content {
    Text(Arc<Vec<u8>>),
    Image(Arc<Vec<u8>>),
}
impl Content {
    fn bytes(&self) -> &Arc<Vec<u8>> {
        match self {
            Self::Text(v) | Self::Image(v) => v,
        }
    }
}
enum Command {
    Sequence(Reply<u32>),
    Publish(Content, u32, ImageValidity, Reply<()>),
    Subscribe(mpsc::SyncSender<()>, Reply<()>),
}
struct Runtime {
    tx: mpsc::SyncSender<Command>,
    window: u32,
}
fn runtime() -> Result<&'static Runtime, String> {
    static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            vnt_linux::session::check_x11()?;
            let (ready, rx) = mpsc::sync_channel(1);
            let (tx, commands) = mpsc::sync_channel(16);
            std::thread::Builder::new()
                .name("x11-clipboard".into())
                .spawn(move || match Owner::new() {
                    Ok(mut owner) => {
                        let _ = ready.send(Ok(owner.window));
                        if let Err(e) = owner.run(commands) {
                            log::warn!("X11 clipboard stopped: {e}");
                        }
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                    }
                })
                .map_err(err)?;
            Ok(Runtime {
                tx,
                window: rx.recv_timeout(TIMEOUT).map_err(err)??,
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}
fn request<T>(make: impl FnOnce(Reply<T>) -> Command) -> Result<T, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    runtime()?.tx.try_send(make(tx)).map_err(err)?;
    rx.recv_timeout(TIMEOUT + Duration::from_secs(1))
        .map_err(err)?
}
fn atom(conn: &RustConnection, name: &[u8]) -> Result<u32, String> {
    Ok(conn
        .intern_atom(false, name)
        .map_err(err)?
        .reply()
        .map_err(err)?
        .atom)
}
struct Atoms {
    clipboard: u32,
    targets: u32,
    utf8: u32,
    plain: u32,
    png: u32,
    incr: u32,
    property: u32,
}
impl Atoms {
    fn new(c: &RustConnection) -> Result<Self, String> {
        Ok(Self {
            clipboard: atom(c, b"CLIPBOARD")?,
            targets: atom(c, b"TARGETS")?,
            utf8: atom(c, b"UTF8_STRING")?,
            plain: atom(c, b"text/plain;charset=utf-8")?,
            png: atom(c, b"image/png")?,
            incr: atom(c, b"INCR")?,
            property: atom(c, b"VNT_CLIPBOARD")?,
        })
    }
}
fn window(conn: &RustConnection, screen: usize) -> Result<u32, String> {
    let id = conn.generate_id().map_err(err)?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        id,
        conn.setup().roots[screen].root,
        0,
        0,
        1,
        1,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        0,
        &xproto::CreateWindowAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE),
    )
    .map_err(err)?
    .check()
    .map_err(err)?;
    Ok(id)
}
struct Transfer {
    data: Arc<Vec<u8>>,
    offset: usize,
    target: u32,
    deadline: Instant,
}
struct Owner {
    conn: RustConnection,
    window: u32,
    atoms: Atoms,
    sequence: u32,
    owner: u32,
    content: Option<Content>,
    transfers: HashMap<(u32, u32), Transfer>,
    subscribers: Vec<mpsc::SyncSender<()>>,
}
impl Owner {
    fn new() -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(None).map_err(err)?;
        let window = window(&conn, screen)?;
        let atoms = Atoms::new(&conn)?;
        xfixes::query_version(&conn, 5, 0)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        xfixes::select_selection_input(
            &conn,
            window,
            atoms.clipboard,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(err)?
        .check()
        .map_err(err)?;
        let owner = conn
            .get_selection_owner(atoms.clipboard)
            .map_err(err)?
            .reply()
            .map_err(err)?
            .owner;
        Ok(Self {
            conn,
            window,
            atoms,
            owner,
            sequence: 1,
            content: None,
            transfers: HashMap::new(),
            subscribers: Vec::new(),
        })
    }
    fn changed(&mut self, owner: u32) {
        self.owner = owner;
        self.sequence = self.sequence.wrapping_add(1);
        OWNER_SEQUENCE.set(Some(self.sequence));
        if owner != self.window {
            self.content = None;
        }
        self.subscribers
            .retain(|tx| !matches!(tx.try_send(()), Err(mpsc::TrySendError::Disconnected(_))));
    }
    fn drain(&mut self) -> Result<(), String> {
        while let Some(event) = self.conn.poll_for_event().map_err(err)? {
            self.event(event)?;
        }
        Ok(())
    }
    fn run(&mut self, commands: mpsc::Receiver<Command>) -> Result<(), String> {
        OWNER_SEQUENCE.set(Some(self.sequence));
        loop {
            self.drain()?;
            self.transfers
                .retain(|_, transfer| transfer.deadline > Instant::now());
            match commands.recv_timeout(Duration::from_millis(10)) {
                Ok(Command::Sequence(reply)) => {
                    // A round trip ensures earlier owner notifications have arrived,
                    // including a copy by the same selection owner.
                    let owner = self
                        .conn
                        .get_selection_owner(self.atoms.clipboard)
                        .map_err(err)?
                        .reply()
                        .map_err(err)?
                        .owner;
                    self.drain()?;
                    if owner != self.owner {
                        self.changed(owner);
                    }
                    let _ = reply.send(Ok(self.sequence));
                }
                Ok(Command::Subscribe(tx, reply)) => {
                    self.subscribers.push(tx);
                    let _ = reply.send(Ok(()));
                }
                Ok(Command::Publish(data, expected, valid, reply)) => {
                    let result = self.publish(data, expected, &*valid);
                    let _ = reply.send(result);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
            }
            self.conn.flush().map_err(err)?;
        }
    }
    fn publish(
        &mut self,
        data: Content,
        expected: u32,
        valid: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        self.conn
            .get_selection_owner(self.atoms.clipboard)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        self.drain()?;
        if expected != self.sequence || !valid() {
            return Err("本机剪贴板已更新或请求过期".into());
        }
        self.conn
            .set_selection_owner(self.window, self.atoms.clipboard, x11rb::CURRENT_TIME)
            .map_err(err)?
            .check()
            .map_err(err)?;
        self.drain()?;
        let owner = self
            .conn
            .get_selection_owner(self.atoms.clipboard)
            .map_err(err)?
            .reply()
            .map_err(err)?
            .owner;
        if owner != self.window {
            return Err("本机剪贴板已更新".into());
        }
        self.content = Some(data);
        Ok(())
    }
    fn event(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::XfixesSelectionNotify(e) if e.selection == self.atoms.clipboard => {
                self.changed(e.owner)
            }
            Event::SelectionRequest(e) if e.selection == self.atoms.clipboard => {
                let property = if e.property == x11rb::NONE {
                    e.target
                } else {
                    e.property
                };
                let mut accepted = false;
                if let Some(content) = &self.content {
                    if e.target == self.atoms.targets {
                        let mut targets = vec![self.atoms.targets];
                        match content {
                            Content::Text(_) => targets.extend([self.atoms.utf8, self.atoms.plain]),
                            Content::Image(_) => targets.push(self.atoms.png),
                        }
                        accepted = self
                            .conn
                            .change_property32(
                                xproto::PropMode::REPLACE,
                                e.requestor,
                                property,
                                xproto::AtomEnum::ATOM,
                                &targets,
                            )
                            .map_err(err)?
                            .check()
                            .is_ok();
                    } else if matches!(content, Content::Text(_))
                        && [self.atoms.utf8, self.atoms.plain].contains(&e.target)
                        || matches!(content, Content::Image(_)) && e.target == self.atoms.png
                    {
                        let bytes = content.bytes();
                        if bytes.len() <= CHUNK {
                            accepted = self
                                .conn
                                .change_property8(
                                    xproto::PropMode::REPLACE,
                                    e.requestor,
                                    property,
                                    e.target,
                                    bytes,
                                )
                                .map_err(err)?
                                .check()
                                .is_ok();
                        } else if self.transfers.len() < MAX_TRANSFERS {
                            let selected = self
                                .conn
                                .change_window_attributes(
                                    e.requestor,
                                    &xproto::ChangeWindowAttributesAux::new()
                                        .event_mask(xproto::EventMask::PROPERTY_CHANGE),
                                )
                                .map_err(err)?
                                .check()
                                .is_ok();
                            if selected
                                && self
                                    .conn
                                    .change_property32(
                                        xproto::PropMode::REPLACE,
                                        e.requestor,
                                        property,
                                        self.atoms.incr,
                                        &[bytes.len() as u32],
                                    )
                                    .map_err(err)?
                                    .check()
                                    .is_ok()
                            {
                                self.transfers.insert(
                                    (e.requestor, property),
                                    Transfer {
                                        data: bytes.clone(),
                                        offset: 0,
                                        target: e.target,
                                        deadline: Instant::now() + TIMEOUT,
                                    },
                                );
                                accepted = true;
                            }
                        }
                    }
                }
                let notify = xproto::SelectionNotifyEvent {
                    response_type: xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: e.time,
                    requestor: e.requestor,
                    selection: e.selection,
                    target: e.target,
                    property: if accepted { property } else { x11rb::NONE },
                };
                let _ = self
                    .conn
                    .send_event(false, e.requestor, xproto::EventMask::NO_EVENT, notify)
                    .map_err(err)?
                    .check();
            }
            Event::PropertyNotify(e) if e.state == xproto::Property::DELETE => {
                let key = (e.window, e.atom);
                if let Some(transfer) = self.transfers.get_mut(&key) {
                    let end = (transfer.offset + CHUNK).min(transfer.data.len());
                    let result = self
                        .conn
                        .change_property8(
                            xproto::PropMode::REPLACE,
                            e.window,
                            e.atom,
                            transfer.target,
                            &transfer.data[transfer.offset..end],
                        )
                        .map_err(err)?
                        .check();
                    let finished = transfer.offset == end || result.is_err();
                    transfer.offset = end;
                    transfer.deadline = Instant::now() + TIMEOUT;
                    if finished {
                        self.transfers.remove(&key);
                    }
                }
            }
            _ => (),
        }
        Ok(())
    }
}

/// A separate requestor connection lets the owner serve INCR while a caller
/// waits for another application's selection. Both size and duration are bounded.
fn load(target: u32, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let (conn, screen) = x11rb::connect(None).map_err(err)?;
    let window = window(&conn, screen)?;
    let a = Atoms::new(&conn)?;
    conn.convert_selection(window, a.clipboard, target, a.property, x11rb::CURRENT_TIME)
        .map_err(err)?
        .check()
        .map_err(err)?;
    conn.flush().map_err(err)?;
    let deadline = Instant::now() + TIMEOUT;
    let mut data = Vec::new();
    let mut incremental = false;
    loop {
        if Instant::now() >= deadline {
            return Err("读取 X11 剪贴板超时".into());
        }
        let Some(event) = conn.poll_for_event().map_err(err)? else {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };
        let read = match event {
            Event::SelectionNotify(e)
                if e.requestor == window && e.selection == a.clipboard && e.target == target =>
            {
                if e.property == x11rb::NONE {
                    return Ok(None);
                }
                true
            }
            Event::PropertyNotify(e)
                if incremental
                    && e.window == window
                    && e.atom == a.property
                    && e.state == xproto::Property::NEW_VALUE =>
            {
                true
            }
            _ => false,
        };
        if !read {
            continue;
        }
        let reply = conn
            .get_property(
                true,
                window,
                a.property,
                xproto::AtomEnum::ANY,
                0,
                (limit / 4 + 1) as u32,
            )
            .map_err(err)?
            .reply()
            .map_err(err)?;
        if reply.type_ == a.incr && !incremental {
            let announced = reply.value32().and_then(|mut v| v.next());
            // Some X11 clients (including xclip) use an empty INCR header.
            // Treat its size as unknown and enforce the bound on every chunk.
            if announced.is_some_and(|n| n as usize > limit)
                || (reply.format != 32 && !reply.value.is_empty())
            {
                return Err(format!(
                    "剪贴板声明长度 {:?} 超过上限 {limit}（target={target}）",
                    announced
                ));
            }
            incremental = true;
        } else {
            if reply.type_
                != (if target == a.targets {
                    xproto::AtomEnum::ATOM.into()
                } else {
                    target
                })
                || reply.bytes_after != 0
            {
                return Err("剪贴板格式无效或内容超过上限".into());
            }
            if reply.value.len() > limit.saturating_sub(data.len()) {
                return Err(format!(
                    "剪贴板累计长度 {} + {} 超过上限 {limit}",
                    data.len(),
                    reply.value.len()
                ));
            }
            let finished = !incremental || reply.value.is_empty();
            data.extend_from_slice(&reply.value);
            if finished {
                return Ok(Some(data));
            }
        }
        conn.flush().map_err(err)?;
    }
}

pub enum LocalSnapshot {
    Image(RawImage),
    Text(String),
    Files(Vec<std::path::PathBuf>),
    Other,
    Ours,
}
pub enum RawImage {
    Png(Vec<u8>),
    Dib(Vec<u8>),
}
impl RawImage {
    pub fn decode(self) -> ClipboardResult<crate::image::ClipboardImage> {
        match self {
            Self::Png(b) => crate::image::from_png(b),
            Self::Dib(b) => crate::image::from_dib(&b),
        }
    }
}
pub fn sequence_number() -> u32 {
    OWNER_SEQUENCE
        .get()
        .unwrap_or_else(|| request(Command::Sequence).unwrap_or_default())
}
pub fn snapshot() -> ClipboardResult<LocalSnapshot> {
    fn read() -> Result<LocalSnapshot, String> {
        let before = request(Command::Sequence)?;
        let (conn, _) = x11rb::connect(None).map_err(err)?;
        let a = Atoms::new(&conn)?;
        let owner = conn
            .get_selection_owner(a.clipboard)
            .map_err(err)?
            .reply()
            .map_err(err)?
            .owner;
        if owner == runtime()?.window {
            return Ok(LocalSnapshot::Ours);
        }
        if owner == x11rb::NONE {
            return Ok(LocalSnapshot::Other);
        }
        let targets = load(a.targets, 4096)?.unwrap_or_default();
        let contains = |target: u32| {
            targets
                .chunks_exact(4)
                .any(|b| u32::from_ne_bytes(b.try_into().unwrap()) == target)
        };
        let result = if contains(a.png) {
            load(a.png, vnt_proto::clipboard::MAX_IMAGE_BYTES)?
                .map(|b| LocalSnapshot::Image(RawImage::Png(b)))
                .unwrap_or(LocalSnapshot::Other)
        } else if contains(a.utf8) || contains(a.plain) {
            let target = if contains(a.utf8) { a.utf8 } else { a.plain };
            match load(target, vnt_proto::control::MAX_CLIPBOARD_TEXT)? {
                Some(b) => LocalSnapshot::Text(String::from_utf8(b).map_err(err)?),
                None => LocalSnapshot::Other,
            }
        } else {
            LocalSnapshot::Other
        };
        if request(Command::Sequence)? != before {
            return Err("读取期间剪贴板已变化".into());
        }
        Ok(result)
    }
    read().map_err(ClipboardError::Read)
}
pub fn read() -> ClipboardResult<Option<ClipboardContent>> {
    Ok(match snapshot()? {
        LocalSnapshot::Text(t) => Some(ClipboardContent::Text(t)),
        LocalSnapshot::Image(i) => Some(ClipboardContent::Image(i.decode()?)),
        _ => None,
    })
}
pub fn set_text(text: &str) -> ClipboardResult<()> {
    NativeClipboard::start()
        .map_err(ClipboardError::Write)?
        .text(text.into(), sequence_number())
        .map_err(ClipboardError::Write)
}
pub trait FileOperation: Send + Sync {
    fn read(&self, index: u32, offset: u64, length: u32) -> Result<Vec<u8>, String>;
    fn close(&self, index: u32);
    fn finish(&self, ok: bool, reason: &str);
}
pub trait FileProvider: Send + Sync {
    fn begin(&self) -> Result<Arc<dyn FileOperation>, String>;
    fn available(&self) -> bool;
}
#[derive(Clone)]
pub struct NativeClipboard;
impl NativeClipboard {
    pub fn start() -> Result<Self, String> {
        runtime()?;
        Ok(Self)
    }
    pub fn start_with_updates() -> Result<(Self, mpsc::Receiver<()>), String> {
        let (tx, rx) = mpsc::sync_channel(1);
        request(|reply| Command::Subscribe(tx, reply))?;
        Ok((Self, rx))
    }
    pub fn text(&self, text: String, sequence: u32) -> Result<(), String> {
        if text.len() > vnt_proto::control::MAX_CLIPBOARD_TEXT {
            return Err("文本超过上限".into());
        }
        request(|reply| {
            Command::Publish(
                Content::Text(Arc::new(text.into_bytes())),
                sequence,
                Arc::new(|| true),
                reply,
            )
        })
    }
    pub fn image(
        &self,
        image: crate::image::PreparedImage,
        sequence: u32,
        valid: ImageValidity,
    ) -> Result<(), String> {
        if image.png.len() > vnt_proto::clipboard::MAX_IMAGE_BYTES {
            return Err("图片超过上限".into());
        }
        request(|reply| {
            Command::Publish(Content::Image(Arc::new(image.png)), sequence, valid, reply)
        })
    }
    pub fn paste_text(&self, text: String, valid: ImageValidity) -> Result<(), String> {
        if !valid() {
            return Err("粘贴请求已过期".into());
        }
        if text.len() > vnt_proto::control::MAX_CLIPBOARD_TEXT {
            return Err("文本超过上限".into());
        }
        let sequence = sequence_number();
        let check = valid.clone();
        request(|reply| {
            Command::Publish(
                Content::Text(Arc::new(text.into_bytes())),
                sequence,
                check,
                reply,
            )
        })?;
        if !valid() {
            return Err("粘贴请求已过期".into());
        }
        vnt_input::paste_shortcut().map_err(err)
    }
    pub fn files(
        &self,
        _: Arc<vnt_proto::pb::ClipboardOffer>,
        _: Arc<dyn FileProvider>,
        _: u32,
    ) -> Result<(), String> {
        Err("Linux 首版不支持文件剪贴板".into())
    }
}
