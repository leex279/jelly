//! The local daemon: an HTTP API over the store, a live event stream, and the
//! views. The jelly page people look at is itself an internal jelly named `surface`.

use crate::store::{AttentionKind, Error, Event, EventKind, InboxBatch, InboxCommit, InboxItem, InboxRequest, Store, PAGE};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::body::Bytes;
use axum::http::{header, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

const RUNTIME_JS: &str = include_str!("../assets/runtime.js");
const SURFACE_VIEW: &str = include_str!("../assets/surface.html");
const SURFACE_VERBS: &str = include_str!("../assets/surface.verbs.js");
const SNAPSHOT_JS: &str = include_str!("../assets/snapshot.js");
const ASK_VIEW: &str = include_str!("../assets/ask.html");
const ASK_VERBS: &str = include_str!("../assets/ask.verbs.js");
const ARCHIVE_AFTER_MS: u64 = 7 * 24 * 3600 * 1000;

struct App {
    store: Mutex<Store>,
    events: broadcast::Sender<Event>,
    /// What some browser last showed of each jelly, as text. Memory only.
    screens: Mutex<std::collections::HashMap<String, ViewScreen>>,
}
type Shared = Arc<App>;

impl App {
    /// Broadcasts committed batch events even when a later change fails.
    fn change_many(&self, f: impl FnOnce(&mut Store) -> Result<Vec<Event>, crate::store::BatchError>) -> Result<Json<Value>, ApiError> {
        let mut store = self.store.lock().expect("store lock");
        let result = f(&mut store);
        let (evs, error) = match result {
            Ok(events) => (events, None),
            Err(failed) => (failed.events, Some(failed.error)),
        };
        self.invalidate_screens(&store, &evs);
        for ev in &evs {
            let _ = self.events.send(ev.clone());
        }
        if let Some(error) = error {
            if let Some(last) = evs.last() {
                return Err(Error::Invalid(format!("{error}; committed {} changes to {} through seq:{}. Inspect with `jelly show {}` before retrying", evs.len(), last.jelly, last.seq, last.jelly)).into());
            }
            return Err(error.into());
        }
        Ok(Json(json!({"jelly": evs.last().map(|e| e.jelly.clone()), "events": evs.len(), "seq": evs.last().map(|e| e.seq), "state": evs.last().and_then(|e| e.state.clone())})))
    }

    /// Runs one store change and broadcasts its event. The store lock is what
    /// makes verbs run one at a time.
    fn change(&self, f: impl FnOnce(&mut Store) -> Result<Event, Error>) -> Result<Json<Event>, ApiError> {
        let mut store = self.store.lock().expect("store lock");
        let ev = f(&mut store)?;
        self.invalidate_screens(&store, std::slice::from_ref(&ev));
        let _ = self.events.send(ev.clone());
        Ok(Json(ev))
    }

    // All combined access takes the store lock before the screens lock, so a
    // late post cannot refill a cache entry invalidated by a lifecycle change.
    fn invalidate_screens(&self, store: &Store, events: &[Event]) {
        let mut screens = self.screens.lock().expect("screens lock");
        for ev in events {
            if ev.kind == EventKind::Move {
                if let Some(from) = ev.args.as_ref().and_then(|a| a["from"].as_str()) {
                    screens.remove(from);
                }
            }
            if matches!(ev.kind, EventKind::New | EventKind::Remove | EventKind::Move)
                || screens.get(&ev.jelly).is_some_and(|s| store.get(&ev.jelly).map_or(true, |j| j.view_rev != s.view_rev)) {
                screens.remove(&ev.jelly);
            }
        }
    }

    fn snapshot_owner(&self, name: &str, view_rev: u64) -> Result<String, ApiError> {
        use base64::Engine as _;
        let mut screens = self.screens.lock().expect("screens lock");
        if let Some(entry) = screens.get(name).filter(|s| s.view_rev == view_rev) {
            return Ok(entry.owner.clone());
        }
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(|e| ApiError::Internal(format!("could not issue snapshot owner: {e}")))?;
        let owner = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        screens.insert(name.into(), ViewScreen { owner: owner.clone(), view_rev, screen: None });
        Ok(owner)
    }

    fn screen(&self, name: &str, owner: &str) -> Result<Option<Screen>, ApiError> {
        let store = self.store.lock().expect("store lock");
        let j = store.get(name)?;
        let screens = self.screens.lock().expect("screens lock");
        let entry = screens.get(name).filter(|s| s.owner == owner && s.view_rev == j.view_rev)
            .ok_or_else(snapshot_changed)?;
        Ok(entry.screen.clone())
    }

    fn validate_see(&self, name: &str, owner: &str, version: u64) -> Result<(), ApiError> {
        let store = self.store.lock().expect("store lock");
        let j = store.get(name)?;
        let screens = self.screens.lock().expect("screens lock");
        if j.version != version || !screens.get(name).is_some_and(|s| s.owner == owner && s.view_rev == j.view_rev) {
            return Err(snapshot_changed());
        }
        Ok(())
    }
}

pub async fn serve(store: Store, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    let mut store = store;
    if store.get(PAGE).is_err() {
        store
            .create(PAGE, SURFACE_VIEW.into(), SURFACE_VERBS.into(), json!({}), "jelly", false, "The jelly page", "")
            .and_then(|_| store.pin(PAGE, true, "jelly"))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }
    let (tx, _) = broadcast::channel(1024);
    let app: Shared = Arc::new(App { store: Mutex::new(store), events: tx, screens: Mutex::default() });

    // Unpinned jellies nobody touched for a week are archived: hidden from the
    // jelly page, restorable, and back on their own as soon as anything changes them.
    let sweeper = app.clone();
    tokio::spawn(async move {
        loop {
            let swept = sweeper.store.lock().expect("store lock").sweep(ARCHIVE_AFTER_MS);
            for ev in swept {
                let _ = sweeper.events.send(ev);
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });

    let port = listener.local_addr()?.port();
    eprintln!("jelly on http://127.0.0.1:{port}");
    axum::serve(listener, router(app)).await
}

fn router(app: Shared) -> Router {
    Router::new()
        // The jelly page: every project at /, one project at /p/<project>.
        .route("/", get(|st: State<Shared>| async move { view(st, Path(PAGE.into())).await }))
        .route("/runtime.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], RUNTIME_JS) }))
        // Views live at /j/<name>/ so a view can use relative `assets/<file>` paths.
        .route("/j/{name}", get(|Path(name): Path<String>| async move { Redirect::permanent(&format!("/j/{name}/")) }))
        .route("/j/{name}/", get(view))
        .route("/j/{name}/assets/{file}", get(asset))
        // A project's page is the same page, showing only that project's jellies.
        .route("/p/{project}", get(|st: State<Shared>| async move { view(st, Path(PAGE.into())).await }))
        .route("/api/events", get(events))
        .route("/api/wait", get(wait))
        .route("/api/ws", get(ws))
        .route("/api/jellies", get(list).post(create))
        .route("/api/jellies/{name}", get(show).delete(remove))
        .route("/api/jellies/{name}/state", get(state))
        .route("/api/jellies/{name}/call", post(call))
        .route("/api/jellies/{name}/reshape", post(reshape))
        .route("/api/jellies/{name}/set", post(set_state))
        .route("/api/jellies/{name}/undo", post(undo))
        .route("/api/jellies/{name}/pin", post(pin))
        .route("/api/jellies/{name}/move", post(move_project))
        .route("/api/jellies/{name}/restore", post(restore))
        .route("/api/jellies/{name}/archive", post(archive))
        .route("/api/jellies/{name}/assets", get(list_assets))
        .route("/api/jellies/{name}/assets/{file}", post(put_asset).layer(DefaultBodyLimit::max(25 * 1024 * 1024)))
        .route("/api/jellies/{name}/share", get(share))
        .route("/api/inbox", get(inbox).post(read_inbox))
        .route("/api/inbox/commit", post(commit_inbox))
        .route("/api/jellies/{name}/screen", post(put_screen))
        .route("/api/jellies/{name}/note", post(note))
        .route("/api/jellies/{name}/see", get(see))
        .route("/see-frame/{name}", get(see_frame))
        .route("/api/projects/{project}/design", get(get_design).put(put_design))
        .route("/p/{project}/DESIGN.md", get(get_design))
        .route("/p/{project}/design.css", get(design_css))
        .route("/api/attention", get(attention_list))
        .route("/api/focus", post(focus))
        .route("/api/jellies/{name}/attention", post(attend))
        .route("/api/jellies/{name}/ask", post(ask))
        .route("/api/jellies/{name}/attention/{id}/answer", post(answer))
        .route("/api/jellies/{name}/attention/{id}/withdraw", post(withdraw))
        .route("/api/jellies/{name}/working", post(working))
        .route("/api/jellies/{name}/comment", post(comment))
        .route("/api/jellies/{name}/resolve", post(resolve))
        .route("/api/jellies/{name}/reply", post(reply))
        .with_state(app)
}

#[derive(Debug)]
enum ApiError {
    Store(Error),
    Conflict(String),
    Internal(String),
}

fn snapshot_changed() -> ApiError {
    ApiError::Conflict("the jelly changed; reload the page or retry see".into())
}
impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError::Store(e)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Store(e) => {
                let status = match e {
                    Error::NotFound(_) => StatusCode::NOT_FOUND,
                    Error::Exists(_) => StatusCode::CONFLICT,
                    Error::Invalid(_) => StatusCode::BAD_REQUEST,
                };
                (status, e.to_string())
            }
            Self::Conflict(message) => (StatusCode::CONFLICT, message),
            Self::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

/// Every change says who made it. Nothing defaults to the person, so a script
/// that forgets cannot answer on their behalf.
fn by(by: Option<String>) -> Result<String, Error> {
    by.filter(|b| !b.trim().is_empty()).ok_or_else(|| Error::Invalid("say who is making this change: by".into()))
}

async fn list(State(app): State<Shared>) -> Json<Value> {
    let store = app.store.lock().expect("store lock");
    // The page that shows the jellies is internal; it is not one of yours.
    Json(json!(store
        .list()
        .filter(|j| j.name != PAGE)
        .map(|j| json!({
            "name": j.name, "version": j.version, "view_rev": j.view_rev,
            "pinned": j.pinned, "made_by": j.made_by, "updated_ms": j.updated_ms, "about": j.about, "project": j.project(), "archived": j.archived,
            "created_ms": if j.created_ms > 0 { j.created_ms } else { j.updated_ms },
            "open_comments": j.comments.iter().filter(|f| !f.resolved).count(),
            "replies_waiting": j.comments.iter().filter(|c| c.awaiting_person()).count(),
            "attention": j.attention.iter().filter(|a| a.is_open()).count(),
            "working": j.working,
            "poster": store.assets(&j.name).into_iter().find(|f| f.starts_with("poster.")),
        }))
        .collect::<Vec<_>>()))
}

async fn show(State(app): State<Shared>, Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(app.store.lock().expect("store lock").get(&name)?)))
}

#[derive(Deserialize)]
struct PointerQ {
    pointer: Option<String>,
}

async fn state(State(app): State<Shared>, Path(name): Path<String>, Query(q): Query<PointerQ>) -> Result<Json<Value>, ApiError> {
    let store = app.store.lock().expect("store lock");
    let s = &store.get(&name)?.state;
    match q.pointer.as_deref() {
        None | Some("") => Ok(Json(s.clone())),
        Some(p) => s.pointer(p).cloned().map(Json).ok_or_else(|| Error::Invalid(format!("{name} has nothing at {p}")).into()),
    }
}

#[derive(Deserialize)]
struct CreateReq {
    name: String,
    view: String,
    verbs: String,
    #[serde(default)]
    state: Value,
    by: Option<String>,
    #[serde(default)]
    replace: bool,
    #[serde(default)]
    about: String,
    #[serde(default)]
    project: String,
}

async fn create(State(app): State<Shared>, Json(r): Json<CreateReq>) -> Result<Json<Event>, ApiError> {
    let state = if r.state.is_null() { json!({}) } else { r.state };
    app.change(|s| s.create(&r.name, r.view, r.verbs, state, &by(r.by)?, r.replace, &r.about, &r.project))
}

#[derive(Deserialize)]
struct CallReq {
    verb: String,
    #[serde(default)]
    args: Value,
    by: Option<String>,
    /// The element the person used, sent by the runtime.
    #[serde(default)]
    source: Option<Value>,
}

async fn call(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<CallReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.call_from(&name, &r.verb, r.args, &by(r.by)?, r.source))
}

#[derive(Deserialize)]
struct SetReq {
    #[serde(default)]
    pointer: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    remove: bool,
    by: Option<String>,
}

async fn set_state(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<SetReq>) -> Result<Json<Event>, ApiError> {
    let value = match (r.remove, r.value) {
        (true, _) => None,
        (false, Some(v)) => Some(v),
        (false, None) => return Err(Error::Invalid("give a value, or remove: true".into()).into()),
    };
    app.change(|s| s.set_state(&name, &r.pointer, value, &by(r.by)?))
}

#[derive(Deserialize)]
struct ReshapeReq {
    view: Option<String>,
    verbs: Option<String>,
    about: Option<String>,
    by: Option<String>,
}

async fn reshape(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<ReshapeReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.reshape(&name, r.view, r.verbs, r.about, &by(r.by)?))
}

#[derive(Deserialize)]
struct ByReq {
    by: Option<String>,
    #[serde(default)]
    pinned: Option<bool>,
}

async fn undo(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<ByReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.undo(&name, &by(r.by)?))
}

async fn pin(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<ByReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.pin(&name, r.pinned.unwrap_or(true), &by(r.by)?))
}

#[derive(Deserialize)]
struct CommentReq {
    #[serde(default)]
    anchor: Value,
    text: String,
    by: Option<String>,
}

async fn comment(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<CommentReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.comment(&name, r.anchor, &r.text, &by(r.by)?))
}

#[derive(Deserialize)]
struct ResolveReq {
    id: String,
    by: Option<String>,
}

async fn resolve(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<ResolveReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.resolve(&name, &r.id, &by(r.by)?))
}

#[derive(Deserialize)]
struct ReplyReq {
    id: String,
    text: String,
    #[serde(default)]
    resolve: bool,
    by: Option<String>,
}

async fn reply(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<ReplyReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.reply(&name, &r.id, &r.text, &by(r.by)?, r.resolve))
}

#[derive(Deserialize)]
struct ProjectReq {
    project: String,
    by: Option<String>,
}

async fn move_project(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<ProjectReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.set_project(&name, &r.project, &by(r.by)?))
}

async fn remove(State(app): State<Shared>, Path(name): Path<String>, Query(q): Query<ByQ>) -> Result<Json<Event>, ApiError> {
    if name == PAGE {
        return Err(Error::Invalid("the jelly page cannot be removed; reshape or undo it instead".into()).into());
    }
    app.change(|s| s.remove(&name, &by(q.by)?))
}

