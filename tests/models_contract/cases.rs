use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use serde_json::Value;

use crate::support::{
    adapter_spec, capabilities, chat_outcome, config, config_with_public_routes, load_text,
    models_request, response_body, response_json, server_with, server_without_adapters,
    try_server_with,
};

fn ids(value: &Value) -> Vec<&str> {
    value["data"]
        .as_array()
        .expect("model data")
        .iter()
        .map(|model| model["id"].as_str().expect("model id"))
        .collect()
}

#[tokio::test]
async fn models_are_sorted_deduplicated_authorized_and_bound() {
    let config = config();
    let capabilities = capabilities(&config, "vllm-private");
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities,
            chat_outcome("unused"),
        )],
    );
    let response = server
        .client_oneshot(models_request())
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let value = response_json(response).await;
    assert_eq!(value["object"], "list");
    assert_eq!(
        ids(&value),
        vec!["private-chat", "private-transcribe"],
        "unbound routes must not be advertised"
    );
    for model in value["data"].as_array().expect("models") {
        assert_eq!(model["object"], "model");
        assert_eq!(model["created"], 0);
        assert_eq!(model["owned_by"], "kanata");
    }
}

#[tokio::test]
async fn models_keep_exact_key_permissions_with_a_bound_adapter() {
    let old = "permissions = [\n  { model_alias = \"local-chat\", operation = \"chat\" },\n  { model_alias = \"private-chat\", operation = \"chat\" },\n  { model_alias = \"private-transcribe\", operation = \"transcription\" },\n  { model_alias = \"remote-chat\", operation = \"chat\" },\n  { model_alias = \"chatgpt-chat\", operation = \"chat\" },\n]";
    let config = load_variant(&[(
        old,
        "permissions = [{ model_alias = \"private-chat\", operation = \"chat\" }]",
    )]);

    let capabilities = capabilities(&config, "vllm-private");
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities,
            chat_outcome("unused"),
        )],
    );
    let response = server
        .client_oneshot(models_request())
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(ids(&response_json(response).await), vec!["private-chat"]);
}

#[tokio::test]
async fn models_only_constructor_hides_unbound_routes() {
    let server = server_without_adapters(&config());
    let response = server
        .client_oneshot(models_request())
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(ids(&response_json(response).await).is_empty());
}

#[tokio::test]
async fn public_models_default_empty_and_intersect_exact_public_routes() {
    let config = config_with_public_routes(&[]);
    let capabilities = capabilities(&config, "vllm-private");
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities.clone(),
            chat_outcome("unused"),
        )],
    );
    let private = server
        .client_oneshot(models_request())
        .await
        .expect("private models response");
    assert_eq!(private.status(), StatusCode::OK);
    assert_eq!(
        ids(&response_json(private).await),
        ["private-chat", "private-transcribe"]
    );

    let public = server
        .public_oneshot(models_request())
        .await
        .expect("configured public router");
    assert_eq!(public.status(), StatusCode::OK);
    assert!(ids(&response_json(public).await).is_empty());

    let config = config_with_public_routes(&[("private-chat", "chat")]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities,
            chat_outcome("unused"),
        )],
    );
    let public = server
        .public_oneshot(models_request())
        .await
        .expect("configured public router");
    assert_eq!(public.status(), StatusCode::OK);
    assert_eq!(ids(&response_json(public).await), ["private-chat"]);

    let config = config_with_public_routes(&[("private-transcribe", "transcription")]);
    let asr_capabilities = crate::support::capabilities(&config, "vllm-private");
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            asr_capabilities,
            chat_outcome("unused"),
        )],
    );
    let public = server
        .public_oneshot(models_request())
        .await
        .expect("configured public router");
    assert_eq!(public.status(), StatusCode::OK);
    assert_eq!(ids(&response_json(public).await), ["private-transcribe"]);
}

#[test]
fn duplicate_missing_and_mismatched_adapter_bindings_fail_closed() {
    let config = config();
    let capabilities = capabilities(&config, "vllm-private");
    assert!(
        try_server_with(
            &config,
            vec![
                adapter_spec("vllm-private", capabilities.clone(), chat_outcome("unused")),
                adapter_spec("vllm-private", capabilities.clone(), chat_outcome("unused")),
            ],
        )
        .is_err()
    );
    assert!(
        try_server_with(
            &config,
            vec![adapter_spec(
                "missing-adapter",
                capabilities.clone(),
                chat_outcome("unused")
            )],
        )
        .is_err()
    );

    let mut mismatched = capabilities;
    mismatched.function_tools = false;
    assert!(
        try_server_with(
            &config,
            vec![adapter_spec(
                "vllm-private",
                mismatched,
                chat_outcome("unused")
            )],
        )
        .is_err()
    );
}

