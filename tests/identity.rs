use axum::{extract::Request, http::StatusCode, response::IntoResponse, routing::any, Json, Router};
use serde_json::json;
use std::process::Command;
use std::sync::mpsc;

struct LookupServer {
    url: String,
    requests: mpsc::Receiver<String>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LookupServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (sent, requests) = mpsc::channel();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let router = Router::new().fallback(any(move |req: Request| {
                    let sent = sent.clone();
                    async move {
                        sent.send(format!("{} {}", req.method(), req.uri())).unwrap();
                        if req.uri().path() == "/api/jellies" {
                            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "identity lookup unavailable"}))).into_response()
                        } else {
                            Json(json!({})).into_response()
                        }
                    }
                }));
                axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), router)
                    .with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
            });
        });
        Self { url, requests, shutdown: Some(shutdown), thread: Some(thread) }
    }
}

impl Drop for LookupServer {
    fn drop(&mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn short_name_commands_stop_when_identity_lookup_fails() {
    let server = LookupServer::start();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("spec.json"), "{}").unwrap();
    std::fs::write(dir.path().join("asset.txt"), "asset").unwrap();
    // A broken resolver must not launch the real browser during this regression.
    let open = dir.path().join("open");
    std::fs::write(&open, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let commands = [
        vec!["rm", "plan"], vec!["call", "plan", "tick"], vec!["set", "plan", "{}"],
        vec!["state", "plan"], vec!["reshape", "plan", "--about", "updated"],
        vec!["undo", "plan"], vec!["pin", "plan"], vec!["unpin", "plan"],
        vec!["show", "plan"], vec!["events", "plan"], vec!["ask", "plan", "Which?"],
        vec!["look", "plan", "Check"], vec!["withdraw", "plan", "a1"], vec!["focus", "plan"],
        vec!["working", "plan", "updating"], vec!["done", "plan"],
        vec!["ask", "plan", "--spec", "spec.json"], vec!["wait", "plan", "--timeout", "0"],
        vec!["wait", "plan", "--since", "seq:0", "--timeout", "0"], vec!["comments", "plan"],
        vec!["reply", "plan", "c1", "answer"], vec!["move", "plan", "other"],
        vec!["see", "plan"], vec!["note", "plan", "note"], vec!["pull", "plan"],
        vec!["dev", "plan", "missing"], vec!["archive", "plan"], vec!["restore", "plan"],
        vec!["restore", "plan", "2"], vec!["history", "plan"], vec!["keep", "plan", "2", "label"],
        vec!["see", "plan", "--version", "2"], vec!["show", "plan", "--version", "v2"],
        vec!["asset", "plan", "asset.txt"], vec!["share", "plan"], vec!["resolve", "plan", "c1"],
        vec!["open", "plan"],
    ];
    let mut failures = Vec::new();
    for project in ["test", ""] {
        for args in &commands {
            let output = Command::new(env!("CARGO_BIN_EXE_jelly"))
                .current_dir(dir.path()).env("PATH", dir.path()).env_remove("JELLY_AS")
                .args(["--url", &server.url, "--project", project, "--as", "agent"])
                .args(args).output().unwrap();
            let requests: Vec<_> = server.requests.try_iter().collect();
            if output.status.code() != Some(1) || !output.stdout.is_empty()
                || String::from_utf8_lossy(&output.stderr).trim() != "jelly: identity lookup unavailable"
                || requests != ["GET /api/jellies"] {
                failures.push(format!("project={project:?}, {args:?}: {output:?}, requests={requests:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn explicit_ids_bypass_candidate_lookup() {
    let server = LookupServer::start();
    for name in ["test:plan", "surface"] {
        let output = Command::new(env!("CARGO_BIN_EXE_jelly"))
            .args(["--url", &server.url, "--project", "test", "show", name]).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(server.requests.try_iter().collect::<Vec<_>>(), [format!("GET /api/jellies/{name}")]);
    }
}
