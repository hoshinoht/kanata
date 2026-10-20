//! `kind = "cliproxy"`: CLIProxyAPI on the vLLM chat path, private-only like ChatGPT/Codex.

#[path = "support/scratch.rs"]
mod scratch;
#[path = "support/gateway.rs"]
mod support;
#[path = "adapter_ollama/support.rs"]
mod upstream;

use std::sync::Arc;

use axum::http::StatusCode;
use kanata::{
    adapter::{Adapter, ollama::OllamaAdapter, vllm::VllmAdapter},
    config::{Plane, ProviderKind, ValidatedConfig},
    server::{Readiness, TwoPlaneServer},
};
use serde_json::{Value, json};
use support::{
    TempFile, chat_request, compact_config, load_text as load, models_request, response_json,
};

const RESPONSE: &str = include_str!("fixtures/cliproxy/chat-text-response.json");
const LOOPBACK: &str = "http://127.0.0.1:9/v1";
const PUBLIC_ONLY_KEY: &str = r#"
[[application_keys]]
id = "public-only"
secret_ref = "env:PUBLIC_ONLY_KEY"
permissions = [{ model_alias = "local", operation = "chat" }]
"#;

/// An Ollama route published on the public listener next to a CLIProxyAPI effort family.
fn source(base_url: &str, extra_keys: &str) -> String {
    compact_config(
        r#"[{ model_alias = "local", operation = "chat" }]"#,
        &format!(
            r#"[[adapters]]
id = "ollama"
kind = "ollama"
base_url = "http://127.0.0.1:9"
trust_zone = "local"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = false
function_tools = false
[[adapters]]
id = "cliproxy"
kind = "cliproxy"
base_url = "{base_url}"
trust_zone = "external"
secret_ref = "env:CLIPROXY_KEY"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = true
function_tools = true
reasoning_control = true
[models.chat.local]
adapter_id = "ollama"
upstream_id = "qwen3:0.6b"
[models.chat.claude]
adapter_id = "cliproxy"
upstream_id = "claude-sonnet"
default_effort = "medium"
efforts = ["low", "medium", "high"]
[[application_keys]]
id = "mixed"
secret_ref = "env:MIXED_KEY"
permissions = [
  {{ model_alias = "local", operation = "chat" }},
  {{ model_alias = "claude", operation = "chat" }},
  {{ model_alias = "claude:high", operation = "chat" }},
]
{extra_keys}"#
        ),
    )
}

fn error(text: &str) -> String {
    load(text).unwrap_err().to_string()
}

fn server(config: &ValidatedConfig) -> TwoPlaneServer {
    let ollama = OllamaAdapter::new(&config.adapters()[0], config.timeouts(), config.limits())
        .expect("ollama adapter");
    let cliproxy =
        VllmAdapter::from_config_with_secrets(config, "cliproxy", "claude", &support::Resolver)
            .expect("cliproxy adapter");
    let adapters: Vec<Arc<dyn Adapter>> = vec![Arc::new(ollama), Arc::new(cliproxy)];
    TwoPlaneServer::from_validated_with_adapters(
        config,
        &support::Resolver,
        Readiness::new(true),
        adapters,
    )
    .expect("server")
}

fn model_ids(models: &Value) -> Vec<&str> {
    models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect()
}

#[test]
fn config_accepts_loopback_http_and_private_ca_https_with_an_effort_family() {
    let config = load(&source(LOOPBACK, "")).unwrap();
    let adapter = &config.adapters()[1];
    assert_eq!(adapter.kind(), ProviderKind::Cliproxy);
    assert!(adapter.kind().is_private_only());
    assert!(!adapter.kind().applies_route_effort_pin());
    let family: Vec<_> = config
        .routes()
        .iter()
        .filter(|route| route.adapter_id() == "cliproxy")
        .map(|route| {
            (
                route.identity().selector.model_alias.0.as_str(),
                route.pinned_reasoning_effort(),
            )
        })
        .collect();
    assert_eq!(
        family,
        [
            ("claude", Some("medium")),
            ("claude:low", Some("low")),
            ("claude:medium", Some("medium")),
            ("claude:high", Some("high")),
        ]
    );

    // The TLS sidecar form; the bundle itself is read only when files are loaded.
    let sidecar = source("https://cliproxy-tls:8443/v1", "").replace(
        "secret_ref = \"env:CLIPROXY_KEY\"",
        "secret_ref = \"env:CLIPROXY_KEY\"\nca_file = \"/run/secrets/cliproxy-ca.pem\"",
    );
    let sidecar = TempFile::new("cliproxy", "toml", sidecar);
    let config = kanata::config::load_deferring_keys(sidecar.path()).unwrap();
    let adapter = &config.adapters()[1];
    assert_eq!(adapter.kind(), ProviderKind::Cliproxy);
    assert!(adapter.ca_bundle().is_some());
}