/// The fixture, keys file records included, with every `replacements` pair applied.
fn load_variant(replacements: &[(&str, &str)]) -> kanata::config::ValidatedConfig {
    let mut contents = crate::scratch::example_with_keys_text();
    for (old, new) in replacements {
        assert!(contents.contains(old), "fixture contains {old}");
        contents = contents.replace(old, new);
    }
    load_text(&contents).expect("variant config")
}

fn entry<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["data"]
        .as_array()
        .expect("model data")
        .iter()
        .find(|model| model["id"] == id)
        .unwrap_or_else(|| panic!("model {id} listed"))
}

#[tokio::test]
async fn models_report_kanata_capabilities_per_alias() {
    let config = load_variant(&[
        (
            "trust_zone = \"local\"\n[adapters.capabilities]\noperations = [\"chat\"]\n",
            "trust_zone = \"local\"\n[adapters.capabilities]\noperations = [\"chat\"]\nstructured_output = true\nsampling_controls = true\nreasoning_control = true\n",
        ),
        (
            "api.openai.com/v1\"\ntrust_zone = \"external\"\n[adapters.capabilities]\n",
            "api.openai.com/v1\"\ntrust_zone = \"external\"\n[adapters.capabilities]\nreasoning_control = true\n",
        ),
        (
            "upstream_id = \"gpt-5\"\n",
            "upstream_id = \"gpt-5\"\nreasoning_effort = \"medium\"\n",
        ),
        (
            "upstream_id = \"llama3.2:latest\"\n",
            "upstream_id = \"llama3.2:latest\"\ncontext_tokens = 16384\nmax_output_tokens = 4096\n",
        ),
    ]);
    let specs = ["ollama-local", "chatgpt-private"]
        .into_iter()
        .map(|id| adapter_spec(id, capabilities(&config, id), chat_outcome("unused")))
        .collect();
    let (server, _) = server_with(&config, specs);
    let value = response_json(
        server
            .client_oneshot(models_request())
            .await
            .expect("response"),
    )
    .await;
    assert_eq!(ids(&value), ["chatgpt-chat", "local-chat"]);
    assert_eq!(
        entry(&value, "local-chat")["kanata"],
        serde_json::json!({
            "operations": ["chat"], "structured_output": true, "sampling_controls": true,
            "reasoning_control": true, "function_tools": true, "streaming": true,
            "input_audio": false, "input_images": false, "images": null, "trust_zone": "local", "reasoning_efforts": null,
            "context_tokens": 16384, "max_output_tokens": 4096,
            "responses": {"endpoint":"/v1/responses","operation":"chat","stateless":true,"input":["text"],"function_tools":true,"streaming":true,"max_output_bytes":8388608},
            "messages": {"endpoint":"/v1/messages","operation":"chat","stateless":true,"function_tools":true,"streaming":true},
            "admission": {
                "max_in_flight": 8, "max_queue": 32, "queue_ms": 1000,
                "adapter_max_in_flight": null,
                "in_flight": 0, "queued": 0, "adapter_in_flight": null, "breaker": "closed"
            }
        })
    );
    assert_eq!(
        entry(&value, "chatgpt-chat")["kanata"],
        serde_json::json!({
            "operations": ["chat"], "structured_output": false, "sampling_controls": false,
            "reasoning_control": true, "function_tools": true, "streaming": true,
            "input_audio": false, "input_images": false, "images": null, "trust_zone": "external",
            "reasoning_efforts": ["medium"], "context_tokens": null,
            "max_output_tokens": null,
            "responses": {"endpoint":"/v1/responses","operation":"chat","stateless":true,"input":["text"],"function_tools":true,"streaming":true,"max_output_bytes":8388608},
            "messages": {"endpoint":"/v1/messages","operation":"chat","stateless":true,"function_tools":true,"streaming":true},
            "admission": {
                "max_in_flight": 8, "max_queue": 32, "queue_ms": 1000,
                "adapter_max_in_flight": null,
                "in_flight": 0, "queued": 0, "adapter_in_flight": null, "breaker": "closed"
            }
        })
    );
}

