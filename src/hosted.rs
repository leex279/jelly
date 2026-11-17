//! A hosted instance: one complete daemon per person under `<home>/people/<name>/`,
//! mounted at `/people/<name>/`. Reads need no key; every write needs the owner's.
//! Keys are issued on the host with `jelly keys` and kept hashed in `keys.json`.

use crate::server::{self, App, Shared};
use crate::store::{self, Store};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tower::ServiceExt;

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct KeyRecord {
    pub name: String,
    pub sha256: String,
    pub created_ms: u64,
}

pub(crate) fn keys_path(home: &FsPath) -> PathBuf {
    home.join("keys.json")
}

fn read_keys(home: &FsPath) -> Result<Vec<KeyRecord>, String> {
    let path = keys_path(home);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn write_keys(home: &FsPath, keys: &[KeyRecord]) -> Result<(), String> {
    std::fs::create_dir_all(home).map_err(|e| format!("{}: {e}", home.display()))?;
    let bytes = serde_json::to_vec_pretty(keys).expect("keys serialize");
    store::write_atomic(&keys_path(home), &bytes).map_err(|e| format!("{}: {e}", keys_path(home).display()))
}

fn digest(key: &str) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(key.as_bytes()))
}

/// Issues a key for `name` and returns it once; the file keeps only its hash.
pub(crate) fn issue(home: &FsPath, name: &str) -> Result<String, String> {
    if name.contains(':') {
        return Err(format!("bad name {name:?}: a person's name has no ':'"));
    }
    store::valid_name(name).map_err(|e| e.to_string())?;
    let mut keys = read_keys(home)?;
    if keys.iter().any(|k| k.name == name) {
        return Err(format!("{name} already has a key; jelly keys revoke {name} first"));
    }
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("could not make a key: {e}"))?;
    let key = format!("jk_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes));
    let created_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    keys.push(KeyRecord { name: name.into(), sha256: digest(&key), created_ms });
    write_keys(home, &keys)?;
    Ok(key)
}

pub(crate) fn revoke(home: &FsPath, name: &str) -> Result<(), String> {
    let mut keys = read_keys(home)?;
    let before = keys.len();
    keys.retain(|k| k.name != name);
    if keys.len() == before {
        return Err(format!("{name} has no key"));
    }
    write_keys(home, &keys)
}

pub(crate) fn list(home: &FsPath) -> Result<Vec<KeyRecord>, String> {
    read_keys(home)
}

/// The person a presented key belongs to. Reads the file every time, so a
/// revoked key stops working at once.
fn person_for(home: &FsPath, bearer: Option<&str>) -> Option<String> {
    let hash = digest(bearer?);
    read_keys(home).ok()?.into_iter().find(|k| k.sha256 == hash).map(|k| k.name)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim).filter(|k| !k.is_empty())
}

#[derive(Clone)]
struct Person {
    app: Shared,
    router: Router,
}

struct Hosted {
    home: PathBuf,
    people: Mutex<BTreeMap<String, Person>>,
}

impl Hosted {
    fn open_person(&self, name: &str) -> std::io::Result<Person> {
        let mut store = Store::open(&self.home.join("people").join(name))?;
        server::boot_store(&mut store)?;
        let (events, _) = broadcast::channel(1024);
        let app: Shared = Arc::new(App::new(store, events, &format!("/people/{name}"), name, None));
        Ok(Person { router: server::router(app.clone()), app })
    }

    /// A person's daemon. `create` opens a store for a person who has none yet;
    /// only an authenticated write by that person passes it.
    fn person(&self, name: &str, create: bool) -> std::io::Result<Option<Person>> {
        let mut people = self.people.lock().expect("people lock");
        if let Some(person) = people.get(name) {
            return Ok(Some(person.clone()));
        }
        if !create {
            return Ok(None);
        }
        let person = self.open_person(name)?;
        people.insert(name.into(), person.clone());
        Ok(Some(person))
    }

    fn people(&self) -> Vec<(String, Shared)> {
        self.people.lock().expect("people lock").iter().map(|(name, p)| (name.clone(), p.app.clone())).collect()
    }
}

fn error(status: StatusCode, message: String) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

async fn dispatch(State(hosted): State<Arc<Hosted>>, Path(params): Path<HashMap<String, String>>, req: Request) -> Response {
    let owner = params.get("owner").cloned().unwrap_or_default();
    if owner.contains(':') || store::valid_name(&owner).is_err() {
        return error(StatusCode::NOT_FOUND, format!("no jellies at /people/{owner}"));
    }
    let write = !matches!(*req.method(), Method::GET | Method::HEAD);
    if write {
        match person_for(&hosted.home, bearer(req.headers())) {
            None => return error(StatusCode::UNAUTHORIZED, "this hosted jelly needs a key for changes: jelly remote add URL KEY, then push from your own daemon".into()),
            Some(name) if name != owner => return error(StatusCode::FORBIDDEN, format!("/people/{owner} belongs to {owner}; this key is {name}'s")),
            Some(_) => {}
        }
    }
    let person = match hosted.person(&owner, write) {
        Ok(Some(person)) => person,
        Ok(None) => return error(StatusCode::NOT_FOUND, format!("no jellies at /people/{owner}")),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("could not open {owner}'s jellies: {e}")),
    };
    // The person's router sees the path under its mount, raw, so encoded names survive.
    let prefix = format!("/people/{owner}");
    let Some(rest) = req.uri().path().strip_prefix(prefix.as_str()) else {
        return error(StatusCode::NOT_FOUND, format!("no jellies at /people/{owner}"));
    };
    let path = if rest.is_empty() { "/" } else { rest };
    let uri: Uri = match format!("{path}{}", req.uri().query().map(|q| format!("?{q}")).unwrap_or_default()).parse() {
        Ok(uri) => uri,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("bad path: {e}")),
    };
    // The outer match leaves its captures in the extensions and the inner router adds
    // its own, so the inner `Path` would see both. Forward a bare request that keeps
    // only what the connection needs: the upgrade handle a WebSocket switches on.
    let (mut parts, body) = req.into_parts();
    let upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
    parts.extensions.clear();
    if let Some(upgrade) = upgrade {
        parts.extensions.insert(upgrade);
    }
    parts.uri = uri;
    person.router.oneshot(Request::from_parts(parts, body)).await.into_response()
}

