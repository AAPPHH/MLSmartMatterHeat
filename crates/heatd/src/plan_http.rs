use crate::assistant::{Failure, MAX_TEXT};
use crate::http::AppState;
use crate::plan::{self, NewPlan, Plan, DEFAULT_PREVIEW_HOURS, MAX_PREVIEW_HOURS};
use crate::storage::now_ms;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

type Body<T> = Result<Json<T>, axum::extract::rejection::JsonRejection>;

#[derive(Deserialize)]
struct AskBody {
    text: String,
}

#[derive(Deserialize)]
struct ConfirmBody {
    id: String,
    accept: bool,
}

#[derive(Deserialize)]
struct PreviewBody {
    #[serde(default)]
    plans: Vec<NewPlan>,
    #[serde(default)]
    hours: Option<u32>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/plans", get(list).post(create))
        .route("/api/plans/preview", post(preview))
        .route("/api/plans/{id}", delete(remove))
        .route("/api/assistant", post(ask))
        .route("/api/assistant/confirm", post(confirm))
}

fn fail(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({"error": error}))).into_response()
}

fn bad_json() -> Response {
    fail(StatusCode::BAD_REQUEST, "Ungueltige Anfrage")
}

async fn settle(state: &AppState) {
    let sink = state.sink.clone();
    if let Ok(Err(e)) = tokio::task::spawn_blocking(move || sink.flush().map(|_| ())).await {
        eprintln!("plan flush: {e:#}");
    }
    state.boost.waker().notify_one();
}

fn local() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

async fn list(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"plans": state.plans.list()}))
}

async fn create(State(state): State<AppState>, body: Body<NewPlan>) -> Response {
    let Ok(Json(new)) = body else { return bad_json() };
    let result = plan::create(&state, &new, local(), now_ms());
    settle(&state).await;
    match result {
        Ok((plan, summary)) => Json(json!({"plan": plan, "summary": summary})).into_response(),
        Err(e) => fail(StatusCode::BAD_REQUEST, &e),
    }
}

async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let result = plan::delete(&state, &id);
    settle(&state).await;
    match result {
        Ok(Some(plan)) => Json(json!({"ok": true, "plan": plan})).into_response(),
        Ok(None) => fail(StatusCode::NOT_FOUND, "Plan unbekannt"),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

async fn preview(State(state): State<AppState>, body: Body<PreviewBody>) -> Response {
    let Ok(Json(body)) = body else { return bad_json() };
    let hours = body.hours.unwrap_or(DEFAULT_PREVIEW_HOURS);
    if !(1..=MAX_PREVIEW_HOURS).contains(&hours) {
        return fail(StatusCode::BAD_REQUEST, &format!("hours muss zwischen 1 und {MAX_PREVIEW_HOURS} liegen"));
    }
    let (now, now_ms) = (local(), now_ms());
    let mut plans = state.plans.list();
    for (i, new) in body.plans.iter().enumerate() {
        match plan::validate(&state.config, new, now) {
            Ok(v) => plans.push(Plan { id: "preview".into(), rooms: v.rooms, action: v.action, when: v.when, label: v.label, source: v.source, created_ms: now_ms + 1 + i as i64 }),
            Err(e) => return fail(StatusCode::BAD_REQUEST, &e),
        }
    }
    Json(plan::preview(&state.config, &plans, now, hours)).into_response()
}

fn failure(error: Failure) -> Response {
    match error {
        Failure::Disabled => fail(StatusCode::SERVICE_UNAVAILABLE, "assistant disabled"),
        Failure::Timeout => fail(StatusCode::GATEWAY_TIMEOUT, "assistant timeout"),
        Failure::Model(m) => {
            eprintln!("assistant model error: {m}");
            fail(StatusCode::BAD_GATEWAY, "assistant model error")
        }
        Failure::Unknown => {
            fail(StatusCode::NOT_FOUND, "unbekannt oder abgelaufen")
        }
    }
}

async fn ask(State(state): State<AppState>, body: Body<AskBody>) -> Response {
    let Ok(Json(body)) = body else { return bad_json() };
    let text = body.text.trim().to_string();
    if text.is_empty() || text.chars().count() > MAX_TEXT {
        return fail(StatusCode::BAD_REQUEST, &format!("text muss 1 bis {MAX_TEXT} Zeichen lang sein"));
    }
    if !state.assistant.enabled() {
        return failure(Failure::Disabled);
    }
    let (worker, assistant) = (state.clone(), state.assistant.clone());
    let result = tokio::task::spawn_blocking(move || assistant.ask(&worker, &text, local(), now_ms())).await;
    settle(&state).await;
    match result {
        Ok(Ok(body)) => Json(body).into_response(),
        Ok(Err(e)) => failure(e),
        Err(e) => failure(Failure::Model(e.to_string())),
    }
}

async fn confirm(State(state): State<AppState>, body: Body<ConfirmBody>) -> Response {
    let Ok(Json(body)) = body else { return bad_json() };
    let (worker, assistant) = (state.clone(), state.assistant.clone());
    let result = tokio::task::spawn_blocking(move || assistant.confirm(&worker, &body.id, body.accept, local(), now_ms())).await;
    settle(&state).await;
    match result {
        Ok(Ok(body)) => Json(body).into_response(),
        Ok(Err(e)) => failure(e),
        Err(e) => failure(Failure::Model(e.to_string())),
    }
}
