//! `jelly`: the daemon and the CLI agents use to make and shape jellies.

mod design;
mod disk;
mod server;
mod store;
mod startup;
mod verbs;

use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "jelly", about = "Small live apps that agents and people shape while they run")]
struct Cli {
    /// Daemon address for client commands.
    #[arg(long, env = "JELLY_URL", default_value = startup::DEFAULT_URL, global = true)]
    url: String,
    /// Who this change is from, recorded on every event (default: agent).
    #[arg(long = "as", env = "JELLY_AS", global = true)]
    by: Option<String>,
    /// The project new jellies belong to. Defaults to the name of the git repo
    /// you run from; the jelly page shows each project on its own page.
    #[arg(long, env = "JELLY_PROJECT", global = true)]
    project: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon and the jelly page.
    Serve {
        #[arg(long, default_value_t = 7878)]
        port: u16,
        /// Where jellies are kept. Defaults to $JELLY_HOME or ~/.jelly.
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// List this project's jellies, one per line (all projects outside a repo).
    Ls {
        /// Every project, archived jellies included.
        #[arg(long)]
        all: bool,
        /// Print the full JSON.
        #[arg(long)]
        json: bool,
    },
    /// Make a jelly from a view and verbs. Use - to read a file from stdin.
    New {
        name: String,
        /// Start from a folder with view.html, verbs.js, state.json and assets/:
        /// a recipe, or what `jelly pull` wrote. --view, --verbs and --state
        /// replace the folder's own.
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long, required_unless_present = "from")]
        view: Option<PathBuf>,
        #[arg(long, required_unless_present = "from")]
        verbs: Option<PathBuf>,
        /// Starting state as a JSON file.
        #[arg(long)]
        state: Option<PathBuf>,
        /// Replace an existing jelly of the same name, losing its state.
        #[arg(long)]
        replace: bool,
        /// One line on what this jelly is for, shown above it on the jelly page.
        #[arg(long, default_value = "")]
        about: String,
    },
    /// Call a verb, exactly as a button in the view would.
    Call {
        name: String,
        verb: String,
        /// Arguments as JSON, @FILE to read a file, or - for stdin.
        #[arg(default_value = "{}")]
        args: String,
    },
    /// Write state directly, without a verb: fix your own content or start over.
    /// Undoable. `jelly set plan '{"items":[]}'` replaces the whole state.
    Set {
        name: String,
        /// JSON, @FILE to read a file, or - for stdin. Leave out with --rm.
        value: Option<String>,
        /// Where in the state, as a JSON pointer: /title, /items/0, /items/- (append).
        #[arg(long, default_value = "")]
        at: String,
        /// Remove what is at --at instead.
        #[arg(long)]
        rm: bool,
    },
    /// Print state, or the part at a JSON pointer such as /labels.
    State { name: String, pointer: Option<String> },
    /// Swap the view and/or verbs while the jelly runs. State is kept.
    Reshape {
        name: String,
        #[arg(long)]
        view: Option<PathBuf>,
        #[arg(long)]
        verbs: Option<PathBuf>,
        /// New one-line description, shown above it on the jelly page.
        #[arg(long)]
        about: Option<String>,
    },
    /// Undo the last change to the view, verbs or state, by anyone. Comments,
    /// answers and pins are not undone.
    Undo { name: String },
    /// Keep a jelly.
    Pin { name: String },
    /// Stop keeping a jelly.
    Unpin { name: String },
    /// Remove a jelly and its history.
    Rm { name: String },
    /// Print the full jelly: view, verbs, state.
    Show { name: String },
    /// Stream events as JSON lines, for one jelly or all.
    #[command(alias = "watch")]
    Events {
        name: Option<String>,
        /// Include the full state in each event.
        #[arg(long)]
        state: bool,
    },
    /// Open the jelly page (this project's, if any), or one jelly, in the browser.
    /// Always prints the address, even if the browser cannot be opened.
    Open { name: Option<String> },
    /// Ask the person something: one question on a jelly, or a set of them.
    ///
    /// Two forms:
    ///
    /// `jelly ask NAME "Which header?" --option a="Bold" --option b="Quiet" --recommend b --why "…" --at "#hero"`
    /// puts one ask on an existing jelly, next to what it is about. It shows on
    /// the jelly and in the person's attention queue. With `--wait` it blocks
    /// until the person answers and prints `a1 answered: <option> · "<note>"`.
    ///
    /// `jelly ask NAME --spec spec.json` makes an ask page: one or more questions,
    /// each an attention item with options (label, does, gains, costs, effort, a
    /// live HTML preview), a recommendation and why. Asking again with the same
    /// name starts a new round. Spec: {"title", "context", "questions": [{"id",
    /// "question", "context", "recommend", "why", "options": [...]}]}.
    Ask {
        name: String,
        /// The question, for an ask on an existing jelly.
        question: Option<String>,
        /// A spec file for an ask page.
        #[arg(long, conflicts_with = "question", group = "json_target")]
        spec: Option<PathBuf>,
        /// An option as id=label, or id=label|what picking it does. Repeat it.
        #[arg(long = "option")]
        options: Vec<String>,
        #[arg(long)]
        recommend: Option<String>,
        /// Why you recommend it, in one sentence.
        #[arg(long, default_value = "")]
        why: String,
        /// The element it is about, as a CSS selector.
        #[arg(long)]
        at: Option<String>,
        /// More to read before answering, shown under the question.
        #[arg(long, alias = "detail", default_value = "")]
        context: String,
        /// Block until the person answers or the ask is withdrawn, then print the answer.
        #[arg(long, conflicts_with = "spec", group = "json_target")]
        wait: bool,
        /// With --wait: give up after this many seconds (exit 2).
        #[arg(long, requires = "wait")]
        timeout: Option<u64>,
        /// With --wait or --spec: print the full JSON.
        #[arg(long, requires = "json_target")]
        json: bool,
    },
    /// Ask the person to look at something: it lands in their attention queue,
    /// and they answer "seen" or with a word back.
    Look {
        name: String,
        what: String,
        #[arg(long)]
        at: Option<String>,
        #[arg(long, alias = "detail", default_value = "")]
        context: String,
    },
    /// Everything waiting on the person in this project (or everywhere with
    /// --all), one line per item.
    Attention {
        #[arg(long)]
        all: bool,
        /// Print the full JSON.
        #[arg(long)]
        json: bool,
    },
    /// See a jelly as an agent: what it is, its verbs and state, what waits on
    /// the person, what they did lately, and what is on screen as text.
    See {
        name: String,
        /// Render at this width (390 for a phone). Default: an open view, else 1280.
        #[arg(long)]
        width: Option<u32>,
        /// Also save a screenshot and print its path.
        #[arg(long)]
        png: bool,
        /// Screenshot the whole page, not just the first screen (implies --png).
        #[arg(long)]
        full: bool,
    },
    /// Leave a note for the next agent who picks this jelly up.
    Note { name: String, text: String },
    /// Write a jelly's view, verbs, state and assets into a folder to edit.
    Pull {
        name: String,
        /// Default: ./<name>
        dir: Option<PathBuf>,
    },
    /// Watch a pulled folder and reshape the jelly on every save.
    Dev {
        name: String,
        dir: Option<PathBuf>,
    },
    /// Print this project's DESIGN.md, or set it with --set FILE. Views can use
    /// its tokens as CSS variables: <link rel="stylesheet" href="/p/PROJECT/design.css">.
    Design {
        #[arg(long)]
        set: Option<PathBuf>,
    },
    /// Take back an ask or look that no longer applies.
    Withdraw { name: String, id: String },
    /// Open a jelly full size on the person's screen. No name closes it.
    Focus { name: Option<String> },
    /// Say you are changing a jelly, so the person waits ("rebuilding the header").
    Working { name: String, what: String },
    /// Say you are done changing a jelly.
    Done { name: String },
    /// Block until someone other than you changes the jelly, then print what
    /// they did in one line. This is how an agent waits for the person.
    Wait {
        /// Exclusive starting point from a mutation (seq:N). Replays recorded events, without state.
        #[arg(long)]
        since: Option<SequenceToken>,
        /// Leave out to wait on every jelly (the jelly page's own changes are skipped).
        name: Option<String>,
        /// Give up after this many seconds (exit 2).
        #[arg(long)]
        timeout: Option<u64>,
        /// Print the full event as JSON (recorded events omit state).
        #[arg(long)]
        json: bool,
    },
    /// List comments pinned to parts of a jelly's view, one line each with its
    /// thread. Leave out NAME for every jelly: this is the person's to-do list for you.
    Comments {
        name: Option<String>,
        /// Include resolved comments.
        #[arg(long)]
        all: bool,
        /// Print the full JSON.
        #[arg(long)]
        json: bool,
    },
    /// Move a jelly to another project ("" for none). Its name becomes project:name.
    #[command(alias = "project")]
    Move { name: String, project: String },
    /// Mark a comment as dealt with.
    Resolve { name: String, id: String },
    /// Hide a jelly from the jelly page without deleting it.
    Archive { name: String },
    /// Bring an archived jelly back to the jelly page.
    Restore { name: String },
    /// Add files (images, video, models, data) next to a jelly. A view refers
    /// to them as `assets/<file>`. They stay out of state and undo.
    Asset {
        name: String,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Write a read-only snapshot of a jelly as one HTML file you can send
    /// anyone: state baked in, assets inlined.
    Share {
        name: String,
        /// Where to write it (default: ./<name>.html).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// What the person did in this project's jellies since you last asked.
    /// Reader identity: explicit --as / JELLY_AS, then --session-id /
    /// JELLY_SESSION_ID, Claude session environment or hook input, then the
    /// invoking parent's PID and start time. Conversation IDs survive resume.
    /// With --hook, starts silently and prints nothing when there is nothing new,
    /// so it can run at the start of every agent turn. After startup, an inbox
    /// request that times out prints one line.
    Inbox {
        #[arg(long)]
        hook: bool,
        /// Stable conversation ID, shared by commands, hooks and resumed sessions.
        #[arg(long, env = "JELLY_SESSION_ID")]
        session_id: Option<String>,
    },
    /// Answer on a comment's thread. The person sees it on the pin.
    Reply {
        name: String,
        id: String,
        text: String,
        /// Close the thread: you dealt with it.
        #[arg(long)]
        resolve: bool,
    },
}


#[derive(Clone, Debug)]
struct SequenceToken(u64);

impl std::str::FromStr for SequenceToken {
    type Err = String;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        token.strip_prefix("seq:")
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse().ok())
            .map(Self)
            .ok_or_else(|| "use seq:N from a mutation's output (N is an unsigned 64-bit sequence)".into())
    }
}

