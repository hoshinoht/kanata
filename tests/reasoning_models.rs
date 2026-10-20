#[path = "support/scratch.rs"]
mod scratch;
#[path = "support/gateway.rs"]
mod support;
#[path = "adapter_ollama/support.rs"]
mod upstream;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use support::{
    adapter_spec, capabilities, chat_outcome, chat_request, load_text as load, models_request,
    response_json, server_with,
};

/// The schema fixture's ChatGPT route as `reasoning-model` with `reasoning_control`, a base
/// pinned to medium, `:low`/`:medium`/`:high` variants, a public listener, and one owner key
/// (secret `test-key`) granted exactly `permissions`.
fn source(permissions: &[&str]) -> String {
    let mut text = std::fs::read_to_string("tests/fixtures/config/example.toml").unwrap();
    text = text.replace("chatgpt-chat", "reasoning-model").replace(
        "model_alias = \"reasoning-model\"\noperation",
        "model_alias = \"reasoning-model\"\nreasoning_effort = \"medium\"\noperation",
    );
    let marker = "id = \"chatgpt-private\"";
    let start = text.find(marker).unwrap();
    let insert = text[start..].find("function_tools = true").unwrap() + start;
    text.insert_str(insert, "reasoning_control = true\n");
    let mut variants = String::new();
    for effort in ["low", "medium", "high"] {
        variants.push_str(&variant(effort));
    }
    let insert = text.find("[keys]").unwrap();
    text.insert_str(insert, &variants);
    text = text
        .replace(
            "[keys]\nfile = \"example-keys.toml\"\n",
            scratch::KEYS_SECTION,
        )
        .replace(
            "[listeners.admin]",
            "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
        );
    let grants = permissions
        .iter()
        .map(|alias| format!("{{ model_alias = \"{alias}\", operation = \"chat\" }}"))
        .collect::<Vec<_>>()
        .join(", ");
    let key = scratch::key_record(
        "personal-client",
        scratch::TEST_KEY,
        &format!("owner = true\npermissions = [{grants}]"),
    );
    format!("{text}{}{key}", scratch::KEYS_FILE_MARKER)
}

/// One pinned `reasoning-model:{effort}` route on the ChatGPT adapter.
fn variant(effort: &str) -> String {
    format!(
        r#"
[[routes]]
id = "reasoning-{effort}"
model_alias = "reasoning-model:{effort}"
operation = "chat"
adapter_id = "chatgpt-private"
upstream_id = "gpt-5"
reasoning_effort = "{effort}"
requires_streaming_chat = true
requires_function_tools = true

"#
    )
}

fn server(permissions: &[&str]) -> support::ServerWithRequests {
    let config = load(&source(permissions)).unwrap();
    let specs = vec![adapter_spec(
        "chatgpt-private",
        capabilities(&config, "chatgpt-private"),
        chat_outcome("fixture answer"),
    )];
    server_with(&config, specs)
}

