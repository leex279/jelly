//! The jellies and every change to them.
//!
//! A jelly is a view (HTML), verbs (JavaScript) and state (JSON) held together
//! under one name. State changes only through verbs, so a person's click and an
//! agent's call go through the same code and produce the same event. Every
//! change keeps an undo step, what it takes to go back, and is appended to
//! `events.jsonl`.

use crate::verbs;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Undo keeps the last `UNDO_DEPTH` changes, and drops the oldest sooner once
/// new steps together hold more than twice the state plus `UNDO_SLACK`, so a
/// few whole-state `set`s cannot grow a jelly's file to many copies of its
/// state. Converted legacy steps are exempt from the byte budget until new
/// history displaces them. The newest step always stays: the last change can
/// always be undone.
const UNDO_DEPTH: usize = 100;
const UNDO_SLACK: usize = 64 * 1024;
/// Myers diffing is quadratic on unrelated text. Past this it settles for a
/// coarser diff, which is still exact.
const DIFF_DEADLINE: Duration = Duration::from_millis(50);
/// `events.jsonl` is trimmed once it passes `LOG_LIMIT`, down to the newest
/// `LOG_KEEP` plus up to `PERSON_CARRY` of the person's older events, which
/// are what the inbox and `jelly see` report.
const LOG_LIMIT: u64 = 8 << 20;
const LOG_KEEP: usize = 4 << 20;
const PERSON_CARRY: usize = 1 << 20;
/// How much of `events.jsonl` a backwards read takes at a time.
const LOG_CHUNK: u64 = 64 * 1024;

