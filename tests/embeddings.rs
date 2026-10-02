#[path = "support/gateway.rs"]
mod gateway;
#[path = "adapter_ollama/support.rs"]
mod upstream;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use kanata::{
    adapter::{Adapter, AdapterFuture, AdapterOutput, ollama::OllamaAdapter},
    config::ValidatedConfig,
    core::{
        Capabilities, EmbeddingResponse, Request as CoreRequest, Response as CoreResponse,
        RoutedRequest, Usage,
    },
    server::{Readiness, TwoPlaneServer},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use upstream::{MockServer, ResponseSpec};

fn config(address: &str, public: bool, permission: bool) -> ValidatedConfig {
    let original = include_str!("fixtures/config/example.toml");
    let mut contents = original
        .replace(
            "http://ollama.invalid:11434",
            &format!("http://{address}/v1"),
        )
        .replacen(
            "operations = [\"chat\"]",
            "operations = [\"chat\", \"embeddings\"]",
            1,
        )
        .replace("owner = true", "owner = false")
        .replace(
            "  { model_alias = \"codex-chat\", operation = \"chat\" },\n",
            "",
        );
    if permission {
        contents = contents.replace(
            "permissions = [",
            "permissions = [\n  { model_alias = \"local-embed\", operation = \"embeddings\" },",
        );
    }
    if public {
        contents = contents.replace("[publication]", "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[publication]\npublic_routes = [{ model_alias = \"local-embed\", operation = \"embeddings\" }]");
    }
    contents.push_str("\n[[routes]]\nid = \"ollama-embed\"\nmodel_alias = \"local-embed\"\noperation = \"embeddings\"\nadapter_id = \"ollama-local\"\nupstream_id = \"embedding-upstream\"\nrequires_streaming_chat = false\nrequires_function_tools = false\n");
    assert_ne!(original, contents);
    let path = std::env::temp_dir().join(format!(
        "kanata-embedding-{:016x}.toml",
        getrandom::u64().unwrap()
    ));
    std::fs::write(&path, contents).unwrap();
    let config = kanata::config::load(&path).expect("embedding config");
    std::fs::remove_file(path).unwrap();
    config
}

fn server(config: &ValidatedConfig, adapter: Arc<dyn Adapter>) -> TwoPlaneServer {
    TwoPlaneServer::from_validated_with_adapters(
        config,
        &gateway::Resolver,
        Readiness::new(true),
        vec![adapter],
    )
    .unwrap()
}

fn request(body: Value) -> Request<Body> {
    Request::post("/v1/embeddings")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-key")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn response() -> Value {
    json!({"object":"list","model":"embedding-upstream","data":[
        {"object":"embedding","index":1,"embedding":[0.5,-0.25]},
        {"object":"embedding","index":0,"embedding":[1.0,0.0]}
    ],"usage":{"prompt_tokens":7,"total_tokens":7}})
}

#[tokio::test]
async fn real_adapter_preserves_batch_order_encoding_usage_and_exact_route() {
    for encoding in ["float", "base64"] {
        let mut mock = MockServer::once(ResponseSpec::json(&response().to_string())).await;
        let config = config(&mock.address, false, true);
        let adapter =
            OllamaAdapter::new(&config.adapters()[0], config.timeouts(), config.limits()).unwrap();
        let server = server(&config, Arc::new(adapter));
        let result = server.client_oneshot(request(json!({"model":"local-embed","input":["first","second"],"dimensions":2,"encoding_format":encoding}))).await.unwrap();
        assert_eq!(result.status(), StatusCode::OK);
        let body = gateway::response_json(result).await;
        assert_eq!(body["model"], "local-embed");
        assert_eq!(body["usage"], json!({"prompt_tokens":7,"total_tokens":7}));
        assert_eq!(body["data"][0]["index"], 0);
        if encoding == "float" {
            assert_eq!(body["data"][0]["embedding"], json!([1.0, 0.0]));
        } else {
            let bytes = STANDARD
                .decode(body["data"][0]["embedding"].as_str().unwrap())
                .unwrap();
            assert_eq!(bytes, [1.0f32.to_le_bytes(), 0.0f32.to_le_bytes()].concat());
        }
        mock.finish().await;
        let records = mock.requests.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "/v1/embeddings");
        let sent: Value = serde_json::from_slice(&records[0].body).unwrap();
        assert_eq!(
            sent,
            json!({"model":"embedding-upstream","input":["first","second"],"dimensions":2,"encoding_format":"float"})
        );
    }
}