#[tokio::test]
async fn discovery_groups_only_accessible_bound_reasoning_routes() {
    let (server, _) = server(&["reasoning-model:low", "reasoning-model:medium"]);
    let models = response_json(server.client_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "reasoning-model");
    assert_eq!(models["data"][0]["kanata"]["operations"], json!(["chat"]));
    assert_eq!(
        models["data"][0]["kanata"]["reasoning_efforts"],
        json!(["low", "medium"])
    );
    let public = response_json(server.public_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(public["data"], json!([]));
}

#[tokio::test]
async fn base_and_legacy_requests_enforce_exact_effort_grants() {
    let (server, requests) = server(&["reasoning-model:low", "reasoning-model:medium"]);
    for (model, effort, status) in [
        ("reasoning-model", None, StatusCode::BAD_REQUEST),
        ("reasoning-model", Some("high"), StatusCode::FORBIDDEN),
        ("reasoning-model", Some("xhigh"), StatusCode::BAD_REQUEST),
        (
            "reasoning-model:low",
            Some("medium"),
            StatusCode::BAD_REQUEST,
        ),
        ("reasoning-model:high", None, StatusCode::FORBIDDEN),
        ("reasoning-model", Some("low"), StatusCode::OK),
        ("reasoning-model:low", Some("low"), StatusCode::OK),
        ("reasoning-model:low", None, StatusCode::OK),
    ] {
        let mut body = json!({"model":model,"messages":[{"role":"user","content":"fixture"}]});
        if let Some(effort) = effort {
            body["reasoning_effort"] = json!(effort);
        }
        let response = server
            .client_oneshot(chat_request(&body.to_string()))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{model} {effort:?}");
        let output = response_json(response).await;
        if status == StatusCode::BAD_REQUEST {
            assert_eq!(output["error"]["param"], "reasoning_effort");
        }
        if status == StatusCode::OK {
            assert_eq!(output["model"], "reasoning-model");
        }
    }
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        assert_eq!(
            request.context().route.selector.model_alias.0,
            "reasoning-model:low"
        );
        assert_eq!(request.request().model_alias().0, "reasoning-model:low");
    }
}

