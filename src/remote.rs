//! Sharing a jelly to a hosted instance. A link ties a local jelly to its copy on
//! a remote; the forwarder brings that copy to the local jelly's current content
//! after every change and catches up once an unreachable remote is back. The
//! local jelly is the truth: nothing flows back.
//!
//! Links (`links.json`) and remotes (`remotes.json`) live beside the jellies,
//! outside state and undo. The forwarder is the only network user here, and it
//! never holds the store lock across a request.

use crate::server::App;
use crate::store::{self, Event, EventKind, Jelly, PAGE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

const TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(10);
pub(crate) const NOT_OUTWARD: &str = "this daemon does not push outward";

#[derive(Serialize, Deserialize, Clone)]
struct Remote {
    key: String,
    owner: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Pending {
    dirty: bool,
    assets: Vec<String>,
    by: String,
    /// Bumps on every mark, so a reconcile clears only what it saw.
    #[serde(skip)]
    generation: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct Link {
    remote: String,
    owner: String,
    /// The id the remote copy has; differs from the local id after a move until it is replayed.
    remote_id: String,
    #[serde(default)]
    pending: Pending,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    removed: bool,
}

impl Link {
    fn has_work(&self, id: &str) -> bool {
        self.removed || self.pending.dirty || !self.pending.assets.is_empty() || self.remote_id != id
    }

    fn mark(&mut self, by: &str) {
        self.pending.by = by.into();
        self.pending.generation += 1;
    }
}

type Remotes = BTreeMap<String, Remote>;
type Links = BTreeMap<String, Link>;

pub(crate) struct Syncer {
    home: PathBuf,
    remotes: Mutex<Remotes>,
    links: Mutex<Links>,
    notify: Notify,
}

fn read_json<T: Default + serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e),
    }
}

fn save_links(home: &Path, links: &Links) {
    let bytes = serde_json::to_vec_pretty(links).expect("links serialize");
    if let Err(e) = store::write_atomic(&home.join("links.json"), &bytes) {
        eprintln!("jelly: could not save links.json: {e}");
    }
}

/// The remotes file holds keys in clear, so only the owner may read it.
fn save_remotes(home: &Path, remotes: &Remotes) -> Result<(), String> {
    let path = home.join("remotes.json");
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(remotes).expect("remotes serialize");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&tmp).and_then(|mut f| std::io::Write::write_all(&mut f, &bytes))
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("could not save {}: {e}", path.display()))
}

