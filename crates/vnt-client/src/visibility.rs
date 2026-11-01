//! Native visibility polling keeps working while minimized WebViews are suspended.
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

#[derive(Default)]
struct StreamState {
    hidden_since: Option<Instant>,
    sent: Option<(u32, bool)>,
}

#[derive(Default)]
pub struct Visibility {
    pub reports: HashMap<String, u32>,
    streams: HashMap<u32, StreamState>,
}

impl Visibility {
    pub fn reconcile(
        &mut self,
        screens: &[(u32, u32, bool)],
        visible: &HashSet<u32>,
        online: bool,
        now: Instant,
    ) -> Vec<(u32, u32, bool)> {
        self.streams
            .retain(|id, _| screens.iter().any(|s| s.0 == *id));
        let mut updates = Vec::new();
        for &(id, generation, available) in screens {
            let state = self.streams.entry(id).or_default();
            if visible.contains(&id) {
                state.hidden_since = None;
            } else {
                state.hidden_since.get_or_insert(now);
            }
            let background = state
                .hidden_since
                .is_some_and(|since| now.duration_since(since) > Duration::from_secs(5));
            if !online || !available {
                state.sent = None;
            } else if state.sent != Some((generation, background)) {
                updates.push((id, generation, background));
            }
        }
        updates
    }

    pub fn sent(&mut self, id: u32, generation: u32, background: bool) {
        if let Some(state) = self.streams.get_mut(&id) {
            state.sent = Some((generation, background));
        }
    }

    pub fn has_stream(&self, id: u32) -> bool {
        self.streams.contains_key(&id)
    }

    pub fn invalidate(&mut self) {
        for state in self.streams.values_mut() {
            state.sent = None;
        }
    }
}