fn listing_with_owner(name: &str, app: &App) -> Vec<Value> {
    let store = app.store.lock().expect("store lock");
    server::listing(&store).into_iter().map(|mut item| {
        let id = item["name"].as_str().unwrap_or("").to_string();
        item["owner"] = json!(name);
        item["url"] = json!(format!("/people/{name}/j/{id}/"));
        item
    }).collect()
}

async fn all_jellies(State(hosted): State<Arc<Hosted>>) -> Json<Value> {
    Json(json!(hosted.people().iter().flat_map(|(name, app)| listing_with_owner(name, app)).collect::<Vec<_>>()))
}

async fn whoami(State(hosted): State<Arc<Hosted>>, headers: HeaderMap) -> Response {
    match person_for(&hosted.home, bearer(&headers)) {
        Some(name) => Json(json!({"name": name})).into_response(),
        None => error(StatusCode::UNAUTHORIZED, "this key is not known here".into()),
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn index(State(hosted): State<Arc<Hosted>>) -> Html<String> {
    let mut body = String::from("<!doctype html><meta charset=\"utf-8\"><title>jelly</title>\
        <style>body{font:15px/1.5 system-ui;max-width:720px;margin:40px auto;padding:0 16px;color:#222}h1{font-size:22px}h2{font-size:16px;margin-top:28px}\
        li{margin:6px 0}small{color:#777}a{color:#d81b60}</style><h1>jelly</h1>");
    let people = hosted.people();
    if people.is_empty() {
        body.push_str("<p>Nobody has pushed a jelly here yet.</p>");
    }
    for (name, app) in &people {
        body.push_str(&format!("<h2><a href=\"/people/{0}/\">{0}</a>'s jellies</h2><ul>", escape(name)));
        let items = listing_with_owner(name, app);
        if items.is_empty() {
            body.push_str("<li><small>nothing yet</small></li>");
        }
        for item in items {
            let id = item["name"].as_str().unwrap_or("");
            let about = item["about"].as_str().unwrap_or("");
            body.push_str(&format!("<li><a href=\"/people/{0}/j/{1}/\">{1}</a> {2} <small>by {0}</small></li>",
                escape(name), escape(id), escape(about)));
        }
        body.push_str("</ul>");
    }
    Html(body)
}

/// Runs the hosted instance. Every existing person opens at start, so a broken
/// store refuses startup loudly, as the local daemon does.
pub(crate) async fn serve_hosted(home: PathBuf, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    let hosted = Arc::new(Hosted { home, people: Mutex::default() });
    match std::fs::read_dir(hosted.home.join("people")) {
        Ok(entries) => for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(String::from) else { continue };
            if entry.file_type()?.is_dir() && store::valid_name(&name).is_ok() {
                hosted.person(&name, true)?;
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let router = Router::new()
        .route("/", get(index))
        .route("/api/jellies", get(all_jellies))
        .route("/api/whoami", get(whoami))
        .route("/people/{owner}", any(dispatch))
        .route("/people/{owner}/", any(dispatch))
        .route("/people/{owner}/{*rest}", any(dispatch))
        .with_state(hosted);
    axum::serve(listener, router).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_issued_once_stored_hashed_and_revocable() {
        let dir = tempfile::tempdir().unwrap();
        let key = issue(dir.path(), "alice").unwrap();
        assert!(key.starts_with("jk_") && key.len() > 40, "{key}");
        let file = std::fs::read_to_string(keys_path(dir.path())).unwrap();
        assert!(!file.contains(&key), "the key must not be stored in clear");
        let records = list(dir.path()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "alice");
        assert_eq!(records[0].sha256, digest(&key));
        assert_eq!(person_for(dir.path(), Some(&key)), Some("alice".into()));
        assert_eq!(person_for(dir.path(), Some("jk_wrong")), None);
        assert_eq!(person_for(dir.path(), None), None);

        assert!(issue(dir.path(), "alice").unwrap_err().contains("already has a key"));
        assert!(issue(dir.path(), "p:alice").unwrap_err().contains("no ':'"));
        assert!(issue(dir.path(), "Alice").is_err());

        revoke(dir.path(), "alice").unwrap();
        assert_eq!(person_for(dir.path(), Some(&key)), None);
        assert!(revoke(dir.path(), "alice").unwrap_err().contains("has no key"));
    }

    #[test]
    fn bearer_header_is_read_strictly() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert("authorization", "Bearer jk_x".parse().unwrap());
        assert_eq!(bearer(&headers), Some("jk_x"));
        headers.insert("authorization", "Basic abc".parse().unwrap());
        assert_eq!(bearer(&headers), None);
        headers.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(bearer(&headers), None);
    }
}