pub fn resolve_name<'a>(name: &str, project: &str, identities: impl IntoIterator<Item = &'a str>) -> String {
    if name.contains(':') || name == PAGE { return name.into(); }
    let preferred = format!("{project}:{name}");
    let hits: Vec<&str> = identities.into_iter()
        .filter(|id| id.split_once(':').is_some_and(|(_, short)| short == name)).collect();
    if !project.is_empty() && hits.contains(&preferred.as_str()) { return preferred; }
    if hits.len() == 1 { hits[0].into() } else { name.into() }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Jelly {
    pub name: String,
    pub view: String,
    pub verbs: String,
    pub state: Value,
    /// Bumps on every change of any kind.
    pub version: u64,
    /// Bumps when the view or verbs change; an open view reloads when it does.
    pub view_rev: u64,
    pub pinned: bool,
    pub made_by: String,
    pub updated_ms: u64,
    /// When the agent made it; the surface's stream is in this order.
    #[serde(default)]
    pub created_ms: u64,
    /// One line from the agent on what this jelly is for, shown beside it.
    #[serde(default)]
    pub about: String,
    /// Hidden from the surface after a quiet week; any change brings it back.
    #[serde(default)]
    pub archived: bool,
    /// Comments pinned to elements of the view. Kept apart from state: they are
    /// about the jelly, not part of it, and undo does not touch them.
    #[serde(default)]
    #[serde(alias = "feedback")]
    pub comments: Vec<Comment>,
    /// What the agent put in front of the person: asks to answer, things to
    /// look at. Beside state like comments; undo does not touch them.
    #[serde(default)]
    pub attention: Vec<Attention>,
    /// Set while an agent says it is changing this jelly, so the person waits.
    #[serde(default)]
    pub working: Option<Value>,
    /// Notes agents leave for the next agent: what this jelly is for, how its
    /// verbs are meant to be used, what the person said. `jelly see` shows them.
    #[serde(default)]
    pub notes: Vec<Value>,
}

/// One item for the person's attention: an ask wants an answer (an option, or
/// text), a look wants a glance (seen, or a word back). Items asked together,
/// as one `jelly ask --spec` round, share a group.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(from = "AttentionOnDisk")]
pub struct Attention {
    pub id: String,
    pub kind: AttentionKind,
    pub title: String,
    /// More to read before answering.
    #[serde(default)]
    pub context: String,
    /// The element it is about: `{"selector": "#hero", "quote": "…"}`, or null.
    #[serde(default)]
    pub anchor: Value,
    #[serde(default)]
    pub options: Vec<AskOption>,
    #[serde(default)]
    pub recommend: Option<String>,
    #[serde(default)]
    pub why: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub by: String,
    pub at_ms: u64,
    #[serde(default)]
    pub answer: Option<Answer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawn: Option<Stamp>,
}

impl Comment {
    /// The agent replied last on an open thread: the person has something to read.
    pub fn awaiting_person(&self) -> bool {
        !self.resolved && self.replies.last().is_some_and(|r| r.by != PERSON)
    }
}

impl Attention {
    pub fn is_open(&self) -> bool {
        self.answer.is_none() && self.withdrawn.is_none()
    }

    /// One line for agents, `a1 ask "Which order?" [a★/b] at #list`: `jelly see`
    /// and `jelly attention` both use it.
    pub fn summary(&self) -> String {
        let opts = self.options.iter().map(|o| format!("{}{}", o.id, if self.recommend.as_deref() == Some(o.id.as_str()) { "★" } else { "" })).collect::<Vec<_>>().join("/");
        let kind = if self.kind == AttentionKind::Look { "look" } else { "ask" };
        format!("{} {kind} {:?}{}{}", self.id, self.title, if opts.is_empty() { String::new() } else { format!(" [{opts}]") }, self.anchor["selector"].as_str().map(|s| format!(" at {s}")).unwrap_or_default())
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AttentionKind {
    Ask,
    Look,
}

/// One choice in an ask. Only `id` and `label` are needed; the rest is there
/// when the agent lays out tradeoffs.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AskOption {
    pub id: String,
    #[serde(default, alias = "title")]
    pub label: String,
    /// What picking it does, in a sentence.
    #[serde(default, alias = "summary", skip_serializing_if = "String::is_empty")]
    pub does: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub costs: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// Any HTML, shown live in a sandbox.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preview: String,
}

/// The person's answer: an option, words, or both. A look answered with
/// neither means "seen".
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Answer {
    #[serde(default)]
    pub option: Option<String>,
    #[serde(default)]
    pub text: String,
    pub by: String,
    pub at_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Stamp {
    pub by: String,
    pub at_ms: u64,
}

/// Reads items saved before withdrawals had their own field
/// (`answer: {"withdrawn": true, …}`) and before `detail` became `context`.
#[derive(Deserialize)]
struct AttentionOnDisk {
    id: String,
    kind: AttentionKind,
    title: String,
    #[serde(default, alias = "detail")]
    context: String,
    #[serde(default)]
    anchor: Value,
    #[serde(default)]
    options: Vec<AskOption>,
    #[serde(default)]
    recommend: Option<String>,
    #[serde(default)]
    why: String,
    #[serde(default)]
    group: Option<String>,
    by: String,
    at_ms: u64,
    #[serde(default)]
    answer: Option<Value>,
    #[serde(default)]
    withdrawn: Option<Stamp>,
}

impl From<AttentionOnDisk> for Attention {
    fn from(d: AttentionOnDisk) -> Self {
        let mut withdrawn = d.withdrawn;
        let answer = match d.answer {
            Some(v) if v.get("withdrawn").is_some() => {
                withdrawn = Some(Stamp { by: v["by"].as_str().unwrap_or("agent").into(), at_ms: v["at_ms"].as_u64().unwrap_or(0) });
                None
            }
            Some(v) => serde_json::from_value(v).ok(),
            None => None,
        };
        Attention { id: d.id, kind: d.kind, title: d.title, context: d.context, anchor: d.anchor, options: d.options, recommend: d.recommend, why: d.why, group: d.group, by: d.by, at_ms: d.at_ms, answer, withdrawn }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Comment {
    pub id: String,
    /// Where in the view: `{"selector": "#opt-b .preview", "quote": "first words"}`.
    pub anchor: Value,
    pub text: String,
    pub by: String,
    pub at_ms: u64,
    pub resolved: bool,
    /// The thread under the comment: the agent's answer, the person's follow-up.
    #[serde(default)]
    pub replies: Vec<Reply>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Reply {
    pub text: String,
    pub by: String,
    pub at_ms: u64,
}

/// What it takes to go back one change. `state` turns the pretty-printed
/// state after the change back into the one before; `view` and `verbs` hold
/// what a reshape replaced, and are absent when it kept them.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct Step {
    /// Existing snapshot history must survive upgrade, even above the new byte budget.
    #[serde(default)]
    legacy: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    state: Vec<Hunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    view: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verbs: Option<String>,
    view_rev: u64,
}

/// Replace `remove` lines of the pretty-printed state, starting at line `at`, with `lines`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Hunk {
    at: usize,
    remove: usize,
    lines: Vec<String>,
}

impl Step {
    fn for_rev(view_rev: u64) -> Step {
        Step { view_rev, ..Step::default() }
    }

    fn budget_bytes(&self) -> usize {
        if self.legacy {
            return 0;
        }
        let lines: usize = self.state.iter().flat_map(|h| &h.lines).map(String::len).sum();
        lines + self.view.as_ref().map_or(0, String::len) + self.verbs.as_ref().map_or(0, String::len)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(from = "EntryOnDisk")]
struct Entry {
    jelly: Jelly,
    steps: Vec<Step>,
}

/// Reads jellies saved when undo kept a full copy of view, verbs and state per change.
#[derive(Deserialize)]
struct EntryOnDisk {
    jelly: Jelly,
    #[serde(default)]
    undo: Vec<Snapshot>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Snapshot {
    view: String,
    verbs: String,
    state: Value,
    view_rev: u64,
}

impl From<EntryOnDisk> for Entry {
    fn from(d: EntryOnDisk) -> Self {
        if d.undo.is_empty() {
            return Entry { jelly: d.jelly, steps: d.steps };
        }
        // Oldest first: each snapshot is what the change after it replaced.
        let mut entry = Entry { jelly: d.jelly, steps: Vec::new() };
        for (i, snap) in d.undo.iter().enumerate() {
            let (view, verbs, state) = match d.undo.get(i + 1) {
                Some(next) => (&next.view, &next.verbs, &next.state),
                None => (&entry.jelly.view, &entry.jelly.verbs, &entry.jelly.state),
            };
            let step = Step {
                legacy: true,
                state: state_step(state, &snap.state),
                view: (snap.view != *view).then(|| snap.view.clone()),
                verbs: (snap.verbs != *verbs).then(|| snap.verbs.clone()),
                view_rev: snap.view_rev,
            };
            entry.steps.push(step);
        }
        entry
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InboxReader {
    Named { name: String },
    Conversation { id: String },
    Parent { pid: u32, start_time: u64 },
}

#[derive(Serialize, Deserialize)]
pub struct InboxRequest {
    #[serde(default)]
    pub project: String,
    pub reader: InboxReader,
}

#[derive(Serialize, Deserialize)]
pub struct InboxCommit {
    #[serde(flatten)]
    pub inbox: InboxRequest,
    pub through: u64,
}

#[derive(Serialize, Deserialize)]
pub struct InboxBatch {
    pub events: Vec<InboxItem>,
    pub through: u64,
}

#[derive(Serialize, Deserialize)]
pub struct InboxItem {
    #[serde(flatten)]
    pub event: Event,
    pub text: String,
}

impl From<Event> for InboxItem {
    fn from(event: Event) -> Self {
        let text = event.describe();
        Self { event, text }
    }
}

#[derive(Serialize, Deserialize)]
struct InboxCursor {
    project: String,
    reader: InboxReader,
    #[serde(flatten)]
    position: InboxPosition,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum InboxPosition {
    After { seq: u64 },
    Recent { since_ms: u64 },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Event {
    pub seq: u64,
    pub at_ms: u64,
    pub jelly: String,
    pub kind: EventKind,
    pub by: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub verb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub args: Option<Value>,
    /// JSON pointers into the state that this change touched.
    #[serde(default)]
    pub changed: Vec<String>,
    /// For a person's verb call: the element they used, `{"selector", "text"}`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<Value>,
    pub version: u64,
    pub view_rev: u64,
    /// Sent to live listeners, never written to the log.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub state: Option<Value>,
}

/// Every kind of change. Adding one means adding it here and to `describe`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    New,
    Call,
    Reshape,
    Undo,
    Pin,
    #[serde(alias = "project")]
    Move,
    Remove,
    Archive,
    Restore,
    Asset,
    Note,
    Working,
    Comment,
    Reply,
    Resolve,
    Attention,
    Answer,
    Withdraw,
    Set,
}

impl EventKind {
    /// Things the person did that an agent should hear about.
    pub fn is_for_agent(self) -> bool {
        matches!(self, EventKind::Call | EventKind::Comment | EventKind::Reply | EventKind::Resolve | EventKind::Undo | EventKind::Answer)
    }
}

/// The person, as recorded in `by`. Agents use their own names.
pub const PERSON: &str = "person";
/// The internal jelly that is the jelly page.
pub const PAGE: &str = "surface";

impl Event {
    pub fn is_inbox_item(&self, project: &str) -> bool {
        self.by == PERSON && self.jelly != PAGE && self.kind.is_for_agent()
            && (project.is_empty() || self.jelly.starts_with(&format!("{project}:")))
    }

    /// One line on what happened, for agents: the inbox and `jelly see` both use it.
    pub fn describe(&self) -> String {
        let a = self.args.clone().unwrap_or(Value::Null);
        let s = |v: &Value| v.as_str().unwrap_or("").to_string();
        let clip = |t: String| if t.chars().count() > 200 { format!("{}…", t.chars().take(200).collect::<String>()) } else { t };
        match self.kind {
            EventKind::Call => {
                let via = self.source.as_ref().and_then(|x| x["text"].as_str()).filter(|t| !t.is_empty()).map(|t| format!(" (via {t:?})")).unwrap_or_default();
                format!("{} {}{via}", self.verb.as_deref().unwrap_or("?"), clip(a.to_string()))
            }
            EventKind::Answer => {
                let pick = a["option"].as_str().map(|o| format!(" → {o}")).unwrap_or_default();
                let text = a["text"].as_str().filter(|t| !t.is_empty()).map(|t| format!(": {:?}", clip(t.into()))).unwrap_or_default();
                let seen = if pick.is_empty() && text.is_empty() { " → seen" } else { "" };
                format!("answered {} {:?}{pick}{seen}{text}", s(&a["id"]), s(&a["title"]))
            }
            EventKind::Comment => format!("commented on {:?}: {}", s(&a["anchor"]["quote"]), clip(s(&a["text"]))),
            EventKind::Reply => format!("replied on comment {}: {}", s(&a["id"]), clip(s(&a["text"]))),
            EventKind::Resolve => format!("resolved comment {}", s(&a)),
            EventKind::Undo => "undid the last state change".into(),
            EventKind::Attention => format!("asked {:?}", s(&a["title"])),
            EventKind::Withdraw => format!("withdrew {}", s(&a)),
            EventKind::Set => match s(&a["pointer"]).as_str() { "" => "set the whole state".into(), p => format!("set {p}") },
            EventKind::Working => match a.as_str() { Some(w) => format!("started working: {w}"), None => "finished working".into() },
            EventKind::Note => format!("left a note: {}", clip(s(&a))),
            EventKind::Move => format!("moved here from {}", s(&a["from"])),
            EventKind::Pin => if a == Value::Bool(true) { "pinned it".into() } else { "unpinned it".into() },
            EventKind::Asset => format!("added {}", s(&a["file"])),
            EventKind::New => "made it".into(),
            EventKind::Reshape => "reshaped it".into(),
            EventKind::Remove => "removed it".into(),
            EventKind::Archive => "archived it".into(),
            EventKind::Restore => "restored it".into(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Error {
    NotFound(String),
    Exists(String),
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NotFound(n) => write!(f, "no jelly named {n}"),
            Error::Exists(n) => write!(f, "a jelly named {n} already exists; reshape it or pass replace"),
            Error::Invalid(m) => f.write_str(m),
        }
    }
}

#[derive(Debug)]
pub struct BatchError {
    pub events: Vec<Event>,
    pub error: Error,
}

impl From<Error> for BatchError {
    fn from(error: Error) -> Self { Self { events: Vec::new(), error } }
}

pub struct Store {
    dir: PathBuf,
    jellies: BTreeMap<String, Entry>,
    seq: u64,
    recent: BTreeMap<String, RecentEvents>,
    timestamps: TimestampIndex,
    /// The element behind the verb call being committed; taken by the next event.
    source: Option<Value>,
    #[cfg(test)]
    pub log_failure: Option<(u64, usize)>,
}

#[derive(Default)]
struct RecentEvents {
    last: Option<Event>,
    person: Vec<Event>,
}

impl RecentEvents {
    fn push(&mut self, event: Event) {
        if event.by == PERSON {
            if self.person.len() == 8 {
                self.person.remove(0);
            }
            self.person.push(event.clone());
        }
        self.last = Some(event);
    }
}

type TimestampIndex = BTreeMap<(u64, u64), (u64, usize)>;

fn read_event_history(path: &Path) -> std::io::Result<(u64, BTreeMap<String, RecentEvents>, TimestampIndex)> {
    let log = read_optional(path)?;
    if !log.is_empty() && !log.ends_with('\n') {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "event log is missing its final newline"));
    }
    let mut seq = 0;
    let mut recent = BTreeMap::<String, RecentEvents>::new();
    let mut timestamps = BTreeMap::new();
    let mut offset = 0;
    for line in log.split_inclusive('\n') {
        let event: Event = serde_json::from_str(line)?;
        timestamps.insert((event.at_ms, event.seq), (offset, line.len()));
        offset += line.len() as u64;
        seq = seq.max(event.seq);
        recent.entry(event.jelly.clone()).or_default().push(event);
    }
    Ok((seq, recent, timestamps))
}

impl Store {
    pub fn open(dir: &Path) -> std::io::Result<Store> {
        fs::create_dir_all(dir.join("jellies"))?;
        let mut jellies = BTreeMap::new();
        for file in fs::read_dir(dir.join("jellies"))? {
            let path = file?.path();
            if path.extension().is_some_and(|e| e == "json") {
                let bytes = fs::read(&path).map_err(|e| {
                    std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
                })?;
                let entry = serde_json::from_slice::<Entry>(&bytes).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))
                })?;
                // The name becomes a file under `jellies/` and a folder under `assets/` (see
                // `disk::name`), so a hand-edited one must not escape the home.
                valid_name(&entry.jelly.name).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))
                })?;
                jellies.insert(entry.jelly.name.clone(), entry);
            }
        }
        // Sequence numbers continue across restarts, so an event's seq is unique.
        let (seq, recent, timestamps) = read_event_history(&dir.join("events.jsonl"))?;
        Ok(Store { dir: dir.to_path_buf(), jellies, seq, recent, timestamps, source: None, #[cfg(test)] log_failure: None })
    }

    pub fn list(&self) -> impl Iterator<Item = &Jelly> {
        self.jellies.values().map(|e| &e.jelly)
    }

    pub fn get(&self, name: &str) -> Result<&Jelly, Error> {
        self.jellies.get(name).map(|e| &e.jelly).ok_or_else(|| Error::NotFound(name.into()))
    }

    #[allow(clippy::too_many_arguments)] // one call site per caller; a struct would only rename the arguments
    pub fn create(
        &mut self,
        name: &str,
        view: String,
        verbs_src: String,
        state: Value,
        by: &str,
        replace: bool,
        about: &str,
        project: &str,
    ) -> Result<Event, Error> {
        // A jelly's id is `project:name`, so names only need to be unique in a project.
        let id = if project.is_empty() || name.contains(':') { name.to_string() } else { format!("{project}:{name}") };
        let name = id.as_str();
        valid_name(name)?;
        if self.jellies.contains_key(name) && !replace {
            return Err(Error::Exists(name.into()));
        }
        verbs::check(&verbs_src).map_err(Error::Invalid)?;
        let pinned = self.jellies.get(name).is_some_and(|e| e.jelly.pinned);
        let jelly = Jelly {
            name: name.into(),
            view,
            verbs: verbs_src,
            state,
            version: 1,
            view_rev: 1,
            pinned,
            made_by: by.into(),
            updated_ms: now_ms(),
            created_ms: now_ms(),
            about: about.trim().into(),
            archived: false,
            comments: Vec::new(),
            attention: Vec::new(),
            working: None,
            notes: Vec::new(),
        };
        let previous_entry = self.jellies.insert(name.into(), Entry { jelly, steps: Vec::new() });
        self.commit(name, EventKind::New, by, None, None, vec![String::new()], previous_entry)
    }

    pub fn call(&mut self, name: &str, verb: &str, args: Value, by: &str) -> Result<Event, Error> {
        self.call_from(name, verb, args, by, None)
    }

    /// A verb call that also records the element the person used.
    pub fn call_from(&mut self, name: &str, verb: &str, args: Value, by: &str, source: Option<Value>) -> Result<Event, Error> {
        let previous_entry = self.jellies.get(name).cloned();
        let entry = self.entry(name)?;
        let next = verbs::run(&entry.jelly.verbs, verb, &entry.jelly.state, &args, by, now_ms()).map_err(Error::Invalid)?;
        let mut changed = Vec::new();
        diff(&entry.jelly.state, &next, &mut String::new(), &mut changed);
        let step = Step { state: state_step(&next, &entry.jelly.state), ..Step::for_rev(entry.jelly.view_rev) };
        entry.jelly.state = next;
        push_step(entry, step);
        self.source = source;
        self.commit(name, EventKind::Call, by, Some(verb.into()), Some(args), changed, previous_entry)
    }

    /// Replaces the view and/or verbs while keeping state. `about` alone only
    /// relabels the jelly; open views do not reload for it.
    pub fn reshape(&mut self, name: &str, view: Option<String>, verbs_src: Option<String>, about: Option<String>, by: &str) -> Result<Event, Error> {
        if view.is_none() && verbs_src.is_none() && about.is_none() {
            return Err(Error::Invalid("reshape needs a new view, new verbs or a new about".into()));
        }
        if let Some(src) = &verbs_src {
            verbs::check(src).map_err(|e| Error::Invalid(format!("new verbs rejected, jelly unchanged: {e}")))?;
        }
        let previous_entry = self.jellies.get(name).cloned();
        let entry = self.entry(name)?;
        if let Some(a) = about {
            entry.jelly.about = a.trim().into();
        }
        if view.is_some() || verbs_src.is_some() {
            let j = &mut entry.jelly;
            let replaced = |old: &mut String, new: Option<String>| new.filter(|n| n != old).map(|n| std::mem::replace(old, n));
            let step = Step { view: replaced(&mut j.view, view), verbs: replaced(&mut j.verbs, verbs_src), ..Step::for_rev(j.view_rev) };
            j.view_rev += 1;
            push_step(entry, step);
        }
        self.commit(name, EventKind::Reshape, by, None, None, Vec::new(), previous_entry)
    }

    /// Writes state directly, outside the verbs: an agent fixing its own
    /// content or starting over. An empty pointer means the whole state;
    /// `value: None` removes what is at the pointer. Undoable like a verb call.
    pub fn set_state(&mut self, name: &str, pointer: &str, value: Option<Value>, by: &str) -> Result<Event, Error> {
        let previous_entry = self.jellies.get(name).cloned();
        let entry = self.entry(name)?;
        let mut next = entry.jelly.state.clone();
        set_at(&mut next, pointer, value).map_err(Error::Invalid)?;
        let mut changed = Vec::new();
        diff(&entry.jelly.state, &next, &mut String::new(), &mut changed);
        let step = Step { state: state_step(&next, &entry.jelly.state), ..Step::for_rev(entry.jelly.view_rev) };
        entry.jelly.state = next;
        push_step(entry, step);
        self.commit(name, EventKind::Set, by, None, Some(serde_json::json!({"pointer": pointer})), changed, previous_entry)
    }

    pub fn undo(&mut self, name: &str, by: &str) -> Result<Event, Error> {
        let previous_entry = self.jellies.get(name).cloned();
        let entry = self.entry(name)?;
        let step = entry.steps.last().ok_or_else(|| Error::Invalid(format!("{name} has nothing to undo")))?;
        let state = restore(&entry.jelly.state, &step.state).map_err(|e| Error::Invalid(format!("the undo history of {name} is damaged: {e}")))?;
        let step = entry.steps.pop().expect("checked above");
        let mut changed = Vec::new();
        diff(&entry.jelly.state, &state, &mut String::new(), &mut changed);
        entry.jelly.state = state;
        if let Some(v) = step.view {
            entry.jelly.view = v;
        }
        if let Some(v) = step.verbs {
            entry.jelly.verbs = v;
        }
        if entry.jelly.view_rev != step.view_rev {
            // A new number, not the old one: views opened on the undone
            // version must notice and reload.
            entry.jelly.view_rev += 1;
        }
        self.commit(name, EventKind::Undo, by, None, None, changed, previous_entry)
    }

    /// Pins a comment to part of the view.
    pub fn comment(&mut self, name: &str, anchor: Value, text: &str, by: &str) -> Result<Event, Error> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::Invalid("empty comment".into()));
        }
        let previous_entry = self.jellies.get(name).cloned();
        let entry = self.entry(name)?;
        let id = format!("c{}", entry.jelly.comments.len() + 1);
        entry.jelly.comments.push(Comment { id: id.clone(), anchor: anchor.clone(), text: text.into(), by: by.into(), at_ms: now_ms(), resolved: false, replies: Vec::new() });
        let args = serde_json::json!({"id": id, "anchor": anchor, "text": text});
        self.commit(name, EventKind::Comment, by, None, Some(args), Vec::new(), previous_entry)
    }

    pub fn resolve(&mut self, name: &str, id: &str, by: &str) -> Result<Event, Error> {
        let previous_entry = self.jellies.get(name).cloned();
        let entry = self.entry(name)?;
        let f = entry.jelly.comments.iter_mut().find(|f| f.id == id).ok_or_else(|| Error::Invalid(format!("{name} has no comment {id}")))?;
        f.resolved = true;
        self.commit(name, EventKind::Resolve, by, None, Some(Value::String(id.into())), Vec::new(), previous_entry)
    }

    /// Answers on a comment's thread. A reply leaves the thread resolved only
    /// when it says so: the agent's "done" can close it, and the person's
    /// follow-up on a closed thread opens it again.
    pub fn reply(&mut self, name: &str, id: &str, text: &str, by: &str, resolve: bool) -> Result<Event, Error> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::Invalid("empty reply".into()));
        }
        let previous_entry = self.jellies.get(name).cloned();
        let entry = self.entry(name)?;
        let f = entry.jelly.comments.iter_mut().find(|f| f.id == id).ok_or_else(|| Error::Invalid(format!("{name} has no comment {id}")))?;
        f.replies.push(Reply { text: text.into(), by: by.into(), at_ms: now_ms() });
        f.resolved = resolve;
        let args = serde_json::json!({"id": id, "text": text, "resolved": resolve, "anchor": f.anchor, "comment": f.text});
        self.commit(name, EventKind::Reply, by, None, Some(args), Vec::new(), previous_entry)
    }

    /// Moves a jelly to another project ("" for none). Its id changes from
    /// `old:name` to `new:name`; assets move with it.
    pub fn set_project(&mut self, name: &str, project: &str, by: &str) -> Result<Event, Error> {
        let short = name.split_once(':').map_or(name, |(_, n)| n);
        let id = if project.is_empty() { short.to_string() } else { format!("{project}:{short}") };
        valid_name(&id)?;
        if id == name {
            return Err(Error::Invalid(format!("{name} is already in that project")));
        }
        if self.jellies.contains_key(&id) {
            return Err(Error::Exists(id));
        }
        let mut entry = self.jellies.get(name).ok_or_else(|| Error::NotFound(name.into()))?.clone();
        entry.jelly.name = id.clone();
        let previous_entry = self.jellies.insert(id.clone(), entry);
        let ev = self.commit(&id, EventKind::Move, by, None, Some(serde_json::json!({"from": name})), Vec::new(), previous_entry)?;
        self.jellies.remove(name);
        let _ = fs::remove_file(self.path(name));
        if self.asset_dir(name).exists() {
            let _ = fs::rename(self.asset_dir(name), self.asset_dir(&id));
        }
        Ok(ev)
    }

    /// Archives unpinned jellies nobody touched for `idle_ms`, unless they still
    /// wait on the person (an open attention item or an open comment).
    pub fn sweep(&mut self, idle_ms: u64) -> Vec<Event> {
        let cutoff = now_ms().saturating_sub(idle_ms);
        let stale: Vec<String> = self.list()
            .filter(|j| !j.pinned && !j.archived && j.name != "surface" && j.updated_ms < cutoff)
            .filter(|j| j.comments.iter().all(|f| f.resolved))
            .filter(|j| j.attention.iter().all(|a| !a.is_open()))
            .map(|j| j.name.clone())
            .collect();
        stale.iter().filter_map(|n| match self.set_archived(n, true, "jelly") {
            Ok(ev) => Some(ev),
            Err(e) => { eprintln!("could not archive {n}: {e}"); None }
        }).collect()
    }

    pub fn set_archived(&mut self, name: &str, archived: bool, by: &str) -> Result<Event, Error> {
        let previous_entry = self.jellies.get(name).cloned();
        self.entry(name)?.jelly.archived = archived;
        self.commit(name, if archived { EventKind::Archive } else { EventKind::Restore }, by, None, None, Vec::new(), previous_entry)
    }

    /// An agent's note for the next agent.
    pub fn note(&mut self, name: &str, text: &str, by: &str) -> Result<Event, Error> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::Invalid("empty note".into()));
        }
        let previous_entry = self.jellies.get(name).cloned();
        self.entry(name)?.jelly.notes.push(serde_json::json!({"text": text, "by": by, "at_ms": now_ms()}));
        self.commit(name, EventKind::Note, by, None, Some(Value::String(text.into())), Vec::new(), previous_entry)
    }

    /// Where a project keeps its DESIGN.md.
    pub fn project_dir(&self, project: &str) -> PathBuf {
        self.dir.join("projects").join(project)
    }

    pub fn home(&self) -> &Path {
        &self.dir
    }

    pub fn asset_dir(&self, name: &str) -> PathBuf {
        self.dir.join("assets").join(crate::disk::name(name))
    }

    /// Saves a file next to a jelly, outside its state and undo history.
    pub fn put_asset(&mut self, name: &str, file: &str, bytes: &[u8], by: &str) -> Result<Event, Error> {
        valid_file(file)?;
        let dir = self.asset_dir(name);
        let previous_entry = self.jellies.get(name).cloned();
        self.entry(name)?;
        let path = dir.join(file);
        let previous = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Error::Invalid(format!("could not read {file} before saving: {e}"))),
        };
        fs::create_dir_all(&dir).and_then(|_| write_atomic(&path, bytes)).map_err(|e| Error::Invalid(format!("could not save {file}: {e}")))?;
        let result = self.commit(name, EventKind::Asset, by, None, Some(serde_json::json!({"file": file, "bytes": bytes.len()})), Vec::new(), previous_entry);
        if let Err(error) = &result {
            let rollback = match previous {
                Some(bytes) => write_atomic(&path, &bytes),
                None => fs::remove_file(&path),
            };
            rollback.map_err(|e| Error::Invalid(format!("{error}; could not restore {file}: {e}")))?;
        }
        result
    }

    pub fn assets(&self, name: &str) -> Vec<String> {
        self.try_assets(name).unwrap_or_default()
    }

    pub fn try_assets(&self, name: &str) -> std::io::Result<Vec<String>> {
        let entries = match fs::read_dir(self.asset_dir(name)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for entry in entries {
            if let Ok(file) = entry?.file_name().into_string() {
                if !file.starts_with('.') { out.push(file); }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Puts something in front of the person: `{"kind": "ask"|"look", "title",
    /// "context"?, "anchor"?, "options"?, "recommend"?, "why"?, "group"?}`.
    pub fn attend(&mut self, name: &str, spec: Value, by: &str) -> Result<Event, Error> {
        let n = self.get(name)?.attention.len();
        let item = attention_from(&spec, format!("a{}", n + 1), by)?;
        let args = serde_json::to_value(&item).expect("serializes");
        let previous_entry = self.jellies.get(name).cloned();
        self.entry(name)?.jelly.attention.push(item);
        self.commit(name, EventKind::Attention, by, None, Some(args), Vec::new(), previous_entry)
    }

    /// One round of an ask: a set of questions the person answers together, on
    /// a jelly made for it (created with `view` and `verbs` if it does not
    /// exist). Asking again starts a new round and withdraws what the last
    /// round left open. All or nothing: a bad question changes nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn ask_round(&mut self, name: &str, spec: &Value, view: &str, verbs_src: &str, project: &str, by: &str) -> Result<Vec<Event>, BatchError> {
        let single = spec.get("questions").is_none();
        let questions: Vec<Value> = if single { vec![spec.clone()] } else { spec["questions"].as_array().cloned().unwrap_or_default() };
        if questions.is_empty() {
            return Err(Error::Invalid("an ask needs at least one question".into()).into());
        }
        let title = spec["title"].as_str().or(spec["question"].as_str()).unwrap_or("").to_string();
        let id = if project.is_empty() || name.contains(':') { name.to_string() } else { format!("{project}:{name}") };
        let exists = self.jellies.contains_key(&id);
        let round = if exists { self.get(&id)?.state["round"].as_u64().unwrap_or(0) + 1 } else { 1 };
        let group = format!("round-{round}");
        let start = if exists { self.get(&id)?.attention.len() } else { 0 };
        // Validate every question before touching anything.
        let mut items = Vec::new();
        for (i, q) in questions.iter().enumerate() {
            let qid = q["id"].as_str().map(String::from).unwrap_or_else(|| format!("q{}", i + 1));
            let mut q = q.clone();
            q["kind"] = "ask".into();
            q["title"] = q.get("question").cloned().or_else(|| q.get("title").cloned()).unwrap_or(Value::Null);
            q["group"] = group.clone().into();
            q["anchor"] = serde_json::json!({"selector": format!("#q-{qid}")});
            items.push(attention_from(&q, format!("a{}", start + i + 1), by)?);
        }
        let mut events = Vec::new();
        let result = (|| -> Result<(), Error> {
            if !exists {
                events.push(self.create(&id, view.into(), verbs_src.into(), serde_json::json!({}), by, false, &title, "")?);
            } else if self.get(&id)?.view != view {
                events.push(self.reshape(&id, Some(view.into()), Some(verbs_src.into()), Some(title.clone()), by)?);
            }
            let open: Vec<String> = self.get(&id)?.attention.iter().filter(|a| a.is_open()).map(|a| a.id.clone()).collect();
            for a in open {
                events.push(self.withdraw(&id, &a, by)?);
            }
            let setup = serde_json::json!({"title": title, "context": spec["context"], "round": round});
            events.push(self.call(&id, "setup", setup, by)?);
            for item in items {
                let args = serde_json::to_value(&item).expect("serializes");
                let previous_entry = self.jellies.get(&id).cloned();
                self.entry(&id)?.jelly.attention.push(item);
                events.push(self.commit(&id, EventKind::Attention, by, None, Some(args), Vec::new(), previous_entry)?);
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(events),
            Err(error) => Err(BatchError { events, error }),
        }
    }

    /// The person answers an item, or changes their answer: an option id,
    /// text, or both. A look can be answered with nothing, meaning "seen".
    pub fn answer(&mut self, name: &str, id: &str, option: Option<&str>, text: &str, by: &str) -> Result<Event, Error> {
        let previous_entry = self.jellies.get(name).cloned();
        let entry = self.entry(name)?;
        let item = entry.jelly.attention.iter_mut().find(|a| a.id == id).ok_or_else(|| Error::Invalid(format!("{name} has no attention item {id}")))?;
        if item.withdrawn.is_some() {
            return Err(Error::Invalid(format!("{id} was withdrawn")));
        }
        if let Some(o) = option {
            if !item.options.iter().any(|x| x.id == o) {
                return Err(Error::Invalid(format!("{o:?} is not an option of {id}")));
            }
        }
        let text = text.trim();
        if item.kind == AttentionKind::Ask && option.is_none() && text.is_empty() {
            return Err(Error::Invalid("an ask needs an option or some text".into()));
        }
        item.answer = Some(Answer { option: option.map(String::from), text: text.into(), by: by.into(), at_ms: now_ms() });
        let args = serde_json::json!({"id": id, "kind": item.kind, "title": item.title, "option": option, "text": text, "group": item.group});
        self.commit(name, EventKind::Answer, by, None, Some(args), Vec::new(), previous_entry)
    }

    /// The agent takes an item back, for example because it no longer applies.
    pub fn withdraw(&mut self, name: &str, id: &str, by: &str) -> Result<Event, Error> {
        let previous_entry = self.jellies.get(name).cloned();
        let entry = self.entry(name)?;
        let item = entry.jelly.attention.iter_mut().find(|a| a.id == id).ok_or_else(|| Error::Invalid(format!("{name} has no attention item {id}")))?;
        item.withdrawn = Some(Stamp { by: by.into(), at_ms: now_ms() });
        self.commit(name, EventKind::Withdraw, by, None, Some(Value::String(id.into())), Vec::new(), previous_entry)
    }

    /// `Some("rebuilding the header")` while an agent works on the jelly, `None` when done.
    pub fn set_working(&mut self, name: &str, what: Option<&str>, by: &str) -> Result<Event, Error> {
        let previous_entry = self.jellies.get(name).cloned();
        self.entry(name)?.jelly.working = what.map(|w| serde_json::json!({"what": w, "by": by, "at_ms": now_ms()}));
        self.commit(name, EventKind::Working, by, None, what.map(|w| Value::String(w.into())), Vec::new(), previous_entry)
    }

    pub fn pin(&mut self, name: &str, pinned: bool, by: &str) -> Result<Event, Error> {
        let previous_entry = self.jellies.get(name).cloned();
        self.entry(name)?.jelly.pinned = pinned;
        self.commit(name, EventKind::Pin, by, None, Some(Value::Bool(pinned)), Vec::new(), previous_entry)
    }

    pub fn remove(&mut self, name: &str, by: &str) -> Result<Event, Error> {
        let jelly = self.get(name)?.clone();
        let ev = self.event(&jelly, EventKind::Remove, by, None, None, Vec::new());
        self.log(&ev)?;
        self.jellies.remove(name);
        let _ = fs::remove_file(self.path(name));
        let _ = fs::remove_dir_all(self.asset_dir(name));
        Ok(ev)
    }

    fn entry(&mut self, name: &str) -> Result<&mut Entry, Error> {
        self.jellies.get_mut(name).ok_or_else(|| Error::NotFound(name.into()))
    }

    /// Bumps the version, saves the jelly, logs the event and returns it with state attached.
    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        name: &str,
        kind: EventKind,
        by: &str,
        verb: Option<String>,
        args: Option<Value>,
        changed: Vec<String>,
        previous: Option<Entry>,
    ) -> Result<Event, Error> {
        let entry = self.jellies.get_mut(name).expect("caller checked");
        if kind != EventKind::New {
            entry.jelly.version += 1;
        }
        entry.jelly.updated_ms = now_ms();
        if kind != EventKind::Archive {
            entry.jelly.archived = false;
        }
        let bytes = serde_json::to_vec(entry).expect("a jelly serializes");
        let jelly = entry.jelly.clone();
        if let Err(error) = write_atomic(&self.path(name), &bytes) {
            self.restore_entry(name, previous);
            return Err(Error::Invalid(format!("could not save {name}: {error}")));
        }
        let mut ev = self.event(&jelly, kind, by, verb, args, changed);
        if let Err(error) = self.log(&ev) {
            let bytes = previous.as_ref().map(|entry| serde_json::to_vec(entry).expect("a jelly serializes"));
            self.restore_entry(name, previous);
            let rollback = match bytes {
                Some(bytes) => write_atomic(&self.path(name), &bytes),
                None => fs::remove_file(self.path(name)),
            };
            rollback.map_err(|e| Error::Invalid(format!("{error}; could not restore {name}: {e}")))?;
            return Err(error);
        }
        ev.state = Some(jelly.state);
        Ok(ev)
    }

    fn restore_entry(&mut self, name: &str, previous: Option<Entry>) {
        match previous {
            Some(entry) => { self.jellies.insert(name.into(), entry); }
            None => { self.jellies.remove(name); }
        }
    }

    fn event(&mut self, j: &Jelly, kind: EventKind, by: &str, verb: Option<String>, args: Option<Value>, changed: Vec<String>) -> Event {
        self.seq += 1;
        Event {
            seq: self.seq,
            at_ms: now_ms(),
            jelly: j.name.clone(),
            kind,
            by: by.into(),
            verb,
            args,
            changed,
            source: self.source.take(),
            version: j.version,
            view_rev: j.view_rev,
            state: None,
        }
    }

    fn log(&mut self, ev: &Event) -> Result<(), Error> {
        let append = || -> std::io::Result<(u64, usize)> {
            // Opened for write, not append: Windows denies `set_len` on an append-only handle,
            // and that is how a failed append is rolled back below.
            let mut file = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(self.events_path())?;
            let len = file.metadata()?.len();
            if len > 0 {
                file.seek(SeekFrom::End(-1))?;
                let mut last = [0];
                file.read_exact(&mut last)?;
                if last != [b'\n'] {
                    return Err(std::io::Error::other("incomplete event history; repair events.jsonl before retrying"));
                }
            }
            file.seek(SeekFrom::End(0))?;
            let mut line = serde_json::to_vec(ev).expect("an event serializes");
            line.push(b'\n');
            #[cfg(test)]
            let write = if let Some((_, bytes)) = self.log_failure.filter(|(seq, _)| *seq == ev.seq) {
                file.write_all(&line[..bytes.min(line.len())]).and_then(|()| Err(std::io::Error::other("injected partial append")))
            } else { file.write_all(&line) };
            #[cfg(not(test))]
            let write = file.write_all(&line);
            if let Err(error) = write {
                // A failed append must not leave a fragment before the next event.
                file.set_len(len).map_err(|rollback| std::io::Error::other(format!("{error}; could not remove partial record: {rollback}; repair events.jsonl before retrying")))?;
                return Err(error);
            }
            Ok((len, line.len()))
        };
        let (offset, length) = append().map_err(|e| Error::Invalid(format!("could not record event for {}: {e}", ev.jelly)))?;
        self.timestamps.insert((ev.at_ms, ev.seq), (offset, length));
        self.recent.entry(ev.jelly.clone()).or_default().push(ev.clone());
        if offset + length as u64 > LOG_LIMIT {
            if let Err(e) = self.trim_events(LOG_KEEP, PERSON_CARRY) {
                let _ = writeln!(std::io::stderr().lock(), "could not trim the event log: {e}");
            }
        }
        Ok(())
    }

    pub fn sequence(&self) -> u64 {
        self.seq
    }

    pub(crate) fn trim_events(&mut self, keep: usize, carry: usize) -> std::io::Result<()> {
        trim_log(&self.events_path(), keep, carry)?;
        let (_, recent, timestamps) = read_event_history(&self.events_path())?;
        self.recent = recent;
        self.timestamps = timestamps;
        Ok(())
    }

    /// Last retained change and the person's last eight events, oldest first.
    /// Built during startup validation and maintained on append and trim so a
    /// see never searches through unrelated history.
    pub fn recent_events(&self, name: &str) -> (Option<Event>, Vec<Event>) {
        self.recent.get(name).map(|r| (r.last.clone(), r.person.clone())).unwrap_or_default()
    }

    /// Timestamp order can differ from append order after a clock step. Index byte
    /// ranges so time windows read only matching records, then restore seq order.
    pub fn events_since(&self, since: u64) -> std::io::Result<Vec<Event>> {
        let mut file = match fs::File::open(self.events_path()) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.timestamps.is_empty() => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "event log is not a file"));
        }
        let mut events = Vec::new();
        for (&(at_ms, seq), &(offset, length)) in self.timestamps.range((std::ops::Bound::Excluded((since, u64::MAX)), std::ops::Bound::Unbounded)) {
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; length];
            file.read_exact(&mut bytes)?;
            let event: Event = serde_json::from_slice(&bytes)?;
            if !bytes.ends_with(b"\n") || (event.at_ms, event.seq) != (at_ms, seq) {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "event log no longer matches its timestamp index"));
            }
            events.push(event);
        }
        events.sort_unstable_by_key(|e| e.seq);
        Ok(events)
    }

    pub fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    /// Hands the lines of `events.jsonl` to `visit`, newest first, until it
    /// returns false or fails. Reads only as far back as `visit` asks for.
    pub fn events_back(&self, mut visit: impl FnMut(&str) -> std::io::Result<bool>) -> std::io::Result<()> {
        let mut file = match fs::File::open(self.events_path()) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        // The start of a line whose beginning is in an earlier chunk.
        let mut pending = Vec::new();
        let mut end = file.metadata()?.len();
        let mut newest = true;
        while end > 0 {
            let start = end.saturating_sub(LOG_CHUNK);
            let mut data = vec![0; (end - start) as usize];
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut data)?;
            data.extend_from_slice(&pending);
            if std::mem::take(&mut newest) && data.ends_with(b"\n") {
                data.pop();
            }
            let mut lines = data.split(|&b| b == b'\n');
            let partial = if start > 0 { lines.next() } else { None };
            for line in lines.rev() {
                let line = std::str::from_utf8(line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                if !visit(line)? {
                    return Ok(());
                }
            }
            pending = partial.unwrap_or_default().to_vec();
            end = start;
        }
        Ok(())
    }

    fn inbox_cursors(&self) -> std::io::Result<Vec<InboxCursor>> {
        match fs::read(self.dir.join("inbox-readers.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn read_inbox(&self, project: &str, reader: &InboxReader) -> Result<InboxBatch, Error> {
        self.read_inbox_at(project, reader, now_ms())
    }

    fn read_inbox_at(&self, project: &str, reader: &InboxReader, at_ms: u64) -> Result<InboxBatch, Error> {
        let read = || -> std::io::Result<InboxBatch> {
            let mut cursors = self.inbox_cursors()?;
            let existing = cursors.iter().find(|c| c.project == project && &c.reader == reader);
            let initial = InboxPosition::Recent { since_ms: at_ms.saturating_sub(3_600_000) };
            let position = existing.map_or(&initial, |c| &c.position);
            let mut through = match position { InboxPosition::After { seq } => *seq, InboxPosition::Recent { .. } => self.seq };
            let events = match position {
                InboxPosition::Recent { since_ms } => self.events_since(*since_ms)?.into_iter()
                    .filter(|event| event.is_inbox_item(project)).map(InboxItem::from).collect(),
                InboxPosition::After { seq } => {
                    let mut events = Vec::new();
                    self.events_back(|line| {
                        let event: Event = serde_json::from_str(line)?;
                        through = through.max(event.seq);
                        if event.seq <= *seq { return Ok(false); }
                        if event.is_inbox_item(project) { events.push(event.into()); }
                        Ok(true)
                    })?;
                    events.reverse();
                    events
                }
            };
            if existing.is_none() {
                // Fix the initial window so an unacknowledged response can be retried even an hour later.
                cursors.push(InboxCursor { project: project.into(), reader: reader.clone(), position: initial });
                write_atomic(&self.dir.join("inbox-readers.json"), &serde_json::to_vec(&cursors)?)?;
            }
            Ok(InboxBatch { events, through })
        };
        read().map_err(|e| Error::Invalid(format!("could not read inbox: {e}")))
    }

    pub fn commit_inbox(&mut self, req: InboxCommit) -> Result<(), Error> {
        let commit = || -> std::io::Result<()> {
            let mut cursors = self.inbox_cursors()?;
            let InboxRequest { project, reader } = req.inbox;
            if let Some(cursor) = cursors.iter_mut().find(|c| c.project == project && c.reader == reader) {
                let seq = match cursor.position {
                    InboxPosition::After { seq } => seq.max(req.through),
                    InboxPosition::Recent { .. } => req.through,
                };
                cursor.position = InboxPosition::After { seq };
            } else {
                cursors.push(InboxCursor { project, reader, position: InboxPosition::After { seq: req.through } });
            }
            write_atomic(&self.dir.join("inbox-readers.json"), &serde_json::to_vec(&cursors)?)
        };
        commit().map_err(|e| Error::Invalid(format!("could not commit inbox progress: {e}")))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join("jellies").join(format!("{}.json", crate::disk::name(name)))
    }
}

/// Keeps `step` for undo, then drops the oldest steps past the depth or the byte budget.
fn push_step(entry: &mut Entry, step: Step) {
    let steps = &mut entry.steps;
    steps.push(step);
    let budget = undo_budget(&entry.jelly.state);
    let mut drop = steps.len().saturating_sub(UNDO_DEPTH);
    let mut total: usize = steps[drop..].iter().map(Step::budget_bytes).sum();
    while drop + 1 < steps.len() && total > budget {
        total -= steps[drop].budget_bytes();
        drop += 1;
    }
    steps.drain(..drop);
}

fn undo_budget(state: &Value) -> usize {
    2 * serde_json::to_vec(state).map_or(0, |b| b.len()) + UNDO_SLACK
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).expect("a state serializes")
}

