#[cfg(test)]
mod stunar_integration {
    use super::super::rendezvous::{discover_stunar_room, submit_stunar_answer, StunarHost};
    use crate::media::peer_transport::{PeerSignal, PeerSignalKind};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    /// One shared probe for all tests: health + a real open/close cycle.
    /// Skips when the Rendezvous is down OR rate-limited (the server allows
    /// 5 opens/min per IP, and the tests themselves need most of that).
    static PROBE: OnceLock<bool> = OnceLock::new();

    fn rendezvous_up(base: &str) -> bool {
        *PROBE.get_or_init(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let client = reqwest::Client::new();
                let health = client
                    .get(format!("{base}/health"))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false);
                if !health {
                    return false;
                }
                let response = client
                    .post(format!("{base}/v1/host/open"))
                    .json(&serde_json::json!({ "password": "probe", "nickname": "Probe" }))
                    .send()
                    .await
                    .ok();
                let Some(response) = response else {
                    return false;
                };
                if !response.status().is_success() {
                    return false; // down or rate-limited: skip
                }
                let json: serde_json::Value = response.json().await.unwrap_or_default();
                let Some(token) = json["host_token"].as_str() else {
                    return false;
                };
                let _ = client
                    .post(format!("{base}/v1/host/close"))
                    .json(&serde_json::json!({ "host_token": token }))
                    .send()
                    .await;
                true
            })
        })
    }

    fn wait_for<F: FnMut() -> Option<T>, T>(mut f: F, what: &str) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = f() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn stunar_signal_path_with_admission() {
        let base = "http://127.0.0.1:8787";
        if !rendezvous_up(base) {
            eprintln!("SKIP: rendezvous not running on {base}");
            return;
        }
        let host = StunarHost::start(
            base,
            "senha",
            "Ana",
            true,
            super::super::room_mode::SessionMode::Broadcast,
        )
        .expect("open");
        let code = host.code();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|ch| ch.is_ascii_alphanumeric()));

        let viewer_base = base.to_owned();
        let viewer_code = code.clone();
        let viewer = std::thread::spawn(move || {
            discover_stunar_room(&viewer_base, &viewer_code, "senha", "Joao")
        });

        // Host sees the pending viewer.
        let (id, nickname) = wait_for(
            || host.pending_roster().into_iter().next(),
            "pending viewer",
        );
        assert_eq!(nickname, "Joao");
        host.decide(&id, true).expect("decide accept");

        // The engine would mint here; send a stand-in offer over the WS.
        host.send_signal(
            &id,
            &PeerSignal {
                kind: PeerSignalKind::Offer,
                sdp: "v=0\r\n".into(),
                id: Some(id.clone()),
            },
        )
        .expect("send offer");

        let (token, offer, viewer_ws) =
            viewer.join().expect("viewer thread").expect("viewer offer");
        assert!(!token.is_empty());
        assert_eq!(offer.sdp, "v=0\r\n");

        // Viewer answers over the WS; the host inbox receives it.
        submit_stunar_answer(
            &viewer_ws,
            &PeerSignal {
                kind: PeerSignalKind::Answer,
                sdp: "v=0\r\n".into(),
                id: None,
            },
        )
        .expect("submit answer");
        let answer = wait_for(|| host.take_answers().into_iter().next(), "host answer");
        assert_eq!(answer.id.as_deref(), Some(id.as_str()));
        assert_eq!(answer.sdp, "v=0\r\n");

        // Close removes the room: a fresh ask is denied.
        host.close();
        std::thread::sleep(Duration::from_millis(300));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let denied = runtime.block_on(async {
            let response = reqwest::Client::new()
                .post(format!("{base}/v1/viewer/ask"))
                .json(
                    &serde_json::json!({ "code": code, "password": "senha", "nickname": "Outro" }),
                )
                .send()
                .await
                .unwrap();
            !response.status().is_success()
        });
        assert!(denied, "room should be gone after close");
    }

    #[test]
    fn stunar_accepted_roster_without_admission() {
        let base = "http://127.0.0.1:8787";
        if !rendezvous_up(base) {
            eprintln!("SKIP: rendezvous not running on {base}");
            return;
        }
        let host = StunarHost::start(
            base,
            "senha",
            "Ana",
            false,
            super::super::room_mode::SessionMode::Broadcast,
        )
        .expect("open");
        let code = host.code();

        // Viewer asks; admission off => accepted immediately, no pending step.
        let viewer_base = base.to_owned();
        let viewer_code = code.clone();
        let viewer = std::thread::spawn(move || {
            discover_stunar_room(&viewer_base, &viewer_code, "senha", "Joao")
        });

        // The host learns the accepted viewer from the roster message.
        let (id, nickname) = wait_for(
            || host.accepted_roster().into_iter().next(),
            "accepted viewer",
        );
        assert_eq!(nickname, "Joao");
        host.send_signal(
            &id,
            &PeerSignal {
                kind: PeerSignalKind::Offer,
                sdp: "v=0\r\n".into(),
                id: Some(id.clone()),
            },
        )
        .expect("send offer");

        let (_token, offer, _viewer_ws) =
            viewer.join().expect("viewer thread").expect("viewer offer");
        assert_eq!(offer.sdp, "v=0\r\n");
        host.close();
    }

    #[test]
    fn stunar_rotate_password_live() {
        let base = "http://127.0.0.1:8787";
        if !rendezvous_up(base) {
            eprintln!("SKIP: rendezvous not running on {base}");
            return;
        }
        let host = StunarHost::start(
            base,
            "senha",
            "Ana",
            false,
            super::super::room_mode::SessionMode::Broadcast,
        )
        .expect("open");
        let code = host.code();
        assert_eq!(code.len(), 6);

        // Rotate the password; the same room (host_token) keeps serving and
        // the server-owned code never changes.
        host.rotate(Some("nova")).expect("rotate");
        assert_eq!(host.code(), code, "code is server-owned and never rotates");

        // Old password rejected; new password works.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ask = |password: &str| {
            runtime.block_on(async {
                let response = reqwest::Client::new()
                    .post(format!("{base}/v1/viewer/ask"))
                    .json(&serde_json::json!({
                        "code": code,
                        "nickname": "Joao",
                        "password": password,
                    }))
                    .send()
                    .await
                    .unwrap();
                let json: serde_json::Value = response.json().await.unwrap();
                json["ok"] == true
            })
        };
        assert!(!ask("senha"), "old password must be rejected");
        assert!(ask("nova"), "new password must work");
        host.close();
    }
}
