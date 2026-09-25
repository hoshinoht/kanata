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
    assert_eq!(json["version"], 1);
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
            requests: 7,
            last_used_at: kanata::keys::time::parse("2026-04-01T00:00:00Z").expect("time"),
        }
    );
    assert!(read_merged(&state.join("missing")).is_empty());
}
