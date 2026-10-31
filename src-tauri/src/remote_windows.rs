//! Screen windows are views of an owner connection, never independent clients.
use crate::{
    client::{ClientSessionInfo, ScreenInputTarget},
    AppState,
};
use tauri::{Emitter, Manager};
use vnt_proto::{control::ControlMessage as Msg, pb};

#[derive(Clone)]
pub struct ChildWindow {
    pub owner: String,
    pub session_id: u32,
    pub generation: u64,
    pub stream_id: u32,
    pub token: String,
}

pub fn owner(state: &AppState, label: &str, session: u32) -> Result<String, String> {
    if let Some(child) = state.child_windows.lock().unwrap().get(label) {
        if child.session_id != session {
            return Err("会话已变化".into());
        }
        return Ok(child.owner.clone());
    }
    Ok(label.to_owned())
}

pub fn owns(state: &AppState, label: &str, owner: &str, session: u32, stream: u32) -> bool {
    let children = state.child_windows.lock().unwrap();
    if let Some(child) = children.get(label) {
        child.owner == owner && child.session_id == session && child.stream_id == stream
    } else {
        label == owner
            && !children
                .values()
                .any(|c| c.owner == owner && c.session_id == session && c.stream_id == stream)
    }
}

fn take_children(state: &AppState, owner: &str) -> Vec<String> {
    let mut children = state.child_windows.lock().unwrap();
    let labels = children
        .iter()
        .filter(|(_, child)| child.owner == owner)
        .map(|(label, _)| label.clone())
        .collect::<Vec<_>>();
    for label in &labels {
        children.remove(label);
    }
    labels
}

pub fn close_children(app: &tauri::AppHandle, owner: &str) {
    let state = app.state::<AppState>();
    let labels = take_children(&state, owner);
    for label in labels {
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
    }
}

