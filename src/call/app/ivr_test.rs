//! Tests for the IVR application.
//!
//! Uses [`MockCallStack`] to drive an [`IvrApp`] through simulated events
//! without any SIP stack, media, or database.

#[cfg(test)]
mod tests {
    use crate::call::app::ivr::IvrApp;
    use crate::call::app::ivr_config::{
        EntryAction, IvrDefinition, IvrFileConfig, MenuEntry, MenuNode,
    };
    use crate::call::app::testing::MockCallStack;
    use crate::proxy::proxy_call::state::SessionAction;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Build a minimal IVR definition with a root menu for testing.
    fn build_simple_ivr() -> IvrDefinition {
        IvrDefinition {
            name: "test-ivr".to_string(),
            description: Some("Test IVR".to_string()),
            lang: Some("en".to_string()),
            root: MenuNode {
                greeting: "sounds/welcome.wav".to_string(),
                timeout_ms: 200, // short for tests
                max_retries: 2,
                invalid_prompt: Some("sounds/invalid.wav".to_string()),
                timeout_action: Some(EntryAction::Repeat),
                max_retries_action: Some(EntryAction::Hangup { prompt: None }),
                entries: vec![
                    MenuEntry {
                        key: "1".to_string(),
                        label: Some("Sales".to_string()),
                        action: EntryAction::Transfer {
                            target: "2001".to_string(),
                        },
                    },
                    MenuEntry {
                        key: "2".to_string(),
                        label: Some("Support".to_string()),
                        action: EntryAction::Menu {
                            menu: "support".to_string(),
                        },
                    },
                    MenuEntry {
                        key: "3".to_string(),
                        label: Some("Address".to_string()),
                        action: EntryAction::Play {
                            prompt: "sounds/address.wav".to_string(),
                        },
                    },
                    MenuEntry {
                        key: "*".to_string(),
                        label: Some("Repeat".to_string()),
                        action: EntryAction::Repeat,
                    },
                    MenuEntry {
                        key: "0".to_string(),
                        label: Some("Hangup".to_string()),
                        action: EntryAction::Hangup {
                            prompt: Some("sounds/goodbye.wav".to_string()),
                        },
                    },
                ],
            },
            menus: {
                let mut m = HashMap::new();
                m.insert(
                    "support".to_string(),
                    MenuNode {
                        greeting: "sounds/support_menu.wav".to_string(),
                        timeout_ms: 200,
                        max_retries: 1,
                        invalid_prompt: None,
                        timeout_action: Some(EntryAction::Transfer {
                            target: "3000".to_string(),
                        }),
                        max_retries_action: Some(EntryAction::Transfer {
                            target: "3000".to_string(),
                        }),
                        entries: vec![
                            MenuEntry {
                                key: "1".to_string(),
                                label: Some("Billing".to_string()),
                                action: EntryAction::Transfer {
                                    target: "3001".to_string(),
                                },
                            },
                            MenuEntry {
                                key: "9".to_string(),
                                label: Some("Back".to_string()),
                                action: EntryAction::Menu {
                                    menu: "root".to_string(),
                                },
                            },
                        ],
                    },
                );
                m
            },
        }
    }

    // ── 1. Basic lifecycle: enter → answer → play greeting → wait ──

