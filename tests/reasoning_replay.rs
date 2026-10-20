//! Plaintext `reasoning_content` replayed on assistant chat turns: accepted on the wire,
//! forwarded as the assistant message's `reasoning` to an upstream that reads it (vLLM),
//! and dropped for one that does not (CLIProxyAPI).

#[path = "support/scratch.rs"]
mod scratch;
#[path = "support/gateway.rs"]
mod support;
#[path = "adapter_ollama/support.rs"]
mod upstream;

use std::sync::Arc;

use axum::http::StatusCode;
use kanata::{
    adapter::{Adapter, vllm::VllmAdapter},
    config::ValidatedConfig,
    server::{Readiness, TwoPlaneServer},
};
use serde_json::{Value, json};
use support::{chat_request, response_json};

const RESPONSE: &str = include_str!("fixtures/cliproxy/chat-text-response.json");
const REASONING: &str = "Need the file list first; call ls.";

fn load(kind: &str, base_url: &str) -> ValidatedConfig {
    let text = support::compact_config(
        "[]",
        &format!(
            r#"[[adapters]]
id = "upstream"
kind = "{kind}"
base_url = "{base_url}"
trust_zone = "external"
secret_ref = "env:UPSTREAM_KEY"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = false
function_tools = true
[models.chat.agent]
adapter_id = "upstream"
upstream_id = "gpt-oss-120b""#
        ),
        &scratch::key_record(
            "agent",
            scratch::TEST_KEY,
            "permissions = [{ model_alias = \"agent\", operation = \"chat\" }]",
        ),
    );
    support::load_text(&text).unwrap_or_else(|error| panic!("{kind} config: {error}"))
}

fn server(config: &ValidatedConfig) -> TwoPlaneServer {
    let adapter =
        VllmAdapter::from_config_with_secrets(config, "upstream", "agent", &support::Resolver)
            .expect("adapter");
    let adapters: Vec<Arc<dyn Adapter>> = vec![Arc::new(adapter)];
    TwoPlaneServer::from_validated_with_adapters(config, Readiness::new(true), adapters)
        .expect("server")
}

/// OMP's second turn after a gpt-oss tool call, replayed over `openai-completions`.
fn omp_turn_two(reasoning: &str) -> Value {
    json!({
        "model": "agent",
        "tools": [{"type": "function", "function": {"name": "ls", "parameters": {"type": "object"}}}],
        "messages": [
            {"role": "system", "content": "You are a coding agent."},
            {"role": "user", "content": "List the files."},
            {"role": "assistant", "content": "", "reasoning_content": reasoning, "tool_calls": [
                {"id": "call_ls", "type": "function", "function": {"name": "ls", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_ls", "content": "README.md"}
        ]
    })
}

/// Sends `body` and returns the single upstream payload it produced.
async fn sent(kind: &str, body: &Value) -> Value {
    let mut mock = upstream::MockServer::once(upstream::ResponseSpec::json(RESPONSE)).await;
    let config = load(kind, &format!("http://{}/v1", mock.address));
    let response = server(&config)
        .client_oneshot(chat_request(&body.to_string()))
        .await
        .expect("response");
    let status = response.status();
    let output = response_json(response).await;
    assert_eq!(status, StatusCode::OK, "{kind}: {output}");
    mock.finish().await;
    let record = mock.requests.lock().expect("requests")[0].clone();
    serde_json::from_slice(&record.body).expect("upstream JSON")
}

#[tokio::test]
async fn vllm_receives_replayed_reasoning_on_the_assistant_turn() {
    let sent = sent("vllm", &omp_turn_two(REASONING)).await;
    let messages = sent["messages"].as_array().expect("messages");
    let roles: Vec<_> = messages.iter().map(|message| &message["role"]).collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool"]);
    assert_eq!(
        messages[2],
        json!({"role": "assistant", "reasoning": REASONING, "tool_calls": [
            {"id": "call_ls", "type": "function", "function": {"name": "ls", "arguments": "{}"}}
        ]})
    );
    for message in [&messages[0], &messages[1], &messages[3]] {
        assert!(message.get("reasoning").is_none(), "{message}");
    }
    assert!(!sent.to_string().contains("reasoning_content"));
}

#[tokio::test]
async fn cliproxy_and_empty_replays_send_no_reasoning() {
    for (kind, reasoning) in [("cliproxy", REASONING), ("vllm", "")] {
        let sent = sent(kind, &omp_turn_two(reasoning)).await;
        let assistant = &sent["messages"][2];
        assert_eq!(assistant["role"], "assistant", "{kind}");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_ls", "{kind}");
        let text = sent.to_string();
        assert!(!text.contains("reasoning"), "{kind}: {text}");
        assert!(!text.contains(REASONING), "{kind}: {text}");
    }
}

#[tokio::test]
async fn reasoning_content_is_rejected_off_assistant_turns_and_over_the_cap() {
    let over_cap = "r".repeat(kanata::core::MAX_REASONING_BYTES + 1);
    let mut on_user = omp_turn_two(REASONING);
    on_user["messages"][1]["reasoning_content"] = json!("hidden");
    let mut on_tool = omp_turn_two(REASONING);
    on_tool["messages"][3]["reasoning_content"] = json!("hidden");
    let mut null_on_system = omp_turn_two(REASONING);
    null_on_system["messages"][0]["reasoning_content"] = Value::Null;
    let mut not_string = omp_turn_two(REASONING);
    not_string["messages"][2]["reasoning_content"] = json!(["hidden"]);
    let cases = [
        ("user", on_user, "messages[1].reasoning_content"),
        ("tool", on_tool, "messages[3].reasoning_content"),
        (
            "system-null",
            null_on_system,
            "messages[0].reasoning_content",
        ),
        (
            "over-cap",
            omp_turn_two(&over_cap),
            "messages[2].reasoning_content",
        ),
        ("not-string", not_string, "messages[2].reasoning_content"),
    ];
    // Port 9 is never reached: every case fails before dispatch.
    let config = load("vllm", "http://127.0.0.1:9/v1");
    for (name, body, param) in cases {
        let response = server(&config)
            .client_oneshot(chat_request(&body.to_string()))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        let error = response_json(response).await;
        assert_eq!(error["error"]["param"], param, "{name}: {error}");
    }
}
