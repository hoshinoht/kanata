//! Reasoning continuity and cached-token usage at the adapter boundary: requests are
//! built as IR directly and the upstream payload is recorded by a loopback mock.

#[path = "support/scratch.rs"]
mod scratch;

use futures_util::StreamExt;
use kanata::{
    adapter::{Adapter, AdapterOutput, ollama::OllamaAdapter, vllm::VllmAdapter},
    config::ValidatedConfig,
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, Extensions, ModelAlias, NormalizedEvent,
        PromptCacheKey, ReasoningItem, Request, RequestContext, Response, RoutedRequest,
        ToolChoice, Usage,
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

/// Serves one HTTP response and returns the request body it received.
async fn mock(content_type: &'static str, body: String) -> (String, JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address").to_string();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut bytes = Vec::new();
        let (head_end, length) = loop {
            let mut chunk = [0_u8; 4096];
            let read = socket.read(&mut chunk).await.expect("read");
            assert_ne!(read, 0, "request ended early");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..index]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .expect("content-length");
                break (index + 4, length);
            }
        };
        while bytes.len() < head_end + length {
            let mut chunk = [0_u8; 4096];
            let read = socket.read(&mut chunk).await.expect("read body");
            assert_ne!(read, 0, "body ended early");
            bytes.extend_from_slice(&chunk[..read]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        let _ = socket.shutdown().await;
        serde_json::from_slice(&bytes[head_end..head_end + length]).expect("JSON request")
    });
    (address, task)
}

fn config(address: &str) -> ValidatedConfig {
    let body = format!(
        r#"[[adapters]]
id = "ollama-local"
kind = "ollama"
base_url = "http://{address}/v1"
trust_zone = "local"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = true
function_tools = false

[[adapters]]
id = "vllm-private"
kind = "vllm"
base_url = "http://{address}/v1"
trust_zone = "local"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = true
function_tools = false

[[routes]]
id = "ollama-chat"
model_alias = "local-chat"
operation = "chat"
adapter_id = "ollama-local"
upstream_id = "fixture-model"
requires_streaming_chat = false
requires_function_tools = false

[[routes]]
id = "vllm-chat"
model_alias = "private-chat"
operation = "chat"
adapter_id = "vllm-private"
upstream_id = "served-checkpoint"
requires_streaming_chat = false
requires_function_tools = false
"#
    );
    scratch::load_valid(&scratch::fixture_text(
        &body,
        r#"{ model_alias = "private-chat", operation = "chat" }"#,
        1_048_576,
    ))
}

fn reasoning(content: &str) -> ChatContent {
    ChatContent::Reasoning {
        item: ReasoningItem::new(Some("rs_fixture".into()), content.into()).expect("item"),
    }
}

fn text(role: ChatRole, text: &str) -> ChatMessage {
    ChatMessage {
        role,
        content: vec![ChatContent::Text { text: text.into() }],
    }
}

/// A transcript a replaying client would send, asking for everything.
fn request(model: &str, stream: bool) -> Request {
    let mut answered = text(ChatRole::Assistant, "earlier answer");
    answered
        .content
        .insert(0, reasoning("ENCRYPTED_MARKER_TWO"));
    Request::Chat(ChatRequest {
        model: ModelAlias(model.into()),
        messages: vec![
            text(ChatRole::User, "first question"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![reasoning("ENCRYPTED_MARKER_ONE")],
            },
            answered,
            text(ChatRole::User, "second question"),
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream,
        options: Default::default(),
        prompt_cache_key: Some(PromptCacheKey::new("cache-key-fixture".into()).expect("key")),
        include_encrypted_reasoning: true,
        extensions: Extensions::default(),
    })
}

fn routed(config: &ValidatedConfig, route_id: &str, request: Request) -> RoutedRequest {
    let route = config
        .routes()
        .iter()
        .find(|route| route.identity().route_id == route_id)
        .expect("route");
    let adapter = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == route.adapter_id())
        .expect("adapter");
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture-request".into(),
            route: route.identity().clone(),
            trust_zone: adapter.trust_zone(),
            extensions: Extensions::default(),
        },
        request,
    )
    .expect("routed request")
}
fn vllm(config: &ValidatedConfig) -> VllmAdapter {
    VllmAdapter::from_config(config, "vllm-private", "vllm-chat").expect("vLLM adapter")
}

