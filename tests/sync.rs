//! A pushed jelly keeps following the local one: every later change reaches the
//! hosted copy without another command, and a remote that was down catches up.

use serde_json::{json, Value};
use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_jelly");

struct Daemon {
    child: Child,
    url: String,
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl Daemon {
    fn start(home: &Path, port: u16, hosted: bool) -> Self {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let mut cmd = Command::new(BIN);
            cmd.arg("serve");
            if hosted { cmd.arg("--hosted"); }
            let child = cmd.args(["--port", &port.to_string(), "--home"]).arg(home)
                .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
            let mut daemon = Self { child, url: format!("http://127.0.0.1:{port}") };
            loop {
                if ureq::get(&format!("{}/api/jellies", daemon.url)).call().is_ok() { return daemon; }
                if let Some(status) = daemon.child.try_wait().unwrap() {
                    let mut errors = String::new();
                    daemon.child.stderr.take().unwrap().read_to_string(&mut errors).unwrap();
                    // The port of a daemon just killed can still be busy for a moment.
                    assert!(Instant::now() < deadline, "daemon exited ({status}): {errors}");
                    std::thread::sleep(Duration::from_millis(200));
                    break;
                }
                assert!(Instant::now() < deadline, "daemon did not start");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    fn jelly(&self, client: &Path) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env_remove("JELLY_AS").env_remove("JELLY_SESSION_ID").env_remove("CLAUDE_CODE_SESSION_ID")
            .arg("--url").arg(&self.url).env("JELLY_PROJECT", "p").env("JELLY_HOME", client)
            .env("HOME", client).stdin(Stdio::null());
        cmd
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stdout(output: Output) -> String {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

fn failure(output: Output) -> String {
    assert!(!output.status.success(), "expected a failure, got: {}", String::from_utf8_lossy(&output.stdout));
    String::from_utf8(output.stderr).unwrap()
}

/// A GET's JSON, or the status it failed with (0 when the daemon is unreachable).
fn get(url: &str) -> Result<Value, u16> {
    match ureq::get(url).call() {
        Ok(r) => Ok(r.into_json().unwrap()),
        Err(ureq::Error::Status(code, _)) => Err(code),
        Err(ureq::Error::Transport(_)) => Err(0),
    }
}

/// Polls `check` until it holds or `secs` pass; the message names what stayed wrong.
fn until(secs: u64, what: &str, mut check: impl FnMut() -> Result<(), String>) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last = String::new();
    while Instant::now() < deadline {
        match check() {
            Ok(()) => return,
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what} did not happen within {secs}s: {last}");
}

const VIEW: &str = "<p>sync test</p>";
const VIEW2: &str = "<p>sync test, reshaped</p>";
const VERBS: &str = "const verbs = {increment(s) { s.count += 1; }, clear() { return null; }};";

#[test]
fn pushed_jellies_follow_their_local_jelly_and_catch_up_after_downtime() {
    let temp = tempfile::tempdir().unwrap();
    let hosted_home = temp.path().join("hosted");
    let local_home = temp.path().join("local");
    let client = temp.path().join("client");
    let key = stdout(Command::new(BIN).args(["keys", "new", "alice", "--home"]).arg(&hosted_home).output().unwrap()).trim().to_string();
    let hosted_port = free_port();
    let hosted = Daemon::start(&hosted_home, hosted_port, true);
    let local = Daemon::start(&local_home, free_port(), false);
    let remote_api = format!("{}/people/alice/api/jellies", hosted.url);

    let wrong = failure(local.jelly(&client).args(["remote", "add", &hosted.url, "jk_wrong"]).output().unwrap());
    assert!(wrong.contains("not known"), "{wrong}");
    let added = stdout(local.jelly(&client).args(["remote", "add", &format!("{}/", hosted.url), &key]).output().unwrap());
    assert_eq!(added.trim(), format!("alice at {}", hosted.url));
    let remotes = stdout(local.jelly(&client).args(["remote", "ls"]).output().unwrap());
    assert!(remotes.contains(&hosted.url) && remotes.contains("as alice") && !remotes.contains(&key), "{remotes}");
    let remotes_file = std::fs::read_to_string(local_home.join("remotes.json")).unwrap();
    assert!(remotes_file.contains(&key), "the local daemon keeps the key it pushes with");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(local_home.join("remotes.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the key file is readable by its owner only");
    }

    std::fs::write(temp.path().join("view.html"), VIEW).unwrap();
    std::fs::write(temp.path().join("view2.html"), VIEW2).unwrap();
    std::fs::write(temp.path().join("verbs.js"), VERBS).unwrap();
    std::fs::write(temp.path().join("state.json"), r#"{"count": 1}"#).unwrap();
    std::fs::write(temp.path().join("note.txt"), b"first asset").unwrap();
    std::fs::write(temp.path().join("later.txt"), b"added later").unwrap();
    stdout(local.jelly(&client).args(["new", "plan", "--about", "A plan", "--view"]).arg(temp.path().join("view.html"))
        .arg("--verbs").arg(temp.path().join("verbs.js")).arg("--state").arg(temp.path().join("state.json")).output().unwrap());
    stdout(local.jelly(&client).args(["asset", "plan"]).arg(temp.path().join("note.txt")).output().unwrap());

    let unshared = stdout(local.jelly(&client).args(["ls"]).output().unwrap());
    assert!(!unshared.contains("shared at"), "{unshared}");
    let no_link = failure(local.jelly(&client).args(["unshare", "plan"]).output().unwrap());
    assert!(no_link.contains("not shared"), "{no_link}");

    let pushed = stdout(local.jelly(&client).args(["push", "plan", "--to", &hosted.url]).output().unwrap());
    let url = format!("{}/people/alice/j/p:plan/", hosted.url);
    assert_eq!(pushed.trim(), format!("{url}  (shared; changes here sync to it)"));
    let remote = get(&format!("{remote_api}/p:plan")).unwrap();
    assert_eq!(remote["view"], VIEW);
    assert_eq!(remote["verbs"], VERBS);
    assert_eq!(remote["state"], json!({"count": 1}));
    assert_eq!(remote["about"], "A plan");
    assert_eq!(get(&format!("{remote_api}/p:plan/assets")).unwrap(), json!(["note.txt"]));
    let bytes = ureq::get(&format!("{}/people/alice/j/p:plan/assets/note.txt", hosted.url)).call().unwrap().into_string().unwrap();
    assert_eq!(bytes, "first asset");
    let listed = stdout(local.jelly(&client).args(["ls"]).output().unwrap());
    assert!(listed.contains(&format!("shared at {url} (synced)")), "{listed}");

    let remote_state = || get(&format!("{remote_api}/p:plan")).map(|j| j["state"].clone());
    stdout(local.jelly(&client).args(["call", "plan", "increment"]).output().unwrap());
    until(10, "a verb call reaching the remote", || (remote_state() == Ok(json!({"count": 2}))).then_some(()).ok_or_else(|| format!("{:?}", remote_state())));
    stdout(local.jelly(&client).args(["set", "plan", r#"{"count": 10}"#]).output().unwrap());
    until(10, "a set reaching the remote", || (remote_state() == Ok(json!({"count": 10}))).then_some(()).ok_or_else(|| format!("{:?}", remote_state())));
    // A verb that returns null leaves the local state null; the remote holds that as {}.
    stdout(local.jelly(&client).args(["call", "plan", "clear"]).output().unwrap());
    until(10, "a null state reaching the remote", || (remote_state() == Ok(json!({}))).then_some(()).ok_or_else(|| format!("{:?}", remote_state())));
    until(10, "ls saying synced after a null state", || { let l = stdout(local.jelly(&client).args(["ls"]).output().unwrap()); l.contains("(synced)").then_some(()).ok_or(l) });
    stdout(local.jelly(&client).args(["set", "plan", r#"{"count": 10}"#]).output().unwrap());
    until(10, "a set after the null state reaching the remote", || (remote_state() == Ok(json!({"count": 10}))).then_some(()).ok_or_else(|| format!("{:?}", remote_state())));
    stdout(local.jelly(&client).args(["reshape", "plan", "--view"]).arg(temp.path().join("view2.html")).output().unwrap());
    let remote_view = || get(&format!("{remote_api}/p:plan")).map(|j| j["view"].clone());
    until(10, "a reshape reaching the remote", || (remote_view() == Ok(json!(VIEW2))).then_some(()).ok_or_else(|| format!("{:?}", remote_view())));
    stdout(local.jelly(&client).args(["undo", "plan"]).output().unwrap());
    until(10, "an undo reaching the remote", || (remote_view() == Ok(json!(VIEW))).then_some(()).ok_or_else(|| format!("{:?}", remote_view())));
    stdout(local.jelly(&client).args(["asset", "plan"]).arg(temp.path().join("later.txt")).output().unwrap());
    let remote_assets = || get(&format!("{remote_api}/p:plan/assets"));
    until(10, "an asset reaching the remote", || (remote_assets() == Ok(json!(["later.txt", "note.txt"]))).then_some(()).ok_or_else(|| format!("{:?}", remote_assets())));
    std::fs::write(temp.path().join("note.txt"), b"changed bytes").unwrap();
    stdout(local.jelly(&client).args(["asset", "plan"]).arg(temp.path().join("note.txt")).output().unwrap());
    until(10, "changed asset bytes reaching the remote", || {
        let bytes = ureq::get(&format!("{}/people/alice/j/p:plan/assets/note.txt", hosted.url)).call().unwrap().into_string().unwrap();
        (bytes == "changed bytes").then_some(()).ok_or(bytes)
    });
    // Comments stay with the owner: the remote copy is not a place to act on them.
    stdout(local.jelly(&client).args(["--as", "person", "call", "plan", "increment"]).output().unwrap());
    until(10, "the person's call reaching the remote", || (remote_state() == Ok(json!({"count": 11}))).then_some(()).ok_or_else(|| format!("{:?}", remote_state())));
    assert_eq!(get(&format!("{remote_api}/p:plan")).unwrap()["comments"], json!([]));

    let ls = || stdout(local.jelly(&client).args(["ls"]).output().unwrap());
    until(10, "ls saying synced", || { let l = ls(); l.contains("(synced)").then_some(()).ok_or(l) });

    // Removing a shared jelly locally takes its public copy down with it.
    stdout(local.jelly(&client).args(["new", "note", "--view"]).arg(temp.path().join("view.html")).arg("--verbs").arg(temp.path().join("verbs.js")).output().unwrap());
    stdout(local.jelly(&client).args(["push", "note"]).output().unwrap());
    assert!(get(&format!("{remote_api}/p:note")).is_ok());
    stdout(local.jelly(&client).args(["rm", "note"]).output().unwrap());
    until(10, "a removed jelly leaving the remote", || match get(&format!("{remote_api}/p:note")) { Err(404) => Ok(()), other => Err(format!("{other:?}")) });
    until(10, "the removed jelly's link going away", || {
        let links = std::fs::read_to_string(local_home.join("links.json")).unwrap();
        (!links.contains("p:note")).then_some(()).ok_or(links)
    });

    // A push that cannot reach the remote leaves the link behind; the forwarder
    // finishes it when the remote returns, with no further change.
    drop(hosted);
    let unreachable = failure(local.jelly(&client).args(["push", "plan"]).output().unwrap());
    assert!(unreachable.contains("cannot reach"), "{unreachable}");
    until(10, "ls saying behind after a failed push", || { let l = ls(); l.contains("behind: cannot reach").then_some(()).ok_or(l) });
    let hosted = Daemon::start(&hosted_home, hosted_port, true);
    until(30, "ls saying synced after the remote returns", || { let l = ls(); l.contains("(synced)").then_some(()).ok_or(l) });

    drop(hosted);
    stdout(local.jelly(&client).args(["set", "plan", r#"{"count": 99}"#]).output().unwrap());
    let state = stdout(local.jelly(&client).args(["state", "plan"]).output().unwrap());
    assert!(state.contains("99"), "the local jelly keeps working: {state}");
    until(15, "ls saying behind", || { let l = ls(); l.contains("behind: cannot reach").then_some(()).ok_or(l) });

    let hosted = Daemon::start(&hosted_home, hosted_port, true);
    until(30, "the remote catching up after downtime", || (remote_state() == Ok(json!({"count": 99}))).then_some(()).ok_or_else(|| format!("{:?}", remote_state())));
    until(10, "ls saying synced again", || { let l = ls(); l.contains("(synced)").then_some(()).ok_or(l) });

    stdout(local.jelly(&client).args(["move", "plan", "other"]).output().unwrap());
    until(10, "a move reaching the remote", || {
        match (get(&format!("{remote_api}/other:plan")), get(&format!("{remote_api}/p:plan"))) {
            (Ok(j), Err(404)) if j["state"] == json!({"count": 99}) => Ok(()),
            other => Err(format!("{other:?}")),
        }
    });
    let moved_url = format!("{}/people/alice/j/other:plan/", hosted.url);
    until(10, "ls showing the moved link", || { let l = stdout(local.jelly(&client).args(["ls", "--all"]).output().unwrap()); l.contains(&format!("shared at {moved_url} (synced)")).then_some(()).ok_or(l) });

    let refused = failure(local.jelly(&client).args(["remote", "rm", &hosted.url]).output().unwrap());
    assert!(refused.contains("unshare other:plan"), "{refused}");

    // A link change that cannot reach disk is an error, not a success: the link stays.
    let links_path = local_home.join("links.json");
    std::fs::remove_file(&links_path).unwrap();
    std::fs::create_dir(&links_path).unwrap();
    let not_saved = failure(local.jelly(&client).args(["unshare", "other:plan"]).output().unwrap());
    assert!(not_saved.contains("could not save") && not_saved.contains("links.json"), "{not_saved}");
    let kept = stdout(local.jelly(&client).args(["ls", "--all"]).output().unwrap());
    assert!(kept.contains(&format!("shared at {moved_url}")), "{kept}");
    std::fs::remove_dir(&links_path).unwrap();
    std::fs::remove_file(local_home.join("links.json.tmp")).ok();

    let unshared = stdout(local.jelly(&client).args(["unshare", "other:plan", "--rm"]).output().unwrap());
    assert_eq!(unshared.trim(), format!("other:plan: removed from {}", hosted.url));
    assert_eq!(get(&format!("{remote_api}/other:plan")), Err(404));
    let after = stdout(local.jelly(&client).args(["ls", "--all"]).output().unwrap());
    assert!(after.contains("other:plan") && !after.contains("shared at"), "{after}");
    stdout(local.jelly(&client).args(["call", "other:plan", "increment"]).output().unwrap());
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(get(&format!("{remote_api}/other:plan")), Err(404), "an unshared jelly no longer syncs");
    stdout(local.jelly(&client).args(["remote", "rm", &hosted.url]).output().unwrap());
    assert!(stdout(local.jelly(&client).args(["remote", "ls"]).output().unwrap()).contains("No remotes"));
    let _ = &hosted;
}

#[test]
fn a_hosted_person_does_not_push_outward() {
    let temp = tempfile::tempdir().unwrap();
    let hosted_home = temp.path().join("hosted");
    let key = stdout(Command::new(BIN).args(["keys", "new", "alice", "--home"]).arg(&hosted_home).output().unwrap()).trim().to_string();
    let hosted = Daemon::start(&hosted_home, free_port(), true);
    let api = format!("{}/people/alice/api/jellies", hosted.url);
    ureq::post(&api).set("authorization", &format!("Bearer {key}"))
        .send_json(json!({"name": "p:plan", "view": VIEW, "verbs": VERBS, "state": {}, "by": "alice"})).unwrap();
    let push = ureq::post(&format!("{api}/p:plan/push")).set("authorization", &format!("Bearer {key}"))
        .send_json(json!({"by": "alice"})).unwrap_err();
    match push {
        ureq::Error::Status(400, r) => assert!(r.into_string().unwrap().contains("does not push outward")),
        other => panic!("{other}"),
    }
    let listing = get(&api).unwrap();
    assert!(listing[0].get("link").is_none(), "a hosted listing has no link field: {listing}");
}