fn normalize(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

impl Syncer {
    pub(crate) fn open(home: &Path) -> std::io::Result<Syncer> {
        Ok(Syncer {
            home: home.to_path_buf(),
            remotes: Mutex::new(read_json(&home.join("remotes.json"))?),
            links: Mutex::new(read_json(&home.join("links.json"))?),
            notify: Notify::new(),
        })
    }

    /// Marks what each committed event changed on its link. Runs under the
    /// store lock, so it only writes the small links file and takes no other lock.
    pub(crate) fn observe(&self, events: &[Event]) {
        let mut links = self.links.lock().expect("links lock");
        let mut changed = false;
        for ev in events {
            let key = match ev.kind {
                EventKind::Move => ev.args.as_ref().and_then(|a| a["from"].as_str()).map(String::from),
                _ => Some(ev.jelly.clone()),
            };
            let Some(key) = key.filter(|k| links.contains_key(k)) else { continue };
            match ev.kind {
                EventKind::New | EventKind::Call | EventKind::Set | EventKind::Undo | EventKind::Reshape => {
                    links.get_mut(&key).expect("checked").pending.dirty = true;
                }
                EventKind::Asset => {
                    if let Some(file) = ev.args.as_ref().and_then(|a| a["file"].as_str()) {
                        let pending = &mut links.get_mut(&key).expect("checked").pending;
                        if !pending.assets.iter().any(|f| f == file) { pending.assets.push(file.into()); }
                    }
                }
                EventKind::Move => {
                    let mut link = links.remove(&key).expect("checked");
                    link.pending.dirty = true;
                    links.insert(ev.jelly.clone(), link);
                }
                EventKind::Remove => links.get_mut(&key).expect("checked").removed = true,
                _ => continue,
            }
            let marked = if ev.kind == EventKind::Move { &ev.jelly } else { &key };
            links.get_mut(marked).expect("just placed").mark(&ev.by);
            changed = true;
        }
        if changed {
            save_links(&self.home, &links);
            self.notify.notify_one();
        }
    }

    /// What `jelly ls` shows for a linked jelly; `None` when it is not shared.
    pub(crate) fn link_status(&self, id: &str) -> Option<Value> {
        let links = self.links.lock().expect("links lock");
        let link = links.get(id)?;
        Some(json!({
            "remote": link.remote, "owner": link.owner, "url": remote_url(link, id),
            "synced": !link.has_work(id) && link.error.is_none(), "error": link.error,
        }))
    }

    fn mark_all_dirty(&self) {
        let mut links = self.links.lock().expect("links lock");
        for link in links.values_mut() {
            link.pending.dirty = true;
            link.pending.generation += 1;
        }
        if !links.is_empty() { save_links(&self.home, &links); }
    }

    fn pick_remote(&self, to: Option<String>) -> Result<(String, String), String> {
        let remotes = self.remotes.lock().expect("remotes lock");
        match to {
            Some(url) => {
                let url = normalize(&url);
                let remote = remotes.get(&url).ok_or_else(|| format!("no key for {url}: jelly remote add {url} KEY"))?;
                Ok((url, remote.owner.clone()))
            }
            None => match remotes.len() {
                0 => Err("no remote configured: jelly remote add URL KEY".into()),
                1 => { let (url, remote) = remotes.iter().next().expect("one"); Ok((url.clone(), remote.owner.clone())) }
                _ => Err(format!("say which remote with --to: {}", remotes.keys().cloned().collect::<Vec<_>>().join(", "))),
            },
        }
    }

    pub(crate) fn remotes(&self) -> Value {
        let remotes = self.remotes.lock().expect("remotes lock");
        json!(remotes.iter().map(|(url, r)| json!({"url": url, "owner": r.owner})).collect::<Vec<_>>())
    }
}

fn remote_url(link: &Link, id: &str) -> String {
    format!("{}/people/{}/j/{}/", link.remote, link.owner, id)
}

fn describe_status(remote: &str, code: u16, resp: ureq::Response) -> String {
    resp.into_json::<Value>().ok()
        .and_then(|v| v["error"].as_str().map(String::from))
        .unwrap_or_else(|| format!("{remote} answered {code}"))
}

/// A remote's answer: a failed status is an answer too, so callers can read 404 and 409.
enum Answer {
    Ok(ureq::Response),
    Status(u16, ureq::Response),
}

enum Outcome {
    Synced,
    Dropped,
}

struct RemoteApi<'a> {
    remote: &'a str,
    api: String,
    key: &'a str,
}

impl RemoteApi<'_> {
    fn request(&self, method: &str, path: &str) -> ureq::Request {
        ureq::request(method, &format!("{}{path}", self.api))
            .set("authorization", &format!("Bearer {}", self.key))
            .timeout(TIMEOUT)
    }

    fn send(&self, method: &str, path: &str, body: Value) -> Result<Answer, String> {
        let req = self.request(method, path);
        match if body.is_null() { req.call() } else { req.send_json(body) } {
            Ok(r) => Ok(Answer::Ok(r)),
            Err(ureq::Error::Status(code, r)) => Ok(Answer::Status(code, r)),
            Err(ureq::Error::Transport(t)) => Err(format!("cannot reach {} ({t})", self.remote)),
        }
    }

    /// A request that must succeed.
    fn must(&self, method: &str, path: &str, body: Value) -> Result<ureq::Response, String> {
        match self.send(method, path, body)? {
            Answer::Ok(r) => Ok(r),
            Answer::Status(code, r) => Err(describe_status(self.remote, code, r)),
        }
    }

    /// Like `must`, but a 404 is an answer, not a failure: `None`.
    fn send_or_missing(&self, method: &str, path: &str, body: Value) -> Result<Option<ureq::Response>, String> {
        match self.send(method, path, body)? {
            Answer::Ok(r) => Ok(Some(r)),
            Answer::Status(404, _) => Ok(None),
            Answer::Status(code, r) => Err(describe_status(self.remote, code, r)),
        }
    }
}