#[test]
fn check_reads_ca_file_only_for_adapters_the_plane_keeps() {
    let missing = support::unique_path("cliproxy-missing-ca").with_extension("pem");
    let sidecar = source("https://cliproxy-tls:8443/v1", "").replace(
        "secret_ref = \"env:CLIPROXY_KEY\"",
        &format!(
            "secret_ref = \"env:CLIPROXY_KEY\"\nca_file = \"{}\"",
            missing.display()
        ),
    );
    let config = TempFile::new("cliproxy", "toml", sidecar);
    let check = |plane: &str| {
        kanata::cli::run(
            ["check", "--config", config.as_str(), "--plane", plane].map(str::to_owned),
        )
    };
    // The public plane drops the private-only adapter, so its CA is never read.
    assert_eq!(check("public"), Ok(Some("configuration valid".to_owned())));
    for plane in ["private", "all"] {
        assert_eq!(
            check(plane),
            Err("config error at adapters[1].ca_file: read_error".to_owned())
        );
    }
    let (full, public) = kanata::config::load_plane(config.path(), Plane::Public).unwrap();
    assert!(
        public
            .adapters()
            .iter()
            .all(|adapter| adapter.id() != "cliproxy")
    );
    assert!(full.adapters()[1].ca_bundle().is_some());
}

#[test]
fn config_rejects_publication_zones_missing_keys_and_non_chat_options() {
    let base = source(LOOPBACK, "");
    for alias in ["claude", "claude:high"] {
        let published = base.replace(
            "public_routes = [{ model_alias = \"local\", operation = \"chat\" }]",
            &format!("public_routes = [{{ model_alias = \"{alias}\", operation = \"chat\" }}]"),
        );
        assert_eq!(
            error(&published),
            "config error at publication.public_routes[0]: cliproxy_not_public"
        );
    }
    for zone in ["local", "private_network"] {
        assert_eq!(
            error(&base.replace(
                "trust_zone = \"external\"",
                &format!("trust_zone = \"{zone}\"")
            )),
            "config error at adapters[1].trust_zone: provider_zone_mismatch"
        );
    }
    assert_eq!(
        error(&base.replace("secret_ref = \"env:CLIPROXY_KEY\"\n", "")),
        "config error at adapters[1].secret_ref: required"
    );
    // Credentials need HTTPS unless the host is a literal loopback IP.
    assert_eq!(
        error(&source("http://cliproxy.invalid/v1", "")),
        "config error at adapters[1].base_url: https_required"
    );
    for (text, expected) in [
        (
            base.replace(
                "operations = [\"chat\"]\nstreaming_chat = true",
                "operations = [\"chat\", \"transcription\"]\nstreaming_chat = true",
            ),
            "adapters[1].capabilities.operations: cliproxy_chat_only",
        ),
        (
            base.replace(
                "secret_ref = \"env:CLIPROXY_KEY\"",
                "secret_ref = \"env:CLIPROXY_KEY\"\ntranscription_mode = \"native_asr\"",
            ),
            "adapters[1].transcription_mode: vllm_only",
        ),
        (
            base.replace(
                "function_tools = true\nreasoning_control = true",
                "function_tools = true\nreasoning_control = true\ninput_audio = true",
            ),
            "adapters[1].capabilities.input_audio: unsupported_by_adapter_kind",
        ),
        (
            base.replace(
                "[models.chat.claude]",
                "[models.chat.claude]\nenable_thinking = false",
            ),
            "enable_thinking: vllm_only",
        ),
    ] {
        let message = error(&text);
        assert!(message.ends_with(expected), "{message}");
    }
}