/// The hunks that turn the pretty-printed `after` back into `before`, by
/// line, so a push that shifts an array costs the lines that moved in and out.
fn state_step(after: &Value, before: &Value) -> Vec<Hunk> {
    if after == before {
        return Vec::new();
    }
    let (a, b) = (pretty(after), pretty(before));
    let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let ops = similar::capture_diff_slices_deadline(similar::Algorithm::Myers, &a, &b, Some(Instant::now() + DIFF_DEADLINE));
    ops.iter().filter_map(|op| {
        let (at, remove, from, len) = match *op {
            similar::DiffOp::Equal { .. } => return None,
            similar::DiffOp::Delete { old_index, old_len, new_index } => (old_index, old_len, new_index, 0),
            similar::DiffOp::Insert { old_index, new_index, new_len } => (old_index, 0, new_index, new_len),
            similar::DiffOp::Replace { old_index, old_len, new_index, new_len } => (old_index, old_len, new_index, new_len),
        };
        Some(Hunk { at, remove, lines: b[from..from + len].iter().map(|l| l.to_string()).collect() })
    }).collect()
}

/// Applies `state_step`'s hunks to `current`, giving the state before the change.
fn restore(current: &Value, hunks: &[Hunk]) -> Result<Value, String> {
    if hunks.is_empty() {
        return Ok(current.clone());
    }
    let text = pretty(current);
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut pos = 0;
    for h in hunks {
        if h.at < pos || h.at + h.remove > lines.len() {
            return Err(format!("a step does not fit the state (line {} of {})", h.at, lines.len()));
        }
        out.extend(&lines[pos..h.at]);
        out.extend(h.lines.iter().map(String::as_str));
        pos = h.at + h.remove;
    }
    out.extend(&lines[pos..]);
    serde_json::from_str(&out.join("\n")).map_err(|e| e.to_string())
}

