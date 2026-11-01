//! Desktop window visibility adapter.
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use tauri::Manager;
#[cfg(test)]
use vnt_client::visibility::Visibility;
use vnt_proto::{control::ControlMessage as Msg, pb, screen};
fn viewed_streams(
    owner: &str,
    main_stream: u32,
    routes: &HashMap<u32, String>,
    minimized: &HashMap<String, bool>,
) -> HashSet<u32> {
    let mut visible = HashSet::new();
    if minimized.get(owner) == Some(&false) && !routes.contains_key(&main_stream) {
        visible.insert(main_stream);
    }
    for (&id, label) in routes {
        if minimized.get(label) == Some(&false) {
            visible.insert(id);
        }
    }
    visible
}

/// Reports what is painted, independently of the host's input ownership.
#[tauri::command]
pub fn client_view_screen(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, crate::AppState>,
    session_id: u32,
    stream_id: u32,
    generation: u32,
) -> Result<(), String> {
    let owner = crate::remote_windows::owner(&state, window.label(), session_id)?;
    if !crate::remote_windows::owns(&state, window.label(), &owner, session_id, stream_id) {
        return Err("屏幕不属于此窗口".into());
    }
    let clients = state.client.lock().unwrap();
    let handle = clients
        .get(&owner)
        .and_then(|c| c.handle.as_ref())
        .filter(|h| h.session_id() == session_id)
        .ok_or("会话已变化")?;
    if !handle
        .screens
        .lock()
        .unwrap()
        .screens
        .iter()
        .any(|s| s.stream_id == stream_id && s.generation == generation)
    {
        return Err("屏幕已变化".into());
    }
    handle
        .visibility
        .lock()
        .unwrap()
        .reports
        .insert(window.label().to_owned(), stream_id);
    Ok(())
}

pub fn start(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_millis(200));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            // Native calls must run outside application locks: Windows can dispatch
            // lifecycle events while retrieving window state.
            let minimized = app
                .webview_windows()
                .into_iter()
                .map(|(label, window)| (label, window.is_minimized().unwrap_or(false)))
                .collect::<HashMap<_, _>>();
            let state = app.state::<crate::AppState>();
            let clients = state.client.lock().unwrap();
            for (owner, connection) in clients.iter() {
                let Some(handle) = connection.handle.as_ref() else {
                    continue;
                };
                let screens = handle.screens.lock().unwrap();
                let routes = handle
                    .bridge
                    .views
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(&id, view)| (id, view.label.clone()))
                    .collect::<HashMap<_, _>>();
                let mut visibility = handle.visibility.lock().unwrap();
                visibility
                    .reports
                    .retain(|label, _| minimized.contains_key(label));
                let main_stream = visibility
                    .reports
                    .get(owner)
                    .copied()
                    .filter(|id| {
                        !routes.contains_key(id)
                            && screens.screens.iter().any(|s| s.stream_id == *id)
                    })
                    .or_else(|| {
                        screens
                            .screens
                            .iter()
                            .find(|s| {
                                s.stream_id == screens.active_stream_id
                                    && !routes.contains_key(&s.stream_id)
                            })
                            .map(|s| s.stream_id)
                    })
                    .or_else(|| {
                        screens
                            .screens
                            .iter()
                            .find(|s| !routes.contains_key(&s.stream_id))
                            .map(|s| s.stream_id)
                    })
                    .unwrap_or(0);
                let visible = viewed_streams(owner, main_stream, &routes, &minimized);
                let metadata = screens
                    .screens
                    .iter()
                    .map(|s| (s.stream_id, s.generation, s.available))
                    .collect::<Vec<_>>();
                for (id, generation, background) in visibility.reconcile(
                    &metadata,
                    &visible,
                    handle.control_available(),
                    Instant::now(),
                ) {
                    if handle
                        .control_tx
                        .try_send(screen::wrap(
                            id,
                            generation,
                            Msg::ScreenThrottle(pb::ScreenThrottle { background }),
                        ))
                        .is_ok()
                    {
                        visibility.sent(id, generation, background);
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_threshold_resets_and_restores_immediately() {
        let mut state = Visibility::default();
        let screens = [(1, 1, true)];
        let now = Instant::now();
        assert_eq!(
            state.reconcile(&screens, &HashSet::new(), true, now),
            [(1, 1, false)]
        );
        state.sent(1, 1, false);
        assert!(state
            .reconcile(
                &screens,
                &HashSet::new(),
                true,
                now + Duration::from_secs(5)
            )
            .is_empty());
        assert_eq!(
            state.reconcile(
                &screens,
                &HashSet::new(),
                true,
                now + Duration::from_millis(5001)
            ),
            [(1, 1, true)]
        );
        state.sent(1, 1, true);
        assert_eq!(
            state.reconcile(
                &screens,
                &HashSet::from([1]),
                true,
                now + Duration::from_secs(6)
            ),
            [(1, 1, false)]
        );
        state.sent(1, 1, false);
        assert!(state
            .reconcile(
                &screens,
                &HashSet::new(),
                true,
                now + Duration::from_secs(7)
            )
            .is_empty());
        assert!(state
            .reconcile(
                &screens,
                &HashSet::new(),
                true,
                now + Duration::from_secs(11)
            )
            .is_empty());
    }

    #[test]
    fn each_window_affects_only_its_own_view_regardless_of_input_owner() {
        let routes = HashMap::from([(2, "child".into())]);
        let mut windows = HashMap::from([("owner".into(), true), ("child".into(), false)]);
        assert_eq!(
            viewed_streams("owner", 1, &routes, &windows),
            HashSet::from([2])
        );
        windows.insert("owner".into(), false);
        windows.insert("child".into(), true);
        assert_eq!(
            viewed_streams("owner", 1, &routes, &windows),
            HashSet::from([1])
        );
        windows.remove("child");
        assert_eq!(
            viewed_streams("owner", 1, &routes, &windows),
            HashSet::from([1])
        );
        assert!(viewed_streams("owner", 2, &routes, &windows).is_empty());
    }

    #[test]
    fn reconnect_generation_changes_and_failed_enqueue_resend_latest_state() {
        let mut state = Visibility::default();
        let now = Instant::now();
        let screens = [(1, 1, true), (2, 1, true)];
        state.reconcile(&screens, &HashSet::new(), true, now);
        state.sent(1, 1, false);
        state.sent(2, 1, false);
        let later = now + Duration::from_secs(6);
        assert_eq!(
            state
                .reconcile(&screens, &HashSet::new(), true, later)
                .len(),
            2
        );
        // Failed enqueue is not committed and is retried on the next poll.
        assert_eq!(
            state.reconcile(&screens, &HashSet::from([1]), true, later),
            [(2, 1, true)]
        );
        state.sent(2, 1, true);
        state.reconcile(&screens, &HashSet::from([1]), false, later);
        assert_eq!(
            state.reconcile(&screens, &HashSet::from([1]), true, later),
            [(1, 1, false), (2, 1, true)]
        );
        state.sent(2, 1, true);
        assert_eq!(
            state.reconcile(&[(2, 2, true)], &HashSet::new(), true, later),
            [(2, 2, true)]
        );
        assert!(!state.has_stream(1));
        state.sent(2, 2, true);
        state.invalidate();
        assert_eq!(
            state.reconcile(&[(2, 2, true)], &HashSet::new(), true, later),
            [(2, 2, true)]
        );
    }
}
