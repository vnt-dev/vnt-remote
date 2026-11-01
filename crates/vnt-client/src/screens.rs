use std::sync::{Arc, Mutex};
use vnt_proto::{control::ControlMessage as Msg, pb, screen};

pub type State = Arc<Mutex<pb::ScreensState>>;

pub fn info(info: &pb::ScreenInfo) -> serde_json::Value {
    serde_json::json!({"streamId":info.stream_id,"name":info.name,"width":info.width,"height":info.height,
        "fps":info.fps,"codec":vnt_proto::Codec::from_proto(info.codec()).map(|c|c.as_str()).unwrap_or("h264"),
        "hardware":info.hardware,"available":info.available,"reason":info.reason,"generation":info.generation})
}

pub fn json(state: &pb::ScreensState) -> serde_json::Value {
    serde_json::json!({"type":"screensState","revision":state.revision,"activeStreamId":state.active_stream_id,
        "screens":state.screens.iter().map(info).collect::<Vec<_>>()})
}

pub fn input(state: &State, msg: Msg, requested: Option<(u32, u32, u64)>) -> Option<Msg> {
    let state = state.lock().unwrap();
    let target = state
        .screens
        .iter()
        .find(|s| s.stream_id == state.active_stream_id && s.available)?;
    let route = (target.stream_id, target.generation, state.revision);
    if requested.is_some_and(|requested| requested != route) {
        return None;
    }
    let Msg::ScreenPacket(mut packet) = screen::wrap(route.0, route.1, msg) else {
        unreachable!()
    };
    packet.revision = route.2;
    Some(Msg::ScreenPacket(packet))
}

pub fn cache_state(cache: &crate::bridge::StreamInfoCache, state: &pb::ScreensState) -> String {
    let mut cache = cache.lock().unwrap();
    let previous = cache
        .as_ref()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok());
    let mut value = json(state);
    if let Some(previous) = previous {
        value["streams"] = previous
            .get("streams")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        value["transportState"] = previous
            .get("transportState")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"control":true,"files":true,"media":true}));
    }
    if let Some(streams) = value["streams"].as_object_mut() {
        streams.retain(|id, _| state.screens.iter().any(|s| s.stream_id.to_string() == *id));
    }
    let json = value.to_string();
    *cache = Some(json.clone());
    json
}

pub fn cache_stream(cache: &crate::bridge::StreamInfoCache, id: u32, message: serde_json::Value) {
    let mut cache = cache.lock().unwrap();
    if let Some(mut value) = cache
        .as_ref()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
    {
        if !value["streams"].is_object() {
            value["streams"] = serde_json::json!({});
        }
        let key = if message["type"] == "streamInfo" {
            "config"
        } else {
            "desktop"
        };
        value["streams"][id.to_string()][key] = message;
        *cache = Some(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_rejects_stale_screen_generation_and_ownership_revision() {
        let state = Arc::new(Mutex::new(pb::ScreensState {
            active_stream_id: 2,
            revision: 7,
            screens: vec![pb::ScreenInfo {
                stream_id: 2,
                generation: 3,
                available: true,
                ..Default::default()
            }],
        }));
        let event = || {
            Msg::InputEvent(pb::InputEvent {
                ev: Some(pb::input_event::Ev::MouseMove(pb::MouseMove {
                    x: 0.5,
                    y: 0.5,
                })),
            })
        };
        assert!(input(&state, event(), Some((1, 3, 7))).is_none());
        assert!(input(&state, event(), Some((2, 2, 7))).is_none());
        assert!(input(&state, event(), Some((2, 3, 6))).is_none());
        let Some(Msg::ScreenPacket(packet)) = input(&state, event(), Some((2, 3, 7))) else {
            panic!("valid input rejected")
        };
        assert_eq!(
            (packet.stream_id, packet.generation, packet.revision),
            (2, 3, 7)
        );
    }
}
