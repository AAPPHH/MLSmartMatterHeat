use crate::http::AppState;
use crate::storage::now_ms;
use axum::http::StatusCode;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Boost { room: String, minutes: Option<f64> },
    WindowOpen { room: String },
    WindowClose { room: String },
}

pub struct Reply {
    pub status: StatusCode,
    pub body: Value,
}

pub fn apply(state: &AppState, intent: Intent) -> Reply {
    match intent {
        Intent::Boost { room, minutes } => {
            let body = state.boost.request(&room, minutes, now_ms());
            let status = if body["ok"].as_bool() == Some(true) {
                StatusCode::OK
            } else if state.config.rooms.iter().any(|r| r.name == room) {
                StatusCode::CONFLICT
            } else {
                StatusCode::NOT_FOUND
            };
            Reply { status, body }
        }
        Intent::WindowOpen { room } => window(state, &room, true),
        Intent::WindowClose { room } => window(state, &room, false),
    }
}

fn window(state: &AppState, room: &str, open: bool) -> Reply {
    let event = state.windows.lock().unwrap().manual(room, open, now_ms());
    event.into_iter().for_each(|e| crate::rules::window_event(state, e));
    let open = state.windows.lock().unwrap().open_rooms();
    Reply { status: StatusCode::OK, body: json!({"window_open": open.iter().collect::<Vec<_>>()}) }
}
