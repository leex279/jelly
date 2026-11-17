//! Sharing a jelly to a hosted instance. A link ties a local jelly to its copy on
//! a remote; the forwarder brings that copy to the local jelly's current content
//! after every change and catches up once an unreachable remote is back. The
//! local jelly is the truth: nothing flows back.
//!
//! Links (`links.json`) and remotes (`remotes.json`) live beside the jellies,
//! outside state and undo. The forwarder is the only network user here, and it
//! never holds the store lock across a request.

use crate::hosted::person_path;
use crate::server::{App, CreateReq, ProjectReq, ReshapeReq, SetReq};
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

/// Reads a JSON file; a missing file is the empty value.
pub(crate) fn read_json<T: Default + serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    }
}

fn save_links(home: &Path, links: &Links) -> Result<(), String> {
    let path = home.join("links.json");
    let bytes = serde_json::to_vec_pretty(links).expect("links serialize");
    store::write_atomic(&path, &bytes).map_err(|e| format!("could not save {}: {e}", path.display()))
}

/// A state the remote can hold: it stores `{}` for a `null` state, so the local
/// copy is compared and sent that way.
fn remote_state(state: &Value) -> Value {
    if state.is_null() { json!({}) } else { state.clone() }
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
        let mut marked: Vec<String> = Vec::new();
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
                // Kept local: they are the owner's conversation with their agent, not the jelly.
                EventKind::Pin | EventKind::Archive | EventKind::Restore | EventKind::Note | EventKind::Working
                | EventKind::Comment | EventKind::Reply | EventKind::Resolve | EventKind::Attention
                | EventKind::Answer | EventKind::Withdraw => continue,
            }
            let placed = if ev.kind == EventKind::Move { ev.jelly.clone() } else { key };
            links.get_mut(&placed).expect("just placed").mark(&ev.by);
            marked.push(placed);
        }
        if marked.is_empty() { return }
        if let Err(e) = save_links(&self.home, &links) {
            eprintln!("jelly: {e}");
            for id in &marked {
                if let Some(link) = links.get_mut(id) { link.error = Some(e.clone()) }
            }
        }
        self.notify.notify_one();
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
        if let Err(e) = save_links(&self.home, &links) { eprintln!("jelly: {e}") }
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
    format!("{}{}/j/{}/", link.remote, person_path(&link.owner), id)
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

/// Why a link needs another go. After `Save`, the remote copy is current but
/// `links.json` does not say so yet, so the link keeps its work until a save lands.
#[derive(Debug)]
enum Retry {
    Sync(String),
    Save(String),
}

struct RemoteApi<'a> {
    remote: &'a str,
    api: String,
    key: &'a str,
}

impl<'a> RemoteApi<'a> {
    fn new(link: &'a Link, key: &'a str) -> Self {
        RemoteApi { remote: &link.remote, api: format!("{}{}/api/jellies", link.remote, person_path(&link.owner)), key }
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        ureq::request(method, &format!("{}{path}", self.api))
            .set("authorization", &format!("Bearer {}", self.key))
            .timeout(TIMEOUT)
    }

    fn send(&self, method: &str, path: &str, body: impl Serialize) -> Result<Answer, String> {
        let body = serde_json::to_value(body).expect("request serializes");
        let req = self.request(method, path);
        match if body.is_null() { req.call() } else { req.send_json(body) } {
            Ok(r) => Ok(Answer::Ok(r)),
            Err(ureq::Error::Status(code, r)) => Ok(Answer::Status(code, r)),
            Err(ureq::Error::Transport(t)) => Err(format!("cannot reach {} ({t})", self.remote)),
        }
    }

    /// A request that must succeed.
    fn must(&self, method: &str, path: &str, body: impl Serialize) -> Result<ureq::Response, String> {
        match self.send(method, path, body)? {
            Answer::Ok(r) => Ok(r),
            Answer::Status(code, r) => Err(describe_status(self.remote, code, r)),
        }
    }

