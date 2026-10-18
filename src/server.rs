//! HTTP API + server-sent events. The CLI, the web UI and phones all use this same API, so
//! remote (headless) and local are one code path. Every /api route needs the token.

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

use crate::accounts;
use crate::adapters::Cmd;
use crate::daemon::{Daemon, Fanout, NewSession};
use crate::events::{Ev, State as SessionState};
use crate::home::short_id;
use crate::review;
use crate::workspace::{self, RepoSpec};

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

pub fn router(d: Arc<Daemon>) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/agents", get(agents))
        .route("/api/models", get(models))
        .route(
            "/api/workspaces",
            get(list_workspaces).post(create_workspace),
        )
        .route(
            "/api/workspaces/{id}",
            axum::routing::delete(delete_workspace),
        )
        .route("/api/workspaces/{id}/diff", get(workspace_diff))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/fanout", post(fanout))
        .route("/api/sessions/{id}", get(get_session))
        .route("/api/sessions/{id}/events", get(session_events))
        .route("/api/sessions/{id}/history", get(session_history))
        .route("/api/sessions/{id}/prompt", post(prompt))
        .route("/api/sessions/{id}/input", post(input))
        .route("/api/sessions/{id}/stop", post(stop))
        .route("/api/sessions/{id}/resume", post(resume))
        .route("/api/sessions/{id}/diff", get(session_diff))
        .route("/api/sessions/{id}/annotations", post(annotate))
        .route("/api/sessions/{id}/feedback", post(feedback))
        .route("/api/sessions/{id}/merge", post(merge))
        .route("/api/groups/{id}", get(compare))
        .route("/api/usage", get(usage))
        .route("/api/accounts", get(list_accounts).post(add_account))
        .route("/api/hook", post(hook))
        .route("/api/shutdown", post(shutdown))
        .route("/api/stream", get(stream))
        .layer(middleware::from_fn_with_state(d.clone(), auth));
    Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", include_str!("../ui/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript", include_str!("../ui/app.js")) }),
        )
        .route(
            "/style.css",
            get(|| async { asset("text/css", include_str!("../ui/style.css")) }),
        )
        .route(
            "/sw.js",
            get(|| async { asset("text/javascript", include_str!("../ui/sw.js")) }),
        )
        .route(
            "/manifest.webmanifest",
            get(|| async {
                asset(
                    "application/manifest+json",
                    include_str!("../ui/manifest.webmanifest"),
                )
            }),
        )
        .route(
            "/icon.svg",
            get(|| async { asset("image/svg+xml", include_str!("../ui/icon.svg")) }),
        )
        .merge(api)
        .with_state(d)
}

fn asset(ct: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
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
        json!({"name": "Fleet-51", "version": env!("CARGO_PKG_VERSION"), "generation": d.generation, "pid": std::process::id(), "url": d.url}),
    )
}

async fn agents(State(d): D) -> Json<Value> {
    let list: Vec<Value> = d
        .manifests()
        .into_iter()
        .map(|m| {
            json!({
                "name": m.name, "display": m.display, "protocol": m.protocol.name(), "available": m.available(),
                "source": m.source, "account_provider": m.provider(), "permissions": m.permission_args.keys().collect::<Vec<_>>(),
                "install_hint": m.install_hint,
            })
        })
        .collect();
    Json(json!(list))
}

