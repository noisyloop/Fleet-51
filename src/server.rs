//! HTTP API and live event stream behind the app window (and any browser or phone pointed at
//! it). Every /api route needs the daemon's token.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::automations::Draft;
use crate::daemon::{Daemon, BOT_COLORS, BOT_SHAPES};
use crate::session::Session;

type D = State<Arc<Daemon>>;

pub struct ApiErr(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiErr {
    fn from(e: E) -> Self {
        ApiErr(e.into())
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": self.0.to_string()})),
        )
            .into_response()
    }
}

type R<T> = Result<Json<T>, ApiErr>;

macro_rules! asset {
    ($ct:expr, $file:expr) => {
        get(|| async {
            (
                [
                    (header::CONTENT_TYPE, $ct),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                include_str!($file),
            )
                .into_response()
        })
    };
}

pub fn router(d: Arc<Daemon>) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/runtime", get(runtime))
        .route("/api/runtime/retry", post(runtime_retry))
        .route("/api/models", get(models))
        .route("/api/models/pull", post(pull_model))
        .route("/api/models/delete", post(delete_model))
        .route("/api/bots", get(list_bots).post(create_bot))
        .route("/api/bots/{id}/messages", get(history).post(send))
        .route("/api/bots/{id}/model", post(bot_model))
        .route("/api/bots/{id}/think", post(bot_think))
        .route("/api/bots/{id}/delete", post(bot_delete))
        .route("/api/bots/{id}/undo", post(bot_undo))
        .route("/api/bots/{id}/clear", post(bot_clear))
        .route("/api/automations/{id}/history/delete", post(delete_runs))
        .route("/api/bots/{id}/reveal", post(bot_reveal))
        .route("/api/automations", get(automations).post(create_automation))
        .route("/api/automations/{id}", post(edit_automation))
        .route("/api/automations/{id}/run", post(run_automation))
        .route("/api/automations/{id}/delete", post(delete_automation))
        .route("/api/stream", get(stream))
        .route("/api/shutdown", post(shutdown))
        .layer(middleware::from_fn_with_state(d.clone(), auth));
    Router::new()
        .route("/", asset!("text/html; charset=utf-8", "../ui/chat.html"))
        .route("/chat.js", asset!("text/javascript", "../ui/chat.js"))
        .route("/chat.css", asset!("text/css", "../ui/chat.css"))
        .route("/icon.svg", asset!("image/svg+xml", "../ui/icon.svg"))
        .route(
            "/manifest.webmanifest",
            asset!("application/manifest+json", "../ui/manifest.webmanifest"),
        )
        // Older versions installed a caching service worker; this one removes itself.
        .route("/sw.js", asset!("text/javascript", "../ui/sw.js"))
        .merge(api)
        .with_state(d)
}

async fn auth(State(d): D, req: Request, next: Next) -> Response {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let query = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
        .map(str::to_string);
    if bearer.or(query).as_deref() == Some(d.token.as_str()) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "missing or wrong token"})),
        )
            .into_response()
    }
}

async fn health(State(d): D) -> Json<Value> {
    Json(
        json!({"name": "Fleet-51", "version": env!("CARGO_PKG_VERSION"), "build": crate::daemon::build_id(),
        "generation": d.generation, "pid": std::process::id(), "url": d.url}),
    )
}

async fn runtime(State(d): D) -> Json<Value> {
    let r = d.runtime.lock().unwrap().clone();
    Json(
        json!({"runtime": r, "online": d.online.load(std::sync::atomic::Ordering::SeqCst), "default_model": crate::runtime::DEFAULT_MODEL, "shapes": BOT_SHAPES, "colors": BOT_COLORS}),
    )
}

async fn runtime_retry(State(d): D) -> Json<Value> {
    d.clone().start_runtime();
    Json(json!({"ok": true}))
}

fn bot_json(s: &Session) -> Value {
    let st = s.state();
    let p = st.profile.clone();
    json!({
        "id": st.id, "name": p.as_ref().map(|p| p.name.clone()).unwrap_or(st.title.clone()),
        "shape": p.as_ref().map(|p| p.shape.clone()), "color": p.as_ref().map(|p| p.color.clone()),
        "model": st.model, "think": st.think, "folder": st.cwd, "status": st.status, "preview": st.last_text,
        "pending": st.pending.len(), "updated": st.updated, "created": st.created,
    })
}

async fn list_bots(State(d): D) -> Json<Value> {
    Json(json!(d
        .bots()
        .iter()
        .map(|s| bot_json(s))
        .collect::<Vec<_>>()))
}

#[derive(Deserialize)]
struct NewBot {
    name: String,
    #[serde(default)]
    shape: String,
    #[serde(default)]
    color: String,
}

async fn create_bot(State(d): D, Json(b): Json<NewBot>) -> R<Value> {
    let d2 = d.clone();
    let s =
        tokio::task::spawn_blocking(move || d2.create_bot(&b.name, &b.shape, &b.color)).await??;
    Ok(Json(bot_json(&s)))
}

#[derive(Deserialize)]
struct Since {
    since: Option<u64>,
}

async fn history(State(d): D, Path(id): Path<String>, Query(q): Query<Since>) -> R<Value> {
    Ok(Json(json!(d.bot(&id)?.history(q.since.unwrap_or(0)))))
}

#[derive(Deserialize)]
struct Text {
    text: String,
}

