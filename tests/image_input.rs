#[path = "support/gateway.rs"]
mod support;
#[path = "adapter_ollama/support.rs"]
mod upstream;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::http::StatusCode;
use kanata::{
    adapter::{Adapter, ollama::OllamaAdapter, vllm::VllmAdapter},
    config,
    core::ChatContent,
    server::{Readiness, TwoPlaneServer},
};
use serde_json::{Value, json};
use support::{
    adapter_spec, capabilities, chat_outcome, chat_request, models_request, recorded_len,
    response_json, server_with, take_request,
};

const PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl2n3QAAAAASUVORK5CYII=";
static NEXT: AtomicUsize = AtomicUsize::new(0);

fn config_text(kind: &str, address: &str, allowed: bool) -> String {
    format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080
[listeners.public]
bind = "172.30.0.3"
port = 18081
[listeners.admin]
bind = "127.0.0.1"
port = 19090
[publication]
tailnet_addresses = ["100.64.0.10"]
public_routes = [{{ model_alias = "vision", operation = "chat" }}]
[[adapters]]
id = "vision-adapter"
kind = "{kind}"
base_url = "http://{address}/v1"
trust_zone = "local"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = false
function_tools = false
input_images = true
[[routes]]
id = "vision-route"
model_alias = "vision"
operation = "chat"
adapter_id = "vision-adapter"
upstream_id = "installed-vision-model"
requires_streaming_chat = false
requires_function_tools = false
allows_input_images = {allowed}
[[application_keys]]
id = "vision-client"
secret_ref = "env:VISION_CLIENT_KEY"
permissions = [{{ model_alias = "vision", operation = "chat" }}]
[limits]
max_queue = 4
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = 1048576
max_extension_bytes = 1024
[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#
    )
}

fn load(text: &str) -> Result<config::ValidatedConfig, config::ConfigError> {
    let path = std::env::temp_dir().join(format!(
        "kanata-image-{}-{}.toml",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, text).unwrap();
    let result = config::load(&path);
    std::fs::remove_file(path).unwrap();
    result
}

fn request(parts: Value) -> Value {
    json!({"model": "vision", "messages": [{"role": "user", "content": parts}]})
}

fn image() -> Value {
    json!({"type": "image_url", "image_url": {"url": PNG}})
}

#[tokio::test]
async fn private_and_public_chat_preserve_image_order_and_discover_scoped_limits() {
    let config = load(&config_text("ollama", "127.0.0.1:9", true)).unwrap();
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "vision-adapter",
            capabilities(&config, "vision-adapter"),
            chat_outcome("seen"),
        )],
    );
    let body = request(json!([image(), {"type": "text", "text": "Describe it"}, image()]));
    for public in [false, true] {
        let response = if public {
            server
                .public_oneshot(chat_request(&body.to_string()))
                .await
                .unwrap()
        } else {
            server
                .client_oneshot(chat_request(&body.to_string()))
                .await
                .unwrap()
        };
        assert_eq!(response.status(), StatusCode::OK);
        let recorded = take_request(&requests);
        let chat = support::core_chat(recorded);
        assert!(
            matches!(&chat.messages[0].content[0], ChatContent::InputImage { image } if image.data_url() == PNG)
        );
        assert!(
            matches!(&chat.messages[0].content[1], ChatContent::Text { text } if text == "Describe it")
        );
        let response = if public {
            server.public_oneshot(models_request()).await.unwrap()
        } else {
            server.client_oneshot(models_request()).await.unwrap()
        };
        let models = response_json(response).await;
        let caps = &models["data"][0]["kanata"];
        assert_eq!(caps["input_images"], true);
        assert_eq!(caps["images"]["max_count"], 4);
        assert_eq!(
            caps["images"]["media_types"],
            json!(["image/png", "image/jpeg"])
        );
        assert!(!models.to_string().contains("installed-vision-model"));
    }
}

