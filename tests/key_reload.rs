#[path = "support/keys.rs"]
mod key_fixtures;
#[path = "admission/support.rs"]
mod pending;
#[path = "support/scratch.rs"]
mod scratch;
#[path = "support/gateway.rs"]
mod support;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode, header},
    response::Response,
};
use kanata::config::{Plane, ValidatedConfig};
use kanata::keys::reload::{KeyReloader, ReloadOutcome};
use kanata::server::TwoPlaneServer;

use key_fixtures::{keyed_example, set_mode, token};
use support::{Capture, ScratchDir};

const LOCAL: &str = "{ model_alias = \"local-chat\", operation = \"chat\" }";
const PRIVATE: &str = "{ model_alias = \"private-chat\", operation = \"chat\" }";
const CHATGPT: &str = "{ model_alias = \"chatgpt-chat\", operation = \"chat\" }";

/// Temp dir with `config.toml` using `[keys] file = "keys.toml"`.
struct Scratch(ScratchDir);

impl Scratch {
    fn new(public: bool, keys: &str) -> Self {
        let dir = ScratchDir::new("key-reload");
        let config = keyed_example("[keys]\nfile = \"keys.toml\"\n", public);
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

/// One `[[keys]]` record whose secret is `token(id)`; `extra` adds fields.
fn record(id: &str, scopes: &[&str], extra: &str) -> String {
    key_fixtures::record(id, &scopes.join(", "), extra)
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
    let mut reloader = KeyReloader::new(config, Plane::All, server.key_handle());
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

#[test]
fn invalid_keys_files_are_rejected_once_and_keep_the_current_keys() {
    let alpha = record("alpha", &[LOCAL], "");
    let scratch = Scratch::new(false, &keys(std::slice::from_ref(&alpha)));
    let config = scratch.load();
    let (server, _) = support::server_with(&config, Vec::new());
    let handle = server.key_handle();
    let mut reloader = KeyReloader::new(config, Plane::All, handle.clone());
    // Tracing caches callsite interest process-wide; other tests' reloaders may
    // register the rejection WARN first, so the cache is rebuilt once this
    // subscriber is the thread default.
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
        tracing::callsite::rebuild_interest_cache();
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
    let mut reloader = KeyReloader::new(config, Plane::All, server.key_handle());
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
    let mut reloader = KeyReloader::new(full, Plane::Public, server.key_handle());
    scratch.write_keys(
        &keys(&[
            record("owner", &[LOCAL], "owner = true"),
            record("chatgpt", &[LOCAL, CHATGPT], ""),
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
    for id in ["owner", "chatgpt", "private"] {
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
    let mut reloader = KeyReloader::new(config, Plane::All, server.key_handle());
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

#[tokio::test]
async fn reload_health_and_metric_retention_follow_the_current_key_set() {
    let scratch = Scratch::new(false, &keys(&[record("alpha", &[LOCAL], "")]));
    let config = scratch.load();
    let (server, _) = support::server_with(&config, Vec::new());
    let mut reload = KeyReloader::new(config, Plane::All, server.key_handle());
    parts(
        server
            .client_oneshot(models("alpha"))
            .await
            .expect("response"),
    )
    .await;
    let server_ref = &server;
    let scrape = |path: &'static str| async move {
        let server = server_ref;
        let response = server
            .admin_oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        String::from_utf8(parts(response).await.2.to_vec()).expect("text")
    };
    assert!(scrape("/metrics").await.contains("key=\"alpha\""));
    scratch.write_keys("not a keys file", 0o600);
    assert_eq!(reload.poll_once(), ReloadOutcome::Rejected);
    let status: serde_json::Value = serde_json::from_str(&scrape("/status").await).expect("status");
    assert_eq!(status["key_reload"]["healthy"], false);
    assert_eq!(status["key_reload"]["failures"], 1);
    scratch.write_keys(&keys(&[record("beta", &[LOCAL], "")]), 0o600);
    assert_eq!(reload.poll_once(), ReloadOutcome::Applied);
    assert!(!scrape("/metrics").await.contains("key=\"alpha\""));
    let status: serde_json::Value = serde_json::from_str(&scrape("/status").await).expect("status");
    assert_eq!(status["key_reload"]["healthy"], true);
    assert!(
        status["key_reload"]["last_success"]
            .as_u64()
            .expect("timestamp")
            > 0
    );
}