/// Rewrites the log as its newest `keep` bytes of whole lines (at least the
/// newest line), preceded by up to `carry` bytes of the person's older events.
/// Lines are copied as they are, so `seq` still ascends through the file.
fn trim_log(path: &Path, keep: usize, carry: usize) -> std::io::Result<()> {
    let text = fs::read(path)?;
    let body = text.strip_suffix(b"\n").unwrap_or(&text);
    let from = text.len().saturating_sub(keep);
    let split = match from {
        0 => 0,
        _ => text[from - 1..].iter().position(|&b| b == b'\n').map(|i| from + i).filter(|&i| i < body.len())
            .unwrap_or_else(|| body.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)),
    };
    let needle = format!("\"by\":\"{PERSON}\"");
    let mut carried = Vec::new();
    let mut room = carry;
    for line in text[..split].split(|&b| b == b'\n').rev() {
        let theirs = line.windows(needle.len()).any(|w| w == needle.as_bytes())
            && serde_json::from_slice::<Event>(line).is_ok_and(|e| e.by == PERSON);
        if theirs {
            if line.len() + 1 > room {
                break;
            }
            room -= line.len() + 1;
            carried.push(line);
        }
    }
    let mut out = Vec::with_capacity(carry - room + text.len() - split + 1);
    for line in carried.iter().rev() {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.extend_from_slice(&text[split..]);
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    write_atomic(path, &out)
}

