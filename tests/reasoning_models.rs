#[path = "support/gateway.rs"]
mod support;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use support::{
    adapter_spec, capabilities, chat_outcome, chat_request, models_request, response_json,
    server_with,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn source(permissions: &[&str]) -> String {
    let mut text = std::fs::read_to_string("tests/fixtures/config/example.toml").unwrap();
    text = text.replace("codex-chat", "reasoning-model");
    let marker = "id = \"codex-private\"";
    let start = text.find(marker).unwrap();
    let insert = text[start..].find("function_tools = true").unwrap() + start;
    text.insert_str(insert, "reasoning_control = true\n");
    let mut variants = String::new();
    for effort in ["low", "medium", "high"] {
        variants.push_str(&format!(
            r#"
[[routes]]
id = "reasoning-{effort}"
model_alias = "reasoning-model:{effort}"
operation = "chat"
adapter_id = "codex-private"
upstream_id = "gpt-5-codex"
codex_reasoning_effort = "{effort}"
requires_streaming_chat = true
requires_function_tools = true

"#
        ));
    }
    let insert = text.find("[[application_keys]]").unwrap();
    text.insert_str(insert, &variants);
    let start = text.find("permissions = [").unwrap();
    let end = text[start..].find("\n]").unwrap() + start + 2;
    let grants = permissions
        .iter()
        .map(|alias| format!("{{ model_alias = \"{alias}\", operation = \"chat\" }}"))
        .collect::<Vec<_>>()
        .join(", ");
    text.replace_range(start..end, &format!("permissions = [{grants}]"));
    text = text.replace(
        "[listeners.admin]",
        "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
    );
    text
}

fn load(text: &str) -> Result<kanata::config::ValidatedConfig, kanata::config::ConfigError> {
    let path = std::env::temp_dir().join(format!(
        "kanata-reasoning-{}-{}.toml",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, text).unwrap();
    let result = kanata::config::load(&path);
    std::fs::remove_file(path).unwrap();
    result
}

fn server(permissions: &[&str]) -> support::ServerWithRequests {
    let config = load(&source(permissions)).unwrap();
    let specs = vec![adapter_spec(
        "codex-private",
        capabilities(&config, "codex-private"),
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
        "upstream_id = \"different-upstream\"\ncodex_reasoning_effort = \"low\"",
        "upstream_id = \"gpt-5-codex\"\ncontext_tokens = 4096\ncodex_reasoning_effort = \"low\"",
        "upstream_id = \"gpt-5-codex\"\ncodex_reasoning_summary = \"auto\"\ncodex_reasoning_effort = \"low\"",
    ] {
        let text = base.replace(
            "upstream_id = \"gpt-5-codex\"\ncodex_reasoning_effort = \"low\"",
            replacement,
        );
        let error = load(&text).expect_err("inconsistent family");
        assert!(
            error.to_string().contains("inconsistent_reasoning_family"),
            "{error}"
        );
    }
}

struct StreamAdapter {
    caps: kanata::core::Capabilities,
    wrong_model: bool,
}

impl kanata::adapter::Adapter for StreamAdapter {
    fn id(&self) -> &str {
        "codex-private"
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
            caps: capabilities(&config, "codex-private"),
            wrong_model,
        };
        let server = kanata::server::TwoPlaneServer::from_validated_with_adapters(
            &config,
            &support::Resolver,
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
    let end = text[start..].find("[[application_keys]]").unwrap() + start;
    text.replace_range(start..end, "");
    let config = load(&text).unwrap();
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "codex-private",
            capabilities(&config, "codex-private"),
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

#[tokio::test]
async fn disabled_reasoning_control_preserves_exact_legacy_discovery_and_calls() {
    let config = load(
        &source(&["reasoning-model:low"])
            .replace("reasoning_control = true", "reasoning_control = false")
            .replace(
                "upstream_id = \"gpt-5-codex\"\ncodex_reasoning_effort = \"low\"",
                "upstream_id = \"legacy-low-upstream\"\ncodex_reasoning_effort = \"low\"",
            ),
    )
    .unwrap();
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "codex-private",
            capabilities(&config, "codex-private"),
            chat_outcome("fixture"),
        )],
    );
    let models = response_json(server.client_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(models["data"][0]["id"], "reasoning-model:low");
    assert_eq!(models["data"][0]["kanata"]["reasoning_control"], false);
    assert_eq!(
        models["data"][0]["kanata"]["reasoning_efforts"],
        Value::Null
    );
    let response = server.client_oneshot(chat_request(&json!({"model":"reasoning-model:low","messages":[{"role":"user","content":"fixture"}]}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await["model"],
        "reasoning-model:low"
    );
    let response = server.client_oneshot(chat_request(&json!({"model":"reasoning-model","messages":[{"role":"user","content":"fixture"}],"reasoning_effort":"low"}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

fn chatgpt_source(permissions: &[&str]) -> String {
    let mut text = source(permissions)
        .replace("[codex_auth]\nstore = \"keyring\"", "[chatgpt_auth]")
        .replace("kind = \"codex\"", "kind = \"chatgpt\"")
        .replace(
            "https://chatgpt.invalid/backend-api/codex",
            "https://api.openai.com/v1",
        )
        .replace("codex_reasoning_effort =", "reasoning_effort =")
        .replace(
            "model_alias = \"reasoning-model\"\noperation",
            "model_alias = \"reasoning-model\"\nreasoning_effort = \"medium\"\noperation",
        );
    let mut variants = String::new();
    for effort in ["none", "xhigh", "max"] {
        variants.push_str(&format!(
            r#"
[[routes]]
id = "reasoning-{effort}"
model_alias = "reasoning-model:{effort}"
reasoning_effort = "{effort}"
operation = "chat"
adapter_id = "codex-private"
upstream_id = "gpt-5-codex"
requires_streaming_chat = true
requires_function_tools = true

"#
        ));
    }
    let insert = text.find("[[application_keys]]").unwrap();
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
            "codex-private",
            capabilities(&config, "codex-private"),
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
                "codex-private",
                capabilities(&config, "codex-private"),
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