#[derive(Debug)]
enum CommandError {
    Message(String),
    Output(std::io::Error),
}

impl From<String> for CommandError {
    fn from(message: String) -> Self { Self::Message(message) }
}

impl From<&str> for CommandError {
    fn from(message: &str) -> Self { Self::Message(message.into()) }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => message.fmt(f),
            Self::Output(error) => error.fmt(f),
        }
    }
}

fn write_output(writer: &mut impl Write, args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
    writer.write_fmt(args)?;
    writer.flush()
}

fn output(args: std::fmt::Arguments<'_>) -> Result<(), CommandError> {
    write_output(&mut std::io::stdout().lock(), args).map_err(CommandError::Output)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CommandError::Output(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jelly: {e}");
            ExitCode::FAILURE
        }
    }
}

fn daemon_command() -> Result<std::process::Command, String> {
    let mut command = std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command.arg("serve");
    Ok(command)
}

fn run(cli: Cli) -> Result<(), CommandError> {
    run_with_start(cli, |url, quiet| startup::ensure_running(url, quiet, daemon_command()?))
}

fn run_with_start(mut cli: Cli, start: impl FnOnce(&str, bool) -> Result<(), String>) -> Result<(), CommandError> {
    if !matches!(cli.cmd, Cmd::Serve { .. }) && startup::is_default_endpoint(&cli.url) {
        cli.url = startup::DEFAULT_URL.into();
        let quiet = matches!(cli.cmd, Cmd::Inbox { hook: true, .. });
        if let Err(e) = start(&cli.url, quiet) {
            return if quiet { Ok(()) } else { Err(e.into()) };
        }
    }
    let api = |path: &str| format!("{}/api/jellies{path}", cli.url);
    let by = cli.by.clone().unwrap_or_else(|| "agent".into());
    // Only commands that make or open jellies need it, so git runs only for those.
    let project = || detect_project(cli.project.clone());
    // Names are unique per project: a short name means this project's jelly
    // when there is one, and `project:name` always means exactly that jelly.
    let id = |name: &str| -> Result<String, String> {
        if name.contains(':') || name == store::PAGE {
            return Ok(name.into());
        }
        let list = get(&api(""))?;
        Ok(store::resolve_name(name, &project(), list.as_array().into_iter().flatten()
            .filter_map(|j| j["name"].as_str())))
    };
    match cli.cmd {
        Cmd::Serve { port, home } => {
            let home = home
                .or_else(|| std::env::var_os("JELLY_HOME").map(PathBuf::from))
                .or_else(|| std::env::home_dir().map(|h| h.join(".jelly")))
                .ok_or("could not find your home directory: set --home or JELLY_HOME")?;
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
            rt.block_on(async {
                // Reserve the endpoint before store initialization can write anything.
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
                let store = store::Store::open(&home).map_err(|e| format!("{}: {e}", home.display()))?;
                server::serve(store, listener).await.map_err(|e| e.to_string().into())
            })
        }
        Cmd::Ls { all, json } => {
            let p = if all { String::new() } else { project() };
            let list: Vec<Value> = get(&api(""))?.as_array().cloned().unwrap_or_default().into_iter()
                .filter(|j| all || !j["archived"].as_bool().unwrap_or(false))
                .filter(|j| p.is_empty() || j["project"].as_str() == Some(p.as_str()))
                .collect();
            if json {
                return print(json!(list));
            }
            if list.is_empty() {
                output(format_args!("No jellies{}. The daemon is up.\n", if p.is_empty() { String::new() } else { format!(" in project {p} (--all for every project)") }))?;
            }
            for j in &list {
                let mut tags = vec![format!("v{}", j["version"])];
                let n = |k: &str| j[k].as_u64().unwrap_or(0);
                if n("attention") > 0 { tags.push(format!("{} waiting on the person", n("attention"))); }
                if n("open_comments") > 0 { tags.push(format!("{} open comments", n("open_comments"))); }
                if j["pinned"].as_bool() == Some(true) { tags.push("pinned".into()); }
                if j["archived"].as_bool() == Some(true) { tags.push("archived".into()); }
                output(format_args!("{}  {}  ({})\n", j["name"].as_str().unwrap_or("?"), j["about"].as_str().unwrap_or(""), tags.join(", ")))?;
            }
            Ok(())
        }
        Cmd::New { name, from, view, verbs, state, replace, about } => {
            sync_design(&cli.url, &project());
            let in_from = |file: &str| from.as_ref().map(|d| d.join(file));
            let view = view.or_else(|| in_from("view.html")).ok_or("give --view or --from")?;
            let verbs = verbs.or_else(|| in_from("verbs.js")).ok_or("give --verbs or --from")?;
            let state: Value = match state.or_else(|| in_from("state.json").filter(|p| p.exists())) {
                Some(p) => serde_json::from_str(&read(&p)?).map_err(|e| format!("{}: {e}", p.display()))?,
                None => json!({}),
            };
            let body = json!({"name": name, "view": read(&view)?, "verbs": read(&verbs)?, "state": state, "by": by, "replace": replace, "about": about, "project": project()});
            let ev = send("POST", &api(""), body)?;
            // The folder's assets come along, as `jelly asset` would add them.
            let made = ev["jelly"].as_str().unwrap_or(&name).to_string();
            let mut assets: Vec<PathBuf> = in_from("assets").and_then(|d| std::fs::read_dir(d).ok()).into_iter().flatten().flatten()
                .map(|f| f.path()).filter(|p| p.is_file()).collect();
            assets.sort();
            let added = add_assets(&api(&format!("/{made}")), &assets, &by)?;
            report(ev)?;
            for line in added {
                output(format_args!("{line}\n"))?;
            }
            Ok(())
        }
        Cmd::Call { name, verb, args } => {
            let args = json_arg(&args, "args")?;
            report(send("POST", &api(&format!("/{}/call", id(&name)?)), json!({"verb": verb, "args": args, "by": by}))?)
        }
        Cmd::Set { name, value, at, rm } => {
            let value = match (rm, value) {
                (true, _) => Value::Null,
                (false, Some(v)) => json_arg(&v, "value")?,
                (false, None) => return Err("give a value, or --rm".into()),
            };
            report(send("POST", &api(&format!("/{}/set", id(&name)?)), json!({"pointer": at, "value": value, "remove": rm, "by": by}))?)
        }
        Cmd::State { name, pointer } => {
            let p = pointer.map(|p| format!("?pointer={}", encode(&p))).unwrap_or_default();
            print(get(&api(&format!("/{}/state{p}", id(&name)?)))?)
        }
        Cmd::Reshape { name, view, verbs, about } => {
            let body = json!({"view": view.map(|p| read(&p)).transpose()?, "verbs": verbs.map(|p| read(&p)).transpose()?, "about": about, "by": by});
            report(send("POST", &api(&format!("/{}/reshape", id(&name)?)), body)?)
        }
        Cmd::Undo { name } => report(send("POST", &api(&format!("/{}/undo", id(&name)?)), json!({"by": by}))?),
        Cmd::Pin { name } => report(send("POST", &api(&format!("/{}/pin", id(&name)?)), json!({"by": by, "pinned": true}))?),
        Cmd::Unpin { name } => report(send("POST", &api(&format!("/{}/pin", id(&name)?)), json!({"by": by, "pinned": false}))?),
        Cmd::Rm { name } => report(send("DELETE", &api(&format!("/{}?by={}", id(&name)?, encode(&by))), Value::Null)?),
        Cmd::Show { name } => print(get(&api(&format!("/{}", id(&name)?)))?),
        Cmd::Events { name, state } => {
            let name = name.map(|n| id(&n)).transpose()?;
            let q = name.map(|n| format!("?jelly={}", encode(&n))).unwrap_or_default();
            let resp = ureq::get(&format!("{}/api/events{q}", cli.url)).call().map_err(describe)?;
            for line in BufReader::new(resp.into_reader()).lines() {
                let line = line.map_err(|e| e.to_string())?;
                if let Some(data) = line.strip_prefix("data:") {
                    let mut ev: Value = serde_json::from_str(data.trim()).map_err(|e| e.to_string())?;
                    if !state {
                        ev.as_object_mut().map(|o| o.remove("state"));
                    }
                    output(format_args!("{ev}\n"))?;
                }
            }
            Ok(())
        }
        Cmd::Ask { name, question: Some(q), options, recommend, why, at, context, wait, timeout, json, .. } => {
            let options: Vec<Value> = options.iter().map(|o| {
                let (id, rest) = o.split_once('=').ok_or_else(|| format!("--option {o:?}: use id=label"))?;
                let (label, does) = rest.split_once('|').unwrap_or((rest, ""));
                Ok(json!({"id": id.trim(), "label": label.trim(), "does": does.trim()}))
            }).collect::<Result<_, String>>()?;
            let anchor = at.map(|sel| json!({"selector": sel})).unwrap_or(Value::Null);
            let body = json!({"kind": "ask", "title": q, "options": options, "recommend": recommend, "why": why, "anchor": anchor, "context": context, "by": by});
            let ev = send("POST", &api(&format!("/{}/attention", id(&name)?)), body)?;
            if !wait {
                return report(ev);
            }
            // stdout carries only the answer; the ask line still says where the card shows.
            eprintln!("{}", report_line(&ev));
            let (Some(jelly), Some(item)) = (ev["jelly"].as_str(), ev["args"]["id"].as_str()) else {
                return Err("the daemon did not say which attention item it made".into());
            };
            let item = await_answer(&cli.url, jelly, item, timeout)?;
            if !json {
                output(format_args!("{}\n", answer_line(&item)))?;
                return Ok(());
            }
            let mut out = json!(item);
            out["jelly"] = json!(jelly);
            print(out)
        }
        Cmd::Look { name, what, at, context } => {
            let anchor = at.map(|sel| json!({"selector": sel})).unwrap_or(Value::Null);
            let body = json!({"kind": "look", "title": what, "anchor": anchor, "context": context, "by": by});
            report(send("POST", &api(&format!("/{}/attention", id(&name)?)), body)?)
        }
        Cmd::Attention { all, json } => {
            let p = if all { String::new() } else { project() };
            let list = get(&format!("{}/api/attention?project={}", cli.url, encode(&p)))?;
            if json {
                return print(list);
            }
            let items = list.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                output(format_args!("Nothing waits on the person{}.\n", if p.is_empty() { String::new() } else { format!(" in project {p} (--all for every project)") }))?;
            }
            for item in items {
                let a = store::Attention::deserialize(&item).map_err(|e| e.to_string())?;
                output(format_args!("{}  {}\n", item["jelly"].as_str().unwrap_or("?"), a.summary()))?;
            }
            Ok(())
        }
        Cmd::Withdraw { name, id: aid } => report(send("POST", &api(&format!("/{}/attention/{}/withdraw", id(&name)?, encode(&aid))), json!({"by": by}))?),
        Cmd::Focus { name } => report(send("POST", &format!("{}/api/focus", cli.url), json!({"name": name.map(|n| id(&n)).transpose()?, "by": by}))?),
        Cmd::Working { name, what } => report(send("POST", &api(&format!("/{}/working", id(&name)?)), json!({"what": what, "by": by}))?),
        Cmd::Done { name } => report(send("POST", &api(&format!("/{}/working", id(&name)?)), json!({"what": null, "by": by}))?),
        Cmd::Ask { spec: None, .. } => Err("give a question (an ask on that jelly) or --spec FILE (an ask jelly)".into()),
        Cmd::Ask { name, spec: Some(spec), json, .. } => {
            let spec: Value = serde_json::from_str(&read(&spec)?).map_err(|e| format!("spec is not JSON: {e}"))?;
            // The store's rule: a spec without `questions` is one question.
            let questions = spec.get("questions").map_or(1, |q| q.as_array().map_or(0, Vec::len));
            let r = send("POST", &api(&format!("/{}/ask", id(&name)?)), json!({"spec": spec, "project": project(), "by": by}))?;
            let jelly = r["jelly"].as_str().unwrap_or(&name).to_string();
            let round = &r["state"]["round"];
            let since = format!("seq:{}", r["seq"]);
            if json {
                return print(json!({"jelly": jelly, "round": round, "questions": questions, "since": since, "next": format!("jelly wait {jelly} --since {since} --json: the answers arrive as answer events")}));
            }
            output(format_args!("{}\n", ask_line(&jelly, round, questions, &since)))?;
            Ok(())
        }
        Cmd::Wait { name, timeout, since, json } => {
            let project_jelly = name.as_ref().filter(|_| since.is_some()).filter(|n| !n.contains(':') && *n != store::PAGE)
                .map(|n| { let p = project(); if p.is_empty() { n.clone() } else { format!("{p}:{n}") } });
            let name = name.map(|n| id(&n)).transpose()?;
            if let Some(SequenceToken(seq)) = since {
                let mut req = ureq::get(&format!("{}/api/wait", cli.url))
                    .query("since", &seq.to_string()).query("by", &by);
                if let Some(n) = &name { req = req.query("jelly", n); }
                if let Some(preferred) = project_jelly {
                    req = req.query("project_jelly", &preferred);
                }
                if let Some(t) = timeout { req = req.query("timeout", &t.to_string()); }
                let resp = req.call().map_err(describe)?;
                if resp.status() == 204 {
                    let what = name.as_deref().unwrap_or("any jelly");
                    eprintln!("jelly: nobody changed {what} in time");
                    std::process::exit(2);
                }
                let ev = resp.into_json().map_err(|e| e.to_string())?;
                if json { return print(ev); }
                output(format_args!("{}\n", wait_line(&store::Event::deserialize(&ev).map_err(|e| e.to_string())?)))?;
                return Ok(());
            }
            let filter = name.as_deref().map(|n| format!("?jelly={}", encode(n))).unwrap_or_default();
            let mut req = ureq::get(&format!("{}/api/events{filter}", cli.url));
            if let Some(t) = timeout {
                req = req.timeout(std::time::Duration::from_secs(t));
            }
            if let Some(n) = &name {
                get(&api(&format!("/{n}/state")))?; // fail fast on a wrong name
            }
            let what = name.as_deref().unwrap_or("any jelly");
            let resp = req.call().map_err(describe)?;
            for line in BufReader::new(resp.into_reader()).lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock => {
                        eprintln!("jelly: nobody changed {what} in time");
                        std::process::exit(2);
                    }
                    Err(e) => return Err(e.to_string().into()),
                };
                let Some(data) = line.strip_prefix("data:") else { continue };
                let ev: Value = serde_json::from_str(data.trim()).map_err(|e| e.to_string())?;
                if ev["by"] != by.as_str() && (name.is_some() || ev["jelly"] != store::PAGE) {
                    if json {
                        return print(ev);
                    }
                    output(format_args!("{}\n", wait_line(&store::Event::deserialize(&ev).map_err(|e| e.to_string())?)))?;
                    return Ok(());
                }
            }
            Err("the event stream ended".into())
        }
        Cmd::Comments { name, all, json } => {
            let one = name.is_some();
            let names: Vec<String> = match name {
                Some(n) => vec![id(&n)?],
                None => {
                    let p = project();
                    get(&api(""))?.as_array().into_iter().flatten()
                    .filter(|j| p.is_empty() || j["project"] == p.as_str())
                    .filter(|j| all || j["open_comments"].as_u64().unwrap_or(0) > 0)
                    .filter_map(|j| j["name"].as_str().map(String::from)).collect()
                }
            };
            let mut list = Vec::new();
            let mut lines = Vec::new();
            for n in &names {
                let j = get(&api(&format!("/{n}")))?;
                for mut f in j["comments"].as_array().cloned().unwrap_or_default() {
                    if all || f["resolved"] != true {
                        lines.push(comment_line(n, &store::Comment::deserialize(&f).map_err(|e| e.to_string())?));
                        f["jelly"] = json!(n);
                        list.push(f);
                    }
                }
            }
            if json {
                return print(json!(list));
            }
            if lines.is_empty() {
                let p = project();
                let place = if one { format!(" on {}", names[0]) } else if p.is_empty() { String::new() } else { format!(" in project {p}") };
                output(format_args!("No {}comments{place}.\n", if all { "" } else { "open " }))?;
            }
            for l in lines {
                output(format_args!("{l}\n"))?;
            }
            Ok(())
        }
        Cmd::Reply { name, id: cid, text, resolve } => report(send("POST", &api(&format!("/{}/reply", id(&name)?)), json!({"id": cid, "text": text, "resolve": resolve, "by": by}))?),
        Cmd::Move { name, project } => report(send("POST", &api(&format!("/{}/move", id(&name)?)), json!({"project": project, "by": by}))?),
        Cmd::See { name, width, png, full } => {
            let mut q = vec![];
            if let Some(w) = width { q.push(format!("width={w}")); }
            if png || full { q.push("png=true".into()); }
            if full { q.push("full=true".into()); }
            let url = api(&format!("/{}/see?{}", id(&name)?, q.join("&")));
            let resp = ureq::get(&url).timeout(std::time::Duration::from_secs(60)).call().map_err(describe)?;
            output(format_args!("{}", resp.into_string().map_err(|e| e.to_string())?))?;
            Ok(())
        }
        Cmd::Note { name, text } => report(send("POST", &api(&format!("/{}/note", id(&name)?)), json!({"text": text, "by": by}))?),
        Cmd::Pull { name, dir } => {
            let name = id(&name)?;
            let short = name.split_once(':').map_or(name.as_str(), |(_, n)| n).to_string();
            let dir = dir.unwrap_or_else(|| PathBuf::from(&short));
            let j = get(&api(&format!("/{name}")))?;
            let inventory = get(&api(&format!("/{name}/assets")))
                .map_err(|e| format!("could not read asset inventory: {e}"))?;
            let files = serde_json::from_value::<Vec<String>>(inventory)
                .map_err(|e| format!("invalid asset inventory: {e}"))?;
            // This machine decides what it can store: a Unix daemon may hold a name Windows cannot.
            for file in &files {
                store::valid_file(file).map_err(|e| format!("cannot store asset {file} here: {e}"))?;
            }
            std::fs::create_dir_all(dir.join("assets")).map_err(|e| e.to_string())?;
            std::fs::write(dir.join("view.html"), j["view"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
            std::fs::write(dir.join("verbs.js"), j["verbs"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
            std::fs::write(dir.join("state.json"), serde_json::to_string_pretty(&j["state"]).unwrap_or_default()).map_err(|e| e.to_string())?;
            let assets_dir = format!("{}/j/{}/assets", cli.url, name);
            for file in &files {
                let response = ureq::get(&format!("{assets_dir}/{}", encode(file))).call()
                    .map_err(|e| format!("could not download asset {file}: {}", describe(e)))?;
                let mut bytes = Vec::new();
                response.into_reader().read_to_end(&mut bytes)
                    .map_err(|e| format!("could not read asset {file}: {e}"))?;
                std::fs::write(dir.join("assets").join(file), bytes)
                    .map_err(|e| format!("could not write asset {file}: {e}"))?;
            }
            let n = files.len();
            output(format_args!("{}/  view.html  verbs.js  state.json  assets/ ({n})\nEdit, then: jelly dev {name} {}\n", dir.display(), dir.display()))?;
            Ok(())
        }
        Cmd::Dev { name, dir } => {
            let name = id(&name)?;
            let short = name.split_once(':').map_or(name.as_str(), |(_, n)| n).to_string();
            let dir = dir.unwrap_or_else(|| PathBuf::from(&short));
            let stamp = |f: &str| std::fs::metadata(dir.join(f)).and_then(|m| m.modified()).ok();
            let (mut v, mut b) = (stamp("view.html"), stamp("verbs.js"));
            if v.is_none() && b.is_none() {
                return Err(format!("nothing to watch in {}: run jelly pull {name} first", dir.display()).into());
            }
            output(format_args!("watching {} for {name} (Ctrl-C to stop)\n", dir.display()))?;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let (nv, nb) = (stamp("view.html"), stamp("verbs.js"));
                if nv == v && nb == b { continue; }
                let view = (nv != v).then(|| std::fs::read_to_string(dir.join("view.html"))).transpose().map_err(|e| e.to_string())?;
                let verbs = (nb != b).then(|| std::fs::read_to_string(dir.join("verbs.js"))).transpose().map_err(|e| e.to_string())?;
                (v, b) = (nv, nb);
                match send("POST", &api(&format!("/{name}/reshape")), json!({"view": view, "verbs": verbs, "by": by})) {
                    Ok(ev) => output(format_args!("reshaped v{} ({})\n", ev["version"], [view.is_some().then_some("view"), verbs.is_some().then_some("verbs")].into_iter().flatten().collect::<Vec<_>>().join(" + ")))?,
                    Err(e) => { eprintln!("not reshaped: {e}"); },
                }
            }
        }
        Cmd::Design { set } => {
            let p = project();
            if p.is_empty() { return Err("not in a project: run from a git repo or pass --project".into()); }
            let url = format!("{}/api/projects/{}/design", cli.url, encode(&p));
            match set {
                Some(f) => {
                    ureq::put(&url).send_string(&read(&f)?).map_err(describe)?;
                    output(format_args!("{p}: DESIGN.md set. Views use it with <link rel=\"stylesheet\" href=\"/p/{p}/design.css\">\n"))?;
                    Ok(())
                }
                None => { output(format_args!("{}", ureq::get(&url).call().map_err(describe)?.into_string().map_err(|e| e.to_string())?))?; Ok(()) }
            }
        }
        Cmd::Archive { name } => report(send("POST", &api(&format!("/{}/archive", id(&name)?)), json!({"by": by}))?),
        Cmd::Restore { name } => report(send("POST", &api(&format!("/{}/restore", id(&name)?)), json!({"by": by}))?),
        Cmd::Asset { name, files } => {
            let added = add_assets(&api(&format!("/{}", id(&name)?)), &files, &by)?;
            for line in added {
                output(format_args!("{line}\n"))?;
            }
            Ok(())
        }
        Cmd::Share { name, out } => {
            let name = id(&name)?;
            let resp = ureq::get(&api(&format!("/{name}/share"))).call().map_err(describe)?;
            let mut html = String::new();
            resp.into_reader().read_to_string(&mut html).map_err(|e| e.to_string())?;
            let short = name.split_once(':').map_or(name.as_str(), |(_, n)| n);
            let out = out.unwrap_or_else(|| PathBuf::from(format!("{short}.html")));
            std::fs::write(&out, &html).map_err(|e| format!("{}: {e}", out.display()))?;
            output(format_args!("{}  ({} KB, read-only, opens anywhere)\n", out.display(), html.len().div_ceil(1024)))?;
            Ok(())
        }
        Cmd::Inbox { hook, session_id } => inbox(&cli.url, &project(), hook, cli.by, session_id),
        Cmd::Resolve { name, id: cid } => report(send("POST", &api(&format!("/{}/resolve", id(&name)?)), json!({"id": cid, "by": by}))?),
        Cmd::Open { name } => {
            let url = match (name.as_deref(), project().as_str()) {
                (Some(n), _) => format!("{}/j/{}/", cli.url, id(n)?),
                (None, "") => format!("{}/", cli.url),
                (None, p) => format!("{}/p/{p}", cli.url),
            };
            let browser = open_browser(&url);
            let printed = output(format_args!("{url}\n"));
            if let Err(e) = browser {
                eprintln!("jelly: could not open a browser ({e}); open {url} manually.");
            }
            printed
        }
    }
}

fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(not(target_os = "windows"))]
    command.arg(url);

    #[cfg(target_os = "windows")]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("cmd.exe");
        // Delayed expansion keeps URL characters out of cmd's command parsing.
        command.args(["/d", "/v:on", "/c"])
            .raw_arg("start \"\" \"!JELLY_OPEN_URL!\"")
            .env("JELLY_OPEN_URL", url);
        command
    };

    let opener = command.get_program().to_string_lossy().into_owned();
    let status = command.status().map_err(|e| format!("{opener}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{opener}: {status}")) }
}

