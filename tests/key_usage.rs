#[path = "support/gateway.rs"]
mod support;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use kanata::config::{Plane, ValidatedConfig};
use kanata::keys::usage::{FlushOutcome, KeyUsage, read_merged};
use kanata::server::TwoPlaneServer;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tracing_subscriber::fmt::MakeWriter;

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

const EXAMPLE_INLINE_KEY: &str = "[[application_keys]]
id = \"personal-client\"
secret_ref = \"env:KANATA_CLIENT_KEY\"
owner = true
permissions = [
  { model_alias = \"local-chat\", operation = \"chat\" },
  { model_alias = \"private-chat\", operation = \"chat\" },
  { model_alias = \"private-transcribe\", operation = \"transcription\" },
  { model_alias = \"remote-chat\", operation = \"chat\" },
  { model_alias = \"codex-chat\", operation = \"chat\" },
]
";

/// Temp dir with `config.toml` (`[keys] file = "keys.toml"`, `usage_dir = "state"`),
/// keys `alpha`, `beta` and an expired key.
struct Scratch(PathBuf);

impl Scratch {
    fn new(public: bool) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kanata-key-usage-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("state")).expect("scratch dir");
        let example =
            fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
        let mut config = example.replace(
            EXAMPLE_INLINE_KEY,
            "[keys]\nfile = \"keys.toml\"\nusage_dir = \"state\"\n",
        );
        assert_ne!(config, example);
        if public {
            config = config
                .replace(
                    "[listeners.admin]",
                    "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
                )
                .replace(
                    "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]",
                    "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]\npublic_routes = [{ model_alias = \"local-chat\", operation = \"chat\" }]",
                );
            assert!(config.contains("public_routes"));
        }
        fs::write(dir.join("config.toml"), config).expect("config writes");
        let keys = format!(
            "version = 1\n\n{}\n{}\n{}",
            record("alpha", ""),
            record("beta", ""),
            record("expired", "expires_at = \"2026-01-02T00:00:00Z\"")
        );
        let keys_path = dir.join("keys.toml");
        fs::write(&keys_path, keys).expect("keys write");
        set_mode(&keys_path, 0o600);
        Self(dir)
    }

    fn state(&self) -> PathBuf {
        self.0.join("state")
    }

    fn load(&self) -> ValidatedConfig {
        kanata::config::load(self.0.join("config.toml")).expect("config loads")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        set_mode(&self.state(), 0o700);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

fn token(id: &str) -> String {
    format!("kanata_sk_SYNTHETIC_{id}")
}

fn record(id: &str, extra: &str) -> String {
    let digest: [u8; 32] = Sha256::digest(token(id).as_bytes()).into();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "[[keys]]\nid = \"{id}\"\ndigest = \"sha256:{hex}\"\ncreated_at = \"2026-01-01T00:00:00Z\"\npermissions = [{{ model_alias = \"local-chat\", operation = \"chat\" }}]\n{extra}\n"
    )
}

fn models(authorization: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri("/v1/models");
    if let Some(authorization) = authorization {
        builder = builder.header(header::AUTHORIZATION, authorization);
    }
    builder.body(Body::empty()).expect("request")
}

async fn send(server: &TwoPlaneServer, id: &str) -> StatusCode {
    let bearer = format!("Bearer {}", token(id));
    server
        .client_oneshot(models(Some(&bearer)))
        .await
        .expect("response")
        .status()
}

fn usage_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("usage file")).expect("usage json")
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture").clone()).expect("utf8")
    }

    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(self.clone())
            .finish()
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn counts_only_authenticated_requests_and_writes_ids_only() {
    let scratch = Scratch::new(false);
    let server = support::server_without_adapters(&scratch.load());
    let usage = server.open_usage(Plane::All).expect("usage persisted");
    for _ in 0..3 {
        assert_eq!(send(&server, "alpha").await, StatusCode::OK);
    }
    assert_eq!(send(&server, "unknown").await, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&server, "expired").await, StatusCode::UNAUTHORIZED);
    let anonymous = server.client_oneshot(models(None)).await.expect("response");
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    assert_eq!(usage.flush_now(), FlushOutcome::Written);
    assert_eq!(usage.flush_now(), FlushOutcome::Clean);
    let path = scratch.state().join("usage-all.json");
    let json = usage_file(&path);
    assert_eq!(json["version"], 2);
    assert_eq!(json["plane"], "all");
    let keys = json["keys"].as_object().expect("keys");
    assert_eq!(keys.keys().collect::<Vec<_>>(), ["alpha"]);
    assert_eq!(keys["alpha"]["requests"], 3);
    let last_used = keys["alpha"]["last_used_at"].as_str().expect("timestamp");
    assert!(
        kanata::keys::time::parse(last_used).is_some(),
        "{last_used}"
    );

    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o600
    );
    let text = fs::read_to_string(&path).expect("usage text");
    assert!(!text.contains(&token("alpha")));
    assert!(!text.contains("sha256"));
}