#[derive(Deserialize)]
struct ByQ {
    by: Option<String>,
    jelly: Option<String>,
}

async fn events(State(app): State<Shared>, Query(q): Query<ByQ>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(app.events.subscribe()).filter_map(move |ev| {
        let ev = match ev {
            Ok(ev) => ev,
            // Comments wake state readers without becoming events for CLI consumers.
            Err(_) => return Some(Ok(SseEvent::default().comment("resync"))),
        };
        if q.jelly.as_ref().is_some_and(|j| *j != ev.jelly) {
            return None;
        }
        SseEvent::default().json_data(&ev).ok().map(Ok)
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize, Clone)]
struct WaitQ {
    since: u64,
    by: String,
    /// Resolve short names against the jellies present at the starting sequence.
    project_jelly: Option<String>,
    jelly: Option<String>,
    timeout: Option<u64>,
}

async fn wait(State(app): State<Shared>, Query(q): Query<WaitQ>) -> Result<Response, ApiError> {
    by(Some(q.by.clone()))?;
    match wait_since(&app, &q).await? {
        Some(ev) => Ok(Json(ev).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

fn history_error(e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("could not read event history: {e}"))
}

fn recorded_change(reader: &mut impl BufRead, q: &WaitQ) -> Result<Option<Event>, Error> {
    let mut line = String::new();
    while reader.read_line(&mut line).map_err(history_error)? != 0 {
        let ev: Event = serde_json::from_str(&line).map_err(history_error)?;
        if ev.seq > q.since && ev.by != q.by
            && q.jelly.as_ref().map_or(ev.jelly != PAGE, |j| *j == ev.jelly)
        {
            return Ok(Some(ev));
        }
        line.clear();
    }
    Ok(None)
}

fn open_history(store: &Store) -> Result<Option<BufReader<File>>, Error> {
    match File::open(store.events_path()) {
        Ok(file) => Ok(Some(BufReader::new(file))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && store.sequence() == 0 => Ok(None),
        Err(e) => Err(history_error(e)),
    }
}

async fn wait_since(app: &App, q: &WaitQ) -> Result<Option<Event>, Error> {
    wait_since_at_handoff(app, q, |_| {}).await
}

async fn wait_since_at_handoff(app: &App, q: &WaitQ, after_scan: impl FnOnce(&mut Store)) -> Result<Option<Event>, Error> {
    let mut q = q.clone();
    let rx = app.events.subscribe();
    let deadline = q.timeout.map(|t| Instant::now().checked_add(Duration::from_secs(t))
        .ok_or_else(|| Error::Invalid("timeout is too large".into()))).transpose()?;
    let mut reader = {
        let mut store = app.store.lock().expect("store lock");
        let mut reader = open_history(&store)?;
        if let (Some(preferred), Some(reader)) = (&q.project_jelly, &mut reader) {
            let (project, short) = preferred.split_once(':').unwrap_or(("", preferred));
            let mut identities = std::collections::BTreeSet::new();
            let mut line = String::new();
            while reader.read_line(&mut line).map_err(history_error)? != 0 {
                let ev: Event = serde_json::from_str(&line).map_err(history_error)?;
                if ev.seq > q.since { break; }
                if ev.kind == crate::store::EventKind::Move {
                    if let Some(from) = ev.args.as_ref().and_then(|args| args["from"].as_str()) {
                        identities.remove(from);
                    }
                }
                if ev.jelly.split_once(':').map_or(ev.jelly.as_str(), |(_, short)| short) == short {
                    if ev.kind == crate::store::EventKind::Remove {
                        identities.remove(&ev.jelly);
                    } else {
                        identities.insert(ev.jelly);
                    }
                }
                line.clear();
            }
            if !identities.is_empty() {
                q.jelly = Some(crate::store::resolve_name(short, project, identities.iter().map(String::as_str)));
            }
            std::io::Seek::rewind(reader).map_err(history_error)?;
        }
        if let Some(reader) = &mut reader {
            if let Some(ev) = recorded_change(reader, &q)? { return Ok(Some(ev)); }
        }
        if let Some(name) = &q.jelly { store.get(name)?; }
        after_scan(&mut store);
        reader
    };
    wait_for_record(app, &q, &mut reader, rx, deadline).await
}

async fn wait_for_record(
    app: &App,
    q: &WaitQ,
    reader: &mut Option<BufReader<File>>,
    mut rx: broadcast::Receiver<Event>,
    deadline: Option<Instant>,
) -> Result<Option<Event>, Error> {
    loop {
        let notification = if let Some(end) = deadline {
            if Instant::now() >= end { return Ok(None); }
            match tokio::time::timeout_at(end, rx.recv()).await {
                Ok(result) => result,
                Err(_) => return Ok(None),
            }
        } else {
            rx.recv().await
        };
        // Commits are ordered under the store lock; broadcasts happen after it
        // is released, so notifications only wake the reader, never choose a result.
        let store = app.store.lock().expect("store lock");
        if reader.is_none() { *reader = open_history(&store)?; }
        if let Some(reader) = reader {
            if let Some(ev) = recorded_change(reader, q)? { return Ok(Some(ev)); }
        }
        if matches!(notification, Err(broadcast::error::RecvError::Closed)) {
            return Err(Error::Invalid("the event stream ended".into()));
        }
    }
}

/// Browsers get events over one WebSocket per page. Server-sent events would
/// use an HTTP connection each, and a page with a few jellies in it would hit
/// the browser's six-connections-per-host limit and stall every request.
async fn ws(State(app): State<Shared>, up: WebSocketUpgrade) -> Response {
    let rx = app.events.subscribe();
    up.on_upgrade(move |socket| pump(socket, rx))
}

async fn pump(mut socket: WebSocket, mut rx: broadcast::Receiver<Event>) {
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let text = match ev {
                    Ok(ev) => serde_json::to_string(&ev).expect("an event serializes"),
                    // Missed events: tell the page to reload its state.
                    Err(broadcast::error::RecvError::Lagged(_)) => r#"{"kind":"resync"}"#.to_string(),
                    Err(_) => break,
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => if !matches!(msg, Some(Ok(_))) { break },
        }
    }
}

/// Serves a jelly's view with the runtime injected, so `window.jelly` exists
/// before any of the view's own scripts run.
async fn view(State(app): State<Shared>, Path(name): Path<String>) -> Result<Html<String>, ApiError> {
    let store = app.store.lock().expect("store lock");
    let j = store.get(&name)?;
    let owner = app.snapshot_owner(&name, j.view_rev)?;
    let boot = json!({"snapshotOwner": owner, "name": j.name, "about": j.about, "version": j.version, "viewRev": j.view_rev, "state": j.state, "comments": j.comments, "attention": j.attention, "working": j.working});
    // `</` cannot appear inside the script, or state text could close the tag.
    let boot = boot.to_string().replace("</", "<\\/");
    let inject = format!("<script>window.__JELLY__={boot};</script><script src=\"/runtime.js\"></script>");
    Ok(Html(inject_head(&j.view, &inject)))
}

/// Puts `inject` first thing in `<head>`, so it runs before the view's scripts.
fn inject_head(html: &str, inject: &str) -> String {
    let at = html.to_ascii_lowercase().find("<head>").map(|i| i + "<head>".len());
    match at {
        Some(i) => format!("{}{inject}{}", &html[..i], &html[i..]),
        None => format!("<!doctype html><meta charset=\"utf-8\">{inject}{html}"),
    }
}

async fn archive(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<ByReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.set_archived(&name, true, &by(r.by)?))
}

async fn restore(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<ByReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.set_archived(&name, false, &by(r.by)?))
}

#[derive(Deserialize)]
struct AssetQ {
    by: Option<String>,
}

async fn list_assets(State(app): State<Shared>, Path(name): Path<String>) -> Result<Json<Vec<String>>, ApiError> {
    let store = app.store.lock().expect("store lock");
    store.get(&name)?;
    Ok(Json(store.try_assets(&name).map_err(|e| ApiError::Internal(format!("could not list assets for {name}: {e}")))?))
}

async fn put_asset(State(app): State<Shared>, Path((name, file)): Path<(String, String)>, Query(q): Query<AssetQ>, body: Bytes) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.put_asset(&name, &file, &body, &by(q.by)?))
}

fn content_type(file: &str) -> &'static str {
    match file.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "glb" => "model/gltf-binary",
        "json" => "application/json",
        "csv" => "text/csv",
        "txt" | "md" => "text/plain; charset=utf-8",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "woff2" => "font/woff2",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

async fn asset(State(app): State<Shared>, Path((name, file)): Path<(String, String)>) -> Response {
    if file.starts_with('.') || file.contains('/') {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Only a known jelly id names a directory; an absolute or `..` name would escape `assets/`.
    let dir = {
        let store = app.store.lock().expect("store lock");
        match store.get(&name) {
            Ok(_) => store.asset_dir(&name),
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        }
    };
    match tokio::fs::read(dir.join(&file)).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type(&file))], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// One file you can send anyone: the view with its state baked in, assets
/// inlined, and a read-only `jelly` in place of the live runtime.
async fn share(State(app): State<Shared>, Path(name): Path<String>) -> Result<Html<String>, ApiError> {
    use base64::Engine as _;
    use lol_html::html_content::UserData as _;
    let (j, dir, files) = {
        let store = app.store.lock().expect("store lock");
        let j = store.get(&name)?.clone();
        (j, store.asset_dir(&name), store.try_assets(&name)
            .map_err(|e| ApiError::Internal(format!("could not share assets for {name}: {e}")))?)
    };
    let mut assets = Vec::new();
    for file in files {
        let bytes = std::fs::read(dir.join(&file))
            .map_err(|e| Error::Invalid(format!("could not share asset {file}: {e}")))?;
        let uri = format!("data:{};base64,{}", content_type(&file), base64::engine::general_purpose::STANDARD.encode(bytes));
        assets.push((file, uri));
    }
    let mut marker_bytes = [0; 16];
    getrandom::fill(&mut marker_bytes).map_err(|e| ApiError::Internal(format!("could not prepare shared roots: {e}")))?;
    let marker = format!("jelly-snapshot-{}", marker_bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
    let boot = json!({"name": j.name, "about": j.about, "state": j.state, "attention": j.attention, "assets": assets, "rootMarker": marker})
        .to_string().replace("</", "<\\/");
    let inject = format!("<script>window.__JELLY__={boot};</script><script>{SNAPSHOT_JS}</script>");
    let mut base_href = None;
    lol_html::rewrite_str(&j.view, lol_html::RewriteStrSettings {
        element_content_handlers: vec![
            lol_html::element!("template base", |element| { element.set_user_data(true); Ok(()) }),
            lol_html::element!("base[href]", |element| {
                if base_href.is_none() && element.namespace_uri() == "http://www.w3.org/1999/xhtml"
                    && element.user_data().downcast_ref::<bool>() != Some(&true) {
                    base_href = element.get_attribute("href");
                }
                Ok(())
            }),
        ],
        ..Default::default()
    }).map_err(|e| ApiError::Internal(format!("could not read shared view base: {e}")))?;
    let html = lol_html::rewrite_str(&j.view, lol_html::RewriteStrSettings {
        element_content_handlers: vec![
            lol_html::element!("template object", |element| {
                element.set_user_data(true);
                Ok(())
            }),
            lol_html::element!("object[data]", |element| {
                let value = element.get_attribute("data").expect("matched data attribute");
                if let Some(uri) = shared_static_asset(&value, &assets, base_href.as_deref()) {
                    // Only the browser knows whether a declarative template becomes a root.
                    // Its existing cleanup restores this attribute if the template stays inert.
                    if element.user_data().downcast_ref::<bool>() == Some(&true) {
                        element.set_attribute(&format!("data-{marker}-object"), &value)?;
                    }
                    // Objects can commit their missing-file fallback before view scripts run.
                    element.set_attribute("data", &uri)?;
                }
                Ok(())
            }),
            lol_html::element!("template", |element| {
                if element.get_attribute("shadowrootmode").is_some_and(|mode| mode.eq_ignore_ascii_case("open") || mode.eq_ignore_ascii_case("closed")) {
                    element.prepend(&format!("<{marker} hidden></{marker}>"), lol_html::html_content::ContentType::Html);
                }
                element.after(&format!("<script data-{marker}>window.__JELLY__.finishTemplate()</script>"), lol_html::html_content::ContentType::Html);
                Ok(())
            }),
        ],
        ..Default::default()
    }).map_err(|e| ApiError::Internal(format!("could not prepare shared view: {e}")))?;
    Ok(Html(inject_head(&html, &inject)))
}

fn shared_static_asset(value: &str, assets: &[(String, String)], base_href: Option<&str>) -> Option<String> {
    let mut embedded = None;
    // The output directory is unknown. Requiring the same asset relative to two
    // disjoint directories excludes absolute paths and traversal that escapes
    // the document directory then happens to return to a synthetic location.
    let locations = if cfg!(windows) {
        ["file:///C:/jelly-shared/", "file:///C:/elsewhere/output/"]
    } else {
        ["file:///jelly-shared/", "file:///elsewhere/output/"]
    };
    for location in locations {
        let document = url::Url::parse(location).expect("valid view URL");
        let base = base_href.and_then(|href| document.join(href).ok())
            .filter(|url| !matches!(url.scheme(), "data" | "javascript"))
            .unwrap_or_else(|| document.clone());
        let directory = document.join("assets/").ok()?.to_file_path().ok()?;
        let url = base.join(value).ok()?;
        let path = url.to_file_path().ok()?;
        let file = path.strip_prefix(directory).ok()?.to_str()?;
        let (_, uri) = assets.iter().find(|(name, _)| name == file)?;
        let uri = format!("{uri}{}", url.fragment().map(|hash| format!("#{hash}")).unwrap_or_default());
        if embedded.as_ref().is_some_and(|prior| prior != &uri) { return None; }
        embedded = Some(uri);
    }
    embedded
}

#[derive(Deserialize)]
struct InboxQ {
    project: Option<String>,
    since: Option<u64>,
}

/// What the person did since `since`, for jellies in `project`: the events an
/// agent should hear about at the start of its next turn, each with a line of text.
/// Timestamp queries use indexed byte ranges without assuming wall-clock order.
async fn inbox(State(app): State<Shared>, Query(q): Query<InboxQ>) -> Result<Json<Value>, ApiError> {
    let since = q.since.unwrap_or(0);
    let project = q.project.unwrap_or_default();
    let events = app.store.lock().expect("store lock").events_since(since)
        .map_err(|e| ApiError::Internal(format!("could not read the event log: {e}")))?;
    let items: Vec<_> = events.into_iter().filter(|e| e.is_inbox_item(&project)).map(inbox_item).collect();
    Ok(Json(json!(items)))
}


fn inbox_item(event: Event) -> Value {
    json!(InboxItem::from(event))
}

async fn read_inbox(State(app): State<Shared>, Json(req): Json<InboxRequest>) -> Result<Json<InboxBatch>, ApiError> {
    Ok(Json(app.store.lock().expect("store lock").read_inbox(&req.project, &req.reader)?))
}

async fn commit_inbox(State(app): State<Shared>, Json(req): Json<InboxCommit>) -> Result<Json<Value>, ApiError> {
    app.store.lock().expect("store lock").commit_inbox(req)?;
    Ok(Json(json!({"ok": true})))
}


#[derive(Deserialize)]
struct AttendReq {
    #[serde(flatten)]
    spec: Value,
}

async fn attend(State(app): State<Shared>, Path(name): Path<String>, headers: axum::http::HeaderMap, Json(r): Json<AttendReq>) -> Result<Json<Value>, ApiError> {
    let by = r.spec["by"].as_str().map(String::from);
    let asked = now_ms();
    let Json(ev) = app.change(|s| s.attend(&name, r.spec, &self::by(by)?))?;
    let mut out = json!(ev);
    // Tell the asker where the card really shows. The item is made either way,
    // and the event that listeners and the log get is unchanged.
    let args = ev.args.unwrap_or_default();
    if args["anchor"]["selector"].is_string() {
        match anchor_place(&app, port_of(&headers), &name, args["id"].as_str().unwrap_or(""), ev.version, asked).await {
            Ok(AnchorMeasurement { place, exposed, inline }) => {
                out["anchor"] = json!(place.status());
                if inline { out["anchor_inline"] = json!(true); }
                if exposed { out["anchor_exposed"] = json!(true); }
                if let AnchorPlace::Fallback(f) = place {
                    out["anchor_destination"] = json!(f.destination);
                    out["anchor_reason"] = json!(f.reason);
                }
            },
            Err(why) => out["anchor_unchecked"] = json!(why),
        }
    }
    Ok(Json(out))
}

/// How long an ask waits for a browser before it answers "not checked".
const ANCHOR_CHECK: std::time::Duration = std::time::Duration::from_secs(8);

/// Placement from a fresh browser snapshot; failure to render does not undo the ask.
async fn anchor_place(app: &App, port: u16, name: &str, item: &str, version: u64, asked: u64) -> Result<AnchorMeasurement, String> {
    let owner = {
        let store = app.store.lock().expect("store lock");
        let j = store.get(name).map_err(|e| e.to_string())?;
        app.snapshot_owner(name, j.view_rev).map_err(|_| "could not issue snapshot owner".to_string())?
    };
    let render = render(port, name, 1280, 900, None, ANCHOR_CHECK);
    tokio::pin!(render);
    // The browser often posts its snapshot and then lingers, so answer as soon
    // as the snapshot is in; dropping the render stops the browser.
    let mut rendered: Option<(Result<(), String>, std::time::Instant)> = None;
    loop {
        let snap = app.screen(name, &owner).map_err(|_| "the jelly changed during the anchor check".to_string())?
            .filter(|s| s.version >= version && s.at_ms >= asked);
        if let Some(measured) = snap.and_then(|s| s.placement(item).map(|place| AnchorMeasurement { place, exposed: s.exposed.iter().any(|id| id == item), inline: s.inline })) {
            return Ok(measured);
        }
        match &rendered {
            // The browser is gone: give its snapshot a moment to land, then give up.
            Some((r, at)) if at.elapsed() > std::time::Duration::from_secs(2) => {
                return Err(r.clone().err().unwrap_or_else(|| "the page sent no placement measurement for this question".into()));
            }
            Some(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            None => tokio::select! {
                r = &mut render => rendered = Some((r, std::time::Instant::now())),
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            },
        }
    }
}

/// The port this request came in on, for rendering our own pages.
fn port_of(headers: &axum::http::HeaderMap) -> u16 {
    headers.get(header::HOST).and_then(|h| h.to_str().ok()).and_then(|h| h.rsplit(':').next()).and_then(|p| p.parse().ok()).unwrap_or(7878)
}

#[derive(Deserialize)]
struct AnswerReq {
    option: Option<String>,
    #[serde(default)]
    text: String,
    by: Option<String>,
}

async fn answer(State(app): State<Shared>, Path((name, id)): Path<(String, String)>, Json(r): Json<AnswerReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.answer(&name, &id, r.option.as_deref(), &r.text, &by(r.by)?))
}

async fn withdraw(State(app): State<Shared>, Path((name, id)): Path<(String, String)>, Json(r): Json<ByReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.withdraw(&name, &id, &by(r.by)?))
}