/// If the repo has a DESIGN.md, give it to the project so views can use it.
fn sync_design(url: &str, project: &str) {
    if project.is_empty() { return; }
    let Ok(out) = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output() else { return };
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if let Ok(md) = std::fs::read_to_string(Path::new(&top).join("DESIGN.md")) {
        let _ = ureq::put(&format!("{url}/api/projects/{}/design", encode(project))).send_string(&md);
    }
}

fn select_inbox_reader(
    named: Option<String>,
    session: Option<String>,
    claude: Option<String>,
    hook: impl FnOnce() -> Option<String>,
    parent: impl FnOnce() -> Result<store::InboxReader, String>,
) -> Result<store::InboxReader, String> {
    if let Some(name) = named {
        return Ok(store::InboxReader::Named { name });
    }
    if let Some(id) = session.filter(|s| !s.is_empty())
        .or_else(|| claude.filter(|s| !s.is_empty()))
        .or_else(|| hook().filter(|s| !s.is_empty()))
    {
        return Ok(store::InboxReader::Conversation { id });
    }
    parent()
}

fn hook_session(input: impl Read) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct HookInput { session_id: Option<String> }
    serde_json::from_reader::<_, HookInput>(input).ok()?.session_id
}

fn parent_reader() -> Result<store::InboxReader, String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let current = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[current]), true);
    let parent = system.process(current).and_then(|p| p.parent()).ok_or("could not find invoking parent process")?;
    system.refresh_processes(ProcessesToUpdate::Some(&[parent]), true);
    let process = system.process(parent).ok_or("could not find parent process start time")?;
    Ok(store::InboxReader::Parent { pid: parent.as_u32(), start_time: process.start_time() })
}