#[tokio::test]
async fn image_validation_rejects_remote_files_unknown_options_and_wrong_roles_before_dispatch() {
    let config = load(&config_text("ollama", "127.0.0.1:9", true)).unwrap();
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "vision-adapter",
            capabilities(&config, "vision-adapter"),
            chat_outcome("unused"),
        )],
    );
    let mut cases = vec![
        request(json!([{"type": "image_url", "image_url": {"url": "https://127.0.0.1/private"}}])),
        request(json!([{"type": "image_url", "image_url": {"url": "file:///etc/passwd"}}])),
        request(json!([{"type": "image_url", "image_url": {"url": "data:image/png;base64,YQ=="}}])),
        request(json!([{"type": "image_url", "image_url": {"url": PNG, "detail": "high"}}])),
        request(json!([{"type": "image_url", "image_url": {"url": PNG}, "text": "wrong"}])),
    ];
    for role in ["assistant", "system", "developer", "tool"] {
        let mut body = request(json!([image()]));
        body["messages"][0]["role"] = json!(role);
        cases.push(body);
    }
    for body in cases {
        let response = server
            .client_oneshot(chat_request(&body.to_string()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    let too_many = request(Value::Array(vec![image(); 5]));
    assert_eq!(
        server
            .client_oneshot(chat_request(&too_many.to_string()))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(recorded_len(&requests), 0);
}

#[tokio::test]
async fn image_requests_require_route_permission_and_keep_the_configured_body_cap() {
    for allowed in [false, true] {
        let text = config_text("ollama", "127.0.0.1:9", allowed)
            .replace("max_body_bytes = 1048576", "max_body_bytes = 256");
        let config = load(&text).unwrap();
        let (server, requests) = server_with(
            &config,
            vec![adapter_spec(
                "vision-adapter",
                capabilities(&config, "vision-adapter"),
                chat_outcome("unused"),
            )],
        );
        let body = request(json!([image()]));
        let response = server
            .client_oneshot(chat_request(&body.to_string()))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if allowed {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        );
        let models = response_json(server.client_oneshot(models_request()).await.unwrap()).await;
        assert_eq!(models["data"][0]["kanata"]["input_images"], allowed);
        if !allowed {
            assert!(models["data"][0]["kanata"]["images"].is_null());
        }
        let before = recorded_len(&requests);
        let body = request(json!([image(), {"type": "text", "text": "a".repeat(256)}]));
        assert_eq!(
            server
                .client_oneshot(chat_request(&body.to_string()))
                .await
                .unwrap()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(recorded_len(&requests), before);
    }
}

#[test]
fn image_config_rejects_unsupported_provider_and_missing_capability() {
    let text = config_text("ollama", "127.0.0.1:9", true);
    assert!(load(&text.replacen("input_images = true", "input_images = false", 1)).is_err());
    assert!(
        load(&text.replace("kind = \"ollama\"", "kind = \"apple_fm\""))
            .unwrap_err()
            .to_string()
            .contains("input_images")
    );
    let text = include_str!("fixtures/config/example.toml").replace("https://chatgpt.invalid/backend-api/codex\"\ntrust_zone = \"external\"\n[adapters.capabilities]", "https://chatgpt.invalid/backend-api/codex\"\ntrust_zone = \"external\"\n[adapters.capabilities]\ninput_images = true");
    assert!(
        load(&text)
            .unwrap_err()
            .to_string()
            .contains("input_images")
    );
}

#[tokio::test]
async fn configured_compatible_adapters_forward_inline_images_without_fetching_them() {
    for kind in ["ollama", "vllm"] {
        let mut mock =
            upstream::MockServer::once(upstream::ResponseSpec::json(upstream::TEXT_RESPONSE)).await;
        let config = load(&config_text(kind, &mock.address, true)).unwrap();
        let adapter: Arc<dyn Adapter> = match kind {
            "ollama" => Arc::new(
                OllamaAdapter::from_config(&config, "vision-adapter", "vision-route").unwrap(),
            ),
            _ => Arc::new(
                VllmAdapter::from_config(&config, "vision-adapter", "vision-route").unwrap(),
            ),
        };
        let server = TwoPlaneServer::from_validated_with_adapters(
            &config,
            &support::Resolver,
            Readiness::new(true),
            vec![adapter],
        )
        .unwrap();
        let body = request(json!([{"type": "text", "text": "Describe"}, image()]));
        let response = server
            .client_oneshot(chat_request(&body.to_string()))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{kind}: {}",
            response_json(response).await
        );
        mock.finish().await;
        let records = mock.requests.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "/v1/chat/completions");
        let sent: Value = serde_json::from_slice(&records[0].body).unwrap();
        assert_eq!(sent["messages"], body["messages"]);
        assert_eq!(sent["model"], "installed-vision-model");
    }
}