#[tokio::test]
async fn models_merge_operations_for_one_alias() {
    let config = load_variant(&[
        (
            "model_alias = \"private-transcribe\"\noperation",
            "model_alias = \"private-chat\"\noperation",
        ),
        (
            "{ model_alias = \"private-transcribe\", operation = \"transcription\" }",
            "{ model_alias = \"private-chat\", operation = \"transcription\" }",
        ),
    ]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities(&config, "vllm-private"),
            chat_outcome("unused"),
        )],
    );
    let value = response_json(
        server
            .client_oneshot(models_request())
            .await
            .expect("response"),
    )
    .await;
    assert_eq!(ids(&value), ["private-chat"]);
    let kanata = &entry(&value, "private-chat")["kanata"];
    assert_eq!(
        kanata["operations"],
        serde_json::json!(["chat", "transcription"])
    );
    assert_eq!(kanata["trust_zone"], "private_network");
    assert_eq!(kanata["streaming"], true);
}

#[tokio::test]
async fn public_models_carry_same_kanata_object_without_admission() {
    let config = config_with_public_routes(&[("private-chat", "chat")]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities(&config, "vllm-private"),
            chat_outcome("unused"),
        )],
    );
    let private = response_json(
        server
            .client_oneshot(models_request())
            .await
            .expect("private"),
    )
    .await;
    let public = response_json(
        server
            .public_oneshot(models_request())
            .await
            .expect("public router"),
    )
    .await;
    assert_eq!(ids(&public), ["private-chat"]);
    let mut private_kanata = entry(&private, "private-chat")["kanata"].clone();
    assert!(
        private_kanata
            .as_object_mut()
            .expect("kanata object")
            .remove("admission")
            .is_some()
    );
    assert_eq!(entry(&public, "private-chat")["kanata"], private_kanata);
}

fn model_request(method: Method, path: &str, key: Option<&str>) -> Request<Body> {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    request.body(Body::empty()).expect("request")
}

fn get_model(id: &str) -> Request<Body> {
    model_request(Method::GET, &format!("/v1/models/{id}"), Some("test-key"))
}

/// Status and body; the per-request `x-request-id` header is left out.
async fn status_and_body(response: axum::response::Response) -> (StatusCode, Vec<u8>) {
    (response.status(), response_body(response).await.to_vec())
}