/// Prints what the person did since the last call, then moves the cursor.
fn inbox(url: &str, project: &str, hook: bool, named: Option<String>, session_id: Option<String>) -> Result<(), CommandError> {
    if hook && project.is_empty() {
        return Ok(()); // not in a repo: this session has no jellies of its own
    }
    let reader = select_inbox_reader(
        named,
        session_id,
        std::env::var("CLAUDE_CODE_SESSION_ID").ok(),
        || {
            if !hook || std::io::stdin().is_terminal() { return None; }
            hook_session(std::io::stdin().lock())
        },
        parent_reader,
    );
    deliver_inbox(url, project, hook, reader, &mut std::io::stdout().lock())
}

fn deliver_inbox(
    url: &str,
    project: &str,
    hook: bool,
    reader: Result<store::InboxReader, String>,
    output: &mut impl Write,
) -> Result<(), CommandError> {
    let result = (|| {
        let request = store::InboxRequest { project: project.into(), reader: reader? };
        let response = ureq::post(&format!("{url}/api/inbox"))
            .timeout(std::time::Duration::from_secs(1))
            .send_json(&request);
        let response = match response {
            // No daemon is silence; a daemon that does not answer is worth a line.
            Err(ureq::Error::Transport(t)) if hook && t.kind() == ureq::ErrorKind::ConnectionFailed => return Ok(()),
            result => result.map_err(describe)?,
        };
        let batch: store::InboxBatch = response.into_json().map_err(|e| e.to_string())?;
        write_inbox(output, project, hook, &batch).map_err(CommandError::Output)?;
        ureq::post(&format!("{url}/api/inbox/commit"))
            .timeout(std::time::Duration::from_secs(1))
            .send_json(store::InboxCommit { inbox: request, through: batch.through })
            .map_err(describe)?;
        Ok(())
    })();
    match result {
        Err(CommandError::Message(e)) if hook => writeln!(output, "jelly inbox failed: {e}").map_err(CommandError::Output),
        result => result,
    }
}