#[test]
fn public_plane_drops_cliproxy_routes_adapters_and_keys_with_cliproxy_scopes() {
    let config = load(&source(LOOPBACK, PUBLIC_ONLY_KEY)).unwrap();
    let private = config.for_plane(Plane::Private).unwrap();
    assert_eq!(private.application_keys().len(), 2);
    assert!(
        private
            .adapters()
            .iter()
            .any(|adapter| adapter.kind() == ProviderKind::Cliproxy)
    );

    let public = config.for_plane(Plane::Public).unwrap();
    assert!(
        public
            .adapters()
            .iter()
            .all(|adapter| adapter.kind() != ProviderKind::Cliproxy)
    );
    let aliases: Vec<_> = public
        .routes()
        .iter()
        .map(|route| route.identity().selector.model_alias.0.as_str())
        .collect();
    assert_eq!(aliases, ["local"]);
    // The mixed key also holds a public scope but never enters the public process.
    let keys: Vec<_> = public
        .application_keys()
        .iter()
        .map(|key| key.id())
        .collect();
    assert_eq!(keys, ["public-only"]);
}

#[test]
fn routes_command_marks_cliproxy_routes_never_public() {
    let file = TempFile::new("cliproxy", "toml", source(LOOPBACK, ""));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["routes", "--config", file.as_str()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let row = |alias: &str| -> Vec<String> {
        text.lines()
            .find(|line| line.starts_with(&format!("{alias} ")))
            .expect("row")
            .split_whitespace()
            .take(4)
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(row("local"), ["local", "chat", "ollama", "yes"]);
    for alias in ["claude", "claude:high"] {
        assert_eq!(row(alias), [alias, "chat", "cliproxy", "never"]);
    }
}

#[tokio::test]
async fn public_models_never_list_cliproxy_routes() {
    let config = load(&source(LOOPBACK, "")).unwrap();
    let server = server(&config);
    let private = response_json(server.client_oneshot(models_request()).await.unwrap()).await;
    let mut ids = model_ids(&private);
    ids.sort_unstable();
    assert_eq!(ids, ["claude", "local"]);
    let public = response_json(server.public_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(model_ids(&public), ["local"]);
}

#[tokio::test]
async fn pinned_routes_forward_their_effort_with_the_client_key() {
    for (model, effort, upstream_effort) in [
        ("claude:high", None, "high"),
        ("claude", None, "medium"),
        ("claude", Some("high"), "high"),
    ] {
        let mut body =
            json!({"model": model, "messages": [{"role": "user", "content": "fixture"}]});
        if let Some(effort) = effort {
            body["reasoning_effort"] = json!(effort);
        }
        let mut mock = upstream::MockServer::once(upstream::ResponseSpec::json(RESPONSE)).await;
        let config = load(&source(&format!("http://{}/v1", mock.address), "")).unwrap();
        let response = server(&config)
            .client_oneshot(chat_request(&body.to_string()))
            .await
            .unwrap();
        let status = response.status();
        let output = response_json(response).await;
        assert_eq!(status, StatusCode::OK, "{body}: {output}");
        assert_eq!(output["model"], "claude");
        assert_eq!(
            output["choices"][0]["message"]["content"],
            "fixture cliproxy response"
        );
        mock.finish().await;
        let record = mock.requests.lock().unwrap()[0].clone();
        assert_eq!(record.path, "/v1/chat/completions");
        assert_eq!(
            record.headers.get("authorization").map(String::as_str),
            Some("Bearer test-key")
        );
        let sent: Value = serde_json::from_slice(&record.body).unwrap();
        assert_eq!(sent["model"], "claude-sonnet");
        assert_eq!(sent["reasoning_effort"], upstream_effort, "{body}");
        assert!(sent.get("chat_template_kwargs").is_none());
    }
}