async fn send(State(d): D, Path(id): Path<String>, Json(t): Json<Text>) -> R<Value> {
    let s = d.bot(&id)?;
    if t.text.trim().is_empty() {
        return Err(anyhow::anyhow!("empty message").into());
    }
    Ok(Json(json!({"id": d.prompt(&s, &t.text, None)})))
}

#[derive(Deserialize)]
struct Model {
    model: String,
}

async fn bot_model(State(d): D, Path(id): Path<String>, Json(m): Json<Model>) -> R<Value> {
    let s = d.bot(&id)?;
    d.set_model(&s, m.model.trim())?;
    Ok(Json(bot_json(&s)))
}

#[derive(Deserialize)]
struct Think {
    think: bool,
}

async fn bot_think(State(d): D, Path(id): Path<String>, Json(t): Json<Think>) -> R<Value> {
    let s = d.bot(&id)?;
    d.set_think(&s, t.think);
    Ok(Json(bot_json(&s)))
}

#[derive(Deserialize)]
struct Undo {
    checkpoint: String,
}

async fn bot_undo(State(d): D, Path(id): Path<String>, Json(u): Json<Undo>) -> R<Value> {
    let s = d.bot(&id)?;
    let d2 = d.clone();
    tokio::task::spawn_blocking(move || d2.undo(&s, &u.checkpoint)).await??;
    Ok(Json(json!({"ok": true})))
}

async fn bot_clear(State(d): D, Path(id): Path<String>) -> R<Value> {
    let s = d.bot(&id)?;
    d.clear_chat(&s);
    Ok(Json(bot_json(&s)))
}

async fn bot_delete(State(d): D, Path(id): Path<String>) -> R<Value> {
    let s = d.bot(&id)?;
    d.delete_bot(&s);
    Ok(Json(json!({"deleted": s.id})))
}

/// Show the bot's folder in Finder (or the Linux file manager).
async fn bot_reveal(State(d): D, Path(id): Path<String>) -> R<Value> {
    let s = d.bot(&id)?;
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(s.state().cwd)
        .spawn()?;
    Ok(Json(json!({"ok": true})))
}

async fn models(State(d): D) -> Json<Value> {
    let suggested: Vec<Value> = crate::models::SUGGESTED
        .iter()
        .map(|(name, size, tools, note)| json!({"name": name, "size": size, "tools": tools, "note": note}))
        .collect();
    let pulls: Vec<_> = d.pulls.lock().unwrap().values().cloned().collect();
    let (installed, error) = match tokio::task::spawn_blocking(crate::models::installed).await {
        Ok(Ok(m)) => (m, None),
        Ok(Err(e)) => (vec![], Some(e.to_string())),
        Err(e) => (vec![], Some(e.to_string())),
    };
    Json(json!({"installed": installed, "suggested": suggested, "pulls": pulls, "error": error}))
}

#[derive(Deserialize)]
struct ModelName {
    name: String,
}

async fn pull_model(State(d): D, Json(m): Json<ModelName>) -> R<Value> {
    d.pull_model(m.name.trim())?;
    Ok(Json(json!({"pulling": m.name.trim()})))
}

async fn delete_model(Json(m): Json<ModelName>) -> R<Value> {
    if !crate::models::valid_name(&m.name) {
        return Err(anyhow::anyhow!("not a valid model name").into());
    }
    let name = m.name.clone();
    tokio::task::spawn_blocking(move || crate::models::delete(&name)).await??;
    Ok(Json(json!({"deleted": m.name})))
}

async fn automations(State(d): D) -> Json<Value> {
    Json(json!(d.automations.list()))
}

async fn create_automation(State(d): D, Json(a): Json<Draft>) -> R<Value> {
    let d2 = d.clone();
    let a = tokio::task::spawn_blocking(move || crate::automations::create(&d2, a)).await??;
    Ok(Json(json!(a)))
}

async fn edit_automation(State(d): D, Path(id): Path<String>, Json(a): Json<Draft>) -> R<Value> {
    let d2 = d.clone();
    let a = tokio::task::spawn_blocking(move || crate::automations::edit(&d2, &id, a)).await??;
    Ok(Json(json!(a)))
}

async fn run_automation(State(d): D, Path(id): Path<String>) -> R<Value> {
    let d2 = d.clone();
    tokio::task::spawn_blocking(move || crate::automations::run(&d2, &id)).await??;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct DeleteRuns {
    /// One run's id; leave out to clear the whole history.
    run: Option<String>,
}

async fn delete_runs(State(d): D, Path(id): Path<String>, Json(r): Json<DeleteRuns>) -> R<Value> {
    crate::automations::delete_runs(&d, &id, r.run.as_deref())?;
    Ok(Json(json!({"ok": true})))
}

async fn delete_automation(State(d): D, Path(id): Path<String>) -> R<Value> {
    crate::automations::delete(&d, &id)?;
    Ok(Json(json!({"ok": true})))
}

async fn stream(State(d): D) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let s = BroadcastStream::new(d.global.subscribe())
        .filter_map(|r| r.ok())
        .map(|v| Ok(SseEvent::default().data(serde_json::to_string(&v).unwrap_or_default())));
    Sse::new(s).keep_alive(KeepAlive::default())
}

async fn shutdown(State(d): D) -> Json<Value> {
    let d2 = d.clone();
    tokio::spawn(async move {
        d2.suspend_all().await;
        d2.stop.notify_one();
    });
    Json(json!({"ok": true, "generation": d.generation}))
}