#[derive(Deserialize)]
struct WorkingReq {
    what: Option<String>,
    by: Option<String>,
}

async fn working(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<WorkingReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.set_working(&name, r.what.as_deref().filter(|w| !w.trim().is_empty()), &by(r.by)?))
}

#[derive(Deserialize)]
struct FocusReq {
    name: Option<String>,
    by: Option<String>,
}

/// Opens a jelly full size on the person's screen (or closes it with no name).
async fn focus(State(app): State<Shared>, Json(r): Json<FocusReq>) -> Result<Json<Event>, ApiError> {
    if let Some(n) = &r.name {
        app.store.lock().expect("store lock").get(n)?;
    }
    app.change(|s| s.call(PAGE, "focus", json!({"name": r.name}), &by(r.by)?))
}

#[derive(Deserialize)]
struct ProjectQ {
    project: Option<String>,
    /// List answered items instead of open ones: the decision record.
    #[serde(default)]
    answered: bool,
}

/// Everything open for the person, newest first; with `answered=true`, every
/// answered item instead: what was asked, what was picked, by whom, and when.
async fn attention_list(State(app): State<Shared>, Query(q): Query<ProjectQ>) -> Json<Value> {
    let store = app.store.lock().expect("store lock");
    let project = q.project.filter(|p| !p.is_empty());
    let mut items: Vec<Value> = Vec::new();
    for j in store.list().filter(|j| j.name != PAGE && project.as_deref().is_none_or(|p| j.project() == p)) {
        for a in j.attention.iter().filter(|a| if q.answered { a.answer.is_some() } else { a.is_open() }) {
            let mut v = serde_json::to_value(a).expect("serializes");
            v["jelly"] = json!(j.name);
            v["about"] = json!(j.about);
            items.push(v);
        }
    }
    let when = |i: &Value| i["answer"]["at_ms"].as_u64().or(i["at_ms"].as_u64()).unwrap_or(0);
    items.sort_by_key(|i| std::cmp::Reverse(when(i)));
    Json(json!(items))
}

// ---- the agent view -------------------------------------------------------

#[derive(serde::Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum AnchorDestination {
    Top,
    After { selector: String },
}

impl AnchorDestination {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Top => "at the top".into(),
            Self::After { selector } => format!("after {selector}"),
        }
    }
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
struct AnchorFallback {
    id: String,
    destination: AnchorDestination,
    reason: String,
}

#[derive(Debug)]
struct AnchorMeasurement {
    place: AnchorPlace,
    exposed: bool,
    inline: bool,
}

#[derive(Deserialize, Serialize, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AnchorStatus {
    Below,
    Invalid,
    Missing,
    Floating,
    Hidden,
    Fallback,
}

#[derive(Debug, PartialEq)]
enum AnchorPlace {
    Below,
    Invalid,
    Missing,
    Floating,
    Hidden,
    Fallback(AnchorFallback),
}

impl AnchorPlace {
    fn status(&self) -> AnchorStatus {
        match self {
            Self::Below => AnchorStatus::Below,
            Self::Invalid => AnchorStatus::Invalid,
            Self::Missing => AnchorStatus::Missing,
            Self::Floating => AnchorStatus::Floating,
            Self::Hidden => AnchorStatus::Hidden,
            Self::Fallback(_) => AnchorStatus::Fallback,
        }
    }
}

/// What a browser shows of a jelly, as text, posted by the runtime for `jelly see`.
#[derive(Deserialize, Clone)]
struct Screen {
    owner: String,
    text: String,
    width: u32,
    height: u32,
    version: u64,
    #[serde(default)]
    at_ms: u64,
    #[serde(default)]
    inline: bool,
    /// Errors the page threw or logged.
    #[serde(default)]
    errors: Vec<String>,
    #[serde(default)]
    invalid: Vec<String>,
    /// Open questions whose anchor matches nothing: "a2 #f2".
    #[serde(default)]
    missing: Vec<String>,
    /// Open questions whose anchor is in a fixed or sticky part of the page,
    /// so their card shows at the top too.
    #[serde(default)]
    floating: Vec<String>,
    /// Open questions whose anchor is on the page but not shown right now.
    #[serde(default)]
    hidden: Vec<String>,
    /// Cards measured at the top because adjacent placement was unavailable.
    #[serde(default)]
    exposed: Vec<String>,
    #[serde(default)]
    page_height: u32,
    #[serde(default)]
    fallback: Vec<AnchorFallback>,
    #[serde(default)]
    below: Vec<String>,
}

impl Screen {
    fn placement(&self, item: &str) -> Option<AnchorPlace> {
        let lists = |l: &[String]| l.iter().any(|m| m.split_once(' ').is_some_and(|(id, _)| id == item));
        Some(if lists(&self.invalid) { AnchorPlace::Invalid }
        else if lists(&self.missing) { AnchorPlace::Missing }
        else if lists(&self.floating) { AnchorPlace::Floating }
        else if lists(&self.hidden) { AnchorPlace::Hidden }
        else if let Some(f) = self.fallback.iter().find(|f| f.id == item) { AnchorPlace::Fallback(f.clone()) }
        else if lists(&self.below) { AnchorPlace::Below }
        else { return None })
    }
}

struct ViewScreen {
    owner: String,
    view_rev: u64,
    screen: Option<Screen>,
}

async fn put_screen(State(app): State<Shared>, Path(name): Path<String>, Json(mut sc): Json<Screen>) -> Result<StatusCode, ApiError> {
    let store = app.store.lock().expect("store lock");
    let j = store.get(&name)?;
    let mut screens = app.screens.lock().expect("screens lock");
    let entry = screens.get_mut(&name).filter(|s| s.owner == sc.owner && s.view_rev == j.view_rev)
        .ok_or_else(snapshot_changed)?;
    sc.at_ms = now_ms();
    entry.screen = Some(sc);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NoteReq {
    text: String,
    by: Option<String>,
}

async fn note(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<NoteReq>) -> Result<Json<Event>, ApiError> {
    app.change(|s| s.note(&name, &r.text, &by(r.by)?))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A browser the daemon can drive headless: JELLY_CHROME, or the usual installs.
fn chrome() -> Option<String> {
    let mut candidates = vec![
        std::env::var("JELLY_CHROME").unwrap_or_default(),
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
        "/Applications/Chromium.app/Contents/MacOS/Chromium".into(),
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into(),
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser".into(),
        "/usr/bin/google-chrome".into(),
        "/usr/bin/chromium".into(),
        "/usr/bin/chromium-browser".into(),
    ];
    // Windows installs go under one of the program folders, which the environment names.
    for (folder, install) in [
        ("ProgramFiles", r"Google\Chrome\Application\chrome.exe"),
        ("ProgramFiles(x86)", r"Google\Chrome\Application\chrome.exe"),
        ("LOCALAPPDATA", r"Google\Chrome\Application\chrome.exe"),
        ("LOCALAPPDATA", r"Chromium\Application\chrome.exe"),
        ("ProgramFiles(x86)", r"Microsoft\Edge\Application\msedge.exe"),
        ("ProgramFiles", r"BraveSoftware\Brave-Browser\Application\brave.exe"),
    ] {
        if let Some(root) = std::env::var_os(folder) {
            candidates.push(std::path::Path::new(&root).join(install).to_string_lossy().into_owned());
        }
    }
    candidates.into_iter().find(|c| !c.is_empty() && std::path::Path::new(c).exists())
}

/// How long `jelly see` waits for the browser.
const RENDER_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Runs headless Chrome on a jelly at a given size. With `png`, writes a
/// screenshot there; either way the page's runtime posts a fresh snapshot.
async fn render(port: u16, name: &str, width: u32, height: u32, png: Option<&std::path::Path>, limit: std::time::Duration) -> Result<(), String> {
    let bin = chrome().ok_or("no Chrome, Chromium, Edge or Brave found; set JELLY_CHROME to a browser binary")?;
    run_browser(&bin, port, name, width, height, png, limit).await
}

async fn run_browser(bin: &str, port: u16, name: &str, width: u32, height: u32, png: Option<&std::path::Path>, limit: std::time::Duration) -> Result<(), String> {
    // Headless Chrome will not make a window narrower than 500px, so phone
    // widths render the jelly in an iframe of exactly that size.
    let url = if width < 500 {
        format!("http://127.0.0.1:{port}/see-frame/{name}?w={width}&h={height}")
    } else {
        format!("http://127.0.0.1:{port}/j/{name}/?agent=1")
    };
    // The phone frame is scaled up to 500px wide, so the window grows with it.
    let height = if width < 500 { (height as f64 * 500.0 / width.max(1) as f64).ceil() as u32 } else { height };
    let width = width.max(500);
    let profile = std::env::temp_dir().join("jelly-chrome");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["--headless=new", "--disable-gpu", "--hide-scrollbars", "--no-first-run", "--no-default-browser-check", "--mute-audio"])
        .arg(format!("--user-data-dir={}", profile.display()));
    if cfg!(windows) {
        // On Windows `--window-size` is the outer size, so the page comes out narrower than
        // asked and its snapshot is refused; a fullscreen window on a screen of that size is exact.
        cmd.arg(format!("--screen-info={{{width}x{height}}}")).arg("--start-fullscreen");
    } else {
        cmd.arg(format!("--window-size={width},{height}"));
    }
    cmd.arg("--virtual-time-budget=2500")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    match png {
        Some(p) => { cmd.arg(format!("--screenshot={}", p.display())); }
        None => { cmd.arg("--dump-dom"); }
    }
    let run = cmd.arg(url).status();
    match tokio::time::timeout(limit, run).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("the browser failed ({status})")),
        Ok(Err(e)) => Err(format!("could not run the browser: {e}")),
        Err(_) => Err(format!("the browser took longer than {} s", limit.as_secs())),
    }
}

#[derive(Deserialize)]
struct SeeQ {
    width: Option<u32>,
    height: Option<u32>,
    #[serde(default)]
    png: bool,
    /// The whole page, not just the first screen.
    #[serde(default)]
    full: bool,
}

