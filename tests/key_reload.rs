#[path = "admission/support.rs"]
mod pending;
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
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode, header},
    response::Response,
};
use kanata::config::{Plane, ValidatedConfig};
use kanata::keys::reload::{KeyReloader, ReloadOutcome};
use kanata::server::TwoPlaneServer;
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

const LOCAL: &str = "{ model_alias = \"local-chat\", operation = \"chat\" }";
const PRIVATE: &str = "{ model_alias = \"private-chat\", operation = \"chat\" }";
const CODEX: &str = "{ model_alias = \"codex-chat\", operation = \"chat\" }";

/// Temp dir with `config.toml` using `[keys] file = "keys.toml"`.
struct Scratch(PathBuf);

impl Scratch {
    fn new(public: bool, keys: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kanata-key-reload-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).expect("scratch dir");
        let example =
            fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
        let mut config = example.replace(EXAMPLE_INLINE_KEY, "[keys]\nfile = \"keys.toml\"\n");
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
        let scratch = Self(dir);
        scratch.write_keys(keys, 0o600);
        scratch
    }

    fn keys_path(&self) -> PathBuf {
        self.0.join("keys.toml")
    }

    fn write_keys(&self, contents: &str, mode: u32) {
        fs::write(self.keys_path(), contents).expect("keys write");
        set_mode(&self.keys_path(), mode);
    }

    fn load(&self) -> ValidatedConfig {
        kanata::config::load(self.0.join("config.toml")).expect("config loads")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
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

/// One `[[keys]]` record whose secret is `token(id)`; `extra` adds fields.
fn record(id: &str, scopes: &[&str], extra: &str) -> String {
    let digest: [u8; 32] = Sha256::digest(token(id).as_bytes()).into();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "[[keys]]\nid = \"{id}\"\ndigest = \"sha256:{hex}\"\ncreated_at = \"2026-01-01T00:00:00Z\"\npermissions = [{}]\n{extra}\n",
        scopes.join(", ")
    )
}

fn keys(records: &[String]) -> String {
    format!("version = 1\n\n{}", records.join("\n"))
}

fn chat(id: &str) -> Request<Body> {
    support::chat_request_with(
        r#"{"model":"local-chat","messages":[{"role":"user","content":"hello"}]}"#,
        Some(support::CHAT_CONTENT_TYPE),
        Some(&format!("Bearer {}", token(id))),
        &[],
    )
}

fn models(id: &str) -> Request<Body> {
    Request::builder()
        .uri("/v1/models")
        .header(header::AUTHORIZATION, format!("Bearer {}", token(id)))
        .body(Body::empty())
        .expect("request")
}

fn pending_server(config: &ValidatedConfig) -> (Arc<TwoPlaneServer>, pending::PendingProbe) {
    let (adapter, probe) = pending::pending_adapter(
        "ollama-local",
        support::capabilities(config, "ollama-local"),
    );
    (pending::server(config, vec![adapter]), probe)
}

async fn parts(response: Response) -> (StatusCode, HeaderMap, Bytes) {
    let status = response.status();
    let headers = response.headers().clone();
    (status, headers, support::response_body(response).await)
}

async fn code(response: Response) -> serde_json::Value {
    support::response_json(response).await["error"]["code"].clone()
}