/// Brings the remote copy to `jelly`'s content. `None` for the jelly means it is
/// gone locally, so the copy goes too.
fn sync_once(link: &Link, key: &str, id: &str, jelly: Option<Jelly>, files: &[String], dir: &Path) -> Result<Outcome, String> {
    let api = RemoteApi { remote: &link.remote, api: format!("{}/people/{}/api/jellies", link.remote, link.owner), key };
    let by = match (&link.pending.by, &jelly) {
        (by, _) if !by.is_empty() => by.clone(),
        (_, Some(j)) => j.made_by.clone(),
        _ => "agent".into(),
    };
    let by_query = format!("?by={}", crate::encode(&by));
    let Some(jelly) = jelly.filter(|_| !link.removed) else {
        api.send_or_missing("DELETE", &format!("/{}{by_query}", link.remote_id), Value::Null)?;
        return Ok(Outcome::Dropped);
    };
    if link.remote_id != id {
        let project = id.split_once(':').map_or("", |(p, _)| p);
        match api.send("POST", &format!("/{}/move", link.remote_id), json!({"project": project, "by": by}))? {
            Answer::Ok(_) | Answer::Status(404, _) => {}
            // The target name is taken on the remote: the old copy goes and the new one is created below.
            Answer::Status(409, _) => { api.send_or_missing("DELETE", &format!("/{}{by_query}", link.remote_id), Value::Null)?; }
            Answer::Status(code, r) => return Err(describe_status(&link.remote, code, r)),
        }
    }
    let remote: Option<Value> = api.send_or_missing("GET", &format!("/{id}"), Value::Null)?
        .map(|r| r.into_json()).transpose().map_err(|e| e.to_string())?;
    let present: Vec<String> = match remote {
        None => {
            api.must("POST", "", json!({"name": id, "view": jelly.view, "verbs": jelly.verbs, "state": jelly.state,
                "by": by, "replace": true, "about": jelly.about, "project": ""}))?;
            Vec::new()
        }
        Some(remote) => {
            let differs = |key: &str, local: &str| remote[key].as_str() != Some(local);
            let view = differs("view", &jelly.view).then(|| jelly.view.clone());
            let verbs = differs("verbs", &jelly.verbs).then(|| jelly.verbs.clone());
            let about = differs("about", &jelly.about).then(|| jelly.about.clone());
            if view.is_some() || verbs.is_some() || about.is_some() {
                api.must("POST", &format!("/{id}/reshape"), json!({"view": view, "verbs": verbs, "about": about, "by": by}))?;
            }
            if remote["state"] != jelly.state {
                api.must("POST", &format!("/{id}/set"), json!({"pointer": "", "value": jelly.state, "by": by}))?;
            }
            api.must("GET", &format!("/{id}/assets"), Value::Null)?.into_json().map_err(|e| e.to_string())?
        }
    };
    for file in files {
        if present.contains(file) && !link.pending.assets.contains(file) { continue; }
        let bytes = match std::fs::read(dir.join(file)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("could not read asset {file}: {e}")),
        };
        match api.request("POST", &format!("/{id}/assets/{}{by_query}", crate::encode(file)))
            .set("content-type", "application/octet-stream").send_bytes(&bytes) {
            Ok(_) => {}
            Err(ureq::Error::Status(code, r)) => return Err(describe_status(&link.remote, code, r)),
            Err(ureq::Error::Transport(t)) => return Err(format!("cannot reach {} ({t})", link.remote)),
        }
    }
    Ok(Outcome::Synced)
}