#[tokio::test]
async fn default_and_duplicate_medium_use_authorized_exact_routes() {
    let (server, requests) = server(&["reasoning-model", "reasoning-model:medium"]);
    for effort in [None, Some("medium")] {
        let mut body =
            json!({"model":"reasoning-model","messages":[{"role":"user","content":"fixture"}]});
        if let Some(effort) = effort {
            body["reasoning_effort"] = json!(effort);
        }
        assert_eq!(
            server
                .client_oneshot(chat_request(&body.to_string()))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    assert!(
        requests.lock().unwrap().iter().all(|request| request
            .context()
            .route
            .selector
            .model_alias
            .0
            == "reasoning-model")
    );
    let models = response_json(server.client_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(
        models["data"][0]["kanata"]["reasoning_efforts"],
        json!(["medium"])
    );
    let (server, requests) = self::server(&["reasoning-model:medium"]);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model":"reasoning-model","input":"fixture","reasoning":{"effort":"medium"}})
                .to_string(),
        ))
        .unwrap();
    let response = server.client_oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let output: Value = response_json(response).await;
    assert_eq!(output["model"], "reasoning-model");
    assert_eq!(
        requests.lock().unwrap()[0]
            .context()
            .route
            .selector
            .model_alias
            .0,
        "reasoning-model:medium"
    );
}

#[test]
fn reasoning_families_reject_conflicting_backends_and_capabilities() {
    let base = source(&["reasoning-model:low"]);
    for replacement in [
        "upstream_id = \"different-upstream\"\nreasoning_effort = \"low\"",
        "upstream_id = \"gpt-5\"\ncontext_tokens = 4096\nreasoning_effort = \"low\"",
        "upstream_id = \"gpt-5\"\nreasoning_summary = \"auto\"\nreasoning_effort = \"low\"",
    ] {
        let text = base.replace(
            "upstream_id = \"gpt-5\"\nreasoning_effort = \"low\"",
            replacement,
        );
        assert_ne!(text, base);
        let error = load(&text).expect_err("inconsistent family");
        assert!(
            error.to_string().contains("inconsistent_reasoning_family"),
            "{error}"
        );
    }
}

#[test]
fn summaries_validate_provider_operation_and_reject_the_removed_field() {
    let fixture = std::fs::read_to_string("tests/fixtures/config/example.toml").unwrap();
    for id in ["ollama-chat", "vllm-transcription"] {
        let text = fixture.replace(
            &format!("id = \"{id}\"\nmodel_alias"),
            &format!("id = \"{id}\"\nreasoning_summary = \"auto\"\nmodel_alias"),
        );
        assert_ne!(text, fixture);
        assert!(
            load(&text)
                .unwrap_err()
                .to_string()
                .contains("reasoning_summary: unsupported_by_adapter")
        );
    }
    let generic = fixture.replace(
        "id = \"chatgpt-chat\"\nmodel_alias",
        "id = \"chatgpt-chat\"\nreasoning_summary = \"detailed\"\nmodel_alias",
    );
    assert_ne!(generic, fixture);
    let generic_config = load(&generic).unwrap();
    assert_eq!(
        generic_config
            .routes()
            .iter()
            .find(|route| route.identity().route_id == "chatgpt-chat")
            .unwrap()
            .reasoning_summary(),
        Some(kanata::config::ReasoningSummary::Detailed)
    );
    let mut wrong_operation: toml::Value = toml::from_str(&generic).unwrap();
    let route = wrong_operation["routes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|route| route["id"].as_str() == Some("chatgpt-chat"))
        .unwrap();
    route["operation"] = toml::Value::String("transcription".into());
    assert!(
        load(&toml::to_string(&wrong_operation).unwrap())
            .unwrap_err()
            .to_string()
            .contains("reasoning_summary: unsupported_by_adapter")
    );
    // The Codex-era spelling names its replacement, alone or next to the new field.
    for legacy in [
        generic.replace("reasoning_summary =", "codex_reasoning_summary ="),
        generic.replace(
            "reasoning_summary = \"detailed\"",
            "reasoning_summary = \"detailed\"\ncodex_reasoning_summary = \"detailed\"",
        ),
    ] {
        assert_eq!(
            load(&legacy).unwrap_err().to_string(),
            "config error at routes[4].codex_reasoning_summary: use_reasoning_summary"
        );
    }
}

#[test]
fn summary_policy_must_match_every_private_reasoning_variant() {
    let source = chatgpt_source(&["reasoning-model"]);
    let (config, keys) = source.split_once(scratch::KEYS_FILE_MARKER).unwrap();
    let with_keys = |document: &toml::Value| {
        format!(
            "{}{}{keys}",
            toml::to_string(document).unwrap(),
            scratch::KEYS_FILE_MARKER
        )
    };
    let mut document: toml::Value = toml::from_str(config).unwrap();
    for route in document["routes"].as_array_mut().unwrap() {
        if route["adapter_id"].as_str() == Some("chatgpt-private") {
            route.as_table_mut().unwrap().insert(
                "reasoning_summary".into(),
                toml::Value::String("auto".into()),
            );
        }
    }
    assert!(load(&with_keys(&document)).is_ok());
    let routes = document["routes"].as_array_mut().unwrap();
    let low = routes
        .iter_mut()
        .find(|route| route["model_alias"].as_str() == Some("reasoning-model:low"))
        .unwrap();
    low["reasoning_summary"] = toml::Value::String("concise".into());
    assert!(
        load(&with_keys(&document))
            .unwrap_err()
            .to_string()
            .contains("inconsistent_reasoning_family")
    );
}

struct StreamAdapter {
    caps: kanata::core::Capabilities,
    wrong_model: bool,
}

impl kanata::adapter::Adapter for StreamAdapter {
    fn id(&self) -> &str {
        "chatgpt-private"
    }
    fn capabilities(&self) -> &kanata::core::Capabilities {
        &self.caps
    }
    fn execute(&self, request: kanata::core::RoutedRequest) -> kanata::adapter::AdapterFuture {
        use kanata::core::{FinishReason, ModelAlias, NormalizedEvent};
        let model = if self.wrong_model {
            ModelAlias("reasoning-model".into())
        } else {
            request.request().model_alias().clone()
        };
        let events = futures_util::stream::iter(vec![
            Ok(NormalizedEvent::ChatStarted { model }),
            Ok(NormalizedEvent::ChatTextDelta {
                text: "fixture".into(),
            }),
            Ok(NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::Stop,
                usage: None,
            }),
        ]);
        Box::pin(async move { Ok(kanata::adapter::AdapterOutput::Events(Box::pin(events))) })
    }
}

#[tokio::test]
async fn streams_publish_canonical_model_but_validate_exact_adapter_model() {
    for wrong_model in [false, true] {
        let config = load(&source(&["reasoning-model:low"])).unwrap();
        let adapter = StreamAdapter {
            caps: capabilities(&config, "chatgpt-private"),
            wrong_model,
        };
        let server = kanata::server::TwoPlaneServer::from_validated_with_adapters(
            &config,
            kanata::server::Readiness::new(true),
            vec![std::sync::Arc::new(adapter)],
        )
        .unwrap();
        for endpoint in ["/v1/chat/completions", "/v1/responses"] {
            let body = if endpoint == "/v1/responses" {
                json!({"model":"reasoning-model","input":"fixture","stream":true,"reasoning":{"effort":"low"}})
            } else {
                json!({"model":"reasoning-model","messages":[{"role":"user","content":"fixture"}],"stream":true,"reasoning_effort":"low"})
            };
            let request = Request::builder()
                .method("POST")
                .uri(endpoint)
                .header("authorization", "Bearer test-key")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let response = server.client_oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                if wrong_model {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::OK
                }
            );
            let body = support::response_body(response).await;
            let output = std::str::from_utf8(&body).unwrap();
            if wrong_model {
                assert!(output.contains("upstream_failure"));
            } else {
                assert!(output.contains("\"model\":\"reasoning-model\""), "{output}");
                assert!(!output.contains("reasoning-model:low"));
                assert!(output.contains("fixture"));
            }
        }
    }
}

