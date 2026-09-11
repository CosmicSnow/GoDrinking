//! Smoke test: shell commands against the real local `server/`, no UI.
//!
//! Proves create_room → start_share(synthetic) → snapshot(Live) →
//! stop_share → snapshot(Stopped) → leave through the same `AppState`
//! methods the Tauri commands call.

use golive_app::AppState;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct ServerGuard {
    child: Child,
    base: String,
}

impl ServerGuard {
    fn spawn() -> Result<Self, String> {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let server = manifest.join("../server/server.mjs");
        if !server.exists() {
            return Err(format!("server not found: {}", server.display()));
        }
        let mut child = Command::new("node")
            .arg(&server)
            .env("PORT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn node: {e}"))?;
        macro_rules! bail {
            ($msg:expr) => {{
                let _ = child.kill();
                let _ = child.wait();
                return Err($msg.into());
            }};
        }
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => bail!("server stdout was not piped"),
        };
        // Drain stdout (discard): an unread pipe would wedge the server once
        // its kernel buffer fills. Port scrape reads lines directly below is
        // wrong — instead a background thread drains everything and we watch
        // a copy. Simplest correct: drain thread + scrape via temp file.
        let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
        let _ = std::thread::Builder::new()
            .name("smoke-server-log".into())
            .spawn(move || {                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                let mut sent = false;
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if !sent {
                                if let Some(p) = parse_port(&line) {
                                    let _ = port_tx.send(p);
                                    sent = true;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| format!("spawn log thread: {e}"));
        let port = match port_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(port) => port,
            Err(_) => bail!("server never printed a listen line"),
        };
        let base = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if Instant::now() > deadline {
                bail!("server health never turned green");
            }
            if let Ok(resp) = ureq::Agent::new_with_defaults()
                .get(&format!("{base}/health"))
                .call()
            {
                if resp.status().as_u16() == 200 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(Self { child, base })
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_port(line: &str) -> Option<u16> {
    let marker = "listen 127.0.0.1:";
    let start = line.find(marker)? + marker.len();
    line[start..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_create_share_snapshot() {
    let server = ServerGuard::spawn().expect("server");
    let state = Arc::new(AppState::new());

    // Point the shell at the ephemeral server.
    let base = state.set_server(&server.base).expect("set_server");
    assert_eq!(base, server.base);

    // Bad inputs are rejected without side effects.
    assert!(state.set_server("not-a-url").is_err());
    assert!(state.start_share(None, "bogus", None).await.is_err());
    assert!(state.start_share(None, "synthetic", None).await.is_err()); // not in a room

    // create → share → snapshot(Live with a share id).
    // With a plan installed, the room code is also published to code_file.
    let dir = std::env::temp_dir().join("golive-smoke-e2e");
    let _ = std::fs::create_dir_all(&dir);
    state
        .set_e2e_plan(golive_app::E2ePlan {
            role: "host".into(),
            server: server.base.clone(),
            password: "smoke-password-123".into(),
            nickname: "smoke".into(),
            code_file: dir.join("code").to_string_lossy().into_owned(),
            status_file: dir.join("status.json").to_string_lossy().into_owned(),
            share: None,
        })
        .expect("set_e2e_plan");
    let code = state
        .create_room(None, "smoke", "smoke-password-123")
        .await
        .expect("create_room");
    assert_eq!(code.len(), 6);
    assert_eq!(
        std::fs::read_to_string(dir.join("code")).expect("code file"),
        code
    );
    // Plan-gated helpers work with the plan, refuse nothing here.
    assert_eq!(state.e2e_read_code().expect("read code"), code);
    let _ = std::fs::remove_dir_all(&dir);
    state
        .start_share(None, "synthetic", None)
        .await
        .expect("start_share");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let snap = state.get_snapshot().expect("snapshot");
    assert_eq!(
        format!("{:?}", snap.share.state),
        "Live",
        "share is live"
    );
    assert!(snap.share.id.is_some(), "share has an id");

    // stop → snapshot(Stopped, id cleared) → leave. Double-stop is safe.
    state.stop_share().await.expect("stop_share");
    state.stop_share().await.expect("stop_share idempotent");
    let snap = state.get_snapshot().expect("snapshot");
    assert_eq!(format!("{:?}", snap.share.state), "Stopped");
    assert!(snap.share.id.is_none());
    state.leave().await.expect("leave");
    state.leave().await.expect("leave idempotent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_join_does_not_wedge_session() {
    let server = ServerGuard::spawn().expect("server");
    let host = Arc::new(AppState::new());
    host.set_server(&server.base).expect("set_server");
    let code = host
        .create_room(None, "host", "good-password-1")
        .await
        .expect("create_room");

    let guest = Arc::new(AppState::new());
    guest.set_server(&server.base).expect("set_server");
    assert!(
        guest
            .join_room(None, &code, "guest", "wrong-password")
            .await
            .is_err(),
        "wrong password must fail"
    );
    guest
        .join_room(None, &code, "guest", "good-password-1")
        .await
        .expect("retry after failed join must not be SessionBusy");
    guest.leave().await.expect("leave");
}