#[tokio::test]
async fn public_listener_requests_count_once() {
    let scratch = Scratch::new(true);
    let config = scratch
        .load()
        .for_plane(Plane::Public)
        .expect("public plane");
    let server = support::server_without_adapters(&config);
    let usage = server.open_usage(Plane::Public).expect("usage persisted");
    let bearer = format!("Bearer {}", token("alpha"));
    let response = server
        .public_oneshot(models(Some(&bearer)))
        .await
        .expect("public listener");
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(usage.flush_now(), FlushOutcome::Written);
    let json = usage_file(&scratch.state().join("usage-public.json"));
    assert_eq!(json["plane"], "public");
    assert_eq!(json["keys"]["alpha"]["requests"], 1);
}

#[tokio::test]
async fn restart_continues_counts_and_prunes_unknown_ids() {
    let scratch = Scratch::new(false);
    let config = scratch.load();
    let first = support::server_without_adapters(&config);
    let usage = first.open_usage(Plane::All).expect("usage persisted");
    assert_eq!(send(&first, "alpha").await, StatusCode::OK);
    assert_eq!(send(&first, "alpha").await, StatusCode::OK);
    assert_eq!(usage.flush_now(), FlushOutcome::Written);

    let path = scratch.state().join("usage-all.json");
    let mut json = usage_file(&path);
    json["keys"]["ghost"] = json["keys"]["alpha"].clone();
    fs::write(&path, json.to_string()).expect("usage write");

    let second = support::server_without_adapters(&config);
    let usage = second.open_usage(Plane::All).expect("usage persisted");
    assert_eq!(send(&second, "alpha").await, StatusCode::OK);
    assert_eq!(usage.flush_now(), FlushOutcome::Written);
    let json = usage_file(&path);
    assert_eq!(json["keys"]["alpha"]["requests"], 3);
    assert!(json["keys"].get("ghost").is_none(), "{json}");
}

#[tokio::test]
async fn corrupt_state_starts_fresh_and_write_failures_warn_once() {
    // Tracing caches callsite interest process-wide; assert only on WARNs
    // that no other test in this binary reaches.
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(capture.subscriber());
    let scratch = Scratch::new(false);
    let path = scratch.state().join("usage-all.json");
    fs::write(&path, "{not json").expect("corrupt write");
    let server = support::server_without_adapters(&scratch.load());
    let usage = server.open_usage(Plane::All).expect("usage persisted");
    assert!(usage.snapshot().is_empty());
    assert_eq!(
        capture.text().matches("usage state file invalid").count(),
        1
    );

    set_mode(&scratch.state(), 0o500);
    for _ in 0..2 {
        assert_eq!(send(&server, "alpha").await, StatusCode::OK);
        assert_eq!(usage.flush_now(), FlushOutcome::Failed);
    }
    let log = capture.text();
    assert_eq!(log.matches("usage state write failed").count(), 1, "{log}");

    set_mode(&scratch.state(), 0o700);
    assert_eq!(usage.flush_now(), FlushOutcome::Written);
    assert_eq!(usage_file(&path)["keys"]["alpha"]["requests"], 2);
    assert!(!capture.text().contains(&token("alpha")));
}

#[test]
fn inline_keys_have_no_usage_state() {
    let server = support::server_without_adapters(&support::config());
    assert!(server.open_usage(Plane::All).is_none());
    assert!(server.usage_handle().is_none());
}