#[tokio::test]
async fn unconfigured_or_malformed_efforts_are_client_errors() {
    let mut text = source(&["reasoning-model:low"]);
    let start = text.find("[[routes]]\nid = \"reasoning-high\"").unwrap();
    let end = text[start..].find("[keys]").unwrap() + start;
    text.replace_range(start..end, "");
    let config = load(&text).unwrap();
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "chatgpt-private",
            capabilities(&config, "chatgpt-private"),
            chat_outcome("unused"),
        )],
    );
    for effort in [json!("high"), json!("unknown"), Value::Null, json!(3)] {
        let response = server.client_oneshot(chat_request(&json!({"model":"reasoning-model","messages":[{"role":"user","content":"fixture"}],"reasoning_effort":effort}).to_string())).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json(response).await["error"]["param"],
            "reasoning_effort"
        );
    }
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model":"reasoning-model","input":"fixture","reasoning":{"effort":"xhigh"}})
                .to_string(),
        ))
        .unwrap();
    let response = server.client_oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(response).await["error"]["param"],
        "reasoning.effort"
    );
    assert!(requests.lock().unwrap().is_empty());
}

/// [`source`] plus the ChatGPT-only `none`, `xhigh` and `max` variants.
fn chatgpt_source(permissions: &[&str]) -> String {
    let mut text = source(permissions);
    let mut variants = String::new();
    for effort in ["none", "xhigh", "max"] {
        variants.push_str(&variant(effort));
    }
    let insert = text.find("[keys]").unwrap();
    text.insert_str(insert, &variants);
    text
}