pub fn close_window(app: &tauri::AppHandle, label: &str) {
    let state = app.state::<AppState>();
    let child = state.child_windows.lock().unwrap().remove(label);
    if let Some(child) = child {
        {
            let mut clients = state.client.lock().unwrap();
            if let Some(connection) = clients
                .get_mut(&child.owner)
                .filter(|c| c.is_current(child.generation))
            {
                if let Some(handle) = connection
                    .handle
                    .as_ref()
                    .filter(|h| h.session_id() == child.session_id)
                {
                    if connection.keyboard_owner.as_deref() == Some(label) {
                        #[cfg(any(windows, target_os = "macos"))]
                        connection.keyboard.take();
                        connection.keyboard_owner = None;
                    }
                    handle.bridge.close_view(child.stream_id);
                }
            }
        }
        // A closed window's route stays reserved even while control is down.
        // Retry StopVideo against this exact session until the host confirms it.
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                {
                    let state = app.state::<AppState>();
                    let clients = state.client.lock().unwrap();
                    let Some(handle) = clients
                        .get(&child.owner)
                        .filter(|c| c.is_current(child.generation))
                        .and_then(|c| c.handle.as_ref())
                        .filter(|h| h.session_id() == child.session_id)
                    else {
                        break;
                    };
                    let screens = handle.screens.lock().unwrap();
                    if !screens
                        .screens
                        .iter()
                        .any(|s| s.stream_id == child.stream_id)
                    {
                        break;
                    }
                    if handle.control_available() {
                        let _ = handle
                            .control_tx
                            .try_send(Msg::ScreenCommand(pb::ScreenCommand {
                                stream_id: child.stream_id,
                                revision: screens.revision,
                                action: 1,
                            }));
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        });
        return;
    }
    // CloseRequested and Destroyed both arrive. Only the first owner removal
    // notifies Home; children removed during teardown cannot end the session.
    if !state.client.lock().unwrap().contains_key(label) {
        return;
    }
    close_children(app, label);
    state.close_control_window(label);
    let _ = app.emit_to("main", "remote://closed", label.to_string());
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenWindowContext {
    session: ClientSessionInfo,
    stream_id: u32,
    peer: String,
    clipboard_enabled: bool,
}

#[tauri::command]
pub fn screen_window_context(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<ScreenWindowContext, String> {
    let child = state
        .child_windows
        .lock()
        .unwrap()
        .get(window.label())
        .cloned()
        .ok_or("屏幕窗口已关闭")?;
    let clients = state.client.lock().unwrap();
    let connection = clients
        .get(&child.owner)
        .filter(|c| c.is_current(child.generation))
        .ok_or("会话已结束")?;
    let handle = connection
        .handle
        .as_ref()
        .filter(|h| h.session_id() == child.session_id)
        .ok_or("会话已变化")?;
    let mut info = connection.info.clone().ok_or("会话尚未就绪")?;
    info.screens = handle
        .screens
        .lock()
        .unwrap()
        .screens
        .iter()
        .filter(|s| s.stream_id == child.stream_id)
        .map(crate::client::screens::info)
        .collect();
    if info.screens.is_empty() {
        return Err("屏幕已关闭".into());
    }
    let screen = handle
        .screens
        .lock()
        .unwrap()
        .screens
        .iter()
        .find(|s| s.stream_id == child.stream_id)
        .cloned()
        .ok_or("屏幕已关闭")?;
    info.width = screen.width;
    info.height = screen.height;
    info.fps = screen.fps;
    info.codec = vnt_proto::Codec::from_proto(screen.codec())
        .map(|codec| codec.as_str().to_owned())
        .unwrap_or(info.codec);
    info.hardware = screen.hardware;
    info.token = child.token;
    let peer = state
        .window_params
        .lock()
        .unwrap()
        .get(&child.owner)
        .map(|p| p.host.clone())
        .unwrap_or_default();
    let clipboard_enabled = info
        .clipboard_connection_id
        .parse()
        .ok()
        .is_some_and(|id| state.clipboard.session_enabled(id));
    Ok(ScreenWindowContext {
        session: info,
        stream_id: child.stream_id,
        peer,
        clipboard_enabled,
    })
}

#[tauri::command]
pub async fn detach_screen(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    session_id: u32,
    stream_id: u32,
) -> Result<String, String> {
    if state
        .child_windows
        .lock()
        .unwrap()
        .contains_key(window.label())
    {
        return Err("子窗口不能拆分屏幕".into());
    }
    let owner = window.label().to_owned();
    let label = format!(
        "remote-screen-{}",
        state
            .next_window
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let lane = state
        .client
        .lock()
        .unwrap()
        .get(&owner)
        .ok_or("会话已结束")?
        .input_lane
        .clone();
    let input_guard = lane.lock().await;
    let screen_name;
    let connection_generation;
    {
        let mut clients = state.client.lock().unwrap();
        let connection = clients
            .get_mut(&owner)
            .filter(|c| std::sync::Arc::ptr_eq(&c.input_lane, &lane))
            .ok_or("会话已结束")?;
        connection_generation = connection.generation();
        let handle = connection
            .handle
            .as_ref()
            .filter(|h| h.session_id() == session_id)
            .ok_or("会话已变化")?;
        let mut children = state.child_windows.lock().unwrap();
        if let Some((existing, _)) = children.iter().find(|(_, c)| {
            c.owner == owner && c.session_id == session_id && c.stream_id == stream_id
        }) {
            let existing = existing.clone();
            drop(children);
            drop(clients);
            if let Some(window) = app.get_webview_window(&existing) {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            return Ok(existing);
        }
        if !handle.control_available() {
            return Err("控制连接恢复中，请稍后重试".into());
        }
        let screens = handle.screens.lock().unwrap();
        let views = handle.bridge.views.lock().unwrap();
        let remaining = screens
            .screens
            .iter()
            .filter(|s| !views.contains_key(&s.stream_id))
            .count();
        if remaining <= 1 {
            return Err("主窗口至少保留一个屏幕".into());
        }
        if views.contains_key(&stream_id) {
            return Err("屏幕正在关闭".into());
        }
        screen_name = screens
            .screens
            .iter()
            .find(|s| s.stream_id == stream_id)
            .ok_or("屏幕已关闭")?
            .name
            .clone();
        drop(views);
        drop(screens);
        handle.reset_screen_input()?;
        let token = handle.bridge.detach(stream_id, label.clone())?;
        children.insert(
            label.clone(),
            ChildWindow {
                owner: owner.clone(),
                session_id,
                generation: connection.generation(),
                stream_id,
                token,
            },
        );
        // Dropping a hook joins its thread, whose failure callback emits an
        // event. Release the child registry before joining to avoid a cycle.
        drop(children);
        #[cfg(any(windows, target_os = "macos"))]
        connection.keyboard.take();
        connection.keyboard_owner = None;
    }
    drop(input_guard);
    let peer = state
        .window_params
        .lock()
        .unwrap()
        .get(&owner)
        .map(|p| p.host.clone())
        .unwrap_or_default();
    let built = tauri::WebviewWindowBuilder::new(
        &app,
        &label,
        tauri::WebviewUrl::App("index.html?remote=1&screen=1".into()),
    )
    .title(format!("{peer} · {screen_name} · VNT Remote"))
    .inner_size(1280.0, 800.0)
    .min_inner_size(800.0, 520.0)
    .decorations(false)
    .build();
    if let Err(error) = built {
        state.child_windows.lock().unwrap().remove(&label);
        if let Some(handle) = state
            .client
            .lock()
            .unwrap()
            .get(&owner)
            .filter(|c| c.is_current(connection_generation))
            .and_then(|c| c.handle.as_ref())
            .filter(|h| h.session_id() == session_id)
        {
            handle.bridge.views.lock().unwrap().remove(&stream_id);
            handle.bridge.refresh_views();
        }
        return Err(format!("创建屏幕窗口失败：{error}"));
    }
    if !state.child_windows.lock().unwrap().contains_key(&label) {
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
        return Err("会话已结束".into());
    }
    if let Some(handle) = state
        .client
        .lock()
        .unwrap()
        .get(&owner)
        .filter(|c| c.is_current(connection_generation))
        .and_then(|c| c.handle.as_ref())
        .filter(|h| h.session_id() == session_id)
    {
        handle.bridge.refresh_views();
        let generation = handle
            .screens
            .lock()
            .unwrap()
            .screens
            .iter()
            .find(|s| s.stream_id == stream_id)
            .map(|s| s.generation);
        if let Some(generation) = generation {
            let _ = handle.control_tx.try_send(vnt_proto::screen::wrap(
                stream_id,
                generation,
                Msg::RequestKeyframe,
            ));
        }
    }
    Ok(label)
}

/// Check the native topmost window as well as webview intent. A queued release
/// from a window the pointer has left must never reclaim another screen.
fn pointer_in_window(window: &tauri::WebviewWindow) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{
            Foundation::POINT,
            UI::WindowsAndMessaging::{GetAncestor, GetCursorPos, WindowFromPoint, GA_ROOT},
        };
        let mut point = POINT::default();
        window.hwnd().is_ok_and(|hwnd| {
            GetCursorPos(&mut point).is_ok()
                && GetAncestor(WindowFromPoint(point), GA_ROOT).0 == hwnd.0
        })
    }
    #[cfg(not(windows))]
    window.is_focused().unwrap_or(false)
}

/// Caller holds the connection's input_lane across selection and sending.
pub async fn claim(
    window: &tauri::WebviewWindow,
    state: &AppState,
    owner: &str,
    session_id: u32,
    target: ScreenInputTarget,
) -> Result<Option<ScreenInputTarget>, String> {
    if !owns(state, window.label(), owner, session_id, target.stream_id) {
        return Ok(None);
    }
    let (screens, tx, needs_select) = {
        let mut clients = state.client.lock().unwrap();
        let connection = clients.get_mut(owner).ok_or("会话已结束")?;
        let handle = connection
            .handle
            .as_ref()
            .filter(|h| h.session_id() == session_id)
            .ok_or("会话已变化")?;
        if !handle.control_available() {
            return Ok(None);
        }
        let allowed = handle
            .bridge
            .views
            .lock()
            .unwrap()
            .get(&target.stream_id)
            .map_or(window.label() == owner, |view| view.label == window.label());
        if !allowed {
            return Ok(None);
        }
        let screens = handle.screens.lock().unwrap();
        if target.revision > screens.revision
            || !screens.screens.iter().any(|s| {
                s.stream_id == target.stream_id && s.generation == target.generation && s.available
            })
        {
            return Ok(None);
        }
        let needs_select = screens.active_stream_id != target.stream_id;
        drop(screens);
        if (needs_select || connection.keyboard_owner.as_deref() != Some(window.label()))
            && !pointer_in_window(window)
        {
            return Ok(None);
        }
        if needs_select || connection.keyboard_owner.as_deref() != Some(window.label()) {
            #[cfg(any(windows, target_os = "macos"))]
            connection.keyboard.take();
            connection.keyboard_owner = Some(window.label().to_owned());
        }
        (
            handle.screens.clone(),
            handle.control_tx.clone(),
            needs_select,
        )
    };
    if needs_select {
        let revision = screens.lock().unwrap().revision;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tx.send(Msg::ScreenCommand(pb::ScreenCommand {
                stream_id: target.stream_id,
                revision,
                action: 0,
            })),
        )
        .await
        .map_err(|_| "屏幕控制通道超时".to_string())?
        .map_err(|e| e.to_string())?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if screens.lock().unwrap().active_stream_id == target.stream_id {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("屏幕控制权交接超时".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        if !pointer_in_window(window) {
            return Ok(None);
        }
    }
    if !owns(state, window.label(), owner, session_id, target.stream_id) {
        return Ok(None);
    }
    let clients = state.client.lock().unwrap();
    let Some(handle) = clients
        .get(owner)
        .and_then(|c| c.handle.as_ref())
        .filter(|h| {
            h.session_id() == session_id && h.control_available() && h.control_tx.same_channel(&tx)
        })
    else {
        return Ok(None);
    };
    let screens = handle.screens.lock().unwrap();
    if screens.active_stream_id != target.stream_id
        || !screens.screens.iter().any(|s| {
            s.stream_id == target.stream_id && s.generation == target.generation && s.available
        })
    {
        return Ok(None);
    }
    Ok(Some(ScreenInputTarget {
        revision: screens.revision,
        ..target
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        let clipboard =
            || crate::clipboard::ClipboardService::with_writer(std::sync::Arc::new(|_, _| {}));
        AppState {
            link: std::sync::Arc::new(vnt_host::link::ServerLinks::default()),
            store: crate::config::ConfigStore::new(std::path::Path::new(".")),
            host: Default::default(),
            host_clipboard: clipboard(),
            clipboard: clipboard(),
            config: Default::default(),
            config_update: Default::default(),
            next_window: 1.into(),
            resolution_notices: Default::default(),
            window_params: Default::default(),
            child_windows: Default::default(),
            client: Default::default(),
        }
    }

    fn child(owner: &str, stream_id: u32) -> ChildWindow {
        ChildWindow {
            owner: owner.into(),
            session_id: 42,
            generation: 1,
            stream_id,
            token: "local-view-token".into(),
        }
    }

    #[test]
    fn child_windows_resolve_only_their_own_session_and_screen() {
        let state = state();
        state
            .child_windows
            .lock()
            .unwrap()
            .insert("child-2".into(), child("remote-1", 2));
        assert_eq!(owner(&state, "child-2", 42).unwrap(), "remote-1");
        assert!(owner(&state, "child-2", 43).is_err());
        assert!(owns(&state, "child-2", "remote-1", 42, 2));
        assert!(!owns(&state, "child-2", "remote-1", 42, 1));
        assert!(!owns(&state, "child-2", "remote-2", 42, 2));
        assert!(!owns(&state, "remote-1", "remote-1", 42, 2));
        assert!(owns(&state, "remote-1", "remote-1", 42, 1));
    }

    #[test]
    fn owner_teardown_removes_only_its_children_and_is_idempotent() {
        let state = state();
        let (_, pending_owner) = state
            .client
            .lock()
            .unwrap()
            .entry("remote-1".into())
            .or_default()
            .begin();
        let (_, pending_other) = state
            .client
            .lock()
            .unwrap()
            .entry("remote-2".into())
            .or_default()
            .begin();
        {
            let mut children = state.child_windows.lock().unwrap();
            children.insert("child-2".into(), child("remote-1", 2));
            children.insert("child-3".into(), child("remote-1", 3));
            children.insert("unrelated".into(), child("remote-2", 2));
        }
        let mut removed = take_children(&state, "remote-1");
        removed.sort();
        assert_eq!(removed, ["child-2", "child-3"]);
        assert!(take_children(&state, "remote-1").is_empty());
        // Child close callbacks arriving during owner teardown are harmless.
        state.close_control_window("child-2");
        assert!(!pending_owner.is_cancelled());
        state.close_control_window("remote-1");
        assert!(pending_owner.is_cancelled());
        assert!(!pending_other.is_cancelled());
        assert!(state
            .child_windows
            .lock()
            .unwrap()
            .contains_key("unrelated"));
    }
}