/// Syncs one link now, blocking. Reads the store and the link, releases every
/// lock, talks to the remote, then records the outcome on the link.
fn reconcile(syncer: &Syncer, app: &App, id: &str) -> Result<(), String> {
    let (jelly, files, dir) = {
        let store = app.store.lock().expect("store lock");
        (store.get(id).ok().cloned(), store.assets(id), store.asset_dir(id))
    };
    let Some(link) = syncer.links.lock().expect("links lock").get(id).cloned() else { return Ok(()) };
    let key = syncer.remotes.lock().expect("remotes lock").get(&link.remote).map(|r| r.key.clone());
    let result = match key {
        Some(key) => sync_once(&link, &key, id, jelly, &files, &dir),
        None => Err(format!("no key for {}: jelly remote add {} KEY", link.remote, link.remote)),
    };
    let mut links = syncer.links.lock().expect("links lock");
    match &result {
        Ok(Outcome::Dropped) => { links.remove(id); }
        Ok(Outcome::Synced) => if let Some(current) = links.get_mut(id) {
            current.remote_id = id.into();
            current.error = None;
            if current.pending.generation == link.pending.generation {
                current.pending = Pending { generation: link.pending.generation, ..Pending::default() };
            }
        },
        Err(e) => if let Some(current) = links.get_mut(id) { current.error = Some(e.clone()) },
    }
    save_links(&syncer.home, &links);
    result.map(|_| ())
}

/// The forwarder: syncs every link with pending work, one at a time, retrying a
/// failing one with backoff. Every link starts dirty, so a daemon that was down
/// catches its remotes up.
pub(crate) async fn run(app: crate::server::Shared) {
    let Some(syncer) = app.sync.as_ref() else { return };
    syncer.mark_all_dirty();
    let mut retries: BTreeMap<String, (Instant, Duration)> = BTreeMap::new();
    loop {
        let now = Instant::now();
        let due: Vec<String> = {
            let links = syncer.links.lock().expect("links lock");
            retries.retain(|id, _| links.contains_key(id));
            links.iter().filter(|(id, link)| link.has_work(id) && retries.get(*id).is_none_or(|(at, _)| *at <= now))
                .map(|(id, _)| id.clone()).collect()
        };
        for id in due {
            let (app, target) = (app.clone(), id.clone());
            let result = tokio::task::spawn_blocking(move || reconcile(app.sync.as_ref().expect("forwarder runs with a syncer"), &app, &target))
                .await.unwrap_or_else(|e| Err(e.to_string()));
            match result {
                Ok(()) => { retries.remove(&id); }
                Err(_) => {
                    let delay = retries.get(&id).map_or(RETRY_MIN, |(_, d)| (*d * 2).min(RETRY_MAX));
                    retries.insert(id, (Instant::now() + delay, delay));
                }
            }
        }
        let next = {
            let links = syncer.links.lock().expect("links lock");
            retries.iter().filter(|(id, _)| links.get(*id).is_some_and(|l| l.has_work(id))).map(|(_, (at, _))| *at).min()
        };
        match next {
            Some(at) => tokio::select! { _ = syncer.notify.notified() => {}, _ = tokio::time::sleep_until(at) => {} },
            None => syncer.notify.notified().await,
        }
    }
}

/// Links `id` to a remote and syncs it now. A link that fails on its first push is not kept.
pub(crate) fn push(app: &App, id: &str, to: Option<String>, by: &str) -> Result<Value, String> {
    let syncer = app.sync.as_ref().ok_or(NOT_OUTWARD)?;
    if id == PAGE {
        return Err("the jelly page stays here; push one of your jellies".into());
    }
    let version = app.store.lock().expect("store lock").get(id).map_err(|e| e.to_string())?.version;
    let (remote, owner) = syncer.pick_remote(to)?;
    let is_new = {
        let mut links = syncer.links.lock().expect("links lock");
        let is_new = !links.contains_key(id);
        let link = links.entry(id.into()).or_insert_with(|| Link {
            remote: remote.clone(), owner: owner.clone(), remote_id: id.into(), pending: Pending::default(), error: None, removed: false,
        });
        if link.remote != remote {
            return Err(format!("{id} is already shared at {}; jelly unshare {id} first", link.remote));
        }
        link.pending.dirty = true;
        link.mark(by);
        save_links(&syncer.home, &links);
        is_new
    };
    match reconcile(syncer, app, id) {
        Ok(()) => {
            let url = syncer.link_status(id).map(|s| s["url"].as_str().unwrap_or("").to_string()).unwrap_or_default();
            Ok(json!({"jelly": id, "remote": remote, "owner": owner, "url": url, "version": version}))
        }
        Err(e) => {
            if is_new {
                let mut links = syncer.links.lock().expect("links lock");
                links.remove(id);
                save_links(&syncer.home, &links);
            }
            Err(e)
        }
    }
}

