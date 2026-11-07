//! HTTP routes for the system-builder API.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use gb_core::{Manager, ManagerError};
use gb_types::{
    ApiError, BuildLog, BuildSummary, CreateBuildRequest, EstimateRequest, EstimateResponse,
    HistoryResponse, JobState, QueueStatus, StreamEvent, SystemActiveBuild, SystemInfo,
};
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub manager: Manager,
}

pub fn router(manager: Manager) -> Router {
    let state = AppState { manager };
    Router::new()
        .route("/health", get(health))
        .route("/v1/builds", post(create_build).get(list_builds))
        .route("/v1/builds/active", get(active_builds))
        .route("/v1/builds/{id}", get(get_build).delete(cancel_build))
        .route("/v1/builds/{id}/log", get(get_log))
        .route("/v1/builds/{id}/stream", get(stream_build))
        .route("/v1/queue", get(get_queue))
        .route("/v1/systems", get(list_systems))
        .route("/v1/systems/{system}/active", get(system_active))
        .route("/v1/estimates", post(estimate))
        .route("/v1/history", get(history))
        .with_state(state)
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { error: msg.into() }))
}

/// Parse a `{id}` path segment. Done manually (not `Path<Uuid>`) so a
/// malformed id is a JSON 400 instead of axum's plain-text rejection.
fn path_id(raw: &str) -> Result<Uuid, (StatusCode, Json<ApiError>)> {
    Uuid::parse_str(raw)
        .map_err(|_| err(StatusCode::BAD_REQUEST, format!("invalid build id '{raw}'")))
}

fn manager_err(e: ManagerError) -> (StatusCode, Json<ApiError>) {
    match e {
        ManagerError::UnknownSystem(s) => {
            err(StatusCode::NOT_FOUND, format!("unknown system '{s}'"))
        }
        ManagerError::EmptyTargets => err(StatusCode::BAD_REQUEST, "no targets to build"),
        ManagerError::UnknownJob(id) => err(StatusCode::NOT_FOUND, format!("unknown build '{id}'")),
        ManagerError::AlreadyFinished => err(StatusCode::CONFLICT, "build is already finished"),
        ManagerError::Internal(msg) => err(StatusCode::INTERNAL_SERVER_ERROR, msg),
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}

async fn create_build(
    State(state): State<AppState>,
    Json(req): Json<CreateBuildRequest>,
) -> impl IntoResponse {
    match state.manager.submit(req).await {
        Ok(job) => (
            StatusCode::CREATED,
            Json(serde_json::to_value(&job).unwrap()),
        )
            .into_response(),
        Err(e) => manager_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    state: Option<JobState>,
    system: Option<String>,
}

async fn list_builds(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Json<Vec<BuildSummary>> {
    Json(state.manager.list(q.state, q.system.as_deref()).await)
}

async fn active_builds(State(state): State<AppState>) -> Json<Vec<BuildSummary>> {
    let q = state.manager.queue_status().await;
    Json(q.active)
}

// Axum 0.8: `delete` above is the routing helper; this handler needs a
// different name to avoid clashing.
async fn cancel_build(State(state): State<AppState>, Path(raw): Path<String>) -> impl IntoResponse {
    let id = match path_id(&raw) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    match state.manager.cancel(id).await {
        Ok(job) => (StatusCode::OK, Json(serde_json::to_value(&job).unwrap())).into_response(),
        Err(e) => manager_err(e).into_response(),
    }
}

async fn get_build(State(state): State<AppState>, Path(raw): Path<String>) -> impl IntoResponse {
    let id = match path_id(&raw) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    match state.manager.get(id).await {
        Some(job) => (StatusCode::OK, Json(serde_json::to_value(&job).unwrap())).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("unknown build '{id}'")).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    tail: Option<usize>,
}

async fn get_log(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    Query(q): Query<LogQuery>,
) -> impl IntoResponse {
    let id = match path_id(&raw) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    match state.manager.log(id, q.tail).await {
        Some(log @ BuildLog { .. }) => {
            (StatusCode::OK, Json(serde_json::to_value(&log).unwrap())).into_response()
        }
        None => err(StatusCode::NOT_FOUND, format!("unknown build '{id}'")).into_response(),
    }
}

async fn get_queue(State(state): State<AppState>) -> Json<QueueStatus> {
    Json(state.manager.queue_status().await)
}

async fn list_systems(State(state): State<AppState>) -> Json<Vec<SystemInfo>> {
    Json(state.manager.systems().await)
}

async fn system_active(
    State(state): State<AppState>,
    Path(system): Path<String>,
) -> impl IntoResponse {
    match state.manager.system_active(&system).await {
        Ok(active @ SystemActiveBuild { .. }) => {
            (StatusCode::OK, Json(serde_json::to_value(&active).unwrap())).into_response()
        }
        Err(e) => manager_err(e).into_response(),
    }
}

async fn estimate(
    State(state): State<AppState>,
    Json(req): Json<EstimateRequest>,
) -> impl IntoResponse {
    match state.manager.estimate(req).await {
        Ok(resp @ EstimateResponse { .. }) => {
            (StatusCode::OK, Json(serde_json::to_value(&resp).unwrap())).into_response()
        }
        Err(e) => manager_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    system: String,
    package: String,
}

async fn history(
    State(state): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> impl IntoResponse {
    match state.manager.history(&q.system, &q.package).await {
        Ok(resp @ HistoryResponse { .. }) => {
            (StatusCode::OK, Json(serde_json::to_value(&resp).unwrap())).into_response()
        }
        Err(e) => manager_err(e).into_response(),
    }
}

/// `GET /v1/builds/:id/stream` — server-sent events.
///
/// Replays the buffered log first, then forwards live events until the build
/// reaches a terminal state.
async fn stream_build(State(state): State<AppState>, Path(raw): Path<String>) -> impl IntoResponse {
    let id = match path_id(&raw) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let Some((snapshot, backlog, mut rx)) = state.manager.subscribe(id).await else {
        return err(StatusCode::NOT_FOUND, format!("unknown build '{id}'")).into_response();
    };
    let (tx, rx_out) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(256);
    tokio::spawn(async move {
        let send = |tx: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>, ev: &StreamEvent| {
            let data = serde_json::to_string(ev).unwrap_or_default();
            let name = match ev {
                StreamEvent::Status { .. } => "status",
                StreamEvent::Log { .. } => "log",
                StreamEvent::Progress { .. } => "progress",
                StreamEvent::Done { .. } => "done",
            };
            tx.try_send(Ok(Event::default().event(name).data(data)))
                .is_ok()
        };
        // Replay.
        let status = StreamEvent::Status {
            state: snapshot.state,
        };
        if !send(&tx, &status) {
            return;
        }
        for line in &backlog {
            if !send(&tx, &StreamEvent::Log { line: line.clone() }) {
                return;
            }
        }
        if snapshot.progress.current_package.is_some() || snapshot.progress.total_packages > 0 {
            let _ = send(
                &tx,
                &StreamEvent::Progress {
                    progress: snapshot.progress.clone(),
                },
            );
        }
        if snapshot.state.is_terminal() {
            let _ = send(
                &tx,
                &StreamEvent::Done {
                    state: snapshot.state,
                    exit_code: snapshot.exit_code,
                },
            );
            return;
        }
        // Live tail.
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let done = matches!(ev, StreamEvent::Done { .. });
                    if !send(&tx, &ev) {
                        return;
                    }
                    if done {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    let _ = send(
                        &tx,
                        &StreamEvent::Log {
                            line: format!("... skipped {skipped} events (slow consumer) ..."),
                        },
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    let stream = ReceiverStream::new(rx_out);
    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}