/// Everything an agent needs to pick up a jelly, as plain text: what it is,
/// what the person did, what waits on them, and what is on screen.
async fn see(State(app): State<Shared>, Path(name): Path<String>, Query(q): Query<SeeQ>, req_headers: axum::http::HeaderMap) -> Result<Response, ApiError> {
    let port = port_of(&req_headers);
    let render_name = name.clone();
    see_with_renderer(app, name, q, move |width, height, png: Option<std::path::PathBuf>| {
        let name = render_name.clone();
        async move { render(port, &name, width, height, png.as_deref(), RENDER_LIMIT).await }
    }).await
}

async fn see_with_renderer<F, Fut>(app: Shared, name: String, q: SeeQ, mut render_page: F) -> Result<Response, ApiError>
where
    F: FnMut(u32, u32, Option<std::path::PathBuf>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let (j, home, owner) = {
        let store = app.store.lock().expect("store lock");
        let j = store.get(&name)?;
        (j.clone(), store.home().to_path_buf(), app.snapshot_owner(&name, j.view_rev)?)
    };
    let width = q.width.unwrap_or(1280);
    let height = q.height.unwrap_or(if width <= 500 { 844 } else { 900 });
    let fresh = |app: &App| -> Result<Option<Screen>, ApiError> {
        Ok(app.screen(&name, &owner)?.filter(|s| s.version == j.version && s.width == width))
    };
    let mut notes: Vec<String> = Vec::new();
    let mut snap = fresh(&app)?;
    if snap.is_none() {
        let asked = now_ms();
        match render_page(width, height, None).await {
            Ok(()) => {
                for _ in 0..30 {
                    if let Some(s) = fresh(&app)?.filter(|s| s.at_ms >= asked) {
                        snap = Some(s);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if snap.is_none() { notes.push("The page rendered but sent no snapshot.".into()); }
            }
            Err(e) => notes.push(format!("No snapshot: {e}")),
        }
    }
    let png = if q.png {
        let dir = home.join("see");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}-{width}.png", name.replace(':', "_")));
        // The full page: as tall as the page said it was, within reason.
        let h = if q.full { snap.as_ref().map(|s| s.page_height).filter(|h| *h > height).unwrap_or(height).min(12000) } else { height };
        match render_page(width, h, Some(path.clone())).await {
            Ok(()) => Some(path),
            Err(e) => { notes.push(format!("No screenshot: {e}")); None }
        }
    } else { None };

    app.validate_see(&name, &owner, j.version)?;
    let mut out = String::new();
    let line = |out: &mut String, s: String| { out.push_str(&s); out.push('\n'); };
    let (last, mine) = app.store.lock().expect("store lock").recent_events(&name);
    // A trim can drop the newest event while an older one of the person's stays:
    // only the event that made this version says who made the last change.
    let by = last.filter(|e| e.version == j.version).map(|e| format!(" by {}", e.by)).unwrap_or_default();
    let last = format!(" · last change {}{by}", ago(j.updated_ms));
    line(&mut out, format!("{} · {} · v{} · made by {}{last}{}{}", j.name, if j.about.is_empty() { "(no about line)" } else { &j.about }, j.version, j.made_by,
        if j.pinned { " · pinned" } else { "" }, if j.archived { " · archived" } else { "" }));
    if let Some(w) = &j.working { line(&mut out, format!("working  {} is working on it: {}", w["by"].as_str().unwrap_or("?"), w["what"].as_str().unwrap_or(""))); }
    for n in &j.notes { line(&mut out, format!("note     {}: {}", n["by"].as_str().unwrap_or("?"), n["text"].as_str().unwrap_or(""))); }
    let verbs = crate::verbs::check(&j.verbs).unwrap_or_default();
    line(&mut out, format!("verbs    {}", if verbs.is_empty() { "none (read-only)".to_string() } else { verbs.iter().map(|v| verb_sig(&j.verbs, v)).collect::<Vec<_>>().join(" · ") }));
    line(&mut out, format!("state    {}", shape(&j.state)));
    let assets = app.store.lock().expect("store lock").assets(&name);
    if !assets.is_empty() { line(&mut out, format!("assets   {}", assets.join(", "))); }
    for a in j.attention.iter().filter(|a| a.is_open()) {
        line(&mut out, format!("waits    {}", a.summary()));
    }
    for a in j.attention.iter().filter(|a| a.answer.is_some()).rev().take(5) {
        let ans = a.answer.as_ref().expect("filtered");
        let pick = ans.option.as_deref().unwrap_or(if a.kind == AttentionKind::Look { "seen" } else { "" });
        line(&mut out, format!("answered {} {:?} → {pick}{}", a.id, a.title, if ans.text.is_empty() { String::new() } else { format!(" {:?}", ans.text) }));
    }
    for f in j.comments.iter().filter(|f| !f.resolved) {
        line(&mut out, format!("comment  {} on {}: {:?}{}", f.id, f.anchor["selector"].as_str().unwrap_or("the page"), f.text, if f.replies.is_empty() { "" } else { " (thread)" }));
    }
    for e in &mine {
        line(&mut out, format!("person   {} {}", ago(e.at_ms), e.describe()));
    }
    let project = j.project();
    if !project.is_empty() && app.store.lock().expect("store lock").project_dir(project).join("DESIGN.md").exists() {
        line(&mut out, format!("design   project {project} has a DESIGN.md: jelly design · CSS variables at /p/{project}/design.css"));
    }
    if let Some(p) = &png { line(&mut out, format!("png      {}", p.display())); }
    if let Some(sc) = &snap {
        for e in &sc.errors { line(&mut out, format!("error    {e}")); }
        let fallback = if sc.inline { "placement not checked: the view places its own attention" } else { "it shows at the top instead" };
        for m in &sc.invalid {
            let (id, sel) = m.split_once(' ').unwrap_or((m, ""));
            line(&mut out, format!("!        {id} asks about {sel}, which has invalid CSS selector syntax; {fallback}"));
        }
        for m in &sc.missing {
            let (id, sel) = m.split_once(' ').unwrap_or((m, ""));
            line(&mut out, format!("!        {id} asks about {sel}, which matches nothing on the page; {fallback}"));
        }
        for m in &sc.floating {
            let (id, sel) = m.split_once(' ').unwrap_or((m, ""));
            line(&mut out, format!("!        {id} asks about {sel}, which is in a fixed or sticky part of the page; {fallback}"));
        }
        for f in &sc.fallback {
            line(&mut out, format!("!        {} shows {} instead; {}", f.id, f.destination.describe(), f.reason));
        }
        if sc.page_height > sc.height && !q.full && png.is_some() { line(&mut out, format!("!        the page is {}px tall and the png shows the first {}px; --full for all of it", sc.page_height, sc.height)); }
    }
    for n in notes { line(&mut out, format!("!        {n}")); }
    if let Some(s) = snap {
        line(&mut out, format!("\non screen at {}×{} (v{}):", s.width, s.height, s.version));
        out.push_str(&s.text);
        out.push('\n');
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], out).into_response())
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
}

fn ago(ms: u64) -> String {
    let s = now_ms().saturating_sub(ms) / 1000;
    match s { 0..=59 => "just now".into(), 60..=3599 => format!("{}m ago", s / 60), 3600..=86399 => format!("{}h ago", s / 3600), _ => format!("{}d ago", s / 86400) }
}

/// `pick({id, value})`: the verb's name and its second parameter, which holds
/// the args (destructured keys, or the parameter's name).
fn verb_sig(src: &str, name: &str) -> String {
    let Some(at) = src.find(&format!("{name}(")) else { return name.into() };
    let rest = &src[at + name.len() + 1..];
    // Split the parameter list on top-level commas.
    let (mut depth, mut params, mut cur) = (0i32, Vec::new(), String::new());
    for c in rest.chars() {
        match c {
            '(' | '{' | '[' => depth += 1,
            ')' if depth == 0 => break,
            ')' | '}' | ']' => depth -= 1,
            ',' if depth == 0 => { params.push(std::mem::take(&mut cur)); continue; }
            _ => {}
        }
        cur.push(c);
    }
    params.push(cur);
    let Some(args) = params.get(1).map(|p| p.trim()) else { return name.into() };
    match args.strip_prefix('{').and_then(|a| a.rsplit_once('}')).map(|(k, _)| k) {
        Some(keys) => {
            let keys: Vec<&str> = keys.split(',').map(|x| x.split([':', '=']).next().unwrap_or("").trim()).filter(|x| !x.is_empty()).collect();
            format!("{name}({{{}}})", keys.join(", "))
        }
        None if !args.is_empty() => format!("{name}({args})"),
        None => name.into(),
    }
}

/// A one-line sketch of state: top-level keys with their kind and size.
fn shape(v: &Value) -> String {
    match v {
        Value::Object(m) if m.is_empty() => "{}".into(),
        Value::Object(m) => m.iter().take(12).map(|(k, v)| format!("{k}: {}", match v {
            Value::Array(a) => format!("[{}]", a.len()),
            Value::Object(o) => format!("{{{} keys}}", o.len()),
            Value::String(s) => format!("{:?}", clip(s, 30)),
            other => clip(&other.to_string(), 30),
        })).collect::<Vec<_>>().join(" · "),
        other => clip(&other.to_string(), 80),
    }
}

// ---- DESIGN.md per project --------------------------------------------------

async fn put_design(State(app): State<Shared>, Path(project): Path<String>, body: String) -> Result<StatusCode, ApiError> {
    let dir = app.store.lock().expect("store lock").project_dir(&project);
    std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join("DESIGN.md"), body)).map_err(|e| Error::Invalid(format!("could not save DESIGN.md: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_design(State(app): State<Shared>, Path(project): Path<String>) -> Response {
    let path = app.store.lock().expect("store lock").project_dir(&project).join("DESIGN.md");
    match std::fs::read_to_string(path) {
        Ok(t) => ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], t).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, format!("project {project} has no DESIGN.md")).into_response(),
    }
}

/// The DESIGN.md tokens as CSS variables: `--color-primary`, `--rounded-md`,
/// `--spacing-sm`, `--typography-h1-font-size`, …
async fn design_css(State(app): State<Shared>, Path(project): Path<String>) -> Response {
    let path = app.store.lock().expect("store lock").project_dir(&project).join("DESIGN.md");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let css = format!(":root {{\n{}}}\n", crate::design::css_vars(&text));
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], css).into_response()
}

#[derive(Deserialize)]
struct FrameQ {
    w: u32,
    h: u32,
}

async fn see_frame(Path(name): Path<String>, Query(q): Query<FrameQ>) -> Html<String> {
    // Scaled up to fill the 500px window, so the screenshot is the phone layout and nothing else.
    let k = 500.0 / q.w.max(1) as f64;
    Html(format!("<!doctype html><body style=\"margin:0;overflow:hidden\"><iframe src=\"/j/{name}/?agent=1\" style=\"display:block;border:0;width:{w}px;height:{h}px;background:#fff;transform:scale({k});transform-origin:0 0\"></iframe></body>", w = q.w, h = q.h))
}

#[derive(Deserialize)]
struct AskReq {
    spec: Value,
    #[serde(default)]
    project: String,
    by: Option<String>,
}

/// A round of questions on an ask page, made or updated in one step.
async fn ask(State(app): State<Shared>, Path(name): Path<String>, Json(r): Json<AskReq>) -> Result<Json<Value>, ApiError> {
    let who = by(r.by)?;
    app.change_many(|s| s.ask_round(&name, &r.spec, ASK_VIEW, ASK_VERBS, &r.project, &who))
}

#[cfg(test)]
mod share_tests {
    use super::*;
    use crate::store::PERSON;
    use base64::Engine as _;