/// Drops the link; with `remove`, deletes the remote copy first and keeps the
/// link when that fails, so the person can retry.
pub(crate) fn unshare(app: &App, id: &str, remove: bool, by: &str) -> Result<Value, String> {
    let syncer = app.sync.as_ref().ok_or(NOT_OUTWARD)?;
    let link = syncer.links.lock().expect("links lock").get(id).cloned().ok_or_else(|| format!("{id} is not shared"))?;
    if remove {
        let key = syncer.remotes.lock().expect("remotes lock").get(&link.remote).map(|r| r.key.clone())
            .ok_or_else(|| format!("no key for {}: jelly remote add {} KEY", link.remote, link.remote))?;
        let api = RemoteApi { remote: &link.remote, api: format!("{}/people/{}/api/jellies", link.remote, link.owner), key: &key };
        api.send_or_missing("DELETE", &format!("/{}?by={}", link.remote_id, crate::encode(by)), Value::Null)?;
    }
    let mut links = syncer.links.lock().expect("links lock");
    links.remove(id);
    save_links(&syncer.home, &links);
    Ok(json!({"jelly": id, "remote": link.remote, "removed": remove}))
}

/// Checks the key against the remote and remembers it with the person it names.
pub(crate) fn add_remote(app: &App, url: &str, key: &str) -> Result<Value, String> {
    let syncer = app.sync.as_ref().ok_or(NOT_OUTWARD)?;
    let url = normalize(url);
    if url.is_empty() { return Err("give the remote's URL".into()); }
    let who: Value = match ureq::get(&format!("{url}/api/whoami")).set("authorization", &format!("Bearer {key}")).timeout(TIMEOUT).call() {
        Ok(r) => r.into_json().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(code, r)) => return Err(describe_status(&url, code, r)),
        Err(ureq::Error::Transport(t)) => return Err(format!("cannot reach {url} ({t})")),
    };
    let owner = who["name"].as_str().filter(|n| !n.is_empty()).ok_or_else(|| format!("{url} did not say whose key this is"))?.to_string();
    let mut remotes = syncer.remotes.lock().expect("remotes lock");
    remotes.insert(url.clone(), Remote { key: key.into(), owner: owner.clone() });
    save_remotes(&syncer.home, &remotes)?;
    Ok(json!({"url": url, "owner": owner}))
}

pub(crate) fn remove_remote(app: &App, url: &str) -> Result<Value, String> {
    let syncer = app.sync.as_ref().ok_or(NOT_OUTWARD)?;
    let url = normalize(url);
    let linked: Vec<String> = syncer.links.lock().expect("links lock").iter()
        .filter(|(_, l)| l.remote == url).map(|(id, _)| id.clone()).collect();
    if !linked.is_empty() {
        return Err(format!("{url} still has shared jellies: jelly unshare {} first", linked.join(", ")));
    }
    let mut remotes = syncer.remotes.lock().expect("remotes lock");
    remotes.remove(&url).ok_or_else(|| format!("{url} is not a remote"))?;
    save_remotes(&syncer.home, &remotes)?;
    Ok(json!({"url": url}))
}

pub(crate) fn remotes(app: &App) -> Result<Value, String> {
    Ok(app.sync.as_ref().ok_or(NOT_OUTWARD)?.remotes())
}