    /// Like `must`, but a 404 is an answer, not a failure: `None`.
    fn send_or_missing(&self, method: &str, path: &str, body: impl Serialize) -> Result<Option<ureq::Response>, String> {
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
    let api = RemoteApi::new(link, key);
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
        let moved = ProjectReq { project: jelly.project().into(), by: Some(by.clone()) };
        match api.send("POST", &format!("/{}/move", link.remote_id), moved)? {
            Answer::Ok(_) | Answer::Status(404, _) => {}
            // The target name is taken on the remote: the old copy goes and the new one is created below.
            Answer::Status(409, _) => { api.send_or_missing("DELETE", &format!("/{}{by_query}", link.remote_id), Value::Null)?; }
            Answer::Status(code, r) => return Err(describe_status(&link.remote, code, r)),
        }
    }
    let remote: Option<Jelly> = api.send_or_missing("GET", &format!("/{id}"), Value::Null)?
        .map(|r| r.into_json()).transpose().map_err(|e| e.to_string())?;
    let state = remote_state(&jelly.state);
    let present: Vec<String> = match remote {
        None => {
            api.must("POST", "", CreateReq { name: id.into(), view: jelly.view, verbs: jelly.verbs, state,
                by: Some(by), replace: true, about: jelly.about, project: String::new() })?;
            Vec::new()
        }
        Some(remote) => {
            let changed = |remote: &str, local: &str| (remote != local).then(|| local.to_string());
            let reshape = ReshapeReq {
                view: changed(&remote.view, &jelly.view), verbs: changed(&remote.verbs, &jelly.verbs),
                about: changed(&remote.about, &jelly.about), by: Some(by.clone()),
            };
            if reshape.view.is_some() || reshape.verbs.is_some() || reshape.about.is_some() {
                api.must("POST", &format!("/{id}/reshape"), reshape)?;
            }
            if remote.state != state {
                api.must("POST", &format!("/{id}/set"), SetReq { pointer: String::new(), value: Some(state), remove: false, by: Some(by) })?;
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
fn reconcile(syncer: &Syncer, app: &App, id: &str) -> Result<(), Retry> {
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
    syncer.record(id, &link, result)
}

impl Syncer {
    /// Records a sync's outcome on the live link. `seen` is the link as the sync
    /// read it: pending work is cleared only when nothing was marked since, so a
    /// change that landed during the sync is still owed. A save that fails keeps
    /// the link owing work, so the forwarder comes back and saves again.
    fn record(&self, id: &str, seen: &Link, result: Result<Outcome, String>) -> Result<(), Retry> {
        let mut links = self.links.lock().expect("links lock");
        match &result {
            Ok(Outcome::Dropped) => { links.remove(id); }
            Ok(Outcome::Synced) => if let Some(current) = links.get_mut(id) {
                current.remote_id = id.into();
                current.error = None;
                if current.pending.generation == seen.pending.generation {
                    current.pending = Pending { generation: seen.pending.generation, ..Pending::default() };
                }
            },
            Err(e) => if let Some(current) = links.get_mut(id) { current.error = Some(e.clone()) },
        }
        let saved = save_links(&self.home, &links);
        if let (Err(e), Some(current)) = (&saved, links.get_mut(id)) {
            current.error = Some(e.clone());
            current.pending.dirty = true;
        }
        match (result, saved) {
            (Err(e), _) => Err(Retry::Sync(e)),
            (Ok(_), Err(e)) => Err(Retry::Save(e)),
            (Ok(_), Ok(())) => Ok(()),
        }
    }
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
                .await.unwrap_or_else(|e| Err(Retry::Sync(e.to_string())));
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

/// Links `id` to a remote and syncs it now. A link that fails on its first push is
/// not kept; one whose copy reached the remote is shared even when `links.json`
/// could not record that, and the answer carries the save error.
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
        if let Err(e) = save_links(&syncer.home, &links) {
            if is_new { links.remove(id); }
            return Err(e);
        }
        is_new
    };
    let shared = |error: Option<String>| {
        let url = syncer.link_status(id).map(|s| s["url"].as_str().unwrap_or("").to_string()).unwrap_or_default();
        json!({"jelly": id, "remote": remote, "owner": owner, "url": url, "version": version, "error": error})
    };
    match reconcile(syncer, app, id) {
        Ok(()) => Ok(shared(None)),
        Err(Retry::Save(e)) => Ok(shared(Some(e))),
        Err(Retry::Sync(e)) => {
            if !is_new { return Err(e) }
            let mut links = syncer.links.lock().expect("links lock");
            let link = links.remove(id);
            match save_links(&syncer.home, &links) {
                Ok(()) => Err(e),
                Err(save) => {
                    // Memory follows disk: the link stays, shown by `ls` as behind, until a save drops it.
                    if let Some(link) = link { links.insert(id.into(), link); }
                    Err(format!("{e}; the link is kept because it {save}"))
                }
            }
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
        RemoteApi::new(&link, &key).send_or_missing("DELETE", &format!("/{}?by={}", link.remote_id, crate::encode(by)), Value::Null)?;
    }
    let mut links = syncer.links.lock().expect("links lock");
    let dropped = links.remove(id);
    if let Err(e) = save_links(&syncer.home, &links) {
        // Memory follows disk, so the link is still there to retry.
        if let Some(link) = dropped { links.insert(id.into(), link); }
        return Err(e);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn syncer_with_link(home: &Path, id: &str) -> Syncer {
        let syncer = Syncer::open(home).unwrap();
        syncer.links.lock().unwrap().insert(id.into(), Link {
            remote: "http://remote".into(), owner: "alice".into(), remote_id: id.into(),
            pending: Pending { dirty: true, ..Pending::default() }, error: None, removed: false,
        });
        syncer
    }

    #[test]
    fn a_change_during_a_sync_is_still_owed_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let syncer = syncer_with_link(dir.path(), "p:plan");
        let seen = syncer.links.lock().unwrap().get("p:plan").cloned().unwrap();

        syncer.links.lock().unwrap().get_mut("p:plan").unwrap().mark("person");
        syncer.record("p:plan", &seen, Ok(Outcome::Synced)).unwrap();
        let status = syncer.link_status("p:plan").unwrap();
        assert_eq!(status["synced"], json!(false), "the change made during the sync is not on the remote yet: {status}");
        assert!(syncer.links.lock().unwrap()["p:plan"].has_work("p:plan"));

        let seen = syncer.links.lock().unwrap().get("p:plan").cloned().unwrap();
        syncer.record("p:plan", &seen, Ok(Outcome::Synced)).unwrap();
        assert_eq!(syncer.link_status("p:plan").unwrap()["synced"], json!(true));
        let saved: Links = read_json(&dir.path().join("links.json")).unwrap();
        assert!(!saved["p:plan"].pending.dirty);
    }

    #[test]
    fn a_synced_link_whose_save_failed_keeps_its_work_until_a_save_lands() {
        let dir = tempfile::tempdir().unwrap();
        let syncer = syncer_with_link(dir.path(), "p:plan");
        let seen = syncer.links.lock().unwrap().get("p:plan").cloned().unwrap();

        let path = dir.path().join("links.json");
        std::fs::create_dir(&path).unwrap();
        let Err(Retry::Save(e)) = syncer.record("p:plan", &seen, Ok(Outcome::Synced)) else { panic!("a lost save is a save failure") };
        assert!(e.contains("could not save"), "{e}");
        let status = syncer.link_status("p:plan").unwrap();
        assert_eq!(status["synced"], json!(false), "{status}");
        assert_eq!(status["error"], json!(e));
        assert!(syncer.links.lock().unwrap()["p:plan"].has_work("p:plan"), "the forwarder comes back for the save");

        std::fs::remove_dir(&path).unwrap();
        std::fs::remove_file(path.with_extension("json.tmp")).ok();
        let seen = syncer.links.lock().unwrap().get("p:plan").cloned().unwrap();
        assert!(syncer.record("p:plan", &seen, Ok(Outcome::Synced)).is_ok());
        assert_eq!(syncer.link_status("p:plan").unwrap()["synced"], json!(true));
        let saved: Links = read_json(&path).unwrap();
        assert!(!saved["p:plan"].pending.dirty && saved["p:plan"].error.is_none());
    }
}