#[test]
fn merged_reader_sums_planes_across_subdirectories() {
    let scratch = Scratch::new(false);
    let state = scratch.state();
    let write = |relative: &str, contents: &str| {
        let path = state.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, contents).expect("write");
    };
    let usage = |plane: &str, requests: u64, at: &str| {
        format!(
            r#"{{"version":1,"plane":"{plane}","keys":{{"alpha":{{"requests":{requests},"last_used_at":"{at}"}}}}}}"#
        )
    };
    write(
        "private/usage-private.json",
        &usage("private", 4, "2026-03-01T00:00:00Z"),
    );
    write(
        "public/usage-public.json",
        &usage("public", 2, "2026-04-01T00:00:00Z"),
    );
    write("usage-all.json", &usage("all", 1, "2026-02-01T00:00:00Z"));
    write("public/usage-broken.json", "{not json");
    write(
        "private/notes.json",
        &usage("all", 100, "2026-05-01T00:00:00Z"),
    );
    write(
        "private/nested/usage-all.json",
        &usage("all", 100, "2026-05-01T00:00:00Z"),
    );

    let merged = read_merged(&state);
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged["alpha"],
        KeyUsage {
            tokens: Default::default(),
            requests: 7,
            last_used_at: kanata::keys::time::parse("2026-04-01T00:00:00Z").expect("time"),
        }
    );
    assert!(read_merged(&state.join("missing")).is_empty());
}

#[tokio::test]
async fn tokens_are_recorded_for_complete_and_streamed_replies_with_missing_usage_explicit() {
    use kanata::{
        adapter::{Adapter, AdapterFuture, AdapterOutput},
        core::*,
    };
    struct Metered {
        caps: Capabilities,
    }
    impl Adapter for Metered {
        fn id(&self) -> &str {
            "ollama-local"
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        fn execute(&self, request: RoutedRequest) -> AdapterFuture {
            let Request::Chat(chat) = request.request() else {
                unreachable!()
            };
            let model = chat.model.clone();
            let stream = chat.stream;
            let missing = chat
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|part| matches!(part, ChatContent::Text { text } if text == "missing"));
            let usage = (!missing).then_some(Usage {
                input_tokens: 10,
                output_tokens: 3,
                total_tokens: 13,
                reasoning_tokens: Some(2),
            });
            Box::pin(async move {
                if stream {
                    Ok(AdapterOutput::Events(Box::pin(futures_util::stream::iter(
                        [
                            Ok(NormalizedEvent::ChatStarted { model }),
                            Ok(NormalizedEvent::ChatTextDelta {
                                text: "hello".into(),
                            }),
                            Ok(NormalizedEvent::ChatCompleted {
                                finish_reason: FinishReason::Stop,
                                usage,
                            }),
                        ],
                    ))))
                } else {
                    Ok(AdapterOutput::Complete(Response::Chat(ChatResponse {
                        model,
                        message: support::assistant_text("hello"),
                        finish_reason: FinishReason::Stop,
                        usage,
                        reasoning: None,
                    })))
                }
            })
        }
    }
    let scratch = Scratch::new(false);
    let config = scratch.load();
    let server = kanata::server::TwoPlaneServer::from_validated_with_adapters(
        &config,
        &support::Resolver,
        kanata::server::Readiness::new(true),
        vec![Arc::new(Metered {
            caps: support::capabilities(&config, "ollama-local"),
        })],
    )
    .expect("server");
    let usage = server.open_usage(Plane::All).expect("usage");
    for (stream, text) in [(false, "reported"), (true, "reported"), (false, "missing")] {
        let body = serde_json::json!({"model":"local-chat","messages":[{"role":"user","content":text}],"stream":stream}).to_string();
        let request = support::chat_request_with(
            &body,
            Some("application/json"),
            Some(&format!("Bearer {}", token("alpha"))),
            &[],
        );
        let response = server.client_oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        support::response_body(response).await;
    }
    assert_eq!(usage.flush_now(), FlushOutcome::Written);
    let totals = read_merged(&scratch.state())["alpha"].tokens;
    assert_eq!((totals.reported, totals.missing), (2, 1));
    assert_eq!(
        (
            totals.input_tokens,
            totals.output_tokens,
            totals.reasoning_tokens
        ),
        (20, 6, 4)
    );
    assert_eq!(totals.reasoning_reported, 2);
}