#[tokio::test]
async fn reload_revokes_and_adds_keys_without_cancelling_in_flight_requests() {
    let scratch = Scratch::new(false, &keys(&[record("alpha", &[LOCAL], "")]));
    let config = scratch.load();
    let (server, probe) = pending_server(&config);
    let mut reloader =
        KeyReloader::new(config, Plane::All, server.key_handle()).expect("file source reloads");
    assert_eq!(reloader.poll_once(), ReloadOutcome::Unchanged);

    let in_flight = {
        let server = server.clone();
        tokio::spawn(async move {
            server
                .client_oneshot(chat("alpha"))
                .await
                .expect("response")
        })
    };
    probe.wait_for_dispatch(1).await;

    scratch.write_keys(
        &keys(&[
            record("alpha", &[LOCAL], "revoked_at = \"2026-02-01T00:00:00Z\""),
            record("beta", &[LOCAL], ""),
        ]),
        0o600,
    );
    assert_eq!(reloader.poll_once(), ReloadOutcome::Applied);
    assert_eq!(reloader.poll_once(), ReloadOutcome::Unchanged);

    let revoked = server
        .client_oneshot(chat("alpha"))
        .await
        .expect("response");
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(code(revoked).await, "invalid_api_key");

    let added = {
        let server = server.clone();
        tokio::spawn(async move { server.client_oneshot(chat("beta")).await.expect("response") })
    };
    probe.wait_for_dispatch(2).await;
    probe.release();
    probe.release();
    assert_eq!(added.await.expect("task").status(), StatusCode::OK);
    assert_eq!(in_flight.await.expect("task").status(), StatusCode::OK);
    assert_eq!(probe.cancellations(), 0);
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture").clone()).expect("utf8")
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

#[test]
fn invalid_keys_files_are_rejected_once_and_keep_the_current_keys() {
    let alpha = record("alpha", &[LOCAL], "");
    let scratch = Scratch::new(false, &keys(std::slice::from_ref(&alpha)));
    let config = scratch.load();
    let (server, _) = support::server_with(&config, Vec::new());
    let handle = server.key_handle();
    let mut reloader = KeyReloader::new(config, Plane::All, handle.clone()).expect("reloader");
    // Tracing caches callsite interest process-wide, so assert only on the
    // rejection WARN, which no other test in this binary reaches.
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(capture.clone())
        .finish();
    let authenticates = |id: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", token(id)).parse().expect("header"),
        );
        handle.current().authenticate_headers(&headers).is_ok()
    };

    let valid = keys(&[alpha.clone(), record("beta", &[LOCAL], "")]);
    let cases = [
        (
            keys(std::slice::from_ref(&alpha)) + "[[keys]\n",
            0o600,
            "keys_file: parse_error",
        ),
        (
            keys(&[
                alpha.clone(),
                record(
                    "beta",
                    &["{ model_alias = \"gone\", operation = \"chat\" }"],
                    "",
                ),
            ]),
            0o600,
            "keys_file.keys[1].permissions[0]: unknown_route_selector",
        ),
        (valid.clone(), 0o620, "keys.file: insecure_permissions"),
    ];
    tracing::subscriber::with_default(subscriber, || {
        for (index, (contents, mode, error)) in cases.iter().enumerate() {
            scratch.write_keys(contents, *mode);
            assert_eq!(reloader.poll_once(), ReloadOutcome::Rejected, "{error}");
            assert_eq!(reloader.poll_once(), ReloadOutcome::Rejected, "{error}");
            let log = capture.text();
            assert_eq!(
                log.matches("keys file rejected").count(),
                index + 1,
                "{log}"
            );
            assert_eq!(log.matches(error).count(), 1, "{log}");
            assert!(authenticates("alpha"));
            assert!(!authenticates("beta"));
        }

        // The group-writable file is applied once only its mode is fixed.
        set_mode(&scratch.keys_path(), 0o600);
        assert_eq!(reloader.poll_once(), ReloadOutcome::Applied);
    });
    assert!(authenticates("alpha"));
    assert!(authenticates("beta"));
    assert!(!capture.text().contains("sha256:"));
}

#[tokio::test]
async fn expired_keys_get_key_expired_and_revoked_keys_match_unknown_on_both_listeners() {
    let scratch = Scratch::new(
        true,
        &keys(&[
            record("expired", &[LOCAL], "expires_at = \"2026-01-02T00:00:00Z\""),
            record("revoked", &[LOCAL], ""),
        ]),
    );
    let config = scratch.load();
    let server = support::server_without_adapters(&config);
    let mut reloader = KeyReloader::new(config, Plane::All, server.key_handle()).expect("reloader");
    scratch.write_keys(
        &keys(&[
            record("expired", &[LOCAL], "expires_at = \"2026-01-02T00:00:00Z\""),
            record("revoked", &[LOCAL], "revoked_at = \"2026-01-03T00:00:00Z\""),
        ]),
        0o600,
    );
    assert_eq!(reloader.poll_once(), ReloadOutcome::Applied);

    let server = &server;
    for public in [false, true] {
        let send = |request| async move {
            if public {
                server
                    .public_oneshot(request)
                    .await
                    .expect("public listener")
            } else {
                server.client_oneshot(request).await.expect("response")
            }
        };
        let expired = send(models("expired")).await;
        assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            expired.headers()[header::WWW_AUTHENTICATE],
            "Bearer realm=\"kanata\", error=\"invalid_token\""
        );
        let json = support::response_json(expired).await;
        assert_eq!(json["error"]["code"], "key_expired");
        assert_eq!(json["error"]["type"], "authentication_error");

        let revoked = parts(send(models("revoked")).await).await;
        let unknown = parts(send(models("unknown")).await).await;
        let expected = if public {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::UNAUTHORIZED
        };
        assert_eq!(revoked.0, expected);
        assert_eq!(revoked, unknown);
    }
}

