use std::fmt;
use std::io;
use std::sync::Arc;
use xcb::{randr, x};

#[derive(Clone)]
pub struct Monitor {
    index: u32,
    conn: Arc<xcb::Connection>,
    root: x::Window,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    primary: bool,
}

impl Monitor {
    pub fn primary() -> io::Result<Self> {
        let monitors = Self::all()?;
        monitors
            .iter()
            .find(|monitor| monitor.primary)
            .or_else(|| monitors.first())
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Primary monitor not found"))
    }

    pub fn from_index(index: u32) -> io::Result<Self> {
        let monitors = Self::all()?;
        monitors
            .get(index as usize)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Monitor index out of range"))
    }

    pub fn all() -> io::Result<Vec<Self>> {
        let (conn, screen_num) = xcb::Connection::connect(None).map_err(to_io_error)?;
        let conn = Arc::new(conn);
        let screen = conn
            .get_setup()
            .roots()
            .nth(screen_num as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "X11 screen not found"))?;

        let root = screen.root();
        let mut monitors = monitors_from_randr(conn.clone(), root)
            .unwrap_or_else(|| fallback_monitor(conn.clone(), root, &screen));

        monitors.sort_by_key(|monitor| (!monitor.primary, monitor.index));
        for (index, monitor) in monitors.iter_mut().enumerate() {
            monitor.index = index as u32;
        }
        Ok(monitors)
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub(crate) fn conn(&self) -> &xcb::Connection {
        &self.conn
    }

    pub(crate) fn root(&self) -> x::Window {
        self.root
    }

    pub(crate) fn origin(&self) -> (i16, i16) {
        (self.x, self.y)
    }

    pub fn size(&self) -> io::Result<(u32, u32)> {
        Ok((self.width as u32, self.height as u32))
    }
}

impl fmt::Debug for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Monitor")
            .field("index", &self.index)
            .field("x", &self.x)
            .field("y", &self.y)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("primary", &self.primary)
            .finish()
    }
}

fn monitors_from_randr(conn: Arc<xcb::Connection>, root: x::Window) -> Option<Vec<Monitor>> {
    if let Ok(monitors) = monitors_from_get_monitors(conn.clone(), root) {
        if !monitors.is_empty() {
            return Some(monitors);
        }
    }

    if let Ok(monitors) = monitors_from_crtcs(conn, root) {
        if !monitors.is_empty() {
            return Some(monitors);
        }
    }

    None
}

fn monitors_from_get_monitors(
    conn: Arc<xcb::Connection>,
    root: x::Window,
) -> xcb::Result<Vec<Monitor>> {
    let reply = conn.wait_for_reply(conn.send_request(&randr::GetMonitors {
        window: root,
        get_active: true,
    }))?;

    Ok(reply
        .monitors()
        .enumerate()
        .filter(|(_, monitor)| monitor.width() != 0 && monitor.height() != 0)
        .map(|(index, monitor)| Monitor {
            index: index as u32,
            conn: conn.clone(),
            root,
            x: monitor.x(),
            y: monitor.y(),
            width: monitor.width(),
            height: monitor.height(),
            primary: monitor.primary(),
        })
        .collect())
}

fn monitors_from_crtcs(conn: Arc<xcb::Connection>, root: x::Window) -> xcb::Result<Vec<Monitor>> {
    let resources =
        conn.wait_for_reply(conn.send_request(&randr::GetScreenResourcesCurrent { window: root }))?;
    let primary_output = conn
        .wait_for_reply(conn.send_request(&randr::GetOutputPrimary { window: root }))
        .ok()
        .map(|reply| reply.output());

    let mut monitors = Vec::new();
    for crtc in resources.crtcs() {
        let info = conn.wait_for_reply(conn.send_request(&randr::GetCrtcInfo {
            crtc: *crtc,
            config_timestamp: resources.config_timestamp(),
        }))?;

        if info.width() == 0 || info.height() == 0 {
            continue;
        }

        let primary = primary_output
            .map(|output| info.outputs().contains(&output))
            .unwrap_or(false);

        monitors.push(Monitor {
            index: monitors.len() as u32,
            conn: conn.clone(),
            root,
            x: info.x(),
            y: info.y(),
            width: info.width(),
            height: info.height(),
            primary,
        });
    }

    Ok(monitors)
}

fn fallback_monitor(
    conn: Arc<xcb::Connection>,
    root: x::Window,
    screen: &x::Screen,
) -> Vec<Monitor> {
    vec![Monitor {
        index: 0,
        conn,
        root,
        x: 0,
        y: 0,
        width: screen.width_in_pixels(),
        height: screen.height_in_pixels(),
        primary: true,
    }]
}

pub(crate) fn to_io_error(error: impl fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
