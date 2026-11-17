//! A hosted instance keeps one store per person: reads need no key, every
//! change needs the owner's, and listings say whose each jelly is.

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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

impl Daemon {
    fn start_hosted(home: &Path, port: u16) -> Self {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let child = Command::new(BIN).args(["serve", "--hosted", "--port", &port.to_string(), "--home"])
                .arg(home).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
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

fn keys_new(home: &Path, name: &str) -> String {
    let out = stdout(Command::new(BIN).args(["keys", "new", name, "--home"]).arg(home).output().unwrap());
    let key = out.trim().to_string();
    assert!(key.starts_with("jk_"), "{out}");
    key
}

fn status(result: Result<ureq::Response, ureq::Error>) -> u16 {
    match result {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

/// The status a POST gets, with or without a key.
fn post(url: &str, key: Option<&str>, body: Value) -> u16 {
    let mut req = ureq::post(url);
    if let Some(key) = key { req = req.set("authorization", &format!("Bearer {key}")); }
    status(req.send_json(body))
}

fn delete(url: &str, key: Option<&str>) -> u16 {
    let mut req = ureq::delete(url);
    if let Some(key) = key { req = req.set("authorization", &format!("Bearer {key}")); }
    status(req.call())
}

const VIEW: &str = "<p>hosted test</p>";
const VERBS: &str = "const verbs = {};";

#[test]
fn keys_gate_writes_and_everyone_can_read() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("hosted");
    let alice = keys_new(&home, "alice");
    let bob = keys_new(&home, "bob");
    assert_ne!(alice, bob);
    let listed = stdout(Command::new(BIN).args(["keys", "ls", "--home"]).arg(&home).output().unwrap());
    assert!(listed.contains("alice") && listed.contains("bob"), "{listed}");
    let file = std::fs::read_to_string(home.join("keys.json")).unwrap();
    assert!(!file.contains(&alice) && !file.contains(&bob), "keys must be stored hashed");

    let port = free_port();
    let daemon = Daemon::start_hosted(&home, port);
    let plan = json!({"name": "p:plan", "view": VIEW, "verbs": VERBS, "state": {"count": 1}, "by": "agent", "about": "A plan"});
    let alice_api = format!("{}/people/alice/api/jellies", daemon.url);
    let bob_api = format!("{}/people/bob/api/jellies", daemon.url);

    assert_eq!(post(&alice_api, None, plan.clone()), 401);
    assert_eq!(status(ureq::get(&format!("{alice_api}/p:plan")).call()), 404, "nothing was made without a key");
    assert_eq!(post(&alice_api, Some(&bob), plan.clone()), 403);
    assert_eq!(post(&alice_api, Some("jk_made_up"), plan.clone()), 401);
    assert_eq!(post(&alice_api, Some(&alice), plan.clone()), 200);
    assert_eq!(post(&bob_api, Some(&bob), plan.clone()), 200);

    let shown: Value = ureq::get(&format!("{alice_api}/p:plan")).call().unwrap().into_json().unwrap();
    assert_eq!(shown["state"], json!({"count": 1}));
    let page = ureq::get(&format!("{}/people/alice/j/p:plan/", daemon.url)).call().unwrap().into_string().unwrap();
    assert!(page.contains("src=\"/people/alice/runtime.js\"") && page.contains("\"readOnly\":true") && page.contains("\"owner\":\"alice\""), "{page}");
    assert_eq!(status(ureq::get(&format!("{}/people/alice/runtime.js", daemon.url)).call()), 200);
    assert_eq!(status(ureq::get(&format!("{}/people/alice/", daemon.url)).call()), 200, "the person's jelly page");
    assert_eq!(status(ureq::get(&format!("{}/people/carol/", daemon.url)).call()), 404);
    assert_eq!(status(ureq::get(&format!("{}/people/alice/j/p:plan", daemon.url)).call()), 200, "redirect to the slash form stays under the prefix");

    let all: Vec<Value> = ureq::get(&format!("{}/api/jellies", daemon.url)).call().unwrap().into_json().unwrap();
    let mut owners: Vec<(String, String, String)> = all.iter()
        .map(|j| (j["owner"].as_str().unwrap().into(), j["name"].as_str().unwrap().into(), j["url"].as_str().unwrap().into())).collect();
    owners.sort();
    assert_eq!(owners, vec![
        ("alice".into(), "p:plan".into(), "/people/alice/j/p:plan/".into()),
        ("bob".into(), "p:plan".into(), "/people/bob/j/p:plan/".into()),
    ]);
    let index = ureq::get(&format!("{}/", daemon.url)).call().unwrap().into_string().unwrap();
    assert!(index.contains("/people/alice/j/p:plan/") && index.contains("/people/bob/j/p:plan/") && index.contains("A plan"), "{index}");

    assert_eq!(delete(&format!("{bob_api}/p:plan?by=alice"), Some(&alice)), 403);
    assert_eq!(delete(&format!("{bob_api}/p:plan?by=alice"), None), 401);
    assert_eq!(status(ureq::get(&format!("{bob_api}/p:plan")).call()), 200, "bob's copy survives");
    assert_eq!(post(&format!("{bob_api}/p:plan/set"), Some(&alice), json!({"pointer": "", "value": {}, "by": "alice"})), 403);
    assert_eq!(post(&format!("{alice_api}/p:plan/set"), Some(&alice), json!({"pointer": "", "value": {"count": 2}, "by": "alice"})), 200);

    let whoami: Value = ureq::get(&format!("{}/api/whoami", daemon.url)).set("authorization", &format!("Bearer {bob}")).call().unwrap().into_json().unwrap();
    assert_eq!(whoami, json!({"name": "bob"}));
    assert_eq!(status(ureq::get(&format!("{}/api/whoami", daemon.url)).call()), 401);

    let ls = stdout(Command::new(BIN).args(["--url", &daemon.url, "ls", "--all"]).env_remove("JELLY_PROJECT").output().unwrap());
    assert!(ls.contains("by alice") && ls.contains("by bob"), "{ls}");
    let ls_lines: Vec<&str> = ls.lines().filter(|l| l.starts_with("p:plan")).collect();
    assert_eq!(ls_lines.len(), 2, "{ls}");

    stdout(Command::new(BIN).args(["keys", "revoke", "bob", "--home"]).arg(&home).output().unwrap());
    assert_eq!(post(&format!("{bob_api}/p:plan/set"), Some(&bob), json!({"pointer": "", "value": {}, "by": "bob"})), 401, "revocation is immediate");
    assert_eq!(status(ureq::get(&format!("{bob_api}/p:plan")).call()), 200, "bob's jellies stay readable");

    drop(daemon);
    let _restarted = Daemon::start_hosted(&home, port);
    let shown: Value = ureq::get(&format!("{alice_api}/p:plan")).call().unwrap().into_json().unwrap();
    assert_eq!(shown["state"], json!({"count": 2}), "every person's store opens again at start");
    assert_eq!(status(ureq::get(&format!("{bob_api}/p:plan")).call()), 200);
}

#[test]
fn listening_beyond_loopback_needs_hosted() {
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(BIN).args(["serve", "--listen", "0.0.0.0", "--port", "0", "--home"])
        .arg(temp.path().join("home")).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("add --hosted"), "{stderr}");
    assert!(!temp.path().join("home").exists(), "nothing is written before the refusal");
}