fn set_quota(scratch: &Scratch, quota: kanata::keys::quota::DailyQuota) {
    let path = scratch.0.join("keys.toml");
    let bytes = fs::read(&path).unwrap();
    let mut keys = kanata::keys::file::parse_without_routes(&bytes).unwrap();
    keys.records_mut()[0].set_daily_quota(Some(quota));
    fs::write(path, keys.render()).unwrap();
}

fn inference(id: &str) -> Request<Body> {
    support::chat_request_with(
        r#"{"model":"local-chat","messages":[{"role":"user","content":"CONTENT_MUST_NOT_BE_STORED"}]}"#,
        Some("application/json"),
        Some(&format!("Bearer {}", token(id))),
        &[],
    )
}

#[tokio::test]
async fn daily_request_quotas_block_before_dispatch_and_survive_restart() {
    use kanata::keys::quota::{DailyLedger, DailyQuota};
    let scratch = Scratch::new(false);
    set_quota(
        &scratch,
        DailyQuota {
            requests: Some(2),
            tokens: None,
            reservation_tokens: None,
        },
    );
    let config = scratch.load();
    let build = || {
        support::server_with(
            &config,
            vec![support::adapter_spec(
                "ollama-local",
                support::capabilities(&config, "ollama-local"),
                support::chat_outcome("reply"),
            )],
        )
    };
    let (server, requests) = build();
    server.open_usage(Plane::All).unwrap();
    assert_eq!(send(&server, "alpha").await, StatusCode::OK);
    let responses =
        futures_util::future::join_all((0..8).map(|_| server.client_oneshot(inference("alpha"))))
            .await;
    let mut successes = 0;
    for response in responses {
        let response = response.unwrap();
        if response.status() == StatusCode::OK {
            successes += 1;
            support::response_body(response).await;
        } else {
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(response.headers().contains_key("retry-after"));
            assert_eq!(
                support::response_json(response).await["error"]["code"],
                "daily_quota_exceeded"
            );
        }
    }
    assert_eq!(successes, 2);
    assert_eq!(support::recorded_len(&requests), 2);
    let (restarted, requests) = build();
    restarted.open_usage(Plane::All).unwrap();
    assert_eq!(
        restarted
            .client_oneshot(inference("alpha"))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(support::recorded_len(&requests), 0);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if DailyLedger::new(&scratch.state(), Plane::All)
                .read()
                .unwrap()[0]
                .tokens
                .reported
                == 2
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let stored = fs::read_to_string(scratch.state().join("daily-all.json")).unwrap();
    for private in [
        "CONTENT_MUST_NOT_BE_STORED",
        "reply",
        "sha256:",
        &token("alpha"),
    ] {
        assert!(!stored.contains(private));
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args([
            "key",
            "usage",
            "--config",
            scratch.0.join("config.toml").to_str().unwrap(),
            "--key-id",
            "alpha",
            "--input-cost-per-million",
            "2",
            "--output-cost-per-million",
            "4",
            "--cost-unit",
            "units",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["rows"].as_array().unwrap().len(), 1);
    assert_eq!(report["rows"][0]["requests"], 2);
    assert_eq!(report["rows"][0]["tokens"]["input_tokens"], 22);
    assert_eq!(report["rows"][0]["estimated_reported_cost"], 0.0001);
    assert_eq!(report["rows"][0]["cost_incomplete"], false);
}

#[tokio::test]
async fn cancellation_keeps_token_reservation_and_storage_failure_rejects_dispatch() {
    use kanata::{
        adapter::{Adapter, AdapterFuture},
        core::*,
        keys::quota::{DailyLedger, DailyQuota},
    };
    struct Pending {
        caps: Capabilities,
        calls: Arc<AtomicUsize>,
    }
    impl Adapter for Pending {
        fn id(&self) -> &str {
            "ollama-local"
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        fn execute(&self, _: RoutedRequest) -> AdapterFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    }
    let scratch = Scratch::new(false);
    set_quota(
        &scratch,
        DailyQuota {
            requests: None,
            tokens: Some(100),
            reservation_tokens: Some(100),
        },
    );
    let config = scratch.load();
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Arc::new(
        TwoPlaneServer::from_validated_with_adapters(
            &config,
            &support::Resolver,
            kanata::server::Readiness::new(true),
            vec![Arc::new(Pending {
                caps: support::capabilities(&config, "ollama-local"),
                calls: calls.clone(),
            })],
        )
        .unwrap(),
    );
    server.open_usage(Plane::All).unwrap();
    let running = {
        let server = server.clone();
        tokio::spawn(async move { server.client_oneshot(inference("alpha")).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    running.abort();
    let _ = running.await;
    assert_eq!(
        server
            .client_oneshot(inference("alpha"))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let row = DailyLedger::new(&scratch.state(), Plane::All)
        .read()
        .unwrap()
        .remove(0);
    assert_eq!(
        (row.requests, row.charged_tokens, row.tokens.missing),
        (1, 100, 1)
    );
    let (restarted, requests) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "ollama-local",
            support::capabilities(&config, "ollama-local"),
            support::chat_outcome("reply"),
        )],
    );
    restarted.open_usage(Plane::All).unwrap();
    assert_eq!(
        restarted
            .client_oneshot(inference("alpha"))
            .await
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(support::recorded_len(&requests), 0);
    fs::write(scratch.state().join("daily-all.json"), "broken").unwrap();
    let response = server.client_oneshot(inference("alpha")).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        support::response_json(response).await["error"]["code"],
        "quota_unavailable"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn final_usage_reconciles_token_reservations_while_missing_usage_blocks_after_restart() {
    use kanata::keys::quota::{DailyLedger, DailyQuota};
    for reported in [false, true] {
        let scratch = Scratch::new(false);
        set_quota(
            &scratch,
            DailyQuota {
                requests: None,
                tokens: Some(100),
                reservation_tokens: Some(80),
            },
        );
        let config = scratch.load();
        let outcome = if reported {
            support::chat_outcome("reply")
        } else {
            support::Outcome::Chat {
                model: None,
                message: support::assistant_text("reply"),
                finish_reason: kanata::core::FinishReason::Stop,
                usage: None,
            }
        };
        let (server, _) = support::server_with(
            &config,
            vec![support::adapter_spec(
                "ollama-local",
                support::capabilities(&config, "ollama-local"),
                outcome,
            )],
        );
        server.open_usage(Plane::All).unwrap();
        let response = server.client_oneshot(inference("alpha")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        support::response_body(response).await;
        if reported {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while DailyLedger::new(&scratch.state(), Plane::All)
                    .read()
                    .unwrap()[0]
                    .charged_tokens
                    != 18
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        let row = DailyLedger::new(&scratch.state(), Plane::All)
            .read()
            .unwrap()
            .remove(0);
        assert_eq!(
            row.retained_reservation_tokens,
            if reported { 0 } else { 80 }
        );
        assert_eq!(row.tokens.missing, u64::from(!reported));
        let (restarted, requests) = support::server_with(
            &config,
            vec![support::adapter_spec(
                "ollama-local",
                support::capabilities(&config, "ollama-local"),
                support::chat_outcome("reply"),
            )],
        );
        restarted.open_usage(Plane::All).unwrap();
        let response = restarted.client_oneshot(inference("alpha")).await.unwrap();
        assert_eq!(
            response.status(),
            if reported {
                StatusCode::OK
            } else {
                StatusCode::TOO_MANY_REQUESTS
            }
        );
        support::response_body(response).await;
        assert_eq!(support::recorded_len(&requests), usize::from(reported));
    }
}

#[test]
fn quota_requires_durable_usage_directory_at_startup_and_reload() {
    use kanata::keys::quota::DailyQuota;
    let scratch = Scratch::new(false);
    let path = scratch.0.join("config.toml");
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("usage_dir = \"state\"\n", "");
    fs::write(&path, text).unwrap();
    let config = scratch.load();
    set_quota(
        &scratch,
        DailyQuota {
            requests: Some(1),
            tokens: None,
            reservation_tokens: None,
        },
    );
    assert!(
        kanata::config::load(&path)
            .unwrap_err()
            .to_string()
            .contains("required_for_daily_quota")
    );
    let keys =
        kanata::keys::file::parse_without_routes(&fs::read(scratch.0.join("keys.toml")).unwrap())
            .unwrap();
    assert!(
        config
            .with_application_keys(&keys)
            .unwrap_err()
            .to_string()
            .contains("required_for_daily_quota")
    );
}