    #[tokio::test]
    async fn test_ivr_basic_enter() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        // IVR should answer on enter
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // IVR should play the root greeting
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav")
            })
            .await;

        // Simulate greeting complete — IVR waits for DTMF
        stack.audio_complete("default");

        // No command should be emitted (it's waiting)
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            stack.drain_cmds().is_empty(),
            "should be idle waiting for DTMF"
        );

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 2. DTMF transfer ──

    #[tokio::test]
    async fn test_ivr_dtmf_transfer() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        // Skip answer
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        // Skip greeting play
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        // Greeting completes
        stack.audio_complete("default");

        // Press "1" → transfer to 2001
        stack.dtmf("1");

        // The event loop should end with a transfer (which becomes Hangup or similar
        // at the session level — in MockCallStack the loop just exits cleanly).
        // AppAction::Transfer causes the loop to exit.
        stack.join().await.expect("loop should exit after transfer");
    }

    // ── 3. Sub-menu navigation ──

    #[tokio::test]
    async fn test_ivr_submenu_navigation() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        // Answer
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        // Root greeting
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "2" → navigate to support sub-menu
        stack.dtmf("2");

        // Should play support greeting
        stack
            .assert_cmd(200, "PlayPrompt-support", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/support_menu.wav")
            })
            .await;

        // Support greeting completes
        stack.audio_complete("default");

        // Press "1" → transfer to 3001
        stack.dtmf("1");
        stack
            .join()
            .await
            .expect("loop should exit after transfer from sub-menu");
    }

    // ── 4. Back to root from sub-menu ──

    #[tokio::test]
    async fn test_ivr_back_to_root() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        // Answer
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        // Root greeting
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "2" → support
        stack.dtmf("2");
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "9" → back to root
        stack.dtmf("9");

        // Should play root greeting again
        stack
            .assert_cmd(200, "PlayPrompt-root", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav")
            })
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 5. Play announcement returns to menu ──

    #[tokio::test]
    async fn test_ivr_play_returns_to_menu() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "3" → play address
        stack.dtmf("3");
        stack
            .assert_cmd(200, "PlayPrompt-address", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/address.wav")
            })
            .await;

        // Address announcement completes → should return to root menu
        stack.audio_complete("default");

        // Should re-play root greeting
        stack
            .assert_cmd(200, "PlayPrompt-root", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav")
            })
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 6. Repeat action replays greeting ──

    #[tokio::test]
    async fn test_ivr_repeat_action() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "*" → repeat
        stack.dtmf("*");

        // Should replay root greeting
        stack
            .assert_cmd(200, "PlayPrompt-repeat", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav")
            })
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 7. Hangup with goodbye prompt ──

    #[tokio::test]
    async fn test_ivr_hangup_with_prompt() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "0" → hangup with goodbye prompt
        stack.dtmf("0");

        // Should play goodbye
        stack
            .assert_cmd(200, "PlayPrompt-goodbye", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/goodbye.wav")
            })
            .await;

        // After goodbye completes → hangup
        stack.audio_complete("default");
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, SessionAction::Hangup { .. }))
            .await;
    }

    // ── 8. Invalid key → invalid prompt → retry ──

    #[tokio::test]
    async fn test_ivr_invalid_key() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "7" → unmapped key
        stack.dtmf("7");

        // Should play invalid prompt
        stack
            .assert_cmd(200, "PlayPrompt-invalid", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/invalid.wav")
            })
            .await;

        // Invalid prompt completes → replay greeting
        stack.audio_complete("default");
        stack
            .assert_cmd(200, "PlayPrompt-greeting", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav")
            })
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 9. Timeout → repeat (default timeout_action) then max retries → hangup ──

    #[tokio::test]
    async fn test_ivr_timeout_and_max_retries() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Timeout 1 (200ms) → should replay greeting (timeout_action = repeat)
        stack
            .assert_cmd(500, "PlayPrompt-retry1", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav")
            })
            .await;

        // Greeting completes → wait for DTMF again
        stack.audio_complete("default");

        // Timeout 2 → should replay greeting again
        stack
            .assert_cmd(500, "PlayPrompt-retry2", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav")
            })
            .await;

        stack.audio_complete("default");

        // Timeout 3 → max_retries exceeded (max_retries=2, count now 3) → hangup
        stack
            .assert_cmd(500, "Hangup-max-retries", |c| {
                matches!(c, SessionAction::Hangup { .. })
            })
            .await;
    }

    // ── 10. TOML parsing round-trip ──

    #[tokio::test]
    async fn test_ivr_toml_parsing() {
        let toml_str = r#"
[ivr]
name = "toml-test"

[ivr.root]
greeting = "hello.wav"
timeout_ms = 100
max_retries = 1
max_retries_action = { type = "hangup" }

[[ivr.root.entries]]
key = "1"
action = { type = "transfer", target = "100" }
"#;

        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse");
        config.ivr.validate().expect("valid");

        let mut stack = MockCallStack::run(Box::new(IvrApp::new(config.ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "1" → transfer
        stack.dtmf("1");
        stack.join().await.expect("transfer exit");
    }

    // ── 11. Remote hangup during IVR ──

    #[tokio::test]
    async fn test_ivr_remote_hangup() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;

        stack.remote_hangup();
        stack
            .join()
            .await
            .expect("should exit cleanly on remote hangup");
    }

    // ── 12. Sub-menu timeout → transfer ──

    #[tokio::test]
    async fn test_ivr_submenu_timeout_transfer() {
        let ivr = build_simple_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Navigate to support sub-menu
        stack.dtmf("2");
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Timeout in support menu → should transfer to 3000 (timeout_action = transfer)
        // The loop should exit with transfer
        stack.join().await.expect("should transfer on timeout");
    }

    // ── 13. CollectExtension collects digits and transfers ──

    fn build_collect_extension_ivr() -> IvrDefinition {
        IvrDefinition {
            name: "test-collect".to_string(),
            description: None,
            lang: None,
            root: MenuNode {
                greeting: "sounds/collect_menu.wav".to_string(),
                timeout_ms: 200,
                max_retries: 1,
                invalid_prompt: None,
                timeout_action: Some(EntryAction::Hangup { prompt: None }),
                max_retries_action: Some(EntryAction::Hangup { prompt: None }),
                entries: vec![
                    MenuEntry {
                        key: "1".to_string(),
                        label: Some("Dial Extension".to_string()),
                        action: EntryAction::CollectExtension {
                            prompt: "sounds/enter_extension.wav".to_string(),
                            min_digits: 2,
                            max_digits: 4,
                            inter_digit_timeout_ms: 60,
                        },
                    },
                    MenuEntry {
                        key: "2".to_string(),
                        label: Some("Voicemail".to_string()),
                        action: EntryAction::Voicemail {
                            target: "1001".to_string(),
                        },
                    },
                    MenuEntry {
                        key: "3".to_string(),
                        label: Some("Queue".to_string()),
                        action: EntryAction::Queue {
                            target: "sales".to_string(),
                        },
                    },
                ],
            },
            menus: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_ivr_collect_extension_transfer() {
        let ivr = build_collect_extension_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack.assert_cmd(200, "PlayPrompt-greeting", |c| {
            matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/collect_menu.wav")
        }).await;
        stack.audio_complete("default");

        stack.dtmf("1");

        stack.assert_cmd(200, "PlayPrompt-collect", |c| {
            matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/enter_extension.wav")
        }).await;

        stack.dtmf("2");
        tokio::time::sleep(Duration::from_millis(10)).await;
        stack.dtmf("0");
        tokio::time::sleep(Duration::from_millis(10)).await;
        stack.dtmf("1");

        stack
            .assert_cmd(
                400,
                "TransferTarget",
                |c| matches!(c, SessionAction::TransferTarget(t) if t == "201"),
            )
            .await;

        stack
            .join()
            .await
            .expect("should exit after extension transfer");
    }

    #[tokio::test]
    async fn test_ivr_collect_extension_terminator() {
        let ivr = build_collect_extension_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");
        stack.assert_cmd(200, "PlayPrompt-collect", |c| {
            matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/enter_extension.wav")
        }).await;

        stack.dtmf("5").dtmf("5").dtmf("#");

        stack
            .assert_cmd(
                400,
                "TransferTarget",
                |c| matches!(c, SessionAction::TransferTarget(t) if t == "55"),
            )
            .await;

        stack
            .join()
            .await
            .expect("should exit after extension transfer via terminator");
    }

    // ── 14. Voicemail action sends TransferTarget voicemail:{ext} ──

    #[tokio::test]
    async fn test_ivr_voicemail_action_sends_transfer_target() {
        let ivr = build_collect_extension_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        stack.dtmf("2");

        stack
            .assert_cmd(
                300,
                "TransferTarget-voicemail",
                |c| matches!(c, SessionAction::TransferTarget(t) if t == "voicemail:1001"),
            )
            .await;

        stack
            .join()
            .await
            .expect("should exit after voicemail transfer");
    }

    // ── 15. Queue action sends TransferTarget queue:{target} ──

    #[tokio::test]
    async fn test_ivr_queue_action_sends_transfer_target() {
        let ivr = build_collect_extension_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        stack.dtmf("3");

        stack
            .assert_cmd(
                300,
                "TransferTarget-queue",
                |c| matches!(c, SessionAction::TransferTarget(t) if t == "queue:sales"),
            )
            .await;

        stack
            .join()
            .await
            .expect("should exit after queue transfer");
    }

    // ── Webhook helpers ──────────────────────────────────────────────────────

    /// Spawn a one-shot axum HTTP server that always returns `body`.
    /// Returns the base URL (e.g. `"http://127.0.0.1:PORT"`).
    async fn spawn_webhook_server(body: serde_json::Value) -> String {
        use axum::{routing::any, Json, Router};

        let port = portpicker::pick_unused_port().expect("no free port");
        let router = Router::new().route(
            "/hook",
            any(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );

        let listener =
            tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
                .await
                .expect("bind");

        tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });

        format!("http://127.0.0.1:{}/hook", port)
    }

    /// Build an IVR definition whose root menu contains a single webhook entry
    /// on key "1", pointing to the given URL.
    fn build_webhook_ivr(url: impl Into<String>, method: Option<&str>) -> IvrDefinition {
        IvrDefinition {
            name: "webhook-ivr".to_string(),
            description: None,
            lang: None,
            root: MenuNode {
                greeting: "sounds/welcome.wav".to_string(),
                timeout_ms: 200,
                max_retries: 1,
                invalid_prompt: None,
                timeout_action: Some(EntryAction::Hangup { prompt: None }),
                max_retries_action: Some(EntryAction::Hangup { prompt: None }),
                entries: vec![MenuEntry {
                    key: "1".to_string(),
                    label: Some("Webhook".to_string()),
                    action: EntryAction::Webhook {
                        url: url.into(),
                        method: method.map(|s| s.to_string()),
                        headers: HashMap::new(),
                    },
                }],
            },
            menus: HashMap::new(),
        }
    }

    // ── 17. Webhook → transfer ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_transfer() {
        let url = spawn_webhook_server(serde_json::json!({
            "action": "transfer",
            "params": { "target": "9001" }
        }))
        .await;

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt-greeting", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "1" → webhook fires → transfer to 9001
        stack.dtmf("1");

        stack
            .assert_cmd(
                1000,
                "TransferTarget-9001",
                |c| matches!(c, SessionAction::TransferTarget(t) if t == "9001"),
            )
            .await;

        stack.join().await.expect("should exit after webhook transfer");
    }

    // ── 18. Webhook → hangup ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_hangup() {
        let url = spawn_webhook_server(serde_json::json!({
            "action": "hangup",
            "params": { "prompt": null }
        }))
        .await;

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");

        stack
            .assert_cmd(1000, "Hangup-webhook", |c| matches!(c, SessionAction::Hangup { .. }))
            .await;
    }

    // ── 19. Webhook → hangup with goodbye prompt ──────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_hangup_with_prompt() {
        let url = spawn_webhook_server(serde_json::json!({
            "action": "hangup",
            "params": { "prompt": "sounds/goodbye.wav" }
        }))
        .await;

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");

        // Webhook → hangup with prompt → play goodbye
        stack
            .assert_cmd(
                1000,
                "PlayPrompt-goodbye",
                |c| matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/goodbye.wav"),
            )
            .await;

        // After goodbye completes → hangup
        stack.audio_complete("default");
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, SessionAction::Hangup { .. }))
            .await;
    }

    // ── 20. Webhook → play announcement ──────────────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_play() {
        let url = spawn_webhook_server(serde_json::json!({
            "action": "play",
            "params": { "prompt": "sounds/info.wav" }
        }))
        .await;

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");

        // Webhook → play announcement
        stack
            .assert_cmd(
                1000,
                "PlayPrompt-info",
                |c| matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/info.wav"),
            )
            .await;

        // Announcement completes → returns to root greeting
        stack.audio_complete("default");
        stack
            .assert_cmd(
                200,
                "PlayPrompt-root-again",
                |c| matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav"),
            )
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 21. Webhook → menu navigation ────────────────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_menu() {
        use crate::call::app::ivr_config::MenuNode;

        let url = spawn_webhook_server(serde_json::json!({
            "action": "menu",
            "params": { "menu": "support" }
        }))
        .await;

        let mut ivr = build_webhook_ivr(&url, None);

        // Add the "support" sub-menu
        ivr.menus.insert(
            "support".to_string(),
            MenuNode {
                greeting: "sounds/support.wav".to_string(),
                timeout_ms: 200,
                max_retries: 1,
                invalid_prompt: None,
                timeout_action: Some(EntryAction::Hangup { prompt: None }),
                max_retries_action: Some(EntryAction::Hangup { prompt: None }),
                entries: vec![MenuEntry {
                    key: "1".to_string(),
                    label: Some("Billing".to_string()),
                    action: EntryAction::Transfer { target: "3001".to_string() },
                }],
            },
        );

        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt-root", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");

        // Webhook → navigate to "support" sub-menu
        stack
            .assert_cmd(
                1000,
                "PlayPrompt-support",
                |c| matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/support.wav"),
            )
            .await;

        // Press "1" in support menu → transfer to billing
        stack.audio_complete("default");
        stack.dtmf("1");
        stack.join().await.expect("should exit after billing transfer");
    }

    // ── 22. Webhook → repeat current menu ────────────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_repeat() {
        let url = spawn_webhook_server(serde_json::json!({ "action": "repeat" })).await;

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");

        // Webhook → repeat → re-play root greeting
        stack
            .assert_cmd(
                1000,
                "PlayPrompt-repeat",
                |c| matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav"),
            )
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 23. Webhook → queue ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_queue() {
        let url = spawn_webhook_server(serde_json::json!({
            "action": "queue",
            "params": { "target": "sales" }
        }))
        .await;

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");

        stack
            .assert_cmd(
                1000,
                "TransferTarget-queue",
                |c| matches!(c, SessionAction::TransferTarget(t) if t == "queue:sales"),
            )
            .await;

        stack.join().await.expect("should exit after queue transfer");
    }

    // ── 24. Webhook → voicemail ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_voicemail() {
        let url = spawn_webhook_server(serde_json::json!({
            "action": "voicemail",
            "params": { "target": "2001" }
        }))
        .await;

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");

        stack
            .assert_cmd(
                1000,
                "TransferTarget-voicemail",
                |c| matches!(c, SessionAction::TransferTarget(t) if t == "voicemail:2001"),
            )
            .await;

        stack.join().await.expect("should exit after voicemail transfer");
    }

    // ── 25. Webhook → collect_extension ──────────────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_collect_extension() {
        let url = spawn_webhook_server(serde_json::json!({
            "action": "collect_extension",
            "params": {
                "prompt": "sounds/enter_ext.wav",
                "min_digits": 2,
                "max_digits": 4,
                "inter_digit_timeout_ms": 100
            }
        }))
        .await;

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");

        // Webhook → collect_extension → play collect prompt
        stack
            .assert_cmd(
                1000,
                "PlayPrompt-collect",
                |c| matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/enter_ext.wav"),
            )
            .await;

        // Dial digits: 4 2 # (terminator)
        tokio::time::sleep(Duration::from_millis(20)).await;
        stack.dtmf("4");
        tokio::time::sleep(Duration::from_millis(20)).await;
        stack.dtmf("2");
        tokio::time::sleep(Duration::from_millis(20)).await;
        stack.dtmf("#");

        stack
            .assert_cmd(
                400,
                "TransferTarget-ext",
                |c| matches!(c, SessionAction::TransferTarget(t) if t == "42"),
            )
            .await;

        stack.join().await.expect("should exit after collect transfer");
    }

    // ── 26. Webhook GET method ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_get_method() {
        // GET webhooks pass context as query params; server still returns a command
        let url = spawn_webhook_server(serde_json::json!({
            "action": "transfer",
            "params": { "target": "7777" }
        }))
        .await;

        let ivr = build_webhook_ivr(&url, Some("GET"));
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");

        stack
            .assert_cmd(
                1000,
                "TransferTarget-7777",
                |c| matches!(c, SessionAction::TransferTarget(t) if t == "7777"),
            )
            .await;

        stack.join().await.expect("should exit after GET webhook transfer");
    }

    // ── 27. Webhook error → fallback to current menu ──────────────────────────

    #[tokio::test]
    async fn test_ivr_webhook_error_fallback() {
        // Point at a port nothing is listening on → connection refused
        let port = portpicker::pick_unused_port().expect("no free port");
        let url = format!("http://127.0.0.1:{}/hook", port);

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| matches!(c, SessionAction::PlayPrompt { .. }))
            .await;
        stack.audio_complete("default");

        // Trigger webhook entry — it will fail (connection refused)
        stack.dtmf("1");

        // On error the IVR should fall back: re-enter current menu (re-play greeting)
        stack
            .assert_cmd(
                2000,
                "PlayPrompt-fallback-greeting",
                |c| matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav"),
            )
            .await;

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 16. Remote hangup during CollectExtension ──

    #[tokio::test]
    async fn test_ivr_remote_hangup_during_collect_extension() {
        let ivr = build_collect_extension_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;
        stack
            .assert_cmd(200, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        stack.dtmf("1");
        stack.assert_cmd(200, "PlayPrompt-collect", |c| {
            matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/enter_extension.wav")
        }).await;

        tokio::time::sleep(Duration::from_millis(10)).await;
        stack.remote_hangup();

        let result = stack.join().await;
        assert!(
            result.is_err(),
            "hangup during collect_extension should propagate as error"
        );
    }

    // ── 28. PlayAndHangup: plays prompt then hangs up with SIP code ───────────

    fn build_play_and_hangup_ivr() -> IvrDefinition {
        IvrDefinition {
            name: "play-and-hangup-ivr".to_string(),
            description: None,
            lang: None,
            root: MenuNode {
                greeting: "sounds/welcome.wav".to_string(),
                timeout_ms: 200,
                max_retries: 1,
                invalid_prompt: None,
                timeout_action: Some(EntryAction::Hangup { prompt: None }),
                max_retries_action: Some(EntryAction::Hangup { prompt: None }),
                entries: vec![
                    MenuEntry {
                        key: "4".to_string(),
                        label: Some("Busy".to_string()),
                        action: EntryAction::PlayAndHangup {
                            prompt: Some("sounds/busy.wav".to_string()),
                            code: Some(486),
                        },
                    },
                    MenuEntry {
                        key: "5".to_string(),
                        label: Some("Service Unavailable".to_string()),
                        action: EntryAction::PlayAndHangup {
                            prompt: None,
                            code: Some(503),
                        },
                    },
                ],
            },
            menus: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_ivr_play_and_hangup_with_code() {
        let ivr = build_play_and_hangup_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt-greeting", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav")
            })
            .await;
        stack.audio_complete("default");

        // Press "4" → PlayAndHangup with prompt and code 486
        stack.dtmf("4");

        // Should play the busy prompt
        stack
            .assert_cmd(200, "PlayPrompt-busy", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/busy.wav")
            })
            .await;

        // After prompt completes → hangup with SIP code 486
        stack.audio_complete("default");
        stack
            .assert_cmd(200, "Hangup-486", |c| {
                matches!(c, SessionAction::Hangup { code: Some(486), .. })
            })
            .await;
    }

    #[tokio::test]
    async fn test_ivr_play_and_hangup_no_prompt() {
        let ivr = build_play_and_hangup_ivr();
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt-greeting", |c| {
                matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/welcome.wav")
            })
            .await;
        stack.audio_complete("default");

        // Press "5" → PlayAndHangup with no prompt, code 503
        stack.dtmf("5");

        // No prompt — should hang up immediately with SIP code 503
        stack
            .assert_cmd(200, "Hangup-503", |c| {
                matches!(c, SessionAction::Hangup { code: Some(503), .. })
            })
            .await;
    }

    #[tokio::test]
    async fn test_ivr_webhook_play_and_hangup() {
        let url = spawn_webhook_server(serde_json::json!({
            "action": "play_and_hangup",
            "params": { "prompt": "sounds/busy.wav", "code": 486 }
        }))
        .await;

        let ivr = build_webhook_ivr(&url, None);
        let mut stack = MockCallStack::run(Box::new(IvrApp::new(ivr)), "caller", "1000");

        stack
            .assert_cmd(200, "AcceptCall", |c| matches!(c, SessionAction::AcceptCall { .. }))
            .await;
        stack
            .assert_cmd(200, "PlayPrompt-greeting", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;
        stack.audio_complete("default");

        // Press "1" → webhook fires → play_and_hangup with prompt and code 486
        stack.dtmf("1");

        // Webhook should cause the busy prompt to be played
        stack
            .assert_cmd(
                1000,
                "PlayPrompt-busy",
                |c| matches!(c, SessionAction::PlayPrompt { audio_file, .. } if audio_file == "sounds/busy.wav"),
            )
            .await;

        // After prompt completes → hangup with code 486
        stack.audio_complete("default");
        stack
            .assert_cmd(200, "Hangup-486", |c| {
                matches!(c, SessionAction::Hangup { code: Some(486), .. })
            })
            .await;
    }
}
