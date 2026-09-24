use std::{fs, sync::Arc};

use axum::body::{Body, to_bytes};
use http::{Request, StatusCode};
use kanata::{
    adapter::ollama::OllamaAdapter,
    auth::{SecretResolutionError, SecretResolver},
    config::{self, SecretReference},
    server::{Readiness, TwoPlaneServer},
};
use serde_json::{Value, json};

struct SmokeKey;

impl SecretResolver for SmokeKey {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"local-smoke-only".to_vec())
    }
}

#[tokio::test]
#[ignore = "requires explicit authorization and local Ollama qwen3:0.6b"]
async fn local_qwen_chat_and_stream_through_gateway() {
    let contents = include_str!("../tests/fixtures/config/example.toml")
        .replace("http://ollama.invalid:11434", "http://127.0.0.1:11434/v1")
        .replace("llama3.2:latest", "qwen3:0.6b")
        .replacen("function_tools = true", "function_tools = false", 1)
        .replacen(
            "requires_function_tools = true",
            "requires_function_tools = false",
            1,
        )
        .replace("headers_ms = 10000", "headers_ms = 60000")
        .replace("first_byte_ms = 15000", "first_byte_ms = 60000")
        .replace("overall_ms = 60000", "overall_ms = 120000");
    let path = std::env::temp_dir().join(format!("kanata-local-smoke-{}.toml", std::process::id()));
    fs::write(&path, contents).expect("write synthetic smoke config");
    let loaded = config::load(&path);
    fs::remove_file(path).expect("remove synthetic smoke config");
    let config = loaded.expect("validate smoke config");
    assert_eq!(
        config.adapters()[0].base_url().as_str(),
        "http://127.0.0.1:11434/v1"
    );
    assert_eq!(config.routes()[0].identity().upstream_id, "qwen3:0.6b");
    let adapter = OllamaAdapter::new(&config.adapters()[0], config.timeouts(), config.limits())
        .expect("construct local adapter");
    let gateway = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &SmokeKey,
        Readiness::new(true),
        vec![Arc::new(adapter)],
    )
    .expect("construct gateway");

    for stream in [false, true] {
        let mut payload = json!({
            "model": "local-chat",
            "messages": [{"role": "user", "content": "Reply only OK. /no_think"}],
            "stream": stream
        });
        if stream {
            payload["stream_options"] = json!({"include_usage": true});
        }
        let request = Request::post("/v1/chat/completions")
            .header("authorization", "Bearer local-smoke-only")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .expect("build smoke request");
        let response = gateway
            .client_oneshot(request)
            .await
            .expect("gateway response");
        assert_eq!(response.status(), StatusCode::OK, "local inference status");
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("bounded smoke response");
        if !stream {
            let value: Value = serde_json::from_slice(&bytes).expect("completion JSON");
            assert_eq!(value["model"], "local-chat");
            assert_eq!(value["choices"][0]["finish_reason"], "stop");
            assert!(
                value["choices"][0]["message"]["content"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
            );
        } else {
            let text = std::str::from_utf8(&bytes).expect("SSE UTF-8");
            let mut done = 0;
            let mut has_text = false;
            let mut has_finish = false;
            for line in text.lines().filter_map(|line| line.strip_prefix("data: ")) {
                if line == "[DONE]" {
                    done += 1;
                    continue;
                }
                let value: Value = serde_json::from_str(line).expect("SSE JSON");
                assert!(
                    value.get("error").is_none(),
                    "stream must not emit an error"
                );
                assert_eq!(value["model"], "local-chat");
                has_text |= value["choices"][0]["delta"]["content"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty());
                has_finish |= value["choices"][0]["finish_reason"] == "stop";
            }
            assert_eq!(done, 1, "one successful stream terminator");
            assert!(has_text && has_finish, "stream text and finish");
        }
    }
}
