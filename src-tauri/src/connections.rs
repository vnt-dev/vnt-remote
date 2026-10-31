//! Connection requests commit only if they still own the current generation.
use crate::client::ClientHandle;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct Connections {
    generation: u64,
    pub event_generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pending: Option<CancellationToken>,
    pub handle: Option<ClientHandle>,
    pub info: Option<crate::client::ClientSessionInfo>,
    pub input_lane: std::sync::Arc<tokio::sync::Mutex<()>>,
    pub keyboard_owner: Option<String>,

    #[cfg(any(windows, target_os = "macos"))]
    pub keyboard: Option<crate::keyboard::KeyboardCapture>,
}

impl Connections {
    pub fn cancel(&mut self) {
        self.info = None;
        self.keyboard_owner = None;
        self.input_lane = Default::default();
        self.generation = self.generation.wrapping_add(1);
        self.event_generation
            .store(self.generation, std::sync::atomic::Ordering::Release);
        #[cfg(any(windows, target_os = "macos"))]
        self.keyboard.take();
        if let Some(pending) = self.pending.take() {
            pending.cancel();
        }
        if let Some(handle) = self.handle.take() {
            handle.disconnect();
        }
    }

    pub fn begin(&mut self) -> (u64, CancellationToken) {
        self.cancel();
        let cancel = CancellationToken::new();
        self.pending = Some(cancel.clone());
        (self.generation, cancel)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    pub fn commit(&mut self, generation: u64, handle: ClientHandle) -> Result<(), String> {
        if !self.is_current(generation) {
            return Err("连接已取消".into());
        }
        self.pending = None;
        self.handle = Some(handle);
        Ok(())
    }

    pub fn finish_pending(&mut self, generation: u64) {
        if self.is_current(generation) {
            self.pending = None;
        }
    }
}

impl Drop for Connections {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_invalidates_the_old_input_lane_and_window_generation() {
        let mut connection = Connections::default();
        let (generation, _) = connection.begin();
        let lane = connection.input_lane.clone();
        connection.keyboard_owner = Some("child".into());
        connection.begin();
        assert!(!connection.is_current(generation));
        assert_ne!(
            connection
                .event_generation
                .load(std::sync::atomic::Ordering::Acquire),
            generation
        );
        assert!(!std::sync::Arc::ptr_eq(&lane, &connection.input_lane));
        assert!(connection.keyboard_owner.is_none());
    }

    #[test]
    fn dropping_a_window_cancels_its_pending_handshake() {
        let mut state = Connections::default();
        let (_, pending) = state.begin();
        drop(state);
        assert!(pending.is_cancelled());
    }

    #[test]
    fn closing_one_window_does_not_cancel_another_pending_connection() {
        let mut clients = std::collections::HashMap::<String, Connections>::new();
        let (_, first) = clients.entry("remote-1".into()).or_default().begin();
        let (generation, second) = clients.entry("remote-2".into()).or_default().begin();
        clients.remove("remote-1").unwrap().cancel();
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        assert!(clients["remote-2"].is_current(generation));
    }

    #[test]
    fn disconnect_invalidates_pending_connection() {
        let mut state = Connections::default();
        let (generation, cancel) = state.begin();
        state.cancel();
        assert!(cancel.is_cancelled());
        assert!(!state.is_current(generation));
    }

    #[test]
    fn superseded_completion_cannot_clear_new_request() {
        let mut state = Connections::default();
        let (old, old_cancel) = state.begin();
        let (new, new_cancel) = state.begin();
        state.finish_pending(old);
        assert!(old_cancel.is_cancelled());
        assert!(state.is_current(new));
        state.cancel();
        assert!(new_cancel.is_cancelled());
    }
}