/// Puts `value` at a JSON pointer (adding an object key or appending to an
/// array is fine), or removes what is there when `value` is None.
fn set_at(root: &mut Value, pointer: &str, value: Option<Value>) -> Result<(), String> {
    if pointer.is_empty() {
        *root = value.unwrap_or_else(|| Value::Object(Default::default()));
        return Ok(());
    }
    let (parent, last) = pointer.rsplit_once('/').ok_or_else(|| format!("{pointer:?} is not a JSON pointer; start it with /"))?;
    let key = last.replace("~1", "/").replace("~0", "~");
    match root.pointer_mut(parent).ok_or_else(|| format!("nothing at {}", if parent.is_empty() { "/" } else { parent }))? {
        Value::Object(m) => match value {
            Some(v) => { m.insert(key, v); }
            None => { m.remove(&key).ok_or_else(|| format!("nothing at {pointer}"))?; }
        },
        Value::Array(a) => {
            let i = if key == "-" { a.len() } else { key.parse::<usize>().map_err(|_| format!("{key:?} is not an array index"))? };
            match value {
                Some(v) if i < a.len() => a[i] = v,
                Some(v) if i == a.len() => a.push(v),
                None if i < a.len() => { a.remove(i); }
                _ => return Err(format!("{pointer} is past the end of the array")),
            }
        }
        _ => return Err(format!("{parent} is neither an object nor an array")),
    }
    Ok(())
}

/// Names become file names and URL segments.
/// Builds an attention item from an agent's spec, checking it.
fn attention_from(spec: &Value, id: String, by: &str) -> Result<Attention, Error> {
    let kind = match spec["kind"].as_str().unwrap_or("ask") {
        "ask" => AttentionKind::Ask,
        "look" => AttentionKind::Look,
        k => return Err(Error::Invalid(format!("attention kind must be ask or look, not {k:?}"))),
    };
    let title = spec["title"].as_str().unwrap_or("").trim().to_string();
    if title.is_empty() {
        return Err(Error::Invalid("attention needs a title: what you want from the person".into()));
    }
    let options: Vec<AskOption> = serde_json::from_value(spec.get("options").cloned().unwrap_or(Value::Array(vec![])))
        .map_err(|e| Error::Invalid(format!("{title:?}: options: {e}")))?;
    let mut ids = std::collections::HashSet::new();
    for o in &options {
        if o.id.is_empty() {
            return Err(Error::Invalid(format!("{title:?}: every option needs an id")));
        }
        if !ids.insert(o.id.as_str()) {
            return Err(Error::Invalid(format!("{title:?}: option id {:?} appears twice", o.id)));
        }
    }
    let recommend = spec["recommend"].as_str().map(String::from);
    if let Some(r) = &recommend {
        if !ids.contains(r.as_str()) {
            return Err(Error::Invalid(format!("{title:?}: recommend {r:?} is not one of the options")));
        }
    }
    let text = |k: &str| spec[k].as_str().unwrap_or("").to_string();
    Ok(Attention {
        id,
        kind,
        title,
        context: if spec.get("context").is_some_and(|c| c.is_string()) { text("context") } else { text("detail") },
        anchor: spec.get("anchor").cloned().unwrap_or(Value::Null),
        options,
        recommend,
        why: text("why"),
        group: spec["group"].as_str().map(String::from),
        by: by.into(),
        at_ms: now_ms(),
        answer: None,
        withdrawn: None,
    })
}

/// `name` or `project:name`, each part 1-64 of a-z, 0-9, - and _.
fn valid_name(name: &str) -> Result<(), Error> {
    let part = |p: &str| !p.is_empty() && p.len() <= 64 && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    let ok = match name.split_once(':') {
        Some((project, rest)) => part(project) && part(rest),
        None => part(name),
    };
    if ok { Ok(()) } else { Err(Error::Invalid(format!("bad name {name:?}: use 1-64 of a-z, 0-9, - and _, optionally as project:name"))) }
}

fn valid_file(file: &str) -> Result<(), Error> {
    let ok = !file.is_empty() && file.len() <= 128 && !file.starts_with('.')
        && file.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !ok { return Err(Error::Invalid(format!("bad file name {file:?}: use letters, digits, '.', '-' and '_'"))); }
    // Windows treats these stems as devices whatever the extension: `NUL` reports success and keeps nothing.
    let stem = file.split('.').next().unwrap_or("").to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if cfg!(windows) && device {
        return Err(Error::Invalid(format!("bad file name {file:?}: {stem} is a reserved name on Windows")));
    }
    Ok(())
}

impl Jelly {
    /// The project part of `project:name`, or "".
    pub fn project(&self) -> &str {
        self.name.split_once(':').map_or("", |(p, _)| p)
    }
}

/// Collects the JSON pointers (RFC 6901) where `a` and `b` differ.
pub fn diff(a: &Value, b: &Value, path: &mut String, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                let len = path.len();
                path.push('/');
                path.push_str(&k.replace('~', "~0").replace('/', "~1"));
                diff(x.get(k).unwrap_or(&Value::Null), y.get(k).unwrap_or(&Value::Null), path, out);
                if x.contains_key(k) != y.contains_key(k) && !out.contains(path) {
                    out.push(path.clone());
                }
                path.truncate(len);
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                let len = path.len();
                path.push_str(&format!("/{i}"));
                diff(p, q, path, out);
                path.truncate(len);
            }
        }
        _ if a != b => out.push(path.clone()),
        _ => {}
    }
}