fn write_inbox(output: &mut impl Write, project: &str, hook: bool, batch: &store::InboxBatch) -> std::io::Result<()> {
    if batch.events.is_empty() {
        if !hook {
            writeln!(output, "Nothing new from the person.")?;
        }
    } else {
        let scope = if project.is_empty() { String::new() } else { format!(" (project {project})") };
        writeln!(output, "The person acted on your jellies{scope} since you last checked:")?;
        for item in &batch.events {
            writeln!(output, "- {}: {}", item.event.jelly, item.text)?;
        }
        writeln!(output, "Details: `jelly state NAME`, `jelly comments`. Answer on a comment with `jelly reply`.")?;
    }
    output.flush()
}

/// The project from `--project`, else the git repo's folder name, made into a
/// valid name ("Archon" -> "archon"). Empty when neither applies.
fn detect_project(given: Option<String>) -> String {
    let raw = given.or_else(|| {
        let out = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output().ok()?;
        let top = String::from_utf8(out.stdout).ok()?;
        out.status.success().then(|| Path::new(top.trim()).file_name()?.to_str().map(String::from)).flatten()
    });
    let name: String = raw.unwrap_or_default().to_lowercase().chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' { c } else { '-' }).collect();
    name.trim_matches('-').chars().take(64).collect()
}