#[tokio::test]
async fn generic_private_reasoning_families_are_key_scoped_and_exact() {
    let config = load(&chatgpt_source(&[
        "reasoning-model:none",
        "reasoning-model:low",
        "reasoning-model:xhigh",
    ]))
    .unwrap();
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "chatgpt-private",
            capabilities(&config, "chatgpt-private"),
            chat_outcome("fixture"),
        )],
    );
    let models = response_json(server.client_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "reasoning-model");
    assert_eq!(
        models["data"][0]["kanata"]["reasoning_efforts"],
        json!(["none", "low", "xhigh"])
    );
    for (effort, status) in [
        ("none", StatusCode::OK),
        ("low", StatusCode::OK),
        ("xhigh", StatusCode::OK),
        ("medium", StatusCode::FORBIDDEN),
        ("max", StatusCode::FORBIDDEN),
        ("minimal", StatusCode::BAD_REQUEST),
    ] {
        let response = server.client_oneshot(chat_request(&json!({"model":"reasoning-model","messages":[{"role":"user","content":"fixture"}],"reasoning_effort":effort}).to_string())).await.unwrap();
        assert_eq!(response.status(), status, "{effort}");
        if status == StatusCode::OK {
            assert_eq!(response_json(response).await["model"], "reasoning-model");
            assert_eq!(
                requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .context()
                    .route
                    .selector
                    .model_alias
                    .0,
                format!("reasoning-model:{effort}")
            );
        }
    }
    let public = response_json(server.public_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(public["data"], json!([]));
    let (server, requests) = {
        let config = load(&chatgpt_source(&["reasoning-model"])).unwrap();
        server_with(
            &config,
            vec![adapter_spec(
                "chatgpt-private",
                capabilities(&config, "chatgpt-private"),
                chat_outcome("fixture"),
            )],
        )
    };
    let response = server
        .client_oneshot(chat_request(
            &json!({"model":"reasoning-model","messages":[{"role":"user","content":"fixture"}]})
                .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        requests.lock().unwrap()[0]
            .context()
            .route
            .selector
            .model_alias
            .0,
        "reasoning-model"
    );
}

fn vllm_source(address: &str, permissions: &[&str]) -> String {
    let grants = permissions
        .iter()
        .map(|alias| format!("{{ model_alias = \"{alias}\", operation = \"chat\" }}"))
        .collect::<Vec<_>>()
        .join(", ");
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
public_routes = [{{ model_alias = "claude", operation = "chat" }}]
[[adapters]]
id = "cliproxy"
kind = "vllm"
base_url = "http://{address}/v1"
trust_zone = "local"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = false
function_tools = false
reasoning_control = true
[models.chat.claude]
adapter_id = "cliproxy"
upstream_id = "installed-claude"
default_effort = "medium"
efforts = ["low", "medium", "high"]
[keys]
file = "keys.toml"
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
{marker}{key}"#,
        marker = scratch::KEYS_FILE_MARKER,
        key = scratch::key_record(
            "family-client",
            scratch::TEST_KEY,
            &format!("permissions = [{grants}]")
        ),
    )
}

#[test]
fn non_account_reasoning_control_adapters_accept_families_and_pins() {
    let source = vllm_source("127.0.0.1:9", &["claude"]);
    let config = load(&source).unwrap();
    let routes: Vec<_> = config
        .routes()
        .iter()
        .map(|route| {
            (
                route.identity().route_id.as_str(),
                route.identity().selector.model_alias.0.as_str(),
                route.pinned_reasoning_effort(),
            )
        })
        .collect();
    assert_eq!(
        routes,
        vec![
            ("claude", "claude", Some("medium")),
            ("claude-low", "claude:low", Some("low")),
            ("claude-medium", "claude:medium", Some("medium")),
            ("claude-high", "claude:high", Some("high")),
        ]
    );
    let pinned = source.replace(
        "default_effort = \"medium\"\nefforts = [\"low\", \"medium\", \"high\"]",
        "reasoning_effort = \"high\"",
    );
    let config = load(&pinned).unwrap();
    assert_eq!(config.routes()[0].pinned_reasoning_effort(), Some("high"));
    for (text, error) in [
        (
            pinned.replace("reasoning_control = true", "reasoning_control = false"),
            "reasoning_effort: unsupported_by_adapter",
        ),
        (
            source.replace("reasoning_control = true", "reasoning_control = false"),
            "reasoning_chat_family_required",
        ),
    ] {
        let message = load(&text).unwrap_err().to_string();
        assert!(message.ends_with(error), "{message}");
    }
}