fn completion(usage: Value) -> String {
    json!({
        "id": "chatcmpl_fixture",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": "served-checkpoint",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "fixture answer"},
            "finish_reason": "stop"
        }],
        "usage": usage,
    })
    .to_string()
}

/// Non-replaying upstreams see neither reasoning nor the cache hint nor the include flag.
fn assert_nothing_replayed(body: &Value) {
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "reasoning-only turn is dropped");
    assert_eq!(messages[1]["content"], "earlier answer");
    for absent in ["prompt_cache_key", "include", "reasoning"] {
        assert!(body.get(absent).is_none(), "{absent} forwarded");
    }
    assert!(!body.to_string().contains("ENCRYPTED_MARKER"));
}

fn usage(output: AdapterOutput) -> Option<Usage> {
    match output {
        AdapterOutput::Complete(Response::Chat(response)) => {
            assert!(response.reasoning_items.is_empty());
            response.usage
        }
        _ => panic!("complete chat response"),
    }
}

#[tokio::test]
async fn vllm_reports_cached_tokens_and_never_receives_reasoning() {
    let cases = [
        (
            json!({"prompt_tokens":2048,"completion_tokens":3,"total_tokens":2051,
                "prompt_tokens_details":{"cached_tokens":1024}}),
            Some(1024),
        ),
        (
            json!({"prompt_tokens":2048,"completion_tokens":3,"total_tokens":2051}),
            None,
        ),
        (
            json!({"prompt_tokens":2048,"completion_tokens":3,"total_tokens":2051,
                "prompt_tokens_details":null}),
            None,
        ),
    ];
    for (reported, expected) in cases {
        let (address, upstream) = mock("application/json", completion(reported)).await;
        let config = config(&address);
        let output = vllm(&config)
            .execute(routed(&config, "vllm-chat", request("private-chat", false)))
            .await
            .expect("vLLM response");
        let usage = usage(output).expect("usage");
        assert_eq!(usage.input_tokens, 2048);
        assert_eq!(usage.cached_input_tokens, expected);
        assert_nothing_replayed(&upstream.await.expect("mock"));
    }
}

#[tokio::test]
async fn vllm_stream_usage_carries_cached_tokens() {
    let chunk = |choices: Value, usage: Value| {
        format!(
            "data: {}\n\n",
            json!({"id":"chatcmpl_fixture","object":"chat.completion.chunk",
                "created":1_700_000_000,"model":"served-checkpoint","choices":choices,
                "usage":usage})
        )
    };
    let body = chunk(
        json!([{"index":0,"delta":{"role":"assistant","content":"fixture answer"}}]),
        Value::Null,
    ) + &chunk(
        json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
        Value::Null,
    ) + &chunk(
        json!([]),
        json!({"prompt_tokens":2048,"completion_tokens":3,"total_tokens":2051,
            "prompt_tokens_details":{"cached_tokens":1024}}),
    ) + "data: [DONE]\n\n";
    let (address, upstream) = mock("text/event-stream", body).await;
    let config = config(&address);
    let AdapterOutput::Events(mut events) = vllm(&config)
        .execute(routed(&config, "vllm-chat", request("private-chat", true)))
        .await
        .expect("vLLM stream")
    else {
        panic!("event stream")
    };
    let mut completed = None;
    while let Some(event) = events.next().await {
        match event.expect("stream event") {
            NormalizedEvent::ChatCompleted { usage, .. } => completed = usage,
            NormalizedEvent::ChatReasoningItem { .. } => panic!("reasoning item emitted"),
            _ => {}
        }
    }
    assert_eq!(
        completed.and_then(|usage| usage.cached_input_tokens),
        Some(1024)
    );
    assert_nothing_replayed(&upstream.await.expect("mock"));
}

#[tokio::test]
async fn ollama_never_receives_reasoning_or_cache_fields() {
    let (address, upstream) = mock(
        "application/json",
        include_str!("fixtures/ollama/chat-text-response.json").to_owned(),
    )
    .await;
    let config = config(&address);
    let adapter = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "ollama-local")
        .expect("Ollama adapter");
    let ollama =
        OllamaAdapter::new(adapter, config.timeouts(), config.limits()).expect("Ollama adapter");
    let output = ollama
        .execute(routed(&config, "ollama-chat", request("local-chat", false)))
        .await
        .expect("Ollama response");
    assert_eq!(
        usage(output).and_then(|usage| usage.cached_input_tokens),
        None
    );
    assert_nothing_replayed(&upstream.await.expect("mock"));
}