/// Blocks until attention item `item` on `jelly` is answered or withdrawn, and
/// returns it. It subscribes before reading the item, and reads it again on
/// every event of that jelly or lag notification, so no answer is missed.
fn await_answer(url: &str, jelly: &str, item: &str, timeout: Option<u64>) -> Result<store::Attention, String> {
    let mut req = ureq::get(&format!("{url}/api/events?jelly={}", encode(jelly)));
    if let Some(t) = timeout {
        req = req.timeout(std::time::Duration::from_secs(t));
    }
    let settled = || -> Result<Option<store::Attention>, String> {
        let j = get(&format!("{url}/api/jellies/{jelly}"))?;
        let items = Vec::<store::Attention>::deserialize(&j["attention"]).map_err(|e| e.to_string())?;
        let found = items.into_iter().find(|a| a.id == item).ok_or_else(|| format!("{jelly} has no attention item {item}"))?;
        Ok((!found.is_open()).then_some(found))
    };
    let resp = req.call().map_err(describe)?;
    read_answer(BufReader::new(resp.into_reader()), item, settled)
}

fn read_answer(
    events: impl BufRead,
    item: &str,
    mut settled: impl FnMut() -> Result<Option<store::Attention>, String>,
) -> Result<store::Attention, String> {
    if let Some(done) = settled()? {
        return Ok(done);
    }
    for line in events.lines() {
        match line {
            Ok(l) if l.starts_with("data:") || l == ": resync" => {
                if let Some(done) = settled()? {
                    return Ok(done);
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock => {
                eprintln!("jelly: nobody answered {item} in time");
                std::process::exit(2);
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("the event stream ended".into())
}

/// What the person made of an ask: `a1 answered: roomy · "keep the table"`.
fn answer_line(item: &store::Attention) -> String {
    let answer = match (&item.withdrawn, &item.answer) {
        (None, Some(answer)) => answer,
        _ => return format!("{} withdrawn", item.id),
    };
    let option = answer.option.clone().filter(|o| !o.is_empty());
    let note = Some(&answer.text).filter(|t| !t.is_empty()).map(|t| format!("{t:?}"));
    let what: Vec<String> = option.into_iter().chain(note).collect();
    format!("{} answered: {}", item.id, what.join(" · "))
}

/// One line on what a write did. The full event is in `jelly events`.
fn report(ev: Value) -> Result<(), CommandError> {
    output(format_args!("{}\n", report_line(&ev)))?;
    Ok(())
}

fn report_line(ev: &Value) -> String {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let what = match ev["kind"].as_str().unwrap_or("") {
        "call" => format!("{} ran", s(&ev["verb"])),
        "attention" => format!("asked {} {:?}; {}. Answers arrive in `jelly inbox`", s(&ev["args"]["id"]), s(&ev["args"]["title"]), placement(ev)),
        "set" => if s(&ev["args"]["pointer"]).is_empty() { "state replaced".into() } else { format!("set {}", s(&ev["args"]["pointer"])) },
        "new" => "made".into(),
        k => k.to_string(),
    };
    let changed: Vec<String> = ev["changed"].as_array().into_iter().flatten().filter_map(|c| c.as_str()).filter(|c| !c.is_empty()).map(String::from).collect();
    let changed = match changed.len() { 0 => String::new(), 1..=6 => format!(" · changed {}", changed.join(" ")), n => format!(" · changed {n} places") };
    format!("{} v{}: {what}{changed} · since seq:{}", s(&ev["jelly"]), ev["version"], ev["seq"])
}

/// What `jelly ask --spec` did, in one line.
fn ask_line(jelly: &str, round: &Value, questions: usize, since: &str) -> String {
    format!("{jelly} round {round}: asked {questions} question{}. Answers arrive in `jelly inbox`, or block on `jelly wait {jelly} --since {since}`", if questions == 1 { "" } else { "s" })
}

/// One comment and its thread, for an agent's to-do list.
fn comment_line(jelly: &str, c: &store::Comment) -> String {
    let thread = c.replies.last().map(|last| format!(" · {} repl{}, last by {}: {:?}", c.replies.len(), if c.replies.len() == 1 { "y" } else { "ies" }, last.by, last.text)).unwrap_or_default();
    format!("{jelly}  {} on {}: {:?}{thread}{}", c.id, c.anchor["selector"].as_str().unwrap_or("the page"), c.text, if c.resolved { " (resolved)" } else { "" })
}

/// The change that ended `jelly wait`, in one line.
fn wait_line(e: &store::Event) -> String {
    format!("{} v{}: {} {}", e.jelly, e.version, e.by, e.describe())
}

/// Where an ask or look shows on the page, as the daemon found it: `anchor`
/// says what became of its `--at`, and is absent when nothing could check.
fn placement(ev: &Value) -> String {
    let Some(at) = ev["args"]["anchor"]["selector"].as_str() else { return "it shows at the top of the page".into() };
    use server::AnchorStatus;
    let status = AnchorStatus::deserialize(&ev["anchor"]).ok();
    let inline = ev["anchor_inline"] == true;
    if ev["anchor_exposed"] == true && status != Some(AnchorStatus::Invalid) {
        return format!("{at} is in a part that is unavailable right now, so the question is shown at the top");
    }
    let fallback = if inline { "placement not checked: the view places its own attention" } else { "so it shows at the top" };
    match status {
        Some(AnchorStatus::Below) => format!("it shows on the page below {at}"),
        Some(AnchorStatus::Invalid) => format!("{at} has invalid CSS selector syntax, {fallback}"),
        Some(AnchorStatus::Missing) => format!("{at} matches nothing on the page, {fallback}"),
        Some(AnchorStatus::Floating) => format!("{at} is in a fixed or sticky part of the page, {fallback}"),
        Some(AnchorStatus::Hidden) => if inline {
            format!("{at} is not on screen right now, placement not checked: the view places its own attention")
        } else {
            format!("it shows below {at}, which is not on screen right now")
        },
        Some(AnchorStatus::Fallback) => {
            let destination = server::AnchorDestination::deserialize(&ev["anchor_destination"])
                .map_or_else(|_| "somewhere else".into(), |d| d.describe());
            format!("it shows {destination} instead; {}", ev["anchor_reason"].as_str().unwrap_or(""))
        },
        _ => format!("it shows below {at} if that matches something, else at the top: not checked ({})",
            ev["anchor_unchecked"].as_str().unwrap_or("no snapshot")),
    }
}

/// A JSON argument: literal JSON, @FILE, or - for stdin (for data too big for a command line).
fn json_arg(arg: &str, what: &str) -> Result<Value, String> {
    let text = if arg == "-" { read(&PathBuf::from("-"))? } else if let Some(f) = arg.strip_prefix('@') { read(&PathBuf::from(f))? } else { arg.to_string() };
    serde_json::from_str(&text).map_err(|e| format!("{what} are not JSON: {e}").replace("value are", "value is"))
}

fn read(p: &PathBuf) -> Result<String, String> {
    if p.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
        return Ok(s);
    }
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Adds files beside a jelly; `jelly` is its API address.
fn add_assets(jelly: &str, files: &[PathBuf], by: &str) -> Result<Vec<String>, String> {
    let mut added = Vec::new();
    for f in files {
        let file = f.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("{}: not a file name", f.display()))?.to_string();
        let bytes = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
        ureq::post(&format!("{jelly}/assets/{}?by={}", encode(&file), encode(by)))
            .set("content-type", "application/octet-stream")
            .send_bytes(&bytes)
            .map_err(describe)?;
        added.push(format!("assets/{file}  ({} KB)", bytes.len().div_ceil(1024)));
    }
    Ok(added)
}

fn get(url: &str) -> Result<Value, String> {
    ureq::get(url).call().map_err(describe)?.into_json().map_err(|e| e.to_string())
}

fn send(method: &str, url: &str, body: Value) -> Result<Value, String> {
    let req = ureq::request(method, url);
    let resp = if body.is_null() { req.call() } else { req.send_json(body) };
    resp.map_err(describe)?.into_json().map_err(|e| e.to_string())
}

fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(_, resp) => resp
            .into_json::<Value>()
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| "request failed".into()),
        ureq::Error::Transport(t) => format!("cannot reach the selected daemon ({t}); check --url / JELLY_URL and the server"),
    }
}

fn print(v: Value) -> Result<(), CommandError> {
    output(format_args!("{}\n", serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?))?;
    Ok(())
}

/// Percent-encodes a query value.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{answer_line, placement, wait_line, write_output};
    use std::io::{self, Write};
    use serde_json::json;

    #[test]
    fn startup_precedes_dispatch_and_hooks_suppress_its_errors() {
        use super::{run_with_start, Cli};
        use clap::Parser;
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap();
        let command = super::daemon_command().unwrap();
        assert_eq!(command.get_program(), std::env::current_exe().unwrap());
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["serve"]);
        assert_eq!(command.get_envs().count(), 0, "inherit home and other environment defaults");
        let cli = parse(&["jelly", "--url", super::startup::DEFAULT_URL, "open"]);
        let result = run_with_start(cli, |url, quiet| {
            assert_eq!(url, super::startup::DEFAULT_URL);
            assert!(!quiet);
            Err("startup failed before opening a browser".into())
        });
        assert_eq!(result.unwrap_err().to_string(), "startup failed before opening a browser");
        for alias in ["http://localhost:7878/", "HTTP://127.1:7878"] {
            let cli = parse(&["jelly", "--url", alias, "open"]);
            assert_eq!(run_with_start(cli, |url, quiet| {
                assert_eq!(url, super::startup::DEFAULT_URL);
                assert!(!quiet);
                Err("alias startup".into())
            }).unwrap_err().to_string(), "alias startup");
        }
        for project in ["", "test"] {
            let cli = parse(&["jelly", "--url", super::startup::DEFAULT_URL, "--project", project, "inbox", "--hook"]);
            assert!(run_with_start(cli, |_, quiet| {
                assert!(quiet);
                Err("startup failed".into())
            }).is_ok());
        }
        let cli = parse(&["jelly", "--url", super::startup::DEFAULT_URL, "--project", "", "inbox", "--hook"]);
        let mut called = false;
        assert!(run_with_start(cli, |_, quiet| { called = true; assert!(quiet); Ok(()) }).is_ok());
        assert!(called, "outside-project hook must start before its early return");

        let home = tempfile::NamedTempFile::new().unwrap();
        let cli = parse(&["jelly", "serve", "--port", "0", "--home", home.path().to_str().unwrap()]);
        assert!(run_with_start(cli, |_, _| panic!("serve must bypass startup")).is_err());
        let cli = parse(&["jelly", "--url", "http://127.0.0.1:9999", "--project", "", "inbox", "--hook"]);
        assert!(run_with_start(cli, |_, _| panic!("custom URL must bypass startup")).is_ok());
    }

    #[test]
    fn output_preserves_write_and_flush_errors() {
        struct Fault { kind: io::ErrorKind, on_flush: bool }
        impl Write for Fault {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.on_flush { Ok(bytes.len()) } else { Err(self.kind.into()) }
            }
            fn flush(&mut self) -> io::Result<()> { Err(self.kind.into()) }
        }
        for kind in [io::ErrorKind::BrokenPipe, io::ErrorKind::PermissionDenied] {
            for on_flush in [false, true] {
                let error = write_output(&mut Fault { kind, on_flush }, format_args!("no newline")).unwrap_err();
                assert_eq!(error.kind(), kind);
            }
        }
    }

    #[test]
    fn wait_prints_the_change_in_one_line() {
        let ev: crate::store::Event = serde_json::from_value(json!({"seq": 9, "at_ms": 1, "jelly": "p:x", "kind": "answer", "by": "person", "version": 4, "view_rev": 1,
            "args": {"id": "a1", "title": "Which order?", "option": "b", "text": "keep it"}, "state": {"big": "state"}})).unwrap();
        assert_eq!(wait_line(&ev), "p:x v4: person answered a1 \"Which order?\" → b: \"keep it\"");
    }

    fn inbox_response(body: &str) -> (String, std::thread::JoinHandle<std::net::TcpListener>) {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let body = body.to_owned();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut input = std::io::BufReader::new(&stream);
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "POST /api/inbox HTTP/1.1");
            let mut length = 0;
            loop {
                line.clear();
                input.read_line(&mut line).unwrap();
                if line == "\r\n" { break; }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") { length = value.trim().parse().unwrap(); }
                }
            }
            input.read_exact(&mut vec![0; length]).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            listener
        });
        (url, server)
    }

    fn assert_no_inbox_commit(server: std::thread::JoinHandle<std::net::TcpListener>) {
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn inbox_reader_failures_are_visible_in_hooks() {
        for error in ["could not find invoking parent process", "could not find parent process start time"] {
            let mut output = Vec::new();
            super::deliver_inbox("unused", "p", true, Err(error.into()), &mut output).unwrap();
            assert_eq!(String::from_utf8(output).unwrap(), format!("jelly inbox failed: {error}\n"));
            assert_eq!(super::deliver_inbox("unused", "p", false, Err(error.into()), &mut Vec::new()).unwrap_err().to_string(), error);
        }
    }

    #[test]
    fn inbox_undecodable_responses_do_not_commit() {
        for body in ["broken", "", "[]", r#"{"events":[{"jelly":"p:sample"}],"through":4}"#] {
            let (url, server) = inbox_response(body);
            let reader = crate::store::InboxReader::Named { name: "a".into() };
            assert!(super::deliver_inbox(&url, "p", false, Ok(reader), &mut Vec::new()).is_err());
            assert_no_inbox_commit(server);
        }
    }

    #[test]
    fn inbox_output_failures_do_not_commit() {
        use std::io::{self, Write};
        struct FailingOutput { fail_write: bool }
        impl Write for FailingOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.fail_write { Err(io::Error::other("output failed")) } else { Ok(bytes.len()) }
            }
            fn flush(&mut self) -> io::Result<()> { Err(io::Error::other("flush failed")) }
        }
        for fail_write in [true, false] {
            let body = json!({"events": [{"seq": 4, "at_ms": 1, "jelly": "p:sample", "kind": "call",
                "by": "person", "version": 1, "view_rev": 1, "text": "click {}"}], "through": 4}).to_string();
            let (url, server) = inbox_response(&body);
            let reader = crate::store::InboxReader::Named { name: "a".into() };
            let error = super::deliver_inbox(&url, "p", false, Ok(reader), &mut FailingOutput { fail_write }).unwrap_err();
            assert_eq!(error.to_string(), if fail_write { "output failed" } else { "flush failed" });
            assert_no_inbox_commit(server);
        }
    }

    #[test]
    fn inbox_identity_precedence_and_namespaces() {
        use super::{hook_session, select_inbox_reader};
        use crate::store::InboxReader;
        let named = |name: &str| InboxReader::Named { name: name.into() };
        let conversation = |id: &str| InboxReader::Conversation { id: id.into() };
        let parent = || Ok(InboxReader::Parent { pid: 42, start_time: 100 });
        let resolve = |name: Option<&str>, session: Option<&str>, claude: Option<&str>, hook: Option<&str>| {
            select_inbox_reader(name.map(str::to_string), session.map(str::to_string), claude.map(str::to_string), || hook.map(str::to_string), parent).unwrap()
        };
        assert_eq!(resolve(Some("agent"), Some("session"), Some("claude"), Some("hook")), named("agent"));
        assert_eq!(resolve(None, Some("session"), Some("claude"), Some("hook")), conversation("session"));
        assert_eq!(resolve(None, Some(""), Some("claude"), Some("hook")), conversation("claude"));
        assert_eq!(resolve(None, None, Some(""), Some("hook")), conversation("hook"));
        assert_eq!(resolve(None, None, None, None), parent().unwrap());
        assert_ne!(named("agent"), conversation("agent"));
        assert_ne!(parent().unwrap(), InboxReader::Parent { pid: 42, start_time: 101 });
        assert_eq!(select_inbox_reader(Some("a".into()), None, None, || panic!("must not read stdin"), || panic!("must not inspect processes")).unwrap(), named("a"));
        assert_eq!(select_inbox_reader(None, Some("a".into()), None, || panic!("must not read stdin"), || panic!("must not inspect processes")).unwrap(), conversation("a"));
        assert!(select_inbox_reader(None, None, None, || None, || Err("no parent".into())).is_err());
        assert_eq!(hook_session(&b"{\"session_id\":\"resumed\",\"prompt\":\"ignored\"}"[..]), Some("resumed".into()));
        for input in ["", "broken", "{}"] {
            assert_eq!(hook_session(input.as_bytes()), None);
        }
    }

    #[test]
    fn an_answer_is_reread_after_an_sse_resync() {
        for notification in [": resync", "data: {}"] {
            let events = format!("{notification}\n\n");
            let mut reads = 0;
            let answer = super::read_answer(events.as_bytes(), "a1", || {
                reads += 1;
                match reads {
                    1 => Ok(None),
                    2 => Ok(Some(serde_json::from_value(json!({
                        "id":"a1", "kind":"ask", "title":"Q", "by":"agent", "at_ms":1,
                        "answer":{"option":"b", "text":"", "by":"person", "at_ms":2}
                    })).unwrap())),
                    _ => panic!("unexpected attention read"),
                }
            }).unwrap();
            assert_eq!(reads, 2);
            assert_eq!(answer.answer.unwrap().option.as_deref(), Some("b"));
        }
    }

    #[test]
    fn an_answer_is_one_line() {
        let item = |extra: serde_json::Value| {
            let mut a = json!({"id": "a1", "kind": "ask", "title": "Q", "by": "agent", "at_ms": 1});
            a.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            answer_line(&serde_json::from_value(a).unwrap())
        };
        let answered = |option: serde_json::Value, text: &str| item(json!({"answer": {"option": option, "text": text, "by": "person", "at_ms": 2}}));
        assert_eq!(answered(json!("roomy"), "but keep the dense table"), r#"a1 answered: roomy · "but keep the dense table""#);
        assert_eq!(answered(json!("roomy"), ""), "a1 answered: roomy");
        assert_eq!(answered(json!(null), "neither"), r#"a1 answered: "neither""#);
        assert_eq!(item(json!({"withdrawn": {"by": "agent", "at_ms": 2}})), "a1 withdrawn");
    }

    #[test]
    fn an_ask_says_where_its_card_really_shows() {
        let at = |extra: serde_json::Value| {
            let mut ev = json!({"args": {"anchor": {"selector": "#list"}}});
            ev.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            placement(&ev)
        };
        assert_eq!(at(json!({"anchor": "below"})), "it shows on the page below #list");
        for exposed in [false, true] {
            assert_eq!(placement(&json!({"args":{"anchor":{"selector":"["}}, "anchor":"invalid", "anchor_exposed":exposed})), "[ has invalid CSS selector syntax, so it shows at the top");
        }
        for (status, diagnosis) in [("invalid", "has invalid CSS selector syntax"), ("missing", "matches nothing on the page"), ("floating", "is in a fixed or sticky part of the page"), ("hidden", "is not on screen right now")] {
            assert_eq!(at(json!({"anchor": status, "anchor_inline": true})), format!("#list {diagnosis}, placement not checked: the view places its own attention"));
        }
        assert_eq!(at(json!({"anchor": "missing"})), "#list matches nothing on the page, so it shows at the top");
        assert_eq!(at(json!({"anchor": "floating"})), "#list is in a fixed or sticky part of the page, so it shows at the top");
        assert_eq!(at(json!({"anchor": "hidden"})), "it shows below #list, which is not on screen right now");
        assert_eq!(at(json!({"anchor": "hidden", "anchor_exposed": true})), "#list is in a part that is unavailable right now, so the question is shown at the top");
        assert_eq!(at(json!({"anchor":"fallback", "anchor_destination":{"kind":"after", "selector":"#tiles"}, "anchor_reason":"layout"})), "it shows after #tiles instead; layout");
        assert_eq!(at(json!({"anchor":"fallback", "anchor_destination":{"kind":"top"}, "anchor_reason":"no boundary"})), "it shows at the top instead; no boundary");

        let unchecked = at(json!({"anchor_unchecked": "no Chrome found"}));
        assert!(unchecked.contains("not checked (no Chrome found)") && !unchecked.starts_with("it shows on the page below"));
        assert_eq!(placement(&json!({"args": {"anchor": null}})), "it shows at the top of the page");
    }
}