async fn vllm_call(permissions: &[&str], body: Value) -> (StatusCode, Value, Option<Value>) {
    let mut mock =
        upstream::MockServer::once(upstream::ResponseSpec::json(upstream::TEXT_RESPONSE)).await;
    let config = load(&vllm_source(&mock.address, permissions)).unwrap();
    let adapter =
        kanata::adapter::vllm::VllmAdapter::from_config(&config, "cliproxy", "claude").unwrap();
    let server = kanata::server::TwoPlaneServer::from_validated_with_adapters(
        &config,
        kanata::server::Readiness::new(true),
        vec![std::sync::Arc::new(adapter)],
    )
    .unwrap();
    let response = server
        .client_oneshot(chat_request(&body.to_string()))
        .await
        .unwrap();
    let status = response.status();
    let output = response_json(response).await;
    if status != StatusCode::OK {
        return (status, output, None);
    }
    mock.finish().await;
    let sent = serde_json::from_slice(&mock.requests.lock().unwrap()[0].body).unwrap();
    (status, output, Some(sent))
}

#[tokio::test]
async fn vllm_families_advertise_scoped_efforts_and_forward_the_selected_effort() {
    let config = load(&vllm_source("127.0.0.1:9", &["claude", "claude:high"])).unwrap();
    let server = kanata::server::TwoPlaneServer::from_validated_with_adapters(
        &config,
        kanata::server::Readiness::new(true),
        vec![std::sync::Arc::new(
            kanata::adapter::vllm::VllmAdapter::from_config(&config, "cliproxy", "claude").unwrap(),
        )],
    )
    .unwrap();
    let models = response_json(server.client_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "claude");
    assert_eq!(models["data"][0]["kanata"]["reasoning_control"], true);
    assert_eq!(
        models["data"][0]["kanata"]["reasoning_efforts"],
        json!(["medium", "high"])
    );
    let public = response_json(server.public_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(public["data"][0]["id"], "claude");
    assert_eq!(
        public["data"][0]["kanata"]["reasoning_efforts"],
        json!(["medium"])
    );

    let message = json!([{"role":"user","content":"fixture"}]);
    for (permissions, model, effort, upstream_effort) in [
        (&["claude:high"][..], "claude", Some("high"), "high"),
        (&["claude"][..], "claude", None, "medium"),
        (&["claude:low"][..], "claude:low", None, "low"),
    ] {
        let mut body = json!({"model": model, "messages": message});
        if let Some(effort) = effort {
            body["reasoning_effort"] = json!(effort);
        }
        let (status, output, sent) = vllm_call(permissions, body).await;
        assert_eq!(status, StatusCode::OK, "{model} {effort:?}: {output}");
        assert_eq!(output["model"], "claude");
        let sent = sent.unwrap();
        assert_eq!(sent["model"], "installed-claude");
        assert_eq!(sent["reasoning_effort"], upstream_effort);
        assert!(sent.get("chat_template_kwargs").is_none());
    }
    for (permissions, model, effort, status) in [
        (
            &["claude", "claude:high"][..],
            "claude",
            Some("xhigh"),
            StatusCode::BAD_REQUEST,
        ),
        (
            &["claude:high"][..],
            "claude",
            None,
            StatusCode::BAD_REQUEST,
        ),
        (
            &["claude:high"][..],
            "claude",
            Some("low"),
            StatusCode::FORBIDDEN,
        ),
        (
            &["claude:high"][..],
            "claude:high",
            Some("low"),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let mut body = json!({"model": model, "messages": message});
        if let Some(effort) = effort {
            body["reasoning_effort"] = json!(effort);
        }
        let (actual, output, _) = vllm_call(permissions, body).await;
        assert_eq!(actual, status, "{model} {effort:?}: {output}");
        if status == StatusCode::BAD_REQUEST {
            assert_eq!(output["error"]["param"], "reasoning_effort");
        }
    }
}