fn read_optional(path: &Path) -> std::io::Result<String> {
    match fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        result => result,
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const VERBS: &str = "const verbs = { label(s, { id, value }) { s.labels[id] = value; } };";

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        (dir, s)
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_saved_entries_can_commit_and_roll_back() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, mut s) = store();
        s.create("j", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        s.attend("j", json!({"title": "Which?"}), "agent").unwrap();
        s.call("j", "label", json!({"id": "one", "value": "kept"}), "agent").unwrap();
        let path = s.path("j");
        let before = fs::read(&path).unwrap();
        for operation in ["answer", "call", "undo"] {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
            assert!(fs::read(&path).is_err(), "fixture must deny snapshot reads");
            s.log_failure = Some((s.seq + 1, 10));
            let result = match operation {
                "answer" => s.answer("j", "a1", None, "yes", "person"),
                "call" => s.call("j", "label", json!({"id": "one", "value": "lost"}), "person"),
                "undo" => s.undo("j", "person"),
                _ => unreachable!(),
            };
            assert!(result.unwrap_err().to_string().contains("could not record event"));
            assert_eq!(serde_json::to_vec(s.jellies.get("j").unwrap()).unwrap(), before);
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        s.log_failure = None;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
        let answer = s.answer("j", "a1", None, "yes", "person").unwrap();
        s.note("j", "later change", "agent").unwrap();
        assert!(s.get("j").unwrap().attention[0].answer.is_some());
        let history = fs::read_to_string(s.events_path()).unwrap();
        let answers: Vec<Event> = history.lines().map(|line| serde_json::from_str::<Event>(line).unwrap())
            .filter(|event| event.kind == EventKind::Answer).collect();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].seq, answer.seq);
    }

    #[test]
    fn failed_log_preserves_compact_undo_and_event_indexes() {
        let (_dir, mut s) = store();
        s.create("j", "original".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        s.call("j", "label", json!({"id": "one", "value": "kept"}), "person").unwrap();
        let before = fs::read(s.path("j")).unwrap();
        let history = serde_json::to_value(s.events_since(0).unwrap()).unwrap();
        let recent = serde_json::to_value(s.recent_events("j")).unwrap();
        for operation in ["call", "reshape", "set", "undo"] {
            s.log_failure = Some((s.seq + 1, 10));
            let result = match operation {
                "call" => s.call("j", "label", json!({"id": "one", "value": "lost"}), "person"),
                "reshape" => s.reshape("j", Some("replacement".into()), Some("const verbs = {};".into()), None, "person"),
                "set" => s.set_state("j", "", Some(json!({})), "person"),
                "undo" => s.undo("j", "person"),
                _ => unreachable!(),
            };
            assert!(result.unwrap_err().to_string().contains("could not record event"));
            assert_eq!(fs::read(s.path("j")).unwrap(), before);
            assert_eq!(serde_json::to_value(s.events_since(0).unwrap()).unwrap(), history);
            assert_eq!(serde_json::to_value(s.recent_events("j")).unwrap(), recent);
        }
        s.log_failure = None;
        let undone = s.undo("j", "person").unwrap();
        assert_eq!(undone.state, Some(json!({"labels": {}})));
        assert_eq!(s.get("j").unwrap().view, "original");
        assert_eq!(s.get("j").unwrap().verbs, VERBS);
        let events = s.events_since(0).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events.last().unwrap().seq, undone.seq);
        let (last, person) = s.recent_events("j");
        assert_eq!(last.unwrap().seq, undone.seq);
        assert_eq!(person.len(), 2);
        assert_eq!(person.last().unwrap().seq, undone.seq);
    }

    #[test]
    fn failed_log_restores_creation_replacement_move_and_assets() {
        let (dir, mut s) = store();
        let created = s.create("j", "original".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        fs::create_dir_all(s.asset_dir("j")).unwrap();
        fs::write(s.asset_dir("j").join("file.txt"), "kept").unwrap();
        let path = s.events_path();
        let original = fs::read(&path).unwrap();
        for operation in ["create", "replace", "move", "new_asset", "replace_asset"] {
            s.log_failure = Some((s.seq + 1, 10));
            let result = match operation {
                "create" => s.create("new", "".into(), VERBS.into(), json!({}), "agent", false, "", ""),
                "replace" => s.create("j", "replacement".into(), VERBS.into(), json!({}), "agent", true, "", ""),
                "move" => s.set_project("j", "other", "agent"),
                "new_asset" => s.put_asset("j", "new.txt", b"new", "agent"),
                "replace_asset" => s.put_asset("j", "file.txt", b"replacement", "agent"),
                _ => unreachable!(),
            };
            assert!(result.is_err());
            assert_eq!(s.get("j").unwrap().view, "original");
            assert_eq!(s.get("j").unwrap().version, created.version);
            assert!(s.get("new").is_err());
            assert!(s.get("other:j").is_err());
            let reopened = Store::open(dir.path()).unwrap();
            assert_eq!(reopened.get("j").unwrap().view, "original");
            assert!(reopened.get("new").is_err());
            assert!(reopened.get("other:j").is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
            assert_eq!(fs::read_to_string(s.asset_dir("j").join("file.txt")).unwrap(), "kept");
            assert!(!s.asset_dir("j").join("new.txt").exists());
        }
    }

    fn consume_inbox(s: &mut Store, project: &str, reader: InboxReader) -> Result<Vec<Event>, Error> {
        let batch = s.read_inbox(project, &reader)?;
        s.commit_inbox(InboxCommit { inbox: InboxRequest { project: project.into(), reader }, through: batch.through })?;
        Ok(batch.events.into_iter().map(|item| item.event).collect())
    }

    #[test]
    fn inbox_startup_refuses_unreadable_event_log() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("events.jsonl")).unwrap();
        assert!(Store::open(dir.path()).is_err());
        fs::remove_dir(dir.path().join("events.jsonl")).unwrap();
        fs::write(dir.path().join("events.jsonl"), [0xff]).unwrap();
        assert_eq!(Store::open(dir.path()).err().unwrap().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn inbox_startup_refuses_unterminated_event_log() {
        let (dir, mut s) = store();
        s.create("sample", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "p").unwrap();
        let reader = InboxReader::Named { name: "existing".into() };
        consume_inbox(&mut s, "p", reader).unwrap();
        let saved = fs::read(dir.path().join("inbox-readers.json")).unwrap();
        let log = fs::read_to_string(s.events_path()).unwrap();
        let record = log.trim_end();
        for prefix in ["", log.as_str()] {
            for suffix in ["", "\r"] {
                let unterminated = format!("{prefix}{record}{suffix}");
                fs::write(s.events_path(), &unterminated).unwrap();
                let error = Store::open(dir.path()).err().expect("unterminated logs must prevent startup");
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                assert_eq!(fs::read_to_string(s.events_path()).unwrap(), unterminated);
                assert_eq!(fs::read(dir.path().join("inbox-readers.json")).unwrap(), saved);
            }
        }
    }

    #[test]
    fn inbox_accepted_log_remains_readable_after_append_and_restart() {
        for ending in [None, Some("\n"), Some("\r\n")] {
            let (dir, mut s) = store();
            s.create("sample", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "p").unwrap();
            let log = fs::read_to_string(s.events_path()).unwrap();
            let log = ending.map_or_else(String::new, |ending| format!("{}{ending}", log.trim_end()));
            fs::write(s.events_path(), log).unwrap();
            let mut s = Store::open(dir.path()).unwrap();
            let readers = ["a", "b"].map(|name| InboxReader::Named { name: name.into() });
            for reader in &readers {
                assert!(consume_inbox(&mut s, "p", reader.clone()).unwrap().is_empty());
            }
            let event = s.call("p:sample", "label", json!({"id": "a", "value": "x"}), PERSON).unwrap();
            let mut s = Store::open(dir.path()).unwrap();
            for reader in &readers {
                let events = consume_inbox(&mut s, "p", reader.clone()).unwrap();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].seq, event.seq);
            }
            let mut s = Store::open(dir.path()).unwrap();
            for reader in readers {
                assert!(consume_inbox(&mut s, "p", reader).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn inbox_startup_validates_every_event_before_recovering_sequence() {
        let (dir, mut s) = store();
        s.create("sample", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "p").unwrap();
        let mut event = s.call("p:sample", "label", json!({"id": "a", "value": "x"}), PERSON).unwrap();
        event.state = None;
        let reader = InboxReader::Named { name: "existing".into() };
        consume_inbox(&mut s, "p", reader.clone()).unwrap();
        let saved = fs::read(dir.path().join("inbox-readers.json")).unwrap();
        let valid = serde_json::to_value(&event).unwrap();
        let mut damaged = vec!["{".to_string(), "".into()];
        let mut missing_seq = valid.clone();
        missing_seq.as_object_mut().unwrap().remove("seq");
        damaged.push(missing_seq.to_string());
        for seq in [json!(null), json!(-1), json!(1.5), json!("2")] {
            let mut record = valid.clone();
            record["seq"] = seq;
            damaged.push(record.to_string());
        }
        damaged.push(valid.to_string().replace(&format!("\"seq\":{}", event.seq), "\"seq\":18446744073709551616"));
        for field in ["at_ms", "jelly", "kind", "by", "version", "view_rev"] {
            let mut record = valid.clone();
            record.as_object_mut().unwrap().remove(field);
            damaged.push(record.to_string());
        }
        let mut unknown_kind = valid.clone();
        unknown_kind["kind"] = json!("unknown");
        damaged.push(unknown_kind.to_string());
        for record in damaged {
            fs::write(s.events_path(), format!("{valid}\n{record}\n")).unwrap();
            let error = Store::open(dir.path()).err().expect("malformed logs must prevent startup");
            assert!(matches!(error.kind(), std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof), "{record}: {error}");
            assert_eq!(fs::read(dir.path().join("inbox-readers.json")).unwrap(), saved);
        }

        let mut older = event.clone();
        older.seq -= 1;
        fs::write(s.events_path(), format!("{valid}\n{}\n", serde_json::to_string(&older).unwrap())).unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        let next = s.call("p:sample", "label", json!({"id": "b", "value": "y"}), PERSON).unwrap();
        assert_eq!(next.seq, event.seq + 1);
        assert_eq!(consume_inbox(&mut s, "p", reader).unwrap()[0].seq, next.seq);
    }

    #[test]
    fn inbox_positions_preserve_filters_and_initial_window() {
        let (dir, mut s) = store();
        s.create("sample", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "p").unwrap();
        let mut event = s.call("p:sample", "label", json!({"id": "a", "value": "x"}), PERSON).unwrap();
        let reader = InboxReader::Named { name: "a".into() };
        let mut events = Vec::new();
        let mut push = |e: &Event| events.push(serde_json::to_string(e).unwrap());
        event.seq = 1;
        event.at_ms = now_ms() - 7_200_000;
        push(&event);
        event.seq = 2;
        event.at_ms = now_ms();
        push(&event);
        event.seq = 3;
        push(&event);
        event.seq = 4;
        event.by = "agent".into();
        push(&event);
        event.seq = 5;
        event.by = PERSON.into();
        event.jelly = PAGE.into();
        push(&event);
        event.seq = 6;
        event.jelly = "p:sample".into();
        event.kind = EventKind::Working;
        push(&event);
        event.seq = 7;
        event.jelly = "other:sample".into();
        event.kind = EventKind::Call;
        push(&event);
        fs::write(s.events_path(), events.join("\n") + "\n").unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        assert_eq!(consume_inbox(&mut s, "p", reader.clone()).unwrap().iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3]);
        assert!(consume_inbox(&mut s, "p", reader.clone()).unwrap().is_empty());
        assert_eq!(consume_inbox(&mut s, "", reader.clone()).unwrap().len(), 3);
        assert_eq!(consume_inbox(&mut s, "other", reader.clone()).unwrap().len(), 1);
        assert!(consume_inbox(&mut s, "missing", reader.clone()).unwrap().is_empty());
        let records: Vec<InboxCursor> = serde_json::from_slice(&fs::read(dir.path().join("inbox-readers.json")).unwrap()).unwrap();
        assert!(records.iter().all(|c| matches!(c.position, InboxPosition::After { seq: 7 })));
        event.seq = 8;
        event.jelly = "p:sample".into();
        event.at_ms = 1;
        let mut log = fs::OpenOptions::new().append(true).open(s.events_path()).unwrap();
        writeln!(log, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        let mut s = Store::open(dir.path()).unwrap();
        assert_eq!(consume_inbox(&mut s, "p", reader.clone()).unwrap()[0].seq, 8);
        assert!(consume_inbox(&mut s, "p", reader).unwrap().is_empty());
    }

    #[test]
    fn inbox_without_history_needs_no_event_log() {
        let (_dir, s) = store();
        assert!(!s.events_path().exists());
        assert!(s.events_since(0).unwrap().is_empty());
        let reader = InboxReader::Named { name: "new".into() };
        let batch = s.read_inbox("p", &reader).unwrap();
        assert!(batch.events.is_empty());
        assert_eq!(batch.through, 0);
    }

    #[test]
    fn inbox_initial_window_stays_fixed_until_commit() {
        let (dir, mut s) = store();
        s.create("sample", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "p").unwrap();
        let mut event = s.call("p:sample", "label", json!({"id": "a", "value": "x"}), PERSON).unwrap();
        event.state = None;
        let reader = InboxReader::Named { name: "retry".into() };
        let first_read_at = 7_200_000;
        let mut events = Vec::new();
        for (seq, at_ms) in [(1, first_read_at - 3_600_001), (2, first_read_at), (3, first_read_at - 3_599_999), (4, first_read_at - 3_600_002)] {
            event.seq = seq;
            event.at_ms = at_ms;
            events.push(serde_json::to_string(&event).unwrap());
        }
        fs::write(s.events_path(), events.join("\n") + "\n").unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert!(!dir.path().join("inbox-readers.json").exists());
        let path = s.events_path();
        let saved = fs::read(&path).unwrap();
        // Poison excluded records without moving byte ranges: a full scan must fail.
        let mut damaged = saved.clone();
        damaged[0] = 0xff;
        let last_start = saved[..saved.len() - 1].iter().rposition(|&b| b == b'\n').unwrap() + 1;
        damaged[last_start] = 0xff;
        fs::write(&path, damaged).unwrap();
        let first = s.read_inbox_at("p", &reader, first_read_at).unwrap();
        assert_eq!(first.events.iter().map(|item| item.event.seq).collect::<Vec<_>>(), [2, 3]);
        let retry_at = first_read_at + 3_600_001;
        let expected = serde_json::to_value(&first).unwrap();
        assert_eq!(serde_json::to_value(s.read_inbox_at("p", &reader, retry_at).unwrap()).unwrap(), expected);
        let empty_reader = InboxReader::Named { name: "empty".into() };
        assert!(s.read_inbox_at("p", &empty_reader, retry_at).unwrap().events.is_empty());
        fs::write(&path, saved).unwrap();
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        assert_eq!(serde_json::to_value(s.read_inbox_at("p", &reader, retry_at).unwrap()).unwrap(), expected);
        let new_reader = InboxReader::Named { name: "new".into() };
        assert!(s.read_inbox_at("p", &new_reader, retry_at).unwrap().events.is_empty());
        s.commit_inbox(InboxCommit { inbox: InboxRequest { project: "p".into(), reader: reader.clone() }, through: first.through }).unwrap();
        assert!(s.read_inbox_at("p", &reader, retry_at).unwrap().events.is_empty());
    }

    #[test]
    fn inbox_reads_back_only_to_the_readers_position() {
        let (_dir, mut s) = store();
        s.create("sample", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "p").unwrap();
        s.call("p:sample", "label", json!({"id": "a", "value": "x"}), "agent").unwrap();
        let reader = InboxReader::Named { name: "a".into() };
        consume_inbox(&mut s, "p", reader.clone()).unwrap();
        let event = s.call("p:sample", "label", json!({"id": "b", "value": "y"}), PERSON).unwrap();
        // Lines older than the position are never parsed, so damage there goes unread.
        let log = fs::read_to_string(s.events_path()).unwrap();
        let rest = log.split_once('\n').unwrap().1;
        fs::write(s.events_path(), format!("malformed\n{rest}")).unwrap();
        assert_eq!(consume_inbox(&mut s, "p", reader).unwrap().iter().map(|e| e.seq).collect::<Vec<_>>(), [event.seq]);
    }

    #[test]
    fn inbox_malformed_events_do_not_consume() {
        let (dir, mut s) = store();
        s.create("sample", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "p").unwrap();
        let existing = InboxReader::Named { name: "existing".into() };
        let new = InboxReader::Named { name: "new".into() };
        for project in ["p", ""] {
            consume_inbox(&mut s, project, existing.clone()).unwrap();
        }
        s.call("p:sample", "label", json!({"id": "a", "value": "x"}), PERSON).unwrap();
        s.call("p:sample", "label", json!({"id": "b", "value": "y"}), PERSON).unwrap();
        let path = dir.path().join("inbox-readers.json");
        let saved = fs::read(&path).unwrap();
        let log = fs::read_to_string(s.events_path()).unwrap();
        let mut lines: Vec<_> = log.lines().collect();
        lines[1] = "malformed";
        fs::write(s.events_path(), lines.join("\n") + "\n").unwrap();
        for project in ["p", ""] {
            for reader in [&existing, &new] {
                let error = consume_inbox(&mut s, project, reader.clone()).unwrap_err();
                assert!(error.to_string().contains("could not read inbox"));
                assert_eq!(fs::read(&path).unwrap(), saved);
            }
        }
        fs::write(s.events_path(), log).unwrap();
        for project in ["p", ""] {
            for reader in [&existing, &new] {
                assert_eq!(consume_inbox(&mut s, project, reader.clone()).unwrap().len(), 2);
                assert!(consume_inbox(&mut s, project, reader.clone()).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn inbox_storage_failures_do_not_consume() {
        let (dir, mut s) = store();
        s.create("sample", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "p").unwrap();
        let reader = InboxReader::Named { name: "a".into() };
        consume_inbox(&mut s, "p", reader.clone()).unwrap();
        s.call("p:sample", "label", json!({"id": "a", "value": "x"}), PERSON).unwrap();
        let path = dir.path().join("inbox-readers.json");
        let saved = fs::read(&path).unwrap();
        fs::write(&path, "broken").unwrap();
        assert!(consume_inbox(&mut s, "p", reader.clone()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "broken");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(consume_inbox(&mut s, "p", reader.clone()).is_err());
        fs::remove_dir(&path).unwrap();
        fs::write(&path, &saved).unwrap();
        let tmp = path.with_extension("json.tmp");
        fs::create_dir(&tmp).unwrap();
        assert!(consume_inbox(&mut s, "p", reader.clone()).is_err());
        assert_eq!(fs::read(&path).unwrap(), saved);
        fs::remove_dir(tmp).unwrap();
        let log = fs::read(s.events_path()).unwrap();
        fs::remove_file(s.events_path()).unwrap();
        fs::create_dir(s.events_path()).unwrap();
        assert!(consume_inbox(&mut s, "p", reader.clone()).is_err());
        assert_eq!(fs::read(&path).unwrap(), saved);
        fs::remove_dir(s.events_path()).unwrap();
        fs::write(s.events_path(), log).unwrap();
        assert_eq!(consume_inbox(&mut s, "p", reader).unwrap().len(), 1);
    }

    #[test]
    fn diff_names_changed_pointers() {
        let mut out = Vec::new();
        diff(
            &json!({"a": {"x": 1}, "list": [1, 2], "k/v": 1, "gone": true}),
            &json!({"a": {"x": 2}, "list": [1, 2, 3], "k/v": 2, "new": 0}),
            &mut String::new(),
            &mut out,
        );
        assert_eq!(out, vec!["/a/x", "/gone", "/k~1v", "/list", "/new"]);
    }

    #[test]
    fn agents_set_state_directly_and_can_undo_it() {
        let (_d, mut s) = store();
        s.create("doc", "".into(), "const verbs = {};".into(), json!({"title": "Old", "items": [1, 2], "stale": true}), "agent", false, "", "").unwrap();
        let ev = s.set_state("doc", "/title", Some(json!("New")), "agent").unwrap();
        assert_eq!((ev.kind, ev.changed.clone()), (EventKind::Set, vec!["/title".to_string()]));
        s.set_state("doc", "/stale", None, "agent").unwrap();
        s.set_state("doc", "/items/-", Some(json!(3)), "agent").unwrap();
        s.set_state("doc", "/items/0", None, "agent").unwrap();
        assert_eq!(s.get("doc").unwrap().state, json!({"title": "New", "items": [2, 3]}));
        assert!(s.set_state("doc", "/nope/x", Some(json!(1)), "agent").is_err(), "a missing parent is an error, not a silent create");
        s.set_state("doc", "", Some(json!({"fresh": 1})), "agent").unwrap();
        assert_eq!(s.get("doc").unwrap().state, json!({"fresh": 1}));
        s.undo("doc", "agent").unwrap();
        assert_eq!(s.get("doc").unwrap().state, json!({"title": "New", "items": [2, 3]}));
    }

    #[test]
    fn calls_reshape_and_undo_keep_state_and_history() {
        let (_d, mut s) = store();
        s.create("labeler", "<p>v1</p>".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();

        let ev = s.call("labeler", "label", json!({"id": "c14", "value": "fail"}), "person").unwrap();
        assert_eq!((ev.by.as_str(), ev.changed.clone(), ev.version), ("person", vec!["/labels/c14".to_string()], 2));

        let ev = s.reshape("labeler", Some("<p>v2</p>".into()), None, None, "agent").unwrap();
        assert_eq!(ev.view_rev, 2);
        assert_eq!(s.get("labeler").unwrap().state, json!({"labels": {"c14": "fail"}}), "reshape keeps state");

        let ev = s.undo("labeler", "person").unwrap();
        let j = s.get("labeler").unwrap();
        assert_eq!(j.view, "<p>v1</p>");
        assert_eq!(ev.view_rev, 3, "undoing a reshape is a new view revision");
        assert_eq!(j.state, json!({"labels": {"c14": "fail"}}));

        s.undo("labeler", "person").unwrap();
        assert_eq!(s.get("labeler").unwrap().state, json!({"labels": {}}));
        assert!(s.undo("labeler", "person").is_err());
    }

    #[test]
    fn bad_verbs_never_replace_good_ones() {
        let (_d, mut s) = store();
        s.create("j", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        let err = s.reshape("j", None, Some("const verbs = {".into()), None, "agent").unwrap_err();
        assert!(matches!(err, Error::Invalid(m) if m.contains("unchanged")));
        assert_eq!(s.get("j").unwrap().verbs, VERBS);
        assert!(s.create("j", "".into(), VERBS.into(), json!({}), "agent", false, "", "").is_err());
        assert!(s.create("../x", "".into(), VERBS.into(), json!({}), "agent", false, "", "").is_err());
    }

    #[test]
    fn comments_sit_beside_state_and_survive_undo() {
        let (_d, mut s) = store();
        s.create("j", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        s.call("j", "label", json!({"id": "a", "value": "pass"}), "person").unwrap();
        let ev = s.comment("j", json!({"selector": "#a"}), "too small", "person").unwrap();
        assert_eq!((ev.kind, ev.args.clone().unwrap()["id"].clone()), (EventKind::Comment, json!("c1")));
        s.undo("j", "person").unwrap();
        let j = s.get("j").unwrap();
        assert_eq!(j.state, json!({"labels": {}}));
        assert_eq!(j.comments.len(), 1, "undo is about state, not comments");
        s.resolve("j", "c1", "agent").unwrap();
        assert!(s.get("j").unwrap().comments[0].resolved);
        assert!(s.comment("j", json!({}), "  ", "person").is_err());
    }

    #[test]
    fn names_are_unique_per_project() {
        let (_d, mut s) = store();
        s.create("plan", "".into(), VERBS.into(), json!({}), "agent", false, "", "archon").unwrap();
        s.create("plan", "".into(), VERBS.into(), json!({}), "agent", false, "", "jelly").unwrap();
        assert_eq!(s.get("archon:plan").unwrap().project(), "archon");
        s.put_asset("archon:plan", "shot.png", b"png", "agent").unwrap();
        let ev = s.set_project("archon:plan", "rex", "person").unwrap();
        assert_eq!(ev.jelly, "rex:plan");
        assert!(s.get("archon:plan").is_err());
        assert_eq!(s.assets("rex:plan"), ["shot.png"], "assets move with the jelly");
        assert!(s.set_project("rex:plan", "jelly", "person").is_err(), "jelly:plan exists");
        assert!(s.put_asset("rex:plan", "../x", b"", "agent").is_err());
        assert!(s.create("k", "".into(), VERBS.into(), json!({}), "agent", false, "", "Bad Project").is_err());
    }

    #[test]
    fn asset_names_reject_windows_device_names() {
        let (_d, mut s) = store();
        s.create("plan", "".into(), VERBS.into(), json!({}), "agent", false, "", "").unwrap();
        for device in ["NUL", "con.txt", "COM1", "lpt9.png"] {
            let saved = s.put_asset("plan", device, b"x", "agent");
            assert_eq!(saved.is_err(), cfg!(windows), "{device}: {saved:?}");
        }
        for ordinary in ["console.txt", "nul-report.md", "COM10", "lpt0.txt"] {
            s.put_asset("plan", ordinary, b"x", "agent").unwrap_or_else(|e| panic!("{ordinary}: {e}"));
        }
    }

    #[test]
    fn attention_items_are_answered_beside_state() {
        let (_d, mut s) = store();
        s.create("j", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        let spec = json!({"kind": "ask", "title": "Which header?", "options": [{"id": "a", "label": "Bold"}, {"id": "b", "label": "Quiet"}], "recommend": "b"});
        s.attend("j", spec, "claude").unwrap();
        s.attend("j", json!({"kind": "look", "title": "Check the footer", "anchor": {"selector": "#f"}}), "claude").unwrap();
        assert!(s.attend("j", json!({"title": "x", "options": [{"id": "a"}], "recommend": "z"}), "claude").is_err());
        assert!(s.attend("j", json!({"kind": "poll", "title": "x"}), "claude").is_err());
        assert!(s.answer("j", "a1", Some("c"), "", "person").is_err(), "not an option");
        assert!(s.answer("j", "a1", None, " ", "person").is_err(), "an ask needs something");
        let ev = s.answer("j", "a1", Some("b"), "but larger", "person").unwrap();
        assert_eq!(ev.args.unwrap()["option"], "b");
        assert_eq!(s.get("j").unwrap().attention[0].answer.as_ref().unwrap().text, "but larger");
        s.answer("j", "a2", None, "", "person").unwrap();
        assert!(s.get("j").unwrap().attention.iter().all(|a| !a.is_open()));
        s.attend("j", json!({"title": "Still needed?"}), "claude").unwrap();
        s.withdraw("j", "a3", "claude").unwrap();
        assert!(!s.get("j").unwrap().attention[2].is_open());
        assert!(s.answer("j", "a3", None, "yes", "person").is_err(), "withdrawn items can't be answered");
        s.undo("j", "person").ok();
        assert!(s.get("j").unwrap().attention[0].answer.is_some(), "undo leaves answers alone");
        let old: Attention = serde_json::from_value(json!({"id": "a9", "kind": "ask", "title": "t", "detail": "d", "by": "x", "at_ms": 1, "answer": {"withdrawn": true, "by": "x", "at_ms": 2}})).unwrap();
        assert!(old.withdrawn.is_some() && old.answer.is_none() && old.context == "d", "old files still load");
    }

    const ASK_VERBS: &str = include_str!("../assets/ask.verbs.js");

    #[test]
    fn an_ask_round_is_attention_items_and_a_new_round_replaces_open_ones() {
        let (_d, mut s) = store();
        let spec = json!({"title": "Sharing", "context": "why", "questions": [
            {"id": "host", "question": "Where?", "recommend": "w", "why": "cheap", "options": [{"id": "w", "title": "Worker", "summary": "A relay", "gains": ["free"]}, {"id": "s", "title": "Static"}]},
            {"id": "who", "question": "Who?", "options": [{"id": "l", "title": "Link"}]}]});
        let evs = s.ask_round("sharing", &spec, "<p>ask</p>", ASK_VERBS, "jelly", "claude").unwrap();
        assert_eq!(evs[0].jelly, "jelly:sharing");
        let j = s.get("jelly:sharing").unwrap();
        assert_eq!((j.state["round"].clone(), j.about.as_str()), (json!(1), "Sharing"));
        assert_eq!(j.attention.len(), 2);
        let host = &j.attention[0];
        assert_eq!((host.options[0].label.as_str(), host.options[0].does.as_str(), host.group.as_deref()), ("Worker", "A relay", Some("round-1")));
        assert_eq!(host.anchor["selector"], "#q-host");
        s.answer("jelly:sharing", "a1", Some("w"), "only if it stays private", "person").unwrap();

        let bad = json!({"questions": [{"question": "Ok?", "options": [{"id": "a"}]}, {"question": "", "options": []}]});
        assert!(s.ask_round("sharing", &bad, "<p>ask</p>", ASK_VERBS, "jelly", "claude").is_err());
        assert_eq!(s.get("jelly:sharing").unwrap().attention.len(), 2, "a bad round changes nothing");

        s.ask_round("sharing", &json!({"question": "And now?", "options": [{"id": "y", "title": "Yes"}]}), "<p>ask</p>", ASK_VERBS, "jelly", "claude").unwrap();
        let j = s.get("jelly:sharing").unwrap();
        assert_eq!(j.state["round"], 2);
        assert!(j.attention[0].answer.is_some(), "answers stay");
        assert!(j.attention[1].withdrawn.is_some(), "what the last round left open is withdrawn");
        assert_eq!(j.attention[2].group.as_deref(), Some("round-2"));
    }

    #[test]
    fn verb_calls_remember_their_source_and_seq_survives_restart() {
        let (dir, mut s) = store();
        s.create("j", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        let ev = s.call_from("j", "label", json!({"id": "a", "value": "x"}), "person", Some(json!({"selector": "#pick-b"}))).unwrap();
        assert_eq!(ev.source.unwrap()["selector"], "#pick-b");
        let seq = s.call("j", "label", json!({"id": "a", "value": "y"}), "agent").unwrap().seq;
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        let next = s.call("j", "label", json!({"id": "a", "value": "z"}), "agent").unwrap();
        assert_eq!(next.seq, seq + 1);
        assert!(next.source.is_none());
    }

    #[test]
    fn quiet_jellies_archive_and_come_back_on_change() {
        let (_d, mut s) = store();
        s.create("old", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        s.create("kept", "".into(), VERBS.into(), json!({}), "agent", false, "", "").unwrap();
        s.pin("kept", true, "person").unwrap();
        s.create("asked", "".into(), VERBS.into(), json!({}), "agent", false, "", "").unwrap();
        s.comment("asked", json!({}), "why?", "person").unwrap();
        let swept: Vec<String> = s.sweep(0).into_iter().map(|e| e.jelly).collect();
        assert_eq!(swept, ["old"], "pinned and open-comment jellies stay");
        assert!(s.get("old").unwrap().archived);
        s.call("old", "label", json!({"id": "a", "value": "x"}), "agent").unwrap();
        assert!(!s.get("old").unwrap().archived, "a change brings it back");
    }

    #[test]
    fn replies_thread_under_a_comment() {
        let (_d, mut s) = store();
        s.create("j", "".into(), VERBS.into(), json!({}), "agent", false, "", "").unwrap();
        s.comment("j", json!({"selector": "#a"}), "split this", "person").unwrap();
        s.reply("j", "c1", "Done, reshaped v2", "claude", true).unwrap();
        assert!(s.get("j").unwrap().comments[0].resolved);
        s.reply("j", "c1", "not quite", "person", false).unwrap();
        let f = &s.get("j").unwrap().comments[0];
        assert!(!f.resolved, "a follow-up opens the thread again");
        assert_eq!(f.replies.iter().map(|r| r.by.as_str()).collect::<Vec<_>>(), ["claude", "person"]);
        assert!(s.reply("j", "c9", "x", "claude", false).is_err());
    }

    const MONITOR_VERBS: &str = include_str!("../skill/recipes/monitor/verbs.js");

    #[test]
    fn hundreds_of_pushes_keep_the_file_near_the_state_and_undo_exact() {
        let (dir, mut s) = store();
        let mut state: Value = serde_json::from_str(include_str!("../skill/recipes/monitor/start.json")).unwrap();
        state["notes"] = json!("n".repeat(95_000));
        let view = format!("<main>{}</main>", "<p>row</p>".repeat(5_000));
        s.create("run", view.clone(), MONITOR_VERBS.into(), state, "agent", false, "", "").unwrap();
        let push = |i: u64| json!({
            "points": [{"m": "latency", "v": 100.0 + (i % 17) as f64 * 1.37}, {"m": "browsers", "v": (i % 3) as f64}, {"m": "memory", "v": 40.0 + i as f64 / 7.0}],
            "items": [{"r": if i.is_multiple_of(9) { "fail" } else { "pass" }, "label": format!("ask {i}")}],
            "done": i,
        });
        for i in 1..300 {
            s.call("run", "push", push(i), "agent").unwrap();
        }
        let before = s.get("run").unwrap().state.clone();
        s.call("run", "push", push(300), "agent").unwrap();

        let state = serde_json::to_vec(&s.get("run").unwrap().state).unwrap().len();
        let file = fs::metadata(dir.path().join("jellies").join("run.json")).unwrap().len() as usize;
        assert!(state > 100_000, "the state is {state} bytes");
        assert!(file <= view.len() + MONITOR_VERBS.len() + 4 * state, "{file} bytes of file for {state} of state");

        s.undo("run", "person").unwrap();
        assert_eq!(s.get("run").unwrap().state, before, "undo restores the state before the last push exactly");
        s.reshape("run", Some("<p>new</p>".into()), None, None, "agent").unwrap();
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        s.undo("run", "person").unwrap();
        assert_eq!(s.get("run").unwrap().view, view);
        s.undo("run", "person").unwrap();
        assert_eq!(s.get("run").unwrap().state["done"], 298, "older pushes undo too, after a restart");
    }

    #[test]
    fn whole_state_rewrites_stay_within_the_undo_budget_and_the_newest_always_undoes() {
        let (dir, mut s) = store();
        let big = |i: u64| json!({"blob": format!("{i}-{}", "x".repeat(100_000))});
        s.create("j", "".into(), VERBS.into(), big(0), "agent", false, "", "").unwrap();
        for i in 1..30 {
            s.set_state("j", "", Some(big(i)), "agent").unwrap();
        }
        let path = dir.path().join("jellies").join("j.json");
        let file = fs::metadata(&path).unwrap().len() as usize;
        let state = serde_json::to_vec(&big(29)).unwrap().len();
        assert!(file <= 4 * state, "{file} bytes of file for {state} of state");
        s.undo("j", "person").unwrap();
        assert_eq!(s.get("j").unwrap().state, big(28));

        // One step bigger than the whole budget of the small state it leaves.
        s.set_state("j", "", Some(json!({})), "agent").unwrap();
        s.undo("j", "person").unwrap();
        assert_eq!(s.get("j").unwrap().state, big(28), "the newest step stays, however big");
    }

    #[test]
    fn jellies_saved_with_full_snapshots_keep_their_undo() {
        let (dir, mut s) = store();
        s.create("j", "<p>v2</p>".into(), VERBS.into(), json!({"labels": {"a": "x", "b": "y"}}), "agent", false, "", "").unwrap();
        let path = dir.path().join("jellies").join("j.json");
        let mut saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved["jelly"]["view_rev"] = json!(2);
        saved["undo"] = json!([
            {"view": "<p>v1</p>", "verbs": VERBS, "state": {"labels": {}}, "view_rev": 1},
            {"view": "<p>v1</p>", "verbs": VERBS, "state": {"labels": {"a": "x"}}, "view_rev": 1},
        ]);
        saved.as_object_mut().unwrap().remove("steps");
        fs::write(&path, saved.to_string()).unwrap();
        drop(s);

        s = Store::open(dir.path()).unwrap();
        s.undo("j", "person").unwrap();
        let j = s.get("j").unwrap();
        assert_eq!((j.state.clone(), j.view.as_str(), j.view_rev), (json!({"labels": {"a": "x"}}), "<p>v1</p>", 3));
        s.undo("j", "person").unwrap();
        assert_eq!(s.get("j").unwrap().state, json!({"labels": {}}));
        assert!(s.undo("j", "person").is_err());
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.get("undo").is_none() && saved.get("steps").is_some());
    }

    #[test]
    fn migration_preserves_large_legacy_undo_until_new_history_replaces_it() {
        let (dir, mut s) = store();
        let old = |i: u64| json!({"blob": format!("{i}-{}", "x".repeat(100_000))});
        s.create("j", "".into(), VERBS.into(), json!({}), "agent", false, "", "").unwrap();
        let path = dir.path().join("jellies").join("j.json");
        let mut saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved["undo"] = json!([
            {"view": "", "verbs": VERBS, "state": old(0), "view_rev": 1},
            {"view": "", "verbs": VERBS, "state": old(1), "view_rev": 1},
        ]);
        saved.as_object_mut().unwrap().remove("steps");
        let legacy = saved.to_string();
        fs::write(&path, &legacy).unwrap();
        drop(s);

        s = Store::open(dir.path()).unwrap();
        s.set_state("j", "/new", Some(json!(true)), "agent").unwrap();
        drop(s);
        s = Store::open(dir.path()).unwrap();
        s.undo("j", "person").unwrap();
        assert_eq!(s.get("j").unwrap().state, json!({}));
        s.undo("j", "person").unwrap();
        assert_eq!(s.get("j").unwrap().state, old(1));
        drop(s);
        s = Store::open(dir.path()).unwrap();
        s.undo("j", "person").unwrap();
        assert_eq!(s.get("j").unwrap().state, old(0));
        assert!(s.undo("j", "person").is_err());

        fs::write(&path, legacy).unwrap();
        s = Store::open(dir.path()).unwrap();
        for i in 0..UNDO_DEPTH {
            s.set_state("j", "/new", Some(json!(i)), "agent").unwrap();
        }
        assert!(fs::metadata(&path).unwrap().len() < UNDO_SLACK as u64,
            "legacy history ages out at the depth limit");
        for _ in 0..UNDO_DEPTH {
            s.undo("j", "person").unwrap();
        }
        assert_eq!(s.get("j").unwrap().state, json!({}));
        assert!(s.undo("j", "person").is_err());

        fs::write(&path, saved.to_string()).unwrap();
        s = Store::open(dir.path()).unwrap();
        s.set_state("j", "", Some(old(2)), "agent").unwrap();
        s.set_state("j", "", Some(json!({})), "agent").unwrap();
        drop(s);
        s = Store::open(dir.path()).unwrap();
        s.undo("j", "person").unwrap();
        assert_eq!(s.get("j").unwrap().state, old(2), "the newest step survives the byte limit");
        assert!(s.undo("j", "person").is_err(), "new history still obeys the byte budget");
    }

    fn logged(s: &Store) -> Vec<String> {
        let mut lines = Vec::new();
        s.events_back(|l| { lines.push(l.to_string()); Ok(true) }).unwrap();
        lines.reverse();
        lines
    }

    #[test]
    fn the_log_reads_back_whole_lines_across_chunks() {
        let (dir, s) = store();
        let text: String = (0..400).map(|i| format!("{{\"seq\":{i},\"pad\":\"{}\"}}\n", "x".repeat(i * 7 % 1500))).collect();
        assert!(text.len() as u64 > 3 * LOG_CHUNK);
        fs::write(dir.path().join("events.jsonl"), &text).unwrap();
        assert_eq!(logged(&s), text.lines().collect::<Vec<_>>());
        let mut seen = 0;
        s.events_back(|_| { seen += 1; Ok(seen < 3) }).unwrap();
        assert_eq!(seen, 3, "stopping stops");
    }

    #[test]
    fn trimming_the_log_keeps_the_newest_lines_and_the_persons_events() {
        let (dir, mut s) = store();
        s.create("j", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        for i in 0..60 {
            let by = if i % 10 == 3 { "person" } else { "agent" };
            s.call("j", "label", json!({"id": format!("k{i}"), "value": "v"}), by).unwrap();
        }
        let path = s.events_path();
        let before = fs::read_to_string(&path).unwrap();
        let keep = before.len() / 4;
        s.trim_events(keep, 1 << 20).unwrap();

        // The newest `keep` bytes of whole lines, after the person's older events.
        let (mut start, mut expected, mut tail) = (0, Vec::new(), Vec::new());
        for l in before.lines() {
            if start >= before.len() - keep {
                tail.push(l.to_string());
            } else if l.contains("\"by\":\"person\"") {
                expected.push(l.to_string());
            }
            start += l.len() + 1;
        }
        assert!(!expected.is_empty() && !tail.is_empty());
        expected.extend(tail);
        let after = logged(&s);
        assert_eq!(after, expected);
        let events: Vec<Event> = after.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert!(events.windows(2).all(|w| w[0].seq < w[1].seq), "seq still ascends");

        let last = events.last().unwrap().seq;
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        assert_eq!(s.call("j", "label", json!({"id": "z", "value": "v"}), "agent").unwrap().seq, last + 1);
    }

    #[test]
    fn malformed_persisted_jellies_refuse_to_open_without_changing_files() {
        for contents in ["{", "{}"] {
            let (dir, mut s) = store();
            s.create("valid", "".into(), VERBS.into(), json!({}), "agent", false, "", "").unwrap();
            drop(s);
            let broken = dir.path().join("jellies").join("broken.json");
            fs::write(&broken, contents).unwrap();
            fs::write(dir.path().join("jellies").join("notes.txt"), "not a jelly").unwrap();
            let error = Store::open(dir.path()).err().expect("must not serve a partial store");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(error.to_string().contains(broken.to_str().unwrap()));
            assert_eq!(fs::read_to_string(&broken).unwrap(), contents);
            fs::remove_file(broken).unwrap();
            assert!(Store::open(dir.path()).unwrap().get("valid").is_ok());
        }
    }

    #[test]
    fn persisted_names_must_pass_name_validation() {
        let (dir, mut s) = store();
        s.create("j", "".into(), VERBS.into(), json!({}), "agent", false, "", "").unwrap();
        s.create("p:k", "".into(), VERBS.into(), json!({}), "agent", false, "", "").unwrap();
        drop(s);
        let s = Store::open(dir.path()).unwrap();
        assert!(s.get("j").is_ok() && s.get("p:k").is_ok());
        drop(s);
        let file = dir.path().join("jellies").join("j.json");
        let original = fs::read_to_string(&file).unwrap();
        for bad in ["/tmp/outside", "../outside"] {
            let mut entry: Value = serde_json::from_str(&original).unwrap();
            entry["jelly"]["name"] = json!(bad);
            fs::write(&file, entry.to_string()).unwrap();
            let error = Store::open(dir.path()).err().expect("an escaping name must prevent startup");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            let message = error.to_string();
            assert!(message.contains(file.to_str().unwrap()) && message.contains(&format!("{bad:?}")), "{message}");
        }
    }

    #[test]
    fn jellies_survive_a_restart() {
        let (dir, mut s) = store();
        s.create("j", "".into(), VERBS.into(), json!({"labels": {}}), "agent", false, "", "").unwrap();
        s.call("j", "label", json!({"id": "a", "value": "pass"}), "person").unwrap();
        drop(s);
        let mut s = Store::open(dir.path()).unwrap();
        assert_eq!(s.get("j").unwrap().state, json!({"labels": {"a": "pass"}}));
        s.undo("j", "person").unwrap();
        assert_eq!(s.get("j").unwrap().state, json!({"labels": {}}), "undo history is saved too");
    }
}
