#[path = "../src/disk.rs"]
#[allow(dead_code)]
mod disk;

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

impl Daemon {
    fn start(home: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let child = Command::new(BIN).args(["serve", "--port", &port.to_string(), "--home"])
            .arg(home).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
        let mut daemon = Self { child, url: format!("http://127.0.0.1:{port}") };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if ureq::get(&format!("{}/api/jellies", daemon.url)).call().is_ok() { break; }
            if let Some(status) = daemon.child.try_wait().unwrap() {
                let mut errors = String::new();
                daemon.child.stderr.take().unwrap().read_to_string(&mut errors).unwrap();
                panic!("daemon exited ({status}): {errors}");
            }
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    fn post(&self, path: &str, body: Value) -> Value {
        ureq::post(&format!("{}{path}", self.url)).send_json(body).unwrap().into_json().unwrap()
    }

    fn command(&self, client: &Path) -> Command {
        let mut cmd = Command::new(BIN);
        configure(&mut cmd, &self.url, client);
        cmd
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn configure(cmd: &mut Command, url: &str, client: &Path) {
    cmd.env_remove("JELLY_AS").env_remove("JELLY_SESSION_ID").env_remove("CLAUDE_CODE_SESSION_ID")
        .arg("--url").arg(url).env("JELLY_PROJECT", "p").env("JELLY_HOME", client)
        .env("HOME", client).stdin(Stdio::null());
}

fn stdout(output: Output) -> String {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

const VIEW: &str = "<p>pull test</p>";
const VERBS: &str = "const verbs = {};";

#[test]
fn pull_gets_assets_from_selected_daemon_with_separate_client_homes() {
    let temp = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(&temp.path().join("daemon"));
    let client = temp.path().join("client");
    let local_assets = client.join("assets").join(disk::name("p:sample"));
    std::fs::create_dir_all(&local_assets).unwrap();
    std::fs::write(local_assets.join("note.txt"), b"wrong client bytes").unwrap();
    std::fs::write(local_assets.join("client-only.txt"), b"not on daemon").unwrap();
    daemon.post("/api/jellies", json!({"name": "sample", "project": "p", "view": VIEW, "verbs": VERBS, "state": {"count": 7}, "by": "agent"}));
    let assets: [(&str, &[u8]); 6] = [
        ("note.txt", b"daemon note\n"), ("binary.bin", &[0, 255, 128, 10]),
        ("a..", b"trailing dots"), ("a..txt", b"consecutive dots"),
        ("a...txt", b"longer dot run"), ("UPPER-_.txt", b"ordinary punctuation"),
    ];
    for (file, bytes) in assets {
        let path = temp.path().join(file);
        std::fs::write(&path, bytes).unwrap();
        stdout(daemon.command(&client).args(["asset", "sample"]).arg(path).output().unwrap());
    }
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let closed_dest = temp.path().join("closed-pull");
    let out = daemon.command(&client).args(["pull", "p:sample"]).arg(&closed_dest)
        .stdout(Stdio::from(writer)).output().unwrap();
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    assert_core(&closed_dest);
    for (file, bytes) in assets {
        assert_eq!(std::fs::read(closed_dest.join("assets").join(file)).unwrap(), bytes);
    }
    let empty_client = temp.path().join("empty-client");
    for (home, name, folder) in [(&client, "sample", "pulled"), (&empty_client, "p:sample", "pulled-again")] {
        let dest = temp.path().join(folder);
        let output = stdout(daemon.command(home).args(["pull", name]).arg(&dest).output().unwrap());
        assert!(output.contains("assets/ (6)"), "{output}");
        assert_core(&dest);
        for (file, bytes) in assets {
            assert_eq!(std::fs::read(dest.join("assets").join(file)).unwrap(), bytes);
        }
        assert!(!dest.join("assets/client-only.txt").exists());
        assert_eq!(std::fs::read_dir(dest.join("assets")).unwrap().count(), assets.len());
    }
}

fn assert_core(dest: &Path) {
    assert_eq!(std::fs::read_to_string(dest.join("view.html")).unwrap(), VIEW);
    assert_eq!(std::fs::read_to_string(dest.join("verbs.js")).unwrap(), VERBS);
    let state: Value = serde_json::from_slice(&std::fs::read(dest.join("state.json")).unwrap()).unwrap();
    assert_eq!(state, json!({"count": 7}));
}

#[test]
fn pull_without_assets_writes_core_and_reports_zero() {
    let temp = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(&temp.path().join("daemon"));
    daemon.post("/api/jellies", json!({"name": "sample", "project": "p", "view": VIEW, "verbs": VERBS, "state": {"count": 7}, "by": "agent"}));
    let dest = temp.path().join("pulled");
    let output = stdout(daemon.command(&temp.path().join("client")).args(["pull", "p:sample"]).arg(&dest).output().unwrap());
    assert!(output.contains("assets/ (0)"), "{output}");
    assert_core(&dest);
    assert_eq!(std::fs::read_dir(dest.join("assets")).unwrap().count(), 0);
}

#[test]
fn pull_reports_inventory_and_download_failures() {
    use std::io::{BufRead, BufReader, Write};

    for (inventory_status, inventory, download, diagnostic) in [
        ("200 OK", json!({"assets": []}), false, "asset inventory"),
        ("500 Internal Server Error", json!({"error": "inventory unavailable"}), false, "inventory unavailable"),
        ("200 OK", json!(["note.txt"]), true, "note.txt"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut responses = vec![
                ("/api/jellies/p:sample", "200 OK", json!({"view": VIEW, "verbs": VERBS, "state": {"count": 7}})),
                ("/api/jellies/p:sample/assets", inventory_status, inventory),
            ];
            if download {
                responses.push(("/j/p:sample/assets/note.txt", "404 Not Found", json!({"error": "asset unavailable"})));
            }
            for (path, status, body) in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "no request for {path}");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut input = BufReader::new(&stream);
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                assert_eq!(line.trim(), format!("GET {path} HTTP/1.1"));
                loop {
                    line.clear();
                    assert_ne!(input.read_line(&mut line).unwrap(), 0, "incomplete request");
                    if line == "\r\n" { break; }
                }
                let body = body.to_string();
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let mut cmd = Command::new(BIN);
        configure(&mut cmd, &url, &temp.path().join("client"));
        let output = cmd.args(["pull", "p:sample"]).arg(temp.path().join("pulled")).output().unwrap();
        server.join().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(diagnostic), "{stderr}");
        if download { assert!(stderr.contains("asset unavailable"), "{stderr}"); }
        assert!(!String::from_utf8_lossy(&output.stdout).contains("assets/ ("));
    }
}

#[test]
fn pull_reports_daemon_inventory_enumeration_failure() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("daemon");
    let daemon = Daemon::start(&home);
    daemon.post("/api/jellies", json!({"name": "sample", "project": "p", "view": VIEW, "verbs": VERBS, "by": "agent"}));
    ureq::post(&format!("{}/api/jellies/p:sample/assets/note.txt?by=agent", daemon.url))
        .send_bytes(b"daemon note").unwrap();
    let assets = home.join("assets").join(disk::name("p:sample"));
    std::fs::rename(&assets, home.join("saved-assets")).unwrap();
    std::fs::write(&assets, b"not a directory").unwrap();
    let inventory = ureq::get(&format!("{}/api/jellies/p:sample/assets", daemon.url)).call();
    let output = daemon.command(&temp.path().join("client")).args(["pull", "p:sample"])
        .arg(temp.path().join("pulled")).output();

    let error = inventory.unwrap_err();
    assert!(matches!(error, ureq::Error::Status(500, _)), "{error}");
    let output = output.unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not list assets"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("assets/ ("));
}