async fn models() -> Json<Value> {
    match tokio::task::spawn_blocking(crate::agent::ollama_models).await {
        Ok(Ok(m)) => Json(json!({"ollama": m})),
        Ok(Err(e)) => Json(json!({"ollama": [], "error": e.to_string()})),
        Err(e) => Json(json!({"ollama": [], "error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct NewWorkspace {
    name: String,
    repos: Vec<RepoSpec>,
}

async fn list_workspaces(State(d): D) -> Json<Value> {
    Json(json!(workspace::list(&d.home)))
}

async fn create_workspace(State(d): D, Json(req): Json<NewWorkspace>) -> R<Value> {
    Ok(Json(json!(workspace::create(
        &d.home, &req.name, &req.repos
    )?)))
}

async fn delete_workspace(State(d): D, Path(id): Path<String>) -> R<Value> {
    let ws = workspace::get(&d.home, &id)?;
    if d.all_sessions()
        .iter()
        .any(|s| s.state().workspace == ws.id && s.is_running())
    {
        return Err(anyhow::anyhow!("a running session uses this workspace; stop it first").into());
    }
    workspace::remove(&d.home, &ws.id)?;
    Ok(Json(json!({"removed": ws.id})))
}

async fn workspace_diff(State(d): D, Path(id): Path<String>) -> R<Value> {
    let ws = workspace::get(&d.home, &id)?;
    Ok(Json(json!(review::diff(&ws))))
}

async fn list_sessions(State(d): D) -> Json<Vec<SessionState>> {
    Json(d.all_sessions().iter().map(|s| s.state()).collect())
}

async fn create_session(State(d): D, Json(req): Json<NewSession>) -> R<SessionState> {
    let d2 = d.clone();
    let s = tokio::task::spawn_blocking(move || d2.create_session(req)).await??;
    Ok(Json(s.state()))
}

async fn fanout(State(d): D, Json(req): Json<Fanout>) -> R<Value> {
    let d2 = d.clone();
    let (group, sessions) = tokio::task::spawn_blocking(move || d2.fanout(req)).await??;
    Ok(Json(
        json!({"group": group, "sessions": sessions.iter().map(|s| s.state()).collect::<Vec<_>>()}),
    ))
}

async fn get_session(State(d): D, Path(id): Path<String>) -> R<SessionState> {
    Ok(Json(d.session(&id)?.state()))
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<u64>,
    follow: Option<bool>,
}

async fn session_events(
    State(d): D,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiErr> {
    let s = d.session(&id)?;
    let rx = s.tx.subscribe();
    let history = s.history(q.since.unwrap_or(0));
    let last = history
        .last()
        .map(|e| e.seq)
        .unwrap_or(q.since.unwrap_or(0));
    let follow = q.follow.unwrap_or(true);
    let live = BroadcastStream::new(rx)
        .filter_map(move |r| r.ok().filter(|e| e.seq > last))
        .take(if follow { usize::MAX } else { 0 });
    let stream = tokio_stream::iter(history)
        .chain(live)
        .map(|e| Ok(SseEvent::default().data(serde_json::to_string(&e).unwrap_or_default())));
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn session_history(
    State(d): D,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> R<Vec<crate::events::Event>> {
    Ok(Json(d.session(&id)?.history(q.since.unwrap_or(0))))
}

async fn stream(State(d): D) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = d.global.subscribe();
    let s = BroadcastStream::new(rx)
        .filter_map(|r| r.ok())
        .map(|st| Ok(SseEvent::default().data(serde_json::to_string(&st).unwrap_or_default())));
    Sse::new(s).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct Text {
    text: String,
}

async fn prompt(State(d): D, Path(id): Path<String>, Json(t): Json<Text>) -> R<Value> {
    let s = d.session(&id)?;
    let pid = d.prompt(&s, &t.text);
    Ok(Json(json!({"prompt": pid})))
}

#[derive(Deserialize)]
struct Input {
    data: String,
}

async fn input(State(d): D, Path(id): Path<String>, Json(i): Json<Input>) -> R<Value> {
    let s = d.session(&id)?;
    Ok(Json(json!({"sent": s.send(Cmd::Input(i.data))})))
}

async fn stop(State(d): D, Path(id): Path<String>) -> R<SessionState> {
    let s = d.session(&id)?;
    d.stop(&s);
    Ok(Json(s.state()))
}

async fn resume(State(d): D, Path(id): Path<String>) -> R<SessionState> {
    let s = d.session(&id)?;
    d.resume(&s)?;
    Ok(Json(s.state()))
}

async fn session_diff(State(d): D, Path(id): Path<String>) -> R<Value> {
    let s = d.session(&id)?;
    let ws = d.workspace_of(&s)?;
    let diff = tokio::task::spawn_blocking(move || review::diff(&ws)).await?;
    Ok(Json(
        json!({"workspace": s.state().workspace, "checkouts": diff, "annotations": s.state().annotations}),
    ))
}

#[derive(Deserialize)]
struct NewAnnotation {
    file: String,
    line: u32,
    comment: String,
}

async fn annotate(State(d): D, Path(id): Path<String>, Json(a): Json<NewAnnotation>) -> R<Value> {
    let s = d.session(&id)?;
    let aid = short_id("a");
    s.emit(Ev::Annotation {
        id: aid.clone(),
        file: a.file,
        line: a.line,
        comment: a.comment,
    });
    Ok(Json(json!({"annotation": aid})))
}

async fn feedback(State(d): D, Path(id): Path<String>) -> R<Value> {
    let s = d.session(&id)?;
    let pid = d.feedback(&s)?;
    Ok(Json(json!({"prompt": pid})))
}

async fn merge(State(d): D, Path(id): Path<String>) -> R<Value> {
    let s = d.session(&id)?;
    let d2 = d.clone();
    let r = tokio::task::spawn_blocking(move || d2.merge(&s)).await??;
    Ok(Json(json!(r)))
}

async fn compare(State(d): D, Path(id): Path<String>) -> R<Value> {
    let members: Vec<_> = d
        .all_sessions()
        .into_iter()
        .filter(|s| s.state().group.as_deref() == Some(id.as_str()))
        .collect();
    if members.is_empty() {
        return Err(anyhow::anyhow!("no group {id}").into());
    }
    let home = d.home.clone();
    let rows = tokio::task::spawn_blocking(move || {
        members
            .iter()
            .map(|s| {
                let st = s.state();
                let (files, added, deleted) = workspace::get(&home, &st.workspace)
                    .map(|ws| {
                        let diffs = review::diff(&ws);
                        let files: Vec<_> = diffs.iter().flat_map(|c| c.files.iter()).collect();
                        (
                            files.len(),
                            files.iter().map(|f| f.added).sum::<u64>(),
                            files.iter().map(|f| f.deleted).sum::<u64>(),
                        )
                    })
                    .unwrap_or((0, 0, 0));
                json!({"session": st, "files_changed": files, "added": added, "deleted": deleted})
            })
            .collect::<Vec<_>>()
    })
    .await?;
    Ok(Json(json!({"group": id, "attempts": rows})))
}

async fn usage(State(d): D) -> Json<Value> {
    let mut by_agent = serde_json::Map::new();
    let mut by_account = serde_json::Map::new();
    let mut total =
        json!({"sessions": 0, "turns": 0, "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0});
    fn add(v: &mut Value, st: &SessionState) {
        v["sessions"] = json!(v["sessions"].as_u64().unwrap_or(0) + 1);
        v["turns"] = json!(v["turns"].as_u64().unwrap_or(0) + st.turns as u64);
        v["input_tokens"] = json!(v["input_tokens"].as_u64().unwrap_or(0) + st.input_tokens);
        v["output_tokens"] = json!(v["output_tokens"].as_u64().unwrap_or(0) + st.output_tokens);
        v["cost_usd"] = json!(v["cost_usd"].as_f64().unwrap_or(0.0) + st.cost_usd);
    }
    for s in d.all_sessions() {
        let st = s.state();
        add(&mut total, &st);
        add(by_agent.entry(st.agent.clone()).or_insert(json!({})), &st);
        let acct = st.account.clone().unwrap_or_else(|| "default".into());
        add(by_account.entry(acct).or_insert(json!({})), &st);
    }
    Json(json!({"total": total, "by_agent": by_agent, "by_account": by_account}))
}

async fn list_accounts(State(d): D) -> Json<Value> {
    Json(json!(accounts::list(&d.home)))
}

#[derive(Deserialize)]
struct NewAccount {
    provider: String,
    name: String,
}

async fn add_account(State(d): D, Json(a): Json<NewAccount>) -> R<Value> {
    let dir = accounts::add(&d.home, &a.provider, &a.name)?;
    Ok(Json(
        json!({"provider": a.provider, "name": a.name, "dir": dir}),
    ))
}

#[derive(Deserialize)]
struct HookCall {
    session: String,
    name: String,
    #[serde(default)]
    payload: Value,
}

async fn hook(State(d): D, Json(h): Json<HookCall>) -> R<Value> {
    let s = d.session(&h.session)?;
    s.emit(Ev::Hook {
        name: h.name,
        payload: h.payload,
    });
    Ok(Json(json!({"ok": true})))
}

async fn shutdown(State(d): D) -> Json<Value> {
    let d2 = d.clone();
    tokio::spawn(async move {
        d2.suspend_all().await;
        d2.stop.notify_one();
    });
    Json(json!({"ok": true, "generation": d.generation}))
}