    fn fixture(view: &str) -> (tempfile::TempDir, Shared) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.create("test:share", view.into(), "const verbs = {};".into(),
            json!({"picture": "assets/dot.png", "file": "dot.png", "textFile": "hello.txt", "label": "Label: assets/dot.png", "closing": "</script>"}),
            "agent", false, "Shared </script> jelly", "").unwrap();
        for (anchor, title) in [("#pictures", "open"), ("#pictures", "answered"), ("#other", "withdrawn"), ("#other", "other")] {
            store.attend("test:share", json!({"kind": "look", "title": title, "anchor": {"selector": anchor}}), "agent").unwrap();
        }
        store.answer("test:share", "a2", None, "captured answer", PERSON).unwrap();
        store.withdraw("test:share", "a3", "agent").unwrap();
        let (events, _) = broadcast::channel(16);
        (dir, Arc::new(App { store: Mutex::new(store), events, screens: Mutex::default() }))
    }

    async fn exported(app: Shared) -> String {
        share(State(app), Path("test:share".into())).await.unwrap_or_else(|e| panic!("{e:?}")).0
    }

    fn boot(html: &str) -> Value {
        let json = html.split_once("window.__JELLY__=").unwrap().1.split_once(";</script>").unwrap().0;
        serde_json::from_str(&json.replace("<\\/", "</")).unwrap()
    }

    #[tokio::test]
    async fn share_embeds_all_assets_and_preserves_captured_data() {
        let (_dir, app) = fixture("<head></head><img src='assets/dot.png.extra'><img src='assets/dot.png'>");
        let expected = {
            let mut store = app.store.lock().unwrap();
            for (file, bytes) in [("dot.png", &b"png"[..]), ("dot.png.extra", &b"long"[..]), ("unused.txt", &b"unused"[..])] {
                store.put_asset("test:share", file, bytes, "agent").unwrap();
            }
            serde_json::to_value(store.get("test:share").unwrap()).unwrap()
        };
        let html = exported(app).await;
        let boot = boot(&html);
        assert_eq!(boot["state"], expected["state"]);
        assert_eq!(boot["attention"], expected["attention"]);
        assert_eq!(boot["about"], expected["about"]);
        assert!(!html.contains("Shared </script>"));
        let assets: Vec<(String, String)> = serde_json::from_value(boot["assets"].clone()).unwrap();
        let assets: std::collections::HashMap<_, _> = assets.into_iter().collect();
        assert_eq!(assets.len(), 3);
        assert_eq!(assets["dot.png"], "data:image/png;base64,cG5n");
        assert_eq!(assets["dot.png.extra"], "data:application/octet-stream;base64,bG9uZw==");
        assert_eq!(assets["unused.txt"], "data:text/plain; charset=utf-8;base64,dW51c2Vk");
        assert!(html.contains("<img src='assets/dot.png.extra'><img src='assets/dot.png'>"));
    }

    #[tokio::test]
    async fn share_without_assets() {
        let (_dir, app) = fixture("<p>No assets</p>");
        let html = exported(app).await;
        assert_eq!(boot(&html)["assets"], json!([]));
        assert!(html.contains("<p>No assets</p>"));
    }

    #[tokio::test]
    async fn share_embeds_parsed_objects_before_browser_loads() {
        let inert = "<object data='assets/green.svg'></object>";
        let view = format!("<head></head><object data='assets/green.svg'></object><template>{inert}</template><script>const markup = \"{inert}\";</script>");
        let (_dir, app) = fixture(&view);
        app.store.lock().unwrap().put_asset("test:share", "green.svg", b"svg", "agent").unwrap();
        let html = exported(app).await;
        assert!(html.contains("<object data=\"data:image/svg+xml;base64,c3Zn\"></object>"));
        let marker = boot(&html)["rootMarker"].as_str().unwrap().to_owned();
        assert!(html.contains(&format!("data-{marker}-object=\"assets/green.svg\"")));
        assert!(html.contains(&format!("const markup = \"{inert}\";")));
    }

    #[tokio::test]
    async fn share_static_objects_follow_the_document_base() {
        for base in ["<base href='https://example.com/'>", "<base href='../'>"] {
            for placement in [true, false] {
                let object = "<object data='assets/green.svg'></object>";
                let view = if placement { format!("<head>{base}</head>{object}") } else { format!("<head></head>{object}{base}") };
                let (_dir, app) = fixture(&view);
                app.store.lock().unwrap().put_asset("test:share", "green.svg", b"svg", "agent").unwrap();
                assert!(exported(app).await.contains(object));
            }
        }
        for path in ["assets/%67reen.svg", r"assets\%67reen.svg", r"assets\t/../%67reen.svg", "././assets/%67reen.svg", "assets/sub/../%67reen.svg?query#part"] {
            let (_dir, app) = fixture(&format!("<head><template><base href='https://example.com/'></template><base href='./'></head><object data='{path}'></object>"));
            app.store.lock().unwrap().put_asset("test:share", "green.svg", b"svg", "agent").unwrap();
            let html = exported(app).await;
            let fragment = if path.ends_with("#part") { "#part" } else { "" };
            assert!(html.contains(&format!("<object data=\"data:image/svg+xml;base64,c3Zn{fragment}\"></object>")), "{path}");
        }
    }

    #[tokio::test]
    async fn share_static_objects_use_only_html_document_bases() {
        let object = "<object data='assets/green.svg'></object>";
        for base in [
            "<svg><base href='https://example.com/'/></svg>",
            "<math><base href='https://example.com/'/></math>",
            "<base href='data:text/html,ignored'>",
            "<base href='javascript:void(0)'>",
            "<base href='data:text/html,ignored'><base href='https://example.com/'>",
            "<base href='javascript:void(0)'><base href='https://example.com/'>",
            "<base href=''><base href='https://example.com/'>",
            "<base href='  '>",
            "<template><base href='https://example.com/'></template>",
            "<div><template shadowrootmode='open'><base href='https://example.com/'></template></div>",
            "<noscript><base href='https://example.com/'></noscript>",
            "<!-- <base href='https://example.com/'> -->",
            "<script>const html = `<base href='https://example.com/'>`;</script>",
        ] {
            let (_dir, app) = fixture(&format!("<head></head>{base}{object}"));
            app.store.lock().unwrap().put_asset("test:share", "green.svg", b"svg", "agent").unwrap();
            assert!(exported(app).await.contains("<object data=\"data:image/svg+xml;base64,c3Zn\"></object>"), "{base}");
        }
        for base in [
            "<svg><foreignObject><base href='https://example.com/'></foreignObject></svg>",
            "<math><mtext><base href='https://example.com/'></mtext></math>",
        ] {
            let (_dir, app) = fixture(&format!("<head></head>{base}{object}"));
            app.store.lock().unwrap().put_asset("test:share", "green.svg", b"svg", "agent").unwrap();
            assert!(exported(app).await.contains(object), "{base}");
        }
    }

    #[tokio::test]
    async fn share_static_objects_do_not_depend_on_the_export_location() {
        for (base, path) in [
            ("/jelly-shared/", "assets/green.svg"),
            ("../jelly-shared/", "assets/green.svg"),
            ("file:///jelly-shared/", "assets/green.svg"),
            ("/elsewhere/output/", "assets/green.svg"),
            ("../output/", "assets/green.svg"),
            ("./", "/jelly-shared/assets/green.svg"),
            ("./", "../jelly-shared/assets/green.svg"),
        ] {
            let object = format!("<object data='{path}'></object>");
            let (_dir, app) = fixture(&format!("<head><base href='{base}'></head>{object}"));
            app.store.lock().unwrap().put_asset("test:share", "green.svg", b"svg", "agent").unwrap();
            assert!(exported(app).await.contains(&object), "base {base}, data {path}");
        }
        let (_dir, app) = fixture("<head><base href='nested/'></head><object data='../assets/green.svg'></object>");
        app.store.lock().unwrap().put_asset("test:share", "green.svg", b"svg", "agent").unwrap();
        assert!(exported(app).await.contains("<object data=\"data:image/svg+xml;base64,c3Zn\"></object>"));
    }

    #[tokio::test]
    async fn share_refuses_an_unreadable_listed_asset() {
        let (_dir, app) = fixture("");
        let path = app.store.lock().unwrap().asset_dir("test:share").join("broken.png");
        std::fs::create_dir_all(path).unwrap();
        let error = share(State(app), Path("test:share".into())).await.expect_err("a directory cannot be read as an asset");
        assert!(format!("{error:?}").contains("could not share asset broken.png"));
    }

    #[tokio::test]
    async fn share_refuses_asset_enumeration_failure() {
        let (_dir, app) = fixture("");
        let path = app.store.lock().unwrap().asset_dir("test:share");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"not a directory").unwrap();
        let error = share(State(app), Path("test:share".into())).await.expect_err("asset enumeration must succeed");
        assert!(matches!(error, ApiError::Internal(message) if message.contains("could not share assets for test:share")));
    }

    #[tokio::test]
    async fn shared_file_renders_assets_and_reading_api() {
        let browser = chrome().expect("Chrome/Chromium required; set JELLY_CHROME to the browser binary");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let callback = format!("http://{}/result", listener.local_addr().unwrap());
        let (send, mut receive) = tokio::sync::mpsc::channel(1);
        let results = Router::new().route("/result", post(move |body: String| async move {
            let result: Value = serde_json::from_str(&body).unwrap();
            send.send(result).await.unwrap();
            StatusCode::NO_CONTENT
        }));
        let server = tokio::spawn(async move { axum::serve(listener, results).await.unwrap(); });
        let view = include_str!("../tests/fixtures/share/view.html")
            .replace("__GC_ROOTS__", &["open", "closed"].into_iter().map(|mode| {
                format!("<div class=discard-root><template shadowrootmode={mode}><share-gc-probe></share-gc-probe></template></div>").repeat(25)
            }).collect::<String>())
            .replace("__TEST_RESULT__", &callback)
            .replace("__LIVE_RUNTIME__", &serde_json::to_string(RUNTIME_JS).unwrap().replace("</", "<\\/"))
            .replace("__SNAPSHOT_RUNTIME__", &serde_json::to_string(SNAPSHOT_JS).unwrap().replace("</", "<\\/"));
        let (dir, app) = fixture(&view);
        {
            let mut store = app.store.lock().unwrap();
            let png = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAYAAACp8Z5+AAAAEklEQVR4nGP4z8DwHxkzkC4AADxAH+HggXe0AAAAAElFTkSuQmCC").unwrap();
            for file in ["dot.png", "unused.png"] {
                store.put_asset("test:share", file, &png, "agent").unwrap();
            }
            store.put_asset("test:share", "hello.txt", b"hello offline", "agent").unwrap();
            store.put_asset("test:share", "green.svg", br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><path fill="green" d="M0 0h4v4H0z"/></svg>"##, "agent").unwrap();
            store.put_asset("test:share", "red.svg", br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><path fill="red" d="M0 0h4v4H0z"/></svg>"##, "agent").unwrap();
            store.put_asset("test:share", "theme.css", b"@import 'nested.css'; #linked { background-image: url(dot.png); }", "agent").unwrap();
            store.put_asset("test:share", "nested.css", b"@import 'theme.css'; #imported { background-image: url(dot.png); }", "agent").unwrap();
            store.put_asset("test:share", "helper.js", b"window.localHelper = true;", "agent").unwrap();
            for (file, prefix, encoding) in [
                ("legacy", "@charset \"windows-1252\";", "legacy"),
                ("utf16", "", "utf16"),
                ("utf8-bom", "@charset \"windows-1252\";", "utf8"),
            ] {
                let css = format!("{prefix} #encoding-{file}.café {{ background-image: url(dot.png); }} #encoding-{file}.café::after {{ content: \"café\"; }}");
                let bytes = match encoding {
                    "legacy" => css.chars().map(|c| c as u8).collect(),
                    "utf16" => [vec![0xff, 0xfe], css.encode_utf16().flat_map(u16::to_le_bytes).collect()].concat(),
                    _ => [vec![0xef, 0xbb, 0xbf], css.into_bytes()].concat(),
                };
                store.put_asset("test:share", &format!("{file}.css"), &bytes, "agent").unwrap();
            }
            for file in ["link-unknown", "document-default"] {
                let css = format!("#encoding-{file}.café {{ background-image: url(dot.png); }} #encoding-{file}.café::after {{ content: 'café'; }}");
                store.put_asset("test:share", &format!("{file}.css"), css.as_bytes(), "agent").unwrap();
            }
            let link_selected = b"#encoding-link-selected.caf\xe9 { background-image: url(dot.png); } #encoding-link-selected.caf\xe9::after { content: 'caf\xe9'; }";
            store.put_asset("test:share", "link-selected.css", link_selected, "agent").unwrap();
            let collision = String::from_utf8_lossy(link_selected);
            store.put_asset("test:share", "link-collision.css", collision.as_bytes(), "agent").unwrap();
            store.put_asset("test:share", "link-imported.css", b"@import 'link-child.css';", "agent").unwrap();
            store.put_asset("test:share", "link-child.css", b"#encoding-link-imported.caf\xe9 { background-image: url(dot.png); } #encoding-link-imported.caf\xe9::after { content: 'caf\xe9'; }", "agent").unwrap();
            store.put_asset("test:share", "inherited.css", b"@charset \"windows-1252\"; @import 'child.css';", "agent").unwrap();
            store.put_asset("test:share", "child.css", b"#encoding-inherited.caf\xe9 { background-image: url(dot.png); } #encoding-inherited.caf\xe9::after { content: 'caf\xe9'; }", "agent").unwrap();
        }
        let html = exported(app).await;
        let path = dir.path().join("shared.html");
        std::fs::write(&path, html).unwrap();
        // Chrome's updater can inherit stderr and hold a pipe open after Chrome exits.
        let log = dir.path().join("browser.log");
        let stderr = std::fs::File::create(&log).unwrap();
        let mut browser = tokio::process::Command::new(browser)
            .args(["--headless=new", "--disable-gpu", "--no-first-run", "--no-default-browser-check", "--enable-automation", "--disable-background-timer-throttling", "--disable-component-update", "--js-flags=--expose-gc"])
            .arg(format!("--user-data-dir={}", dir.path().join("chrome").display()))
            .arg(format!("file://{}", path.display()))
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .kill_on_drop(true).spawn().expect("browser could not start");
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), receive.recv()).await;
        server.abort();
        browser.kill().await.expect("browser could not stop");
        browser.wait().await.expect("browser could not exit");
        let result = result.unwrap_or_else(|_| panic!("browser did not report a result within 30 seconds: {}", std::fs::read_to_string(log).unwrap()))
            .expect("browser result channel closed");
        assert_eq!(result["complete"], true, "{result}");
        assert_eq!(result["ok"], true, "{result}");
        println!("{result}");
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;

    fn fixture(capacity: usize) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        for name in ["j", "other", PAGE] {
            store.create(name, "".into(), "const verbs = {};".into(), json!({}), "agent", false, "", "").unwrap();
        }
        let (events, _) = broadcast::channel(capacity);
        (dir, App { store: Mutex::new(store), events, screens: Mutex::default() })
    }

    fn query(jelly: Option<&str>, since: u64) -> WaitQ {
        WaitQ { since, by: "agent".into(), project_jelly: None, jelly: jelly.map(String::from), timeout: Some(0) }
    }

    fn note(app: &App, jelly: &str, who: &str) -> Event {
        app.change(|s| s.note(jelly, "changed", who)).unwrap().0
    }

    #[tokio::test]
    async fn fresh_history_waits_for_its_first_event_or_times_out() {
        for (first_event, since) in [(false, 0), (true, 0), (false, u64::MAX)] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).unwrap();
            let path = store.events_path();
            let (events, _) = broadcast::channel(1);
            let app = App { store: Mutex::new(store), events, screens: Mutex::default() };
            let q = WaitQ { timeout: Some(1), ..query(None, since) };
            let waiting = wait_since(&app, &q);
            tokio::pin!(waiting);
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(waiting.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            }).await;
            assert!(!path.exists(), "waiting must not create an event log");
            if first_event {
                let expected = app.change(|s| s.create("first", "".into(), "const verbs = {};".into(), json!({}), "person", false, "", "")).unwrap().0;
                let actual = waiting.await.unwrap().unwrap();
                assert_eq!(actual.seq, expected.seq);
                assert_eq!(actual.jelly, "first");
                assert!(actual.state.is_none());
            } else {
                assert!(waiting.await.unwrap().is_none());
                assert!(!path.exists());
            }
        }
    }

    #[tokio::test]
    async fn history_is_exclusive_ordered_and_filtered_by_author_and_jelly() {
        let (_dir, app) = fixture(16);
        let start = note(&app, "j", "person");
        note(&app, "j", "agent");
        let surface = note(&app, PAGE, "person");
        let other = note(&app, "other", "helper");
        let first = note(&app, "j", "helper");
        note(&app, "j", "person");
        assert_eq!(wait_since(&app, &query(Some("j"), start.seq)).await.unwrap().unwrap().seq, first.seq);
        assert_eq!(wait_since(&app, &query(None, start.seq)).await.unwrap().unwrap().seq, other.seq);
        assert_eq!(wait_since(&app, &query(Some(PAGE), start.seq)).await.unwrap().unwrap().seq, surface.seq);
        assert!(wait_since(&app, &query(None, u64::MAX)).await.unwrap().is_none());
        assert!(matches!(wait_since(&app, &query(Some("missing"), 0)).await, Err(Error::NotFound(_))));
    }

    #[tokio::test]
    async fn history_survives_replacement_removal_and_restart() {
        let (dir, mut app) = fixture(16);
        let start = note(&app, "j", "agent");
        let replacement = app.change(|s| s.create("j", "".into(), "const verbs = {};".into(), json!({}), "person", true, "", "")).unwrap().0;
        assert!(replacement.version < start.version);
        let removed = app.change(|s| s.remove("j", "person")).unwrap().0;
        app.store = Mutex::new(Store::open(dir.path()).unwrap());
        assert_eq!(wait_since(&app, &query(Some("j"), start.seq)).await.unwrap().unwrap().seq, replacement.seq);
        assert_eq!(wait_since(&app, &query(Some("j"), replacement.seq)).await.unwrap().unwrap().seq, removed.seq);
    }

    #[tokio::test]
    async fn eof_handoff_and_lag_recover_the_first_committed_answer() {
        for lag in [false, true] {
            let (dir, mut app) = fixture(1);
            let ask = app.change(|s| s.attend("j", json!({"title": "Which?"}), "agent")).unwrap().0;
            let q = WaitQ { timeout: Some(1), ..query(Some("j"), ask.seq) };
            let answer = std::cell::RefCell::new(None);
            let result = wait_since_at_handoff(&app, &q, |store| {
                let ev = store.answer("j", "a1", None, "yes", "person").unwrap();
                let _ = app.events.send(ev.clone());
                answer.replace(Some(ev));
                if lag {
                    for _ in 0..4 {
                        let ev = store.note("j", "changed", "agent").unwrap();
                        let _ = app.events.send(ev);
                    }
                }
            }).await.unwrap().unwrap();
            let answer = answer.into_inner().unwrap();
            assert_eq!(result.seq, answer.seq);
            assert_eq!(result.args.unwrap()["text"], "yes");
            assert!(result.state.is_none());
            app.store = Mutex::new(Store::open(dir.path()).unwrap());
            assert_eq!(wait_since(&app, &q).await.unwrap().unwrap().seq, answer.seq);
        }
    }

    #[tokio::test]
    async fn notification_order_does_not_override_commit_order() {
        let (_dir, app) = fixture(4);
        let rx = app.events.subscribe();
        let q = query(Some("j"), 0);
        let mut reader = BufReader::new(File::open(app.store.lock().unwrap().events_path()).unwrap());
        assert!(recorded_change(&mut reader, &q).unwrap().is_none());
        let (first, second) = {
            let mut s = app.store.lock().unwrap();
            (s.note("j", "first", "helper").unwrap(), s.note("j", "second", "person").unwrap())
        };
        app.events.send(second).unwrap();
        let result = wait_for_record(&app, &q, &mut Some(reader), rx, None).await.unwrap().unwrap();
        assert_eq!(result.seq, first.seq);
    }

    #[tokio::test]
    async fn ignored_activity_stays_pending_until_an_eligible_change() {
        let (_dir, app) = fixture(8);
        let mut q = query(Some("j"), 0);
        q.timeout = None;
        let future = wait_since(&app, &q);
        tokio::pin!(future);
        assert!(tokio::time::timeout(Duration::from_millis(5), &mut future).await.is_err());
        note(&app, "j", "agent");
        note(&app, "other", "person");
        assert!(tokio::time::timeout(Duration::from_millis(5), &mut future).await.is_err());
        let expected = note(&app, "j", "helper");
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), future).await.unwrap().unwrap().unwrap().seq, expected.seq);
    }

    #[tokio::test]
    async fn ignored_notifications_do_not_extend_the_deadline() {
        let (_dir, app) = fixture(8);
        let q = WaitQ { timeout: Some(1), ..query(Some("j"), 0) };
        let producer = async {
            loop {
                note(&app, "j", "agent");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let start = Instant::now();
        tokio::select! {
            result = wait_since(&app, &q) => assert!(result.unwrap().is_none()),
            _ = producer => unreachable!(),
            _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("timeout was extended"),
        }
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn ask_summary_keeps_its_batch_sequence_after_a_person_answers() {
        let (_dir, app) = fixture(8);
        let summary = app.change_many(|s| s.ask_round("sharing", &json!({"question": "Which?"}), ASK_VIEW, ASK_VERBS, "", "agent")).unwrap().0;
        let seq = summary["seq"].as_u64().unwrap();
        let answer = app.change(|s| s.answer("sharing", "a1", None, "quiet", "person")).unwrap().0;
        assert!(answer.seq > seq);
        let mut reader = BufReader::new(File::open(app.store.lock().unwrap().events_path()).unwrap());
        let q = WaitQ { by: "person".into(), ..query(Some("sharing"), seq - 1) };
        let last = recorded_change(&mut reader, &q).unwrap().unwrap();
        assert_eq!(last.seq, seq);
        assert_eq!(last.kind, crate::store::EventKind::Attention);
        assert_eq!(summary["state"]["round"], 1);
    }

    #[test]
    fn log_failures_leave_answers_and_removals_uncommitted() {
        for failure in ["directory", "readonly", "partial"] {
            let (dir, app) = fixture(8);
            let _ = app.change(|s| s.attend("j", json!({"title": "Which?"}), "agent")).unwrap();
            let mut rx = app.events.subscribe();
            let path = dir.path().join("events.jsonl");
            let original = std::fs::read(&path).unwrap();
            let retained = dir.path().join("retained.jsonl");
            match failure {
                "directory" => {
                    std::fs::rename(&path, &retained).unwrap();
                    std::fs::create_dir(&path).unwrap();
                }
                "readonly" => {
                    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
                    permissions.set_readonly(true);
                    std::fs::set_permissions(&path, permissions).unwrap();
                }
                "partial" => {
                    use std::io::Write;
                    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"seq\":").unwrap();
                }
                _ => unreachable!(),
            }
            for remove in [false, true] {
                let result = if remove {
                    app.change(|s| s.remove("j", "person"))
                } else {
                    app.change(|s| s.answer("j", "a1", None, "not accepted", "person"))
                };
                let ApiError::Store(error) = result.unwrap_err() else { panic!("expected store error") };
                let error = error.to_string();
                assert!(error.contains("could not record event"), "{error}");
                assert!(rx.try_recv().is_err(), "failed changes must not be broadcast as accepted");
                if failure == "readonly" {
                    let reopened = Store::open(dir.path()).unwrap();
                    assert!(reopened.get("j").unwrap().attention[0].answer.is_none());
                } else {
                    assert!(Store::open(dir.path()).is_err(), "damaged logs must prevent startup");
                }
                assert!(app.store.lock().unwrap().get("j").unwrap().attention[0].answer.is_none());
            }
            if failure == "directory" {
                std::fs::remove_dir(&path).unwrap();
                std::fs::rename(retained, &path).unwrap();
            }
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, &original).unwrap();
            let recovered = note(&app, "other", "person");
            let mut reader = BufReader::new(File::open(path).unwrap());
            assert_eq!(recorded_change(&mut reader, &query(Some("other"), 0)).unwrap().unwrap().seq, recovered.seq);
        }
    }

    #[tokio::test]
    async fn partial_batches_publish_every_record_before_the_error() {
        let spec = json!({"questions": [{"question": "First?"}, {"question": "Second?"}]});
        for existing in [false, true] {
            let count = if existing { 6 } else { 4 };
            for successful in 0..count {
                let (_dir, app) = fixture(16);
                if existing {
                    let _ = app.change_many(|s| s.ask_round("sharing", &spec, ASK_VIEW, ASK_VERBS, "", "agent")).unwrap();
                    let _ = app.change(|s| s.reshape("sharing", Some("different view".into()), None, None, "agent")).unwrap();
                }
                let path = app.store.lock().unwrap().events_path();
                let head = std::fs::read_to_string(&path).unwrap().lines().last()
                    .map(|line| serde_json::from_str::<Event>(line).unwrap().seq).unwrap();
                let owner = if existing {
                    let view_rev = app.store.lock().unwrap().get("sharing").unwrap().view_rev;
                    Some(app.snapshot_owner("sharing", view_rev).unwrap())
                } else { None };
                app.store.lock().unwrap().log_failure = Some((head + successful + 1, 10));
                let mut rx = app.events.subscribe();
                let q = WaitQ { by: "person".into(), timeout: Some(1), ..query(None, head) };
                let waiting = wait_since(&app, &q);
                tokio::pin!(waiting);
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(waiting.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                }).await;
                let error = app.change_many(|s| s.ask_round("sharing", &spec, ASK_VIEW, ASK_VERBS, "", "agent")).unwrap_err();
                if let Some(owner) = owner {
                    if successful == 0 {
                        assert!(app.screen("sharing", &owner).is_ok());
                    } else {
                        assert!(!app.screens.lock().unwrap().contains_key("sharing"), "a committed reshape invalidates the old screen even when the batch fails");
                        assert!(app.screen("sharing", &owner).is_err());
                    }
                }
                let response = error.into_response();
                let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                let message = body["error"].as_str().unwrap();
                assert!(message.contains("could not record event"), "{message}");
                if successful > 0 {
                    assert!(message.contains(&format!("committed {successful} changes to sharing through seq:{}", head + successful)), "{message}");
                    assert!(message.contains("jelly show sharing"), "{message}");
                    assert!(message.contains("before retrying"), "{message}");
                } else {
                    assert!(!message.contains("committed"), "{message}");
                }
                for seq in head + 1..=head + successful {
                    assert_eq!(rx.try_recv().unwrap().seq, seq);
                }
                assert!(rx.try_recv().is_err());
                if successful > 0 {
                    let event = waiting.await.unwrap().unwrap();
                    assert_eq!(event.seq, head + 1);
                    assert!(event.state.is_none());
                }
                let history = std::fs::read_to_string(path).unwrap();
                assert_eq!(history.lines().last().map(|line| serde_json::from_str::<Event>(line).unwrap().seq), Some(head + successful));
            }
        }
    }

    #[tokio::test]
    async fn a_partial_append_restores_history_and_the_unanswered_item() {
        let (dir, app) = fixture(8);
        let ask = app.change(|s| s.attend("j", json!({"title": "Which?"}), "agent")).unwrap().0;
        let path = app.store.lock().unwrap().events_path();
        let original = std::fs::read(&path).unwrap();
        app.store.lock().unwrap().log_failure = Some((ask.seq + 1, 10));
        assert!(app.change(|s| s.answer("j", "a1", None, "yes", "person")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(app.store.lock().unwrap().get("j").unwrap().attention[0].answer.is_none());
        assert!(Store::open(dir.path()).unwrap().get("j").unwrap().attention[0].answer.is_none());
        app.store.lock().unwrap().log_failure = None;
        let answer = app.change(|s| s.answer("j", "a1", None, "yes", "person")).unwrap().0;
        assert_eq!(wait_since(&app, &query(Some("j"), ask.seq)).await.unwrap().unwrap().seq, answer.seq);
    }

    #[tokio::test]
    async fn missing_committed_history_is_an_error() {
        for restart in [false, true] {
            let (dir, mut app) = fixture(4);
            let ask = app.change(|s| s.attend("j", json!({"title": "Which?"}), "agent")).unwrap().0;
            let _ = app.change(|s| s.answer("j", "a1", None, "yes", "person")).unwrap();
            if restart {
                app.store = Mutex::new(Store::open(dir.path()).unwrap());
            }
            let path = app.store.lock().unwrap().events_path();
            std::fs::remove_file(&path).unwrap();
            for since in [0, ask.seq, u64::MAX] {
                let error = wait_since(&app, &query(Some("j"), since)).await.unwrap_err();
                assert!(error.to_string().contains("event history"));
            }
            assert!(!path.exists());
        }
    }

    #[tokio::test]
    async fn missing_history_after_the_first_notification_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let path = store.events_path();
        let (events, _) = broadcast::channel(1);
        let app = App { store: Mutex::new(store), events, screens: Mutex::default() };
        let q = WaitQ { timeout: Some(1), ..query(None, 0) };
        let waiting = wait_since(&app, &q);
        tokio::pin!(waiting);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(waiting.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
        let _ = app.change(|s| s.create("first", "".into(), "const verbs = {};".into(), json!({}), "person", false, "", "")).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(waiting.await.unwrap_err().to_string().contains("event history"));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn unreadable_or_malformed_history_is_an_error() {
        let (_dir, app) = fixture(4);
        let path = app.store.lock().unwrap().events_path();
        std::fs::write(&path, "invalid json\n").unwrap();
        assert!(wait_since(&app, &query(None, 0)).await.unwrap_err().to_string().contains("event history"));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(wait_since(&app, &query(None, 0)).await.unwrap_err().to_string().contains("event history"));
    }
}

#[cfg(test)]
mod screen_tests {
    use super::Screen;

    #[test]
    fn older_screens_have_no_exposed_cards() {
        let screen: Screen = serde_json::from_value(serde_json::json!({
            "owner": "test", "text": "page", "width": 1280, "height": 900, "version": 1
        })).unwrap();
        assert!(screen.exposed.is_empty());
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;

    #[test]
    fn snapshot_selects_the_actual_destination_for_this_item() {
        let mut screen: Screen = serde_json::from_value(json!({
            "owner": "test", "text": "", "width": 1280, "height": 900, "version": 3,
            "fallback": [
                {"id": "a1", "destination": {"kind": "after", "selector": "#tiles"}, "reason": "layout"},
                {"id": "a2", "destination": {"kind": "top"}, "reason": "no boundary"}
            ]
        })).unwrap();
        assert_eq!(screen.placement("other"), None);
        let AnchorPlace::Fallback(first) = screen.placement("a1").unwrap() else { panic!("expected fallback") };
        assert_eq!(first.destination.describe(), "after #tiles");
        assert_eq!(first.reason, "layout");
        let AnchorPlace::Fallback(second) = screen.placement("a2").unwrap() else { panic!("expected fallback") };
        assert_eq!(second.destination, AnchorDestination::Top);
        screen.hidden.push("a1 #tile".into());
        assert_eq!(screen.placement("a1"), Some(AnchorPlace::Hidden));
        screen.floating.push("a1 #tile".into());
        assert_eq!(screen.placement("a1"), Some(AnchorPlace::Floating));
        screen.missing.push("a1 #tile".into());
        assert_eq!(screen.placement("a1"), Some(AnchorPlace::Missing));
        screen.invalid.push("a3 [".into());
        assert_eq!(screen.placement("a3"), Some(AnchorPlace::Invalid));
        assert_eq!(screen.placement("a3").unwrap().status(), AnchorStatus::Invalid);
    }

    #[tokio::test]
    async fn attention_response_preserves_snapshot_destination_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.create("placement", "<p>Test</p>".into(), "const verbs = {};".into(), json!({}), "test", false, "", "").unwrap();
        let (events, _) = broadcast::channel(16);
        let app = Arc::new(App { store: Mutex::new(store), events, screens: Mutex::default() });
        for kind in ["ask", "look"] {
            for destination in [json!({"kind":"after", "selector":"#tiles"}), json!({"kind":"top"})] {
                let mut received = app.events.subscribe();
                let post_snapshot = async {
                    let ev = received.recv().await.unwrap();
                    let owner = {
                        let store = app.store.lock().unwrap();
                        app.snapshot_owner("placement", store.get("placement").unwrap().view_rev).unwrap()
                    };
                    let snapshot: Screen = serde_json::from_value(json!({
                        "owner":owner, "text":"", "width":1280, "height":900, "version":ev.version,
                        "fallback":[{"id":ev.args.as_ref().unwrap()["id"], "destination":destination, "reason":"layout cannot take a question row"}]
                    })).unwrap();
                    let mut unmeasured = snapshot.clone();
                    unmeasured.fallback.clear();
                    put_screen(State(app.clone()), Path("placement".into()), Json(unmeasured)).await.unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    assert_eq!(put_screen(State(app.clone()), Path("placement".into()), Json(snapshot)).await.unwrap(), StatusCode::NO_CONTENT);
                };
                let request = attend(State(app.clone()), Path("placement".into()), axum::http::HeaderMap::new(), Json(AttendReq {
                    spec: json!({"kind":kind, "title":"Choose", "by":"test", "anchor":{"selector":"#tile"}})
                }));
                let (response, ()) = tokio::join!(request, post_snapshot);
                let Json(response) = response.unwrap_or_else(|_| panic!("attention request failed"));
                assert_eq!(response["anchor"], "fallback");
                assert_eq!(response["anchor_destination"], destination);
                assert_eq!(response["anchor_reason"], "layout cannot take a question row");
                let location = if destination["kind"] == "after" { "after #tiles" } else { "at the top" };
                assert_eq!(crate::placement(&response), format!("it shows {location} instead; layout cannot take a question row"));
            }
        }
        let log = std::fs::read_to_string(app.store.lock().unwrap().events_path()).unwrap();
        assert!(!log.contains("anchor_destination") && !log.contains("anchor_reason"));
    }

    #[test]
    fn unmeasured_snapshots_do_not_confirm_placement() {
        let mut screen: Screen = serde_json::from_value(json!({"owner":"test", "text":"", "width":1, "height":1, "version":1})).unwrap();
        assert!(screen.invalid.is_empty());
        assert!(screen.fallback.is_empty());
        assert_eq!(screen.placement("a1"), None);
        screen.below.push("a1 #tile".into());
        assert_eq!(screen.placement("a1"), Some(AnchorPlace::Below));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PERSON;

    fn app() -> (tempfile::TempDir, Shared) {
        let dir = tempfile::tempdir().unwrap();
        let (events, _) = broadcast::channel(32);
        let app = Arc::new(App { store: Mutex::new(Store::open(dir.path()).unwrap()), events, screens: Mutex::default() });
        (dir, app)
    }

    #[tokio::test]
    async fn asset_inventory_is_sorted_read_only_and_requires_a_jelly() {
        let (dir, app) = app();
        let missing = list_assets(State(app.clone()), Path("test:missing".into())).await.unwrap_err();
        assert_eq!(missing.into_response().status(), StatusCode::NOT_FOUND);
        create_page(&app, "PAGE", false).await;
        assert!(list_assets(State(app.clone()), Path("test:x".into())).await.unwrap().0.is_empty());
        for file in ["z.txt", "a.bin"] {
            let _ = put_asset(State(app.clone()), Path(("test:x".into(), file.into())),
                Query(AssetQ { by: Some("test".into()) }), Bytes::from_static(b"asset")).await.unwrap();
        }
        let before = serde_json::to_value(app.store.lock().unwrap().get("test:x").unwrap()).unwrap();
        let log = std::fs::read(dir.path().join("events.jsonl")).unwrap();
        assert_eq!(list_assets(State(app.clone()), Path("test:x".into())).await.unwrap().0, ["a.bin", "z.txt"]);
        assert_eq!(serde_json::to_value(app.store.lock().unwrap().get("test:x").unwrap()).unwrap(), before);
        assert_eq!(std::fs::read(dir.path().join("events.jsonl")).unwrap(), log);
    }

    #[tokio::test]
    async fn asset_route_rejects_hidden_files_and_traversal() {
        let (_dir, app) = app();
        create_page(&app, "PAGE", false).await;
        let dir = app.store.lock().unwrap().asset_dir("test:x");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for path in [dir.join(".hidden"), dir.join("sub/secret.txt"), dir.parent().unwrap().join("secret.txt")] {
            std::fs::write(path, b"secret").unwrap();
        }
        for file in [".hidden", ".", "..", "../secret.txt", "sub/secret.txt"] {
            let response = asset(State(app.clone()), Path(("test:x".into(), file.into()))).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{file}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn asset_urls_only_reach_an_existing_jellys_assets() {
        let (home, app) = app();
        create_page(&app, "PAGE", false).await;
        for (file, bytes) in [("shot.png", &b"png bytes"[..]), ("notes..v2.txt", b"two dots")] {
            let _ = put_asset(State(app.clone()), Path(("test:x".into(), file.into())),
                Query(AssetQ { by: Some("test".into()) }), Bytes::copy_from_slice(bytes)).await.unwrap();
        }
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("note.txt"), b"SECRET-OUTSIDE").unwrap();
        for dir in [home.path().join("beside"), home.path().join("assets").join(crate::disk::name("test:missing"))] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("note.txt"), b"SECRET-OUTSIDE").unwrap();
        }

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(listener, router(app))));
        let fetch = |path: String| {
            let url = format!("{base}{path}");
            tokio::task::spawn_blocking(move || match ureq::get(&url).call() {
                Ok(r) | Err(ureq::Error::Status(_, r)) => {
                    let (status, kind) = (r.status(), r.content_type().to_string());
                    let mut body = Vec::new();
                    r.into_reader().read_to_end(&mut body).unwrap();
                    (status, kind, body)
                }
                Err(e) => panic!("{url}: {e}"),
            })
        };

        let absolute = outside.path().to_str().unwrap().replace('/', "%2F");
        for name in [absolute.as_str(), "..%2Fbeside", "test%3Amissing"] {
            let (status, _, body) = fetch(format!("/j/{name}/assets/note.txt")).await.unwrap();
            assert_eq!(status, 404, "{name}");
            assert!(!String::from_utf8_lossy(&body).contains("SECRET-OUTSIDE"), "{name}");
        }
        for (file, kind, bytes) in [("shot.png", "image/png", &b"png bytes"[..]), ("notes..v2.txt", "text/plain", b"two dots")] {
            assert_eq!(fetch(format!("/j/test:x/assets/{file}")).await.unwrap(), (200, kind.to_string(), bytes.to_vec()));
        }
    }

    #[tokio::test]
    async fn lagging_sse_subscribers_are_told_to_resync_even_when_filtered() {
        for jelly in [None, Some("other:q".into())] {
            let (_dir, app) = app();
            let response = events(State(app.clone()), Query(ByQ { by: None, jelly })).await;
            let mut body = response.into_response().into_body().into_data_stream();
            create_page(&app, "PAGE", false).await;
            for _ in 0..32 {
                let _ = app.change(|s| s.set_state("test:x", "", Some(json!({})), "test")).unwrap();
            }
            let frame = tokio::time::timeout(std::time::Duration::from_secs(1), body.next())
                .await.expect("lag must wake the subscriber").unwrap().unwrap();
            assert_eq!(&frame[..], b": resync\n\n");
        }
    }

    async fn create_page(app: &Shared, text: &str, replace: bool) {
        let _ = create(State(app.clone()), Json(CreateReq {
            name: "test:x".into(), view: format!("<h1>{text}</h1>"), verbs: "const verbs = {};".into(),
            state: json!({}), by: Some("test".into()), replace, about: String::new(), project: String::new(),
        })).await.unwrap();
    }

    async fn owner(app: &Shared, name: &str) -> String {
        let Html(html) = view(State(app.clone()), Path(name.into())).await.unwrap();
        let boot = html.split_once("window.__JELLY__=").unwrap().1.split_once(";</script>").unwrap().0;
        serde_json::from_str::<Value>(boot).unwrap()["snapshotOwner"].as_str().unwrap().into()
    }

    fn snapshot(owner: &str, text: &str, version: u64) -> Screen {
        serde_json::from_value(json!({"owner": owner, "text": text, "width": 1280, "height": 900, "version": version})).unwrap()
    }

    async fn post(app: &Shared, sc: Screen) -> Result<StatusCode, ApiError> {
        put_screen(State(app.clone()), Path("test:x".into()), Json(sc)).await
    }

    async fn seen(app: &Shared) -> String {
        let response = see(State(app.clone()), Path("test:x".into()),
            Query(SeeQ { width: None, height: None, png: false, full: false }), axum::http::HeaderMap::new()).await.unwrap();
        String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn bundled_runtime_posts_served_ownership_and_reloads_obsolete_pages() {
        let (_dir, app) = app();
        create_page(&app, "PAGE", false).await;
        let Html(html) = view(State(app.clone()), Path("test:x".into())).await.unwrap();
        let boot = html.split_once("window.__JELLY__=").unwrap().1.split_once(";</script>").unwrap().0;
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        let eval = |script: &str| ctx.with(|ctx| {
            ctx.eval::<(), _>(script).unwrap_or_else(|err| panic!("{err}: {:?}", ctx.catch()));
        });
        eval(include_str!("../tests/runtime-browser.js"));
        eval(&format!("window.__JELLY__={boot};"));
        eval(RUNTIME_JS);
        eval("loaded();");
        while rt.execute_pending_job().unwrap() {}
        let request: Value = ctx.with(|ctx| {
            serde_json::from_str(&ctx.eval::<String, _>("JSON.stringify(requests[0])").unwrap()).unwrap()
        });
        assert_eq!(request["url"], "/api/jellies/test%3Ax/screen");
        assert_eq!(post(&app, serde_json::from_value(request["body"].clone()).unwrap()).await.unwrap(), StatusCode::NO_CONTENT);
        eval("if (reloads !== 0) throw Error('accepted post reloaded');");
        eval("message({jelly:'other:x', kind:'new', view_rev:1}); if (reloads !== 0) throw Error('other jelly reloaded');");
        eval("message({jelly:'test:x', kind:'new', view_rev:1, version:1}); if (reloads !== 1) throw Error('same-revision replacement did not reload');");
        create_page(&app, "REPLACEMENT", true).await;
        let stale_status = post(&app, serde_json::from_value(request["body"].clone()).unwrap()).await.unwrap_err().into_response().status();
        assert_eq!(stale_status, StatusCode::CONFLICT);
        eval(&format!("responseStatus={}; loaded();", stale_status.as_u16()));
        while rt.execute_pending_job().unwrap() {}
        eval("if (reloads !== 2) throw Error('stale post did not reload');");
    }

    #[tokio::test]
    async fn see_rechecks_currentness_after_text_and_png_rendering() {
        for png in [false, true] {
            for replace in [false, true] {
                let (_dir, app) = app();
                create_page(&app, "OLD PAGE", false).await;
                let current = owner(&app, "test:x").await;
                if png { post(&app, snapshot(&current, "OLD PAGE", 1)).await.unwrap(); }
                let (started, suspended) = tokio::sync::oneshot::channel();
                let (resume, resumed) = tokio::sync::oneshot::channel();
                let mut rendezvous = Some((started, resumed));
                let seeing = see_with_renderer(app.clone(), "test:x".into(),
                    SeeQ { width: None, height: None, png, full: false },
                    move |_, _, path| {
                        assert_eq!(path.is_some(), png);
                        let (started, resumed) = rendezvous.take().expect("one render");
                        async move {
                            started.send(()).unwrap();
                            resumed.await.unwrap();
                            // A failed text render bypasses polling, so only the final
                            // currentness check can reject the captured jelly here.
                            if png { Ok(()) } else { Err("controlled render failure".into()) }
                        }
                    });
                let changing = async {
                    suspended.await.unwrap();
                    if replace { create_page(&app, "NEW PAGE", true).await; }
                    else { let _ = app.change(|s| s.set_state("test:x", "", Some(json!({"n":1})), "test")).unwrap(); }
                    resume.send(()).unwrap();
                };
                let (result, _) = tokio::join!(seeing, changing);
                assert_eq!(result.expect_err("obsolete see must fail").into_response().status(), StatusCode::CONFLICT,
                    "png={png}, replace={replace}");
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn see_png_reports_only_a_screenshot_the_browser_made() {
        use std::os::unix::fs::PermissionsExt;
        for (exit, old_png) in [(3, false), (3, true), (0, false)] {
            let (dir, app) = app();
            create_page(&app, "PAGE", false).await;
            let current = owner(&app, "test:x").await;
            post(&app, snapshot(&current, "PAGE", 1)).await.unwrap();
            let png = dir.path().join("see/test_x-1280.png");
            if old_png {
                std::fs::create_dir_all(png.parent().unwrap()).unwrap();
                std::fs::write(&png, b"old").unwrap();
            }
            let browser = dir.path().join("browser.sh");
            std::fs::write(&browser, format!("#!/bin/sh\nexit {exit}\n")).unwrap();
            std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();
            let browser = browser.to_str().unwrap().to_string();
            let response = see_with_renderer(app.clone(), "test:x".into(),
                SeeQ { width: None, height: None, png: true, full: false },
                move |width, height, path| {
                    let browser = browser.clone();
                    async move {
                        run_browser(&browser, 0, "test:x", width, height, path.as_deref(), std::time::Duration::from_secs(5)).await
                    }
                }).await.unwrap();
            let out = String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
            let png_line = format!("png      {}", png.display());
            if exit == 0 {
                assert!(out.contains(&png_line), "{out}");
            } else {
                assert!(!out.contains("png      "), "old_png={old_png}: {out}");
                assert!(out.contains("No screenshot: the browser failed (exit status: 3)"), "{out}");
            }
        }
    }

    #[tokio::test]
    async fn cold_see_waits_for_matching_version_and_width() {
        for wrong_width in [false, true] {
            let (_dir, app) = app();
            create_page(&app, "PAGE", false).await;
            let current = owner(&app, "test:x").await;
            let (rendered, polling) = tokio::sync::oneshot::channel();
            let mut rendered = Some(rendered);
            let posting_app = app.clone();
            let posting_owner = current.clone();
            let seeing = see_with_renderer(app.clone(), "test:x".into(),
                SeeQ { width: None, height: None, png: false, full: false },
                move |_, _, path| {
                    assert!(path.is_none());
                    let app = posting_app.clone();
                    let owner = posting_owner.clone();
                    let rendered = rendered.take().expect("one render");
                    async move {
                        let mut wrong = snapshot(&owner, "WRONG SNAPSHOT", if wrong_width { 1 } else { 2 });
                        if wrong_width { wrong.width = 390; }
                        post(&app, wrong).await.unwrap();
                        rendered.send(()).unwrap();
                        Ok(())
                    }
                });
            let updating = async {
                polling.await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                post(&app, snapshot(&current, "MATCHING SNAPSHOT", 1)).await.unwrap();
            };
            let (result, _) = tokio::join!(seeing, updating);
            let response = result.unwrap();
            let output = String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
            assert!(output.contains("MATCHING SNAPSHOT"), "{output}");
            assert!(!output.contains("WRONG SNAPSHOT"), "{output}");
        }
    }

    #[tokio::test]
    async fn replaced_and_recreated_pages_never_reuse_old_screens() {
        let (_dir, app) = app();
        create_page(&app, "OLD PAGE", false).await;
        let old = owner(&app, "test:x").await;
        assert_eq!(post(&app, snapshot(&old, "OLD PAGE", 1)).await.unwrap(), StatusCode::NO_CONTENT);
        assert!(seen(&app).await.contains("OLD PAGE"));
        for (text, remove_first) in [("NEW PAGE", false), ("RECREATED PAGE", true)] {
            let previous = owner(&app, "test:x").await;
            if remove_first {
                let _ = remove(State(app.clone()), Path("test:x".into()), Query(ByQ { by: Some("test".into()), jelly: None })).await.unwrap();
                assert!(!app.screens.lock().unwrap().contains_key("test:x"));
                assert_eq!(post(&app, snapshot(&previous, "OLD PAGE", 1)).await.unwrap_err().into_response().status(), StatusCode::NOT_FOUND);
            }
            create_page(&app, text, !remove_first).await;
            assert!(!app.screens.lock().unwrap().contains_key("test:x"));
            assert_eq!(post(&app, snapshot(&previous, "OLD PAGE", 1)).await.unwrap_err().into_response().status(), StatusCode::CONFLICT);
            let current = owner(&app, "test:x").await;
            assert_eq!(owner(&app, "test:x").await, current);
            for invalid in [old.as_str(), previous.as_str(), "", "unknown"] {
                assert_eq!(post(&app, snapshot(invalid, "OLD PAGE", 1)).await.unwrap_err().into_response().status(), StatusCode::CONFLICT);
            }
            assert!(app.screen("test:x", &current).unwrap().is_none());
            post(&app, snapshot(&current, text, 1)).await.unwrap();
            assert!(post(&app, snapshot(&previous, "OLD PAGE", 1)).await.is_err());
            let output = seen(&app).await;
            assert!(output.contains(text), "{output}");
            assert!(!output.contains("OLD PAGE"), "{output}");
            assert!(app.validate_see("test:x", &previous, 1).is_err());
        }
        assert!(serde_json::from_value::<Screen>(json!({"text": "tokenless", "width":1280, "height":900, "version":1})).is_err());
        let dir = app.store.lock().unwrap().home().to_path_buf();
        let (events, _) = broadcast::channel(32);
        let restarted = Arc::new(App { store: Mutex::new(Store::open(&dir).unwrap()), events, screens: Mutex::default() });
        let previous = owner(&app, "test:x").await;
        let current = owner(&restarted, "test:x").await;
        assert!(post(&restarted, snapshot(&previous, "OLD PAGE", 1)).await.is_err());
        post(&restarted, snapshot(&current, "RECREATED PAGE", 1)).await.unwrap();
    }

    #[tokio::test]
    async fn view_changes_invalidate_ownership_but_state_and_failed_validation_preserve_it() {
        let (_dir, app) = app();
        create_page(&app, "OLD PAGE", false).await;
        let first = owner(&app, "test:x").await;
        post(&app, snapshot(&first, "OLD PAGE", 1)).await.unwrap();
        assert!(app.change(|s| s.create("test:x", "bad".into(), "invalid JS!".into(), json!({}), "test", true, "", "")).is_err());
        assert!(app.change(|s| s.reshape("test:x", Some("bad".into()), Some("invalid JS!".into()), None, "test")).is_err());
        assert!(seen(&app).await.contains("OLD PAGE"));
        let _ = app.change(|s| s.set_state("test:x", "", Some(json!({"n":1})), "test")).unwrap();
        assert_eq!(owner(&app, "test:x").await, first);
        assert_eq!(app.screen("test:x", &first).unwrap().unwrap().version, 1);
        assert!(app.validate_see("test:x", &first, 1).is_err());
        let _ = app.change(|s| s.reshape("test:x", Some("<h1>NEW PAGE</h1>".into()), None, None, "test")).unwrap();
        assert!(post(&app, snapshot(&first, "OLD PAGE", 1)).await.is_err());
        let reshaped = owner(&app, "test:x").await;
        let _ = app.change(|s| s.undo("test:x", "test")).unwrap();
        assert!(post(&app, snapshot(&reshaped, "NEW PAGE", 3)).await.is_err());
        let undone = owner(&app, "test:x").await;
        let _ = app.change(|s| s.set_project("test:x", "moved", "test")).unwrap();
        assert!(!app.screens.lock().unwrap().contains_key("test:x"));
        assert!(post(&app, snapshot(&undone, "OLD PAGE", 4)).await.is_err());
        create_page(&app, "REUSED NAME", false).await;
        assert!(post(&app, snapshot(&undone, "OLD PAGE", 1)).await.is_err());
        let moved = owner(&app, "moved:x").await;
        assert_ne!(moved, undone);
    }

    #[tokio::test]
    async fn ask_rounds_invalidate_reshaped_views_and_anchor_checks_require_fresh_owned_screens() {
        let (_dir, app) = app();
        let spec = json!({"question":"Which?", "options":[{"id":"a", "label":"A"}, {"id":"b", "label":"B"}]});
        let _ = app.change_many(|s| s.ask_round("test:x", &spec, ASK_VIEW, ASK_VERBS, "", "test")).unwrap();
        let original = owner(&app, "test:x").await;
        let _ = app.change(|s| s.reshape("test:x", Some("different".into()), None, None, "test")).unwrap();
        let different = owner(&app, "test:x").await;
        let _ = app.change_many(|s| s.ask_round("test:x", &spec, ASK_VIEW, ASK_VERBS, "", "test")).unwrap();
        assert!(post(&app, snapshot(&different, "different", 1)).await.is_err());
        assert!(app.screen("test:x", &original).is_err());
        let current = owner(&app, "test:x").await;
        let version = app.store.lock().unwrap().get("test:x").unwrap().version;
        let asked = now_ms();
        let mut sc = snapshot(&current, "ASK PAGE", version);
        sc.missing.push("a2 #q-q1".into());
        post(&app, sc).await.unwrap();
        assert_eq!(anchor_place(&app, 1, "test:x", "a2", version, asked).await.unwrap().place, AnchorPlace::Missing);
        // The wait must ignore a screen from before the ask and an older state.
        app.screens.lock().unwrap().get_mut("test:x").unwrap().screen.as_mut().unwrap().at_ms = asked - 1;
        let checking = anchor_place(&app, 1, "test:x", "a2", version, asked);
        let update = async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let mut sc = snapshot(&current, "AFTER ASK", version);
            sc.below.push("a2 #q-q1".into());
            post(&app, sc).await.unwrap();
        };
        let (result, _) = tokio::join!(checking, update);
        assert_eq!(result.unwrap().place, AnchorPlace::Below);
        let checking = anchor_place(&app, 1, "test:x", "a2", version + 1, asked);
        let update = async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let mut sc = snapshot(&current, "NEW STATE", version + 1);
            sc.hidden.push("a2 #q-q1".into());
            post(&app, sc).await.unwrap();
        };
        let (result, _) = tokio::join!(checking, update);
        assert_eq!(result.unwrap().place, AnchorPlace::Hidden);
        let checking = anchor_place(&app, 1, "test:x", "a2", version + 2, now_ms());
        let replace = async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            create_page(&app, "REPLACED", true).await;
        };
        let (result, _) = tokio::join!(checking, replace);
        assert!(result.unwrap_err().contains("changed"));
    }

    #[tokio::test]
    async fn the_inbox_reads_back_to_the_cursor_and_keeps_the_person_after_a_trim() {
        let (_d, app) = app();
        let verbs = "const verbs = { label(s, { id }) { s[id] = 1; } };";
        let ask = |since: u64| inbox(State(app.clone()), Query(InboxQ { project: Some("p".into()), since: Some(since) }));
        let ids = |r: Result<Json<Value>, ApiError>| r.unwrap().0.as_array().unwrap().iter().map(|e| e["args"]["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let since = {
            let mut s = app.store.lock().unwrap();
            s.create("j", "".into(), verbs.into(), json!({}), "agent", false, "", "p").unwrap();
            s.call("p:j", "label", json!({"id": "first"}), PERSON).unwrap().at_ms
        };
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        {
            let mut s = app.store.lock().unwrap();
            s.call("p:j", "label", json!({"id": "second"}), PERSON).unwrap();
            for i in 0..200 {
                s.call("p:j", "label", json!({"id": format!("agent{i}")}), "agent").unwrap();
            }
        }
        assert_eq!(ids(ask(since).await), ["second"]);
        assert_eq!(ids(ask(0).await), ["first", "second"]);
        app.store.lock().unwrap().trim_events(2_000, 1 << 20).unwrap();
        assert_eq!(ids(ask(0).await), ["first", "second"], "the person's events outlive a trim");
    }

    #[tokio::test]
    async fn timestamp_inbox_keeps_events_across_a_backward_clock_step() {
        let (dir, app) = app();
        let (recent, later) = {
            let mut store = app.store.lock().unwrap();
            store.create("clock", "".into(), "const verbs = { tick(s) { s.n = 1; } };".into(), json!({}), "agent", false, "", "test").unwrap();
            let mut recent = store.call("test:clock", "tick", json!({}), PERSON).unwrap();
            recent.state = None;
            let mut later = recent.clone();
            later.seq += 1;
            later.at_ms -= 1;
            let mut older = later.clone();
            older.seq += 1;
            older.at_ms = 1;
            std::fs::write(store.events_path(), format!("{}\n{}\n{}\n", serde_json::to_string(&recent).unwrap(), serde_json::to_string(&later).unwrap(), serde_json::to_string(&older).unwrap())).unwrap();
            (recent, later)
        };
        *app.store.lock().unwrap() = Store::open(dir.path()).unwrap();
        let path = app.store.lock().unwrap().events_path();
        let mut bytes = std::fs::read(&path).unwrap();
        let old_start = bytes[..bytes.len() - 1].iter().rposition(|&b| b == b'\n').unwrap() + 1;
        bytes[old_start] = 0xff;
        std::fs::write(&path, bytes).unwrap();
        let empty = inbox(State(app.clone()), Query(InboxQ { project: None, since: Some(recent.at_ms) })).await.unwrap();
        assert!(empty.0.as_array().unwrap().is_empty());
        let result = inbox(State(app.clone()), Query(InboxQ { project: Some("test".into()), since: Some(recent.at_ms - 2) })).await.unwrap();
        assert_eq!(result.0.as_array().unwrap().iter().map(|e| e["seq"].as_u64().unwrap()).collect::<Vec<_>>(), [recent.seq, later.seq]);
        let mut shifted = std::fs::read(&path).unwrap();
        shifted.insert(0, b'\n');
        std::fs::write(&path, shifted).unwrap();
        assert!(inbox(State(app.clone()), Query(InboxQ { project: None, since: Some(recent.at_ms - 2) })).await.is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(inbox(State(app.clone()), Query(InboxQ { project: None, since: Some(u64::MAX) })).await.is_err());
        let reader = crate::store::InboxReader::Named { name: "unreadable".into() };
        assert!(app.store.lock().unwrap().read_inbox("test", &reader).is_err());
    }

    #[test]
    fn see_history_does_not_read_unrelated_retained_events() {
        let (dir, app) = app();
        let mut store = app.store.lock().unwrap();
        for name in ["quiet", "busy"] {
            store.create(name, "".into(), "const verbs = {};".into(), json!({}), "agent", false, "", "test").unwrap();
        }
        for i in 0..10 {
            store.set_state("test:quiet", "/n", Some(json!(i)), PERSON).unwrap();
        }
        let last = store.set_state("test:quiet", "/n", Some(json!(10)), "agent").unwrap();
        for i in 0..200 {
            store.set_state("test:busy", "/n", Some(json!(i)), "agent").unwrap();
        }
        let path = store.events_path();
        drop(store);
        // Removing the backing log makes any per-see log read observable.
        let saved = std::fs::read(&path).unwrap();
        for restarted in [false, true] {
            if restarted {
                *app.store.lock().unwrap() = Store::open(dir.path()).unwrap();
            }
            std::fs::remove_file(&path).unwrap();
            let (event, person) = app.store.lock().unwrap().recent_events("test:quiet");
            assert_eq!(event.unwrap().seq, last.seq);
            assert_eq!(person.iter().map(|e| e.version).collect::<Vec<_>>(), (4..12).collect::<Vec<_>>());
            assert!(app.store.lock().unwrap().recent_events("test:busy").1.is_empty());
            assert!(app.store.lock().unwrap().recent_events("test:missing").0.is_none());
            std::fs::write(&path, &saved).unwrap();
        }
        let mut store = app.store.lock().unwrap();
        store.trim_events(1, 0).unwrap();
        assert!(store.recent_events("test:quiet").0.is_none());
        assert!(store.recent_events("test:quiet").1.is_empty());
        assert!(store.recent_events("test:busy").0.is_some());
        assert!(store.recent_events("test:busy").0.unwrap().state.is_none());
    }

    #[tokio::test]
    async fn see_names_who_made_the_current_version_even_after_a_trim() {
        let (_d, app) = app();
        let verbs = "const verbs = { label(s, { id }) { s[id] = 1; } };";
        {
            let mut s = app.store.lock().unwrap();
            for name in ["x", "y"] {
                s.create(name, "".into(), verbs.into(), json!({}), "agent", false, "", "test").unwrap();
            }
            s.call("test:x", "label", json!({"id": "mine"}), PERSON).unwrap();
            s.call("test:x", "label", json!({"id": "theirs"}), "agent").unwrap();
            for i in 0..100 {
                s.call("test:y", "label", json!({"id": format!("n{i}")}), "agent").unwrap();
            }
        }
        let version = app.store.lock().unwrap().get("test:x").unwrap().version;
        let current = owner(&app, "test:x").await;
        post(&app, snapshot(&current, "PAGE", version)).await.unwrap();
        let see_x = || async {
            let response = see_with_renderer(app.clone(), "test:x".into(), SeeQ { width: None, height: None, png: false, full: false },
                |_, _, _| async { Err("no browser".to_string()) }).await.unwrap();
            String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap()
        };
        let header = |out: &str| out.lines().next().unwrap().to_string();
        let path = app.store.lock().unwrap().events_path();
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let out = see_x().await;
        std::fs::write(&path, saved).unwrap();
        assert!(header(&out).ends_with("last change just now by agent"), "{out}");
        assert!(out.contains(r#"person   just now label {"id":"mine"}"#), "{out}");
        app.store.lock().unwrap().trim_events(2_000, 1 << 20).unwrap();
        let out = see_x().await;
        assert!(header(&out).ends_with("last change just now"), "the agent's change was trimmed, so no author: {out}");
        assert!(out.contains(r#"person   just now label {"id":"mine"}"#), "{out}");
    }
}