struct Recording {
    calls: Arc<AtomicUsize>,
    capabilities: Capabilities,
}

impl Adapter for Recording {
    fn id(&self) -> &str {
        "ollama-local"
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let CoreRequest::Embeddings(request) = routed.into_parts().1 else {
            panic!("embedding request")
        };
        let missing = request.input[0] == "missing";
        Box::pin(async move {
            Ok(AdapterOutput::Complete(CoreResponse::Embeddings(
                EmbeddingResponse {
                    model: request.model,
                    vectors: vec![vec![0.5, -0.5]; request.input.len()],
                    usage: (!missing).then_some(Usage {
                        input_tokens: 3,
                        output_tokens: 0,
                        total_tokens: 3,
                        reasoning_tokens: None,
                    }),
                },
            )))
        })
    }
}

fn recording(config: &ValidatedConfig) -> (TwoPlaneServer, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let adapter = Recording {
        calls: calls.clone(),
        capabilities: config.adapters()[0].capabilities().clone(),
    };
    (server(config, Arc::new(adapter)), calls)
}

#[tokio::test]
async fn malformed_inputs_and_unsupported_fields_never_dispatch() {
    let (server, calls) = recording(&config("127.0.0.1:9", false, true));
    for input in [
        json!(""),
        json!([]),
        json!(["ok", ""]),
        json!([1, 2]),
        json!(vec!["x"; 129]),
    ] {
        let response = server
            .client_oneshot(request(json!({"model":"local-embed","input":input})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            gateway::response_json(response).await["error"]["param"],
            "input"
        );
    }
    for extra in [
        json!({"dimensions":0}),
        json!({"dimensions":16385}),
        json!({"stream":true}),
        json!({"encoding_format":"invalid"}),
        json!({"user":"unforwarded"}),
    ] {
        let mut body = json!({"model":"local-embed","input":"hello"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(
            server.client_oneshot(request(body)).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn operation_scopes_and_public_allowlist_control_inference_and_discovery() {
    for permission in [false, true] {
        let (server, calls) = recording(&config("127.0.0.1:9", true, permission));
        let response = server
            .public_oneshot(request(json!({"model":"local-embed","input":"hello"})))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if permission {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            }
        );
        let models = gateway::response_json(
            server
                .public_oneshot(gateway::models_request())
                .await
                .unwrap(),
        )
        .await;
        if permission {
            assert_eq!(models["data"].as_array().unwrap().len(), 1);
            assert_eq!(
                models["data"][0]["kanata"]["operations"],
                json!(["embeddings"])
            );
            assert_eq!(models["data"][0]["kanata"]["embeddings"]["max_inputs"], 128);
            assert!(models["data"][0]["kanata"].get("admission").is_none());
        } else {
            assert_eq!(models["data"], json!([]));
        }
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(permission));
    }
    let (server, calls) = recording(&config("127.0.0.1:9", false, true));
    let response = server
        .client_oneshot(request(json!({"model":"local-chat","input":"hello"})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalid_upstream_vectors_indices_and_usage_are_rejected() {
    let base = response();
    let mut variants = Vec::new();
    let mut duplicate = base.clone();
    duplicate["data"][0]["index"] = json!(0);
    variants.push(duplicate);
    let mut missing = base.clone();
    missing["data"].as_array_mut().unwrap().pop();
    variants.push(missing);
    let mut length = base.clone();
    length["data"][0]["embedding"] = json!([1.0]);
    variants.push(length);
    let mut usage = base.clone();
    usage["usage"]["total_tokens"] = json!(8);
    variants.push(usage);
    let mut dimensions = base.clone();
    dimensions["data"][0]["embedding"] = json!(vec![0.0; 16385]);
    variants.push(dimensions);
    for variant in variants {
        let mut mock = MockServer::once(ResponseSpec::json(&variant.to_string())).await;
        let config = config(&mock.address, false, true);
        let adapter =
            OllamaAdapter::new(&config.adapters()[0], config.timeouts(), config.limits()).unwrap();
        let server = server(&config, Arc::new(adapter));
        let response = server
            .client_oneshot(request(
                json!({"model":"local-embed","input":["first","second"]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            gateway::response_json(response).await["error"]["code"],
            "upstream_failure"
        );
        mock.finish().await;
    }
}

#[tokio::test]
async fn cancellation_closes_upstream_and_releases_admission_and_input_reservations() {
    let mut spec = ResponseSpec::json(&response().to_string());
    spec.wait_for_close = true;
    let mut mock = MockServer::once(spec).await;
    let config = config(&mock.address, false, true);
    let adapter =
        OllamaAdapter::new(&config.adapters()[0], config.timeouts(), config.limits()).unwrap();
    let server = Arc::new(server(&config, Arc::new(adapter)));
    let worker = server.clone();
    let task = tokio::spawn(async move {
        worker
            .client_oneshot(request(
                json!({"model":"local-embed","input":["first","second"]}),
            ))
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        mock.wait_for_response_headers(),
    )
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(3), mock.wait_for_close())
        .await
        .unwrap();
    mock.finish().await;
    let metrics = server
        .admin_oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let metrics = String::from_utf8(gateway::response_body(metrics).await.to_vec()).unwrap();
    for expected in [
        "kanata_requests_inflight{endpoint=\"embeddings\"} 0",
        "kanata_reserved_request_bytes 0",
        "kanata_buffered_requests 0",
        "kanata_requests_queued 0",
    ] {
        assert!(metrics.contains(expected), "missing {expected}");
    }
    assert!(metrics.contains("endpoint=\"embeddings\",outcome=\"cancelled\""));
}

#[tokio::test]
async fn missing_auth_and_oversized_uploads_never_dispatch() {
    let (server, calls) = recording(&config("127.0.0.1:9", true, true));
    let mut no_auth = request(json!({"model":"local-embed","input":"hello"}));
    no_auth.headers_mut().remove("authorization");
    assert_eq!(
        server.public_oneshot(no_auth).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let mut too_large = request(json!({"model":"local-embed","input":"hello"}));
    too_large
        .headers_mut()
        .insert("content-length", "1048577".parse().unwrap());
    assert_eq!(
        server.client_oneshot(too_large).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reported_and_missing_embedding_usage_are_persisted() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "kanata-embedding-usage-{:016x}",
        getrandom::u64().unwrap()
    ));
    std::fs::create_dir(&dir).unwrap();
    std::fs::create_dir(dir.join("keys")).unwrap();
    std::fs::create_dir(dir.join("state")).unwrap();
    let digest = Sha256::digest(b"test-key");
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let key = format!(
        "version = 1\n[[keys]]\nid = \"search\"\ndigest = \"sha256:{hex}\"\ncreated_at = \"2026-01-01T00:00:00Z\"\npermissions = [{{ model_alias = \"local-embed\", operation = \"embeddings\" }}]\n"
    );
    let path = dir.join("keys/keys.toml");
    std::fs::write(&path, key).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let original = include_str!("../config/embeddings.example.toml");
    let contents = original.replace("ollama-embeddings", "ollama-local");
    assert_ne!(original, contents);
    std::fs::write(dir.join("config.toml"), contents).unwrap();
    let config = kanata::config::load(dir.join("config.toml")).unwrap();
    let (server, _) = recording(&config);
    let usage = server.open_usage(kanata::config::Plane::Private).unwrap();
    for input in ["reported", "missing"] {
        let response = server
            .client_oneshot(request(json!({"model":"local-embed","input":input})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = gateway::response_json(response).await;
        assert_eq!(body.get("usage").is_some(), input == "reported");
    }
    assert_eq!(
        usage.flush_now(),
        kanata::keys::usage::FlushOutcome::Written
    );
    let recorded = kanata::keys::usage::read_merged(&dir.join("state"));
    assert_eq!(recorded["search"].tokens.input_tokens, 3);
    assert_eq!(recorded["search"].tokens.output_tokens, 0);
    assert_eq!(recorded["search"].tokens.reported, 1);
    assert_eq!(recorded["search"].tokens.missing, 1);
    std::fs::remove_dir_all(dir).unwrap();
}