#[tokio::test]
async fn retrieve_returns_the_list_entry_and_hides_unknown_and_unauthorized_ids() {
    let old = "permissions = [\n  { model_alias = \"local-chat\", operation = \"chat\" },\n  { model_alias = \"private-chat\", operation = \"chat\" },\n  { model_alias = \"private-transcribe\", operation = \"transcription\" },\n  { model_alias = \"remote-chat\", operation = \"chat\" },\n  { model_alias = \"chatgpt-chat\", operation = \"chat\" },\n]";
    let config = load_variant(&[(
        old,
        "permissions = [{ model_alias = \"private-chat\", operation = \"chat\" }]",
    )]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities(&config, "vllm-private"),
            chat_outcome("unused"),
        )],
    );
    let list = response_json(server.client_oneshot(models_request()).await.unwrap()).await;
    let response = server
        .client_oneshot(get_model("private-chat"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(&response_json(response).await, entry(&list, "private-chat"));

    let unknown = status_and_body(
        server
            .client_oneshot(get_model("no-such-model"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND);
    let value: Value = serde_json::from_slice(&unknown.1).expect("error body");
    assert_eq!(value["error"]["code"], "not_found");
    assert_eq!(value["error"]["type"], "invalid_request_error");
    // Bound but not permitted; configured but neither permitted nor bound.
    for id in ["private-transcribe", "local-chat"] {
        let hidden = status_and_body(server.client_oneshot(get_model(id)).await.unwrap()).await;
        assert_eq!(hidden, unknown, "{id}");
    }
}

#[tokio::test]
async fn public_retrieve_intersects_public_routes_and_needs_a_key() {
    let config = config_with_public_routes(&[("private-chat", "chat")]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities(&config, "vllm-private"),
            chat_outcome("unused"),
        )],
    );
    let list = response_json(server.public_oneshot(models_request()).await.unwrap()).await;
    let response = server
        .public_oneshot(get_model("private-chat"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = response_json(response).await;
    assert_eq!(&value, entry(&list, "private-chat"));
    assert!(value["kanata"].get("admission").is_none());

    let unknown = status_and_body(
        server
            .public_oneshot(get_model("no-such-model"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND);
    let private_only = status_and_body(
        server
            .public_oneshot(get_model("private-transcribe"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(private_only, unknown);

    let list_without_key = status_and_body(
        server
            .public_oneshot(model_request(Method::GET, "/v1/models", None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list_without_key.0, StatusCode::FORBIDDEN);
    for id in ["private-chat", "no-such-model"] {
        let path = format!("/v1/models/{id}");
        for key in [None, Some("wrong-key")] {
            let rejected = status_and_body(
                server
                    .public_oneshot(model_request(Method::GET, &path, key))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(rejected, list_without_key, "{path} {key:?}");
        }
    }
}

#[tokio::test]
async fn retrieve_matches_list_reasoning_families() {
    let low_route = "[[routes]]\nid = \"chatgpt-low\"\nmodel_alias = \"chatgpt-chat:low\"\noperation = \"chat\"\nadapter_id = \"chatgpt-private\"\nupstream_id = \"gpt-5\"\nreasoning_effort = \"low\"\nrequires_streaming_chat = true\nrequires_function_tools = true\n\n";
    let high_route = low_route.replace("low", "high");
    let config = load_variant(&[
        (
            "api.openai.com/v1\"\ntrust_zone = \"external\"\n[adapters.capabilities]\n",
            "api.openai.com/v1\"\ntrust_zone = \"external\"\n[adapters.capabilities]\nreasoning_control = true\n",
        ),
        (
            "upstream_id = \"gpt-5\"\n",
            "upstream_id = \"gpt-5\"\nreasoning_effort = \"medium\"\n",
        ),
        ("\n[keys]\n", &format!("\n{low_route}{high_route}[keys]\n")),
        (
            "{ model_alias = \"chatgpt-chat\", operation = \"chat\" },\n",
            "{ model_alias = \"chatgpt-chat\", operation = \"chat\" },\n  { model_alias = \"chatgpt-chat:low\", operation = \"chat\" },\n",
        ),
    ]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "chatgpt-private",
            capabilities(&config, "chatgpt-private"),
            chat_outcome("unused"),
        )],
    );
    let list = response_json(server.client_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(ids(&list), ["chatgpt-chat"]);
    let base = response_json(
        server
            .client_oneshot(get_model("chatgpt-chat"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(&base, entry(&list, "chatgpt-chat"));
    assert_eq!(
        base["kanata"]["reasoning_efforts"],
        serde_json::json!(["low", "medium"])
    );
    let unknown = status_and_body(
        server
            .client_oneshot(get_model("no-such-model"))
            .await
            .unwrap(),
    )
    .await;
    for id in [
        "chatgpt-chat:low",
        "chatgpt-chat:high",
        "chatgpt-chat:medium",
    ] {
        let suffixed = status_and_body(server.client_oneshot(get_model(id)).await.unwrap()).await;
        assert_eq!(suffixed, unknown, "{id}");
    }
}

#[tokio::test]
async fn retrieve_answers_head_and_other_methods_like_the_list() {
    let config = config_with_public_routes(&[("private-chat", "chat")]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities(&config, "vllm-private"),
            chat_outcome("unused"),
        )],
    );
    for public in [false, true] {
        for method in [Method::HEAD, Method::POST, Method::DELETE] {
            let send = |path: &str| {
                let request = model_request(method.clone(), path, Some("test-key"));
                let server = &server;
                async move {
                    let response = if public {
                        server.public_oneshot(request).await.expect("public")
                    } else {
                        server.client_oneshot(request).await.expect("private")
                    };
                    let status = response.status();
                    let allow = response.headers().get(header::ALLOW).cloned();
                    let body = response_body(response).await;
                    (status, allow, body.is_empty())
                }
            };
            let list = send("/v1/models").await;
            let one = send("/v1/models/private-chat").await;
            assert_eq!(one, list, "{method} public={public}");
            if method == Method::HEAD {
                assert_eq!(one, (StatusCode::OK, None, true));
            } else {
                assert_eq!(one.0, StatusCode::METHOD_NOT_ALLOWED);
            }
        }
    }
}
