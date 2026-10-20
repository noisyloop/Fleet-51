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
        .route("/api/bots/{id}/run-code", post(bot_run_code))
        .route("/api/bots/{id}/delete", post(bot_delete))
        .route("/api/bots/{id}/undo", post(bot_undo))
        .route(
            "/api/bots/{id}/files",
            post(add_file).layer(axum::extract::DefaultBodyLimit::max(
                crate::files::MAX_BYTES + 1024,
            )),
        )
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
        .layer(middleware::from_fn_with_state(d.clone(), guard))
        .with_state(d)
}

/// For every request: refuse other host names while listening on this computer only (so a web
/// page can't reach the service through DNS rebinding), and tell browsers to run only Fleet-51's
/// own scripts and never show it inside another site's frame.
async fn guard(State(d): D, req: Request, next: Next) -> Response {
    let local_only = d
        .url
        .strip_prefix("http://")
        .and_then(|h| h.rsplit_once(':'))
        .is_some_and(|(ip, _)| ip == "127.0.0.1" || ip == "[::1]" || ip == "localhost");
    if local_only {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let name = host.rsplit_once(':').map(|(n, _)| n).unwrap_or(host);
        if !matches!(name, "127.0.0.1" | "localhost" | "[::1]") {
            return (StatusCode::FORBIDDEN, "unexpected host").into_response();
        }
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
             form-action 'none'; frame-ancestors 'none'",
        ),
    );
    h.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    res
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
        json!({"runtime": r, "online": d.online.load(std::sync::atomic::Ordering::SeqCst), "sandbox": crate::sandbox::available().is_some(), "run_code": crate::sandbox::for_code().is_some(), "default_model": crate::runtime::DEFAULT_MODEL, "memory_gb": crate::runtime::memory_gb(), "shapes": BOT_SHAPES, "colors": BOT_COLORS}),
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
        "model": st.model, "think": st.think, "run_code": st.run_code, "folder": st.cwd, "status": st.status, "preview": st.last_text,
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
    /// Names of files already added to the bot's folder (see add_file).
    #[serde(default)]
    attachments: Vec<String>,
}

async fn send(State(d): D, Path(id): Path<String>, Json(t): Json<Text>) -> R<Value> {
    let s = d.bot(&id)?;
    if t.text.trim().is_empty() && t.attachments.is_empty() {
        return Err(anyhow::anyhow!("empty message").into());
    }
    let folder = std::path::PathBuf::from(s.state().cwd);
    let mut attachments = Vec::new();
    for a in t.attachments.iter().take(20) {
        let name = crate::files::clean_name(a);
        if name != *a || !folder.join(&name).is_file() {
            return Err(anyhow::anyhow!("{a} is not in {}'s folder", s.state().title).into());
        }
        attachments.push(name);
    }
    let text = if t.text.trim().is_empty() {
        "Here are some files.".to_string()
    } else {
        t.text.clone()
    };
    Ok(Json(json!({"id": d.prompt(&s, &text, None, &attachments)})))
}

#[derive(Deserialize)]
struct FileName {
    name: String,
}

/// Add a file to a bot's folder (the request body is the file). Documents get a text copy.
async fn add_file(
    State(d): D,
    Path(id): Path<String>,
    Query(q): Query<FileName>,
    body: axum::body::Bytes,
) -> R<Value> {
    let s = d.bot(&id)?;
    let folder = std::path::PathBuf::from(s.state().cwd);
    let exe = d.exe.clone();
    let added =
        tokio::task::spawn_blocking(move || crate::files::add(&folder, &exe, &q.name, &body))
            .await??;
    Ok(Json(json!(added)))
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

#[derive(Deserialize)]
struct RunCode {
    run_code: bool,
}

async fn bot_run_code(State(d): D, Path(id): Path<String>, Json(r): Json<RunCode>) -> R<Value> {
    let s = d.bot(&id)?;
    let d2 = d.clone();
    let on = r.run_code;
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || d2.set_run_code(&s2, on)).await??;
    Ok(Json(bot_json(&s)))
}

async fn bot_delete(State(d): D, Path(id): Path<String>) -> R<Value> {
    let s = d.bot(&id)?;
    d.delete_bot(&s);
    Ok(Json(json!({"deleted": s.id})))
}

/// Show the bot's folder in Finder (or the Linux file manager).
#[derive(Deserialize, Default)]
struct Reveal {
    /// A file in the bot's folder to point at; the folder itself if left out.
    path: Option<String>,
}

/// Show the bot's folder, or one of its files, in Finder (or the Linux file manager). Files
/// are only ever shown, never opened.
async fn bot_reveal(State(d): D, Path(id): Path<String>, body: Option<Json<Reveal>>) -> R<Value> {
    let s = d.bot(&id)?;
    let folder = std::path::PathBuf::from(s.state().cwd).canonicalize()?;
    let target = match body.and_then(|b| b.0.path) {
        Some(p) => {
            let t = folder.join(p.trim_start_matches('/')).canonicalize()?;
            if !t.starts_with(&folder) {
                return Err(anyhow::anyhow!("that file is outside the bot's folder").into());
            }
            t
        }
        None => folder,
    };
    if cfg!(target_os = "macos") {
        // Always through Finder: `open <folder>` would launch a folder that looks like an app
        // (a bot could build "x.app" in its folder).
        let mut c = std::process::Command::new("/usr/bin/open");
        if target.is_file() {
            c.arg("-R");
        } else {
            c.args(["-a", "Finder"]);
        }
        c.arg(&target).spawn()?;
    } else if cfg!(windows) {
        // Explorer shows the folder, or selects the file; it never opens the file itself.
        let mut c = std::process::Command::new("explorer.exe");
        if target.is_file() {
            c.arg(format!("/select,{}", target.display()));
        } else {
            c.arg(&target);
        }
        c.spawn()?;
    } else {
        let dir = if target.is_file() {
            target.parent().map(|p| p.to_path_buf()).unwrap_or(target)
        } else {
            target
        };
        std::process::Command::new("xdg-open").arg(dir).spawn()?;
    }
    Ok(Json(json!({"ok": true})))
}

async fn models(State(d): D) -> Json<Value> {
    let suggested: Vec<Value> = crate::models::SUGGESTED
        .iter()
        .map(|(name, size, tools, note)| json!({"name": name, "size": size, "bytes": crate::models::size_bytes(size), "tools": tools, "note": note}))
        .collect();
    let pulls: Vec<_> = d.pulls.lock().unwrap().values().cloned().collect();
    let found =
        tokio::task::spawn_blocking(|| (crate::models::installed(), crate::models::free_bytes()))
            .await;
    let (installed, error, free) = match found {
        Ok((Ok(m), free)) => (m, None, free),
        Ok((Err(e), free)) => (vec![], Some(e.to_string()), free),
        Err(e) => (vec![], Some(e.to_string()), None),
    };
    Json(
        json!({"installed": installed, "suggested": suggested, "pulls": pulls, "error": error, "free_bytes": free, "keep_free_bytes": crate::models::KEEP_FREE}),
    )
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