#[tokio::test]
async fn public_plane_reload_admits_only_narrowed_public_keys() {
    let scratch = Scratch::new(true, &keys(&[]));
    let full = scratch.load();
    let config = full.for_plane(Plane::Public).expect("public plane");
    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "ollama-local",
            support::capabilities(&config, "ollama-local"),
            support::chat_outcome("ok"),
        )],
    );
    let mut reloader =
        KeyReloader::new(full, Plane::Public, server.key_handle()).expect("reloader");
    scratch.write_keys(
        &keys(&[
            record("owner", &[LOCAL], "owner = true"),
            record("codex", &[LOCAL, CODEX], ""),
            record("private", &[PRIVATE], ""),
            record("public", &[LOCAL, PRIVATE], ""),
        ]),
        0o600,
    );
    assert_eq!(reloader.poll_once(), ReloadOutcome::Applied);

    let unknown = parts(
        server
            .public_oneshot(models("unknown"))
            .await
            .expect("public"),
    )
    .await;
    assert_eq!(unknown.0, StatusCode::FORBIDDEN);
    for id in ["owner", "codex", "private"] {
        let rejected = parts(server.public_oneshot(models(id)).await.expect("public")).await;
        assert_eq!(rejected, unknown, "{id}");
    }
    let listed = server
        .public_oneshot(models("public"))
        .await
        .expect("public");
    assert_eq!(listed.status(), StatusCode::OK);
    let ids: Vec<_> = support::response_json(listed).await["data"]
        .as_array()
        .expect("model list")
        .iter()
        .map(|model| model["id"].clone())
        .collect();
    assert_eq!(ids, ["local-chat"]);
    let served = server.public_oneshot(chat("public")).await.expect("public");
    assert_eq!(served.status(), StatusCode::OK);
}

#[tokio::test]
async fn per_key_limits_survive_unchanged_reloads_and_changed_limits_apply() {
    let limited = |limit: u32| record("alpha", &[LOCAL], &format!("max_in_flight = {limit}"));
    let scratch = Scratch::new(false, &keys(&[limited(1)]));
    let config = scratch.load();
    let (server, probe) = pending_server(&config);
    let mut reloader = KeyReloader::new(config, Plane::All, server.key_handle()).expect("reloader");
    let spawn = |server: &Arc<TwoPlaneServer>| {
        let server = server.clone();
        tokio::spawn(async move {
            server
                .client_oneshot(chat("alpha"))
                .await
                .expect("response")
        })
    };

    let first = spawn(&server);
    probe.wait_for_dispatch(1).await;
    scratch.write_keys(&keys(&[limited(1), record("beta", &[LOCAL], "")]), 0o600);
    assert_eq!(reloader.poll_once(), ReloadOutcome::Applied);
    let busy = server
        .client_oneshot(chat("alpha"))
        .await
        .expect("response");
    assert_eq!(busy.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code(busy).await, "gateway_key_busy");

    scratch.write_keys(&keys(&[limited(2), record("beta", &[LOCAL], "")]), 0o600);
    assert_eq!(reloader.poll_once(), ReloadOutcome::Applied);
    let second = spawn(&server);
    probe.wait_for_dispatch(2).await;
    probe.release();
    probe.release();
    assert_eq!(first.await.expect("task").status(), StatusCode::OK);
    assert_eq!(second.await.expect("task").status(), StatusCode::OK);
}

#[test]
fn inline_keys_have_no_reloader() {
    let config = support::config();
    let server = support::server_without_adapters(&config);
    assert!(KeyReloader::new(config, Plane::All, server.key_handle()).is_none());
}
