use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use x11rb::{connection::Connection, protocol::xproto::ConnectionExt};

pub fn validate_environment(
    kind: Option<&str>,
    wayland: Option<&str>,
    display: Option<&str>,
) -> Result<(), String> {
    if kind.is_some_and(|s| s.eq_ignore_ascii_case("wayland"))
        || wayland.is_some_and(|s| !s.is_empty())
    {
        return Err("Linux 首版仅支持 X11；请退出登录并选择 Ubuntu on Xorg / X11 会话".into());
    }
    if display.is_none_or(str::is_empty) {
        return Err("未找到 X11 显示会话；请在已登录的桌面中运行应用".into());
    }
    Ok(())
}

pub fn check_x11() -> Result<(), String> {
    validate_environment(
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("DISPLAY").ok().as_deref(),
    )?;
    let (conn, _) = x11rb::connect(None).map_err(|e| format!("无法连接 X11 桌面：{e}"))?;
    for name in ["RANDR", "XTEST", "XFIXES"] {
        let reply = conn
            .query_extension(name.as_bytes())
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        if !reply.present {
            return Err(format!("X11 桌面缺少 {name} 扩展"));
        }
    }
    conn.flush().map_err(|e| e.to_string())
}

/// Fail closed until a desktop lock provider has answered. All D-Bus calls stay
/// off the input/capture threads; loss of the provider pauses existing sessions.
static UNLOCKED_AT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
fn update_unlocked(value: bool) {
    *UNLOCKED_AT.get_or_init(|| Mutex::new(None)).lock().unwrap() = value.then(Instant::now);
}
pub fn unlocked() -> bool {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("linux-session".into())
            .spawn(|| loop {
                update_unlocked(query_unlocked().unwrap_or(false));
                std::thread::sleep(Duration::from_millis(250));
            });
        let _ = std::thread::Builder::new()
            .name("linux-lock-events".into())
            .spawn(|| loop {
                if let Err(error) = watch_lock() {
                    log::debug!("Desktop lock notifications unavailable: {error}");
                }
                std::thread::sleep(Duration::from_secs(1));
            });
    });
    // Even a wedged D-Bus provider cannot leave sharing enabled indefinitely.
    UNLOCKED_AT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
}

fn session_bus() -> Result<zbus::blocking::Connection, zbus::Error> {
    zbus::blocking::connection::Builder::session()?
        .method_timeout(Duration::from_millis(500))
        .build()
}
fn watch_lock() -> Result<(), zbus::Error> {
    let connection = session_bus()?;
    for (name, path) in [
        ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
        ("org.freedesktop.ScreenSaver", "/ScreenSaver"),
    ] {
        let proxy = zbus::blocking::Proxy::new(&connection, name, path, name)?;
        let signals = proxy.receive_signal("ActiveChanged")?;
        if proxy.call::<_, _, bool>("GetActive", &()).is_err() {
            continue;
        }
        for signal in signals {
            let (active,): (bool,) = signal.body().deserialize()?;
            // Unlock resumes only after the periodic readiness query; a lock
            // notification immediately invalidates all existing input epochs.
            if active {
                update_unlocked(false);
            }
        }
        update_unlocked(false);
    }
    Ok(())
}

fn query_unlocked() -> Result<bool, zbus::Error> {
    // ScreenSaver is available in GNOME and KDE user sessions, including when
    // the application was launched outside the session's logind cgroup.
    let connection = session_bus()?;
    for (name, path) in [
        ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
        ("org.freedesktop.ScreenSaver", "/ScreenSaver"),
    ] {
        if let Ok(proxy) = zbus::blocking::Proxy::new(&connection, name, path, name) {
            if let Ok(active) = proxy.call::<_, _, bool>("GetActive", &()) {
                return Ok(!active);
            }
        }
    }
    let connection = zbus::blocking::connection::Builder::system()?
        .method_timeout(Duration::from_millis(500))
        .build()?;
    let manager = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;
    let path: zbus::zvariant::OwnedObjectPath =
        manager.call("GetSessionByPID", &(std::process::id(),))?;
    let session = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.login1.Session",
    )?;
    let active: bool = session.get_property("Active")?;
    let locked: bool = session.get_property("LockedHint")?;
    Ok(active && !locked)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_xwayland_and_missing_displays() {
        assert!(validate_environment(Some("wayland"), None, Some(":0")).is_err());
        assert!(validate_environment(Some("x11"), Some("wayland-0"), Some(":0")).is_err());
        assert!(validate_environment(None, None, None).is_err());
        assert!(validate_environment(Some("x11"), None, Some(":0")).is_ok());
    }
}
