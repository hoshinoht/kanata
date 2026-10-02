#[path = "support/gateway.rs"]
mod gateway;
#[path = "adapter_ollama/support.rs"]
mod upstream;
use std::sync::Arc;
use upstream::{MockServer, ResponseSpec, TEXT_RESPONSE, TEXT_STREAM};

#[tokio::test]
async fn default_output_cap_reaches_upstream_for_complete_and_streaming_requests() {
    for stream in [false, true] {
        let response = if stream {
            ResponseSpec::event_stream(TEXT_STREAM, 47)
        } else {
            ResponseSpec::json(TEXT_RESPONSE)
        };
        let mut mock = MockServer::once(response).await;
        let original = include_str!("fixtures/config/example.toml");
        let contents = original
            .replace(
                "http://ollama.invalid:11434",
                &format!("http://{}", mock.address),
            )
            .replacen(
                "function_tools = true\n",
                "function_tools = true\nsampling_controls = true\n",
                1,
            )
            .replace(
                "upstream_id = \"llama3.2:latest\"",
                "upstream_id = \"llama3.2:latest\"\nmax_output_tokens = 64",
            );
        assert_ne!(original, contents);
        let path = std::env::temp_dir().join(format!(
            "kanata-cap-{:016x}.toml",
            getrandom::u64().expect("random")
        ));
        std::fs::write(&path, contents).expect("config");
        let config = kanata::config::load(&path).expect("valid config");
        std::fs::remove_file(path).expect("remove fixture");
        let server = kanata::server::TwoPlaneServer::from_validated_with_adapters(
            &config,
            &gateway::Resolver,
            kanata::server::Readiness::new(true),
            vec![Arc::new(upstream::adapter(&config))],
        )
        .expect("server");
        let body = serde_json::json!({"model":"local-chat","messages":[{"role":"user","content":"hello"}],"stream":stream}).to_string();
        let response = server
            .client_oneshot(gateway::chat_request(&body))
            .await
            .expect("response");
        assert_eq!(response.status(), http::StatusCode::OK);
        gateway::response_body(response).await;
        mock.finish().await;
        let sent = mock.requests.lock().expect("requests");
        let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("payload");
        assert_eq!(body["max_tokens"], 64);
        assert_eq!(body["stream"], stream);
    }
}
