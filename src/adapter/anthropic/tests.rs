use std::{
    collections::BTreeMap,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use rcgen::generate_simple_self_signed;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

use crate::{
    adapter::{Adapter, AdapterOutput},
    auth::{SecretResolutionError, SecretResolver},
    config::{self, SecretReference, ValidatedConfig},
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, Extensions, FinishReason,
        FunctionTool, JsonSchemaFormat, ModelAlias, NormalizedEvent, ReasoningEffort,
        ReasoningItem, Request, RequestContext, ResponseFormat, RoutedRequest, ToolCall,
        ToolChoice, TrustZone, Usage,
    },
};

use super::AnthropicAdapter;

const HOST: &str = "api.anthropic.com";
const TOKEN: &str = "sk-ant-fixture-not-secret";
const MESSAGE: &str = include_str!("../../../tests/fixtures/anthropic/message-tool-use.json");
const STREAM: &str = include_str!("../../../tests/fixtures/anthropic/stream-thinking-tool-use.sse");

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

struct Resolver;

impl SecretResolver for Resolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        match reference {
            SecretReference::Env(name) if name == "TEST_ANTHROPIC_KEY" => {
                Ok(TOKEN.as_bytes().to_vec())
            }
            _ => Err(SecretResolutionError),
        }
    }
}

struct Recorded {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

fn config(settings: &str) -> ValidatedConfig {
    let contents = format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080
[listeners.admin]
bind = "127.0.0.1"
port = 18081
[publication]
tailnet_addresses = ["100.64.0.1"]
[[adapters]]
id = "anthropic"
kind = "anthropic"
base_url = "https://api.anthropic.com/v1"
trust_zone = "external"
secret_ref = "env:TEST_ANTHROPIC_KEY"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = true
function_tools = true
input_images = true
structured_output = true
reasoning_control = true
{settings}
[[routes]]
id = "claude"
model_alias = "claude"
operation = "chat"
adapter_id = "anthropic"
upstream_id = "claude-opus-5-5"
requires_streaming_chat = false
requires_function_tools = false
allows_input_images = true
[keys]
file = "kanata-unit-missing-keys.toml"
[limits]
max_queue = 2
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = 1048576
max_extension_bytes = 8192
[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#
    );
    let path = std::env::temp_dir().join(format!(
        "kanata-anthropic-unit-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, contents).unwrap();
    let config = config::load(&path).unwrap_or_else(|error| panic!("fixture config: {error}"));
    let _ = std::fs::remove_file(path);
    config
}

/// Serves one response over TLS for `api.anthropic.com` and records the request.
async fn serve(
    status: u16,
    content_type: &'static str,
    body: &'static str,
    settings: &str,
) -> (
    AnthropicAdapter,
    ValidatedConfig,
    tokio::task::JoinHandle<io::Result<Recorded>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let certified = generate_simple_self_signed(vec![HOST.to_owned()]).unwrap();
    let certificate = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let config = config(settings);
    let adapter = AnthropicAdapter::from_config_with_tls_fixture(
        &config,
        "anthropic",
        &Resolver,
        certificate,
        address,
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        let mut socket = acceptor.accept(socket).await.map_err(io::Error::other)?;
        let recorded = read_request(&mut socket).await?;
        let head = format!(
            "HTTP/1.1 {status} Fixture\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        // The client may stop reading after an error status or event.
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body.as_bytes()).await;
        let _ = socket.shutdown().await;
        Ok(recorded)
    });
    (adapter, config, server)
}

async fn read_request<S: AsyncRead + Unpin>(socket: &mut S) -> io::Result<Recorded> {
    let mut bytes = Vec::new();
    let head_end = loop {
        let mut chunk = [0_u8; 4096];
        let count = socket.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..head_end - 4]).into_owned();
    let mut lines = head.split("\r\n");
    let path = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers["content-length"].parse().unwrap();
    let mut body = bytes.split_off(head_end);
    let read = body.len();
    body.resize(length, 0);
    socket.read_exact(&mut body[read..]).await?;
    Ok(Recorded {
        path,
        headers,
        body: serde_json::from_slice(&body).unwrap(),
    })
}

fn routed(config: &ValidatedConfig, chat: ChatRequest) -> RoutedRequest {
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture".into(),
            route: config.routes()[0].identity().clone(),
            trust_zone: TrustZone::External,
            extensions: Extensions::default(),
        },
        Request::Chat(chat),
    )
    .unwrap()
}

fn text(text: &str) -> ChatContent {
    ChatContent::Text { text: text.into() }
}

fn message(role: ChatRole, content: Vec<ChatContent>) -> ChatMessage {
    ChatMessage { role, content }
}

fn reasoning(id: &str, content: &str) -> ChatContent {
    ChatContent::Reasoning {
        item: ReasoningItem::new(Some(id.into()), content.into()).unwrap(),
    }
}

fn chat(messages: Vec<ChatMessage>) -> ChatRequest {
    ChatRequest {
        model: ModelAlias("claude".into()),
        messages,
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        options: Default::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
        extensions: Extensions::default(),
    }
}

fn weather_tool() -> FunctionTool {
    FunctionTool {
        name: "get_weather".into(),
        description: Some("Look up weather".into()),
        parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
    }
}

/// A tool-use turn: system hoisted, roles merged, own thinking replayed and foreign skipped.
fn tool_turn() -> ChatRequest {
    let mut request = chat(vec![
        message(ChatRole::System, vec![text("Be brief.")]),
        message(ChatRole::Developer, vec![text("Use tools.")]),
        message(ChatRole::User, vec![text("Weather in Paris?")]),
        message(ChatRole::User, vec![text("And Rome?")]),
        message(
            ChatRole::Assistant,
            vec![
                text(""),
                ChatContent::ToolCall {
                    call: ToolCall {
                        id: "toolu_a".into(),
                        name: "get_weather".into(),
                        arguments: r#"{"city":"Paris"}"#.into(),
                    },
                },
                reasoning("rs_openai_item", "FOREIGN"),
                reasoning("anthropic_thinking_msg_prior_0", "SIGNATURE_PRIOR"),
                reasoning("anthropic_redacted_thinking_msg_prior_1", "REDACTED_PRIOR"),
                ChatContent::ReasoningText {
                    text: "plain".into(),
                },
            ],
        ),
        message(
            ChatRole::Tool,
            vec![ChatContent::ToolResult {
                call_id: "toolu_a".into(),
                content: "18C".into(),
            }],
        ),
        message(ChatRole::User, vec![text("Thanks")]),
    ]);
    request.tools = vec![weather_tool()];
    request.tool_choice = ToolChoice::Function {
        name: "get_weather".into(),
    };
    request.options.reasoning_effort = Some(ReasoningEffort::High);
    request.options.response_format = Some(ResponseFormat::JsonSchema {
        json_schema: JsonSchemaFormat::new("weather".into(), None, json!({"type": "object"}), None)
            .unwrap(),
    });
    request
}

#[tokio::test]
async fn complete_chat_maps_request_headers_tool_use_reasoning_and_cached_usage() {
    let (adapter, config, server) = serve(
        200,
        "application/json",
        MESSAGE,
        "[adapters.anthropic]\nworkspace_id = \"wrkspc_fixture01\"",
    )
    .await;
    let mut request = tool_turn();
    request.include_encrypted_reasoning = true;
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        adapter.execute(routed(&config, request)),
    )
    .await
    .unwrap()
    .unwrap();
    let AdapterOutput::Complete(crate::core::Response::Chat(reply)) = output else {
        panic!("complete chat reply");
    };
    let recorded = server.await.unwrap().unwrap();

    assert_eq!(recorded.path, "/v1/messages");
    assert_eq!(recorded.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(recorded.headers["anthropic-version"], "2023-06-01");
    assert_eq!(
        recorded.headers["anthropic-workspace-id"],
        "wrkspc_fixture01"
    );
    assert_eq!(recorded.headers["content-type"], "application/json");
    assert!(!recorded.headers.contains_key("x-api-key"));
    assert_eq!(
        recorded.body,
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 16000,
            "system": [
                {"type": "text", "text": "Be brief."},
                {"type": "text", "text": "Use tools."}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "Weather in Paris?"},
                    {"type": "text", "text": "And Rome?"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "", "signature": "SIGNATURE_PRIOR"},
                    {"type": "redacted_thinking", "data": "REDACTED_PRIOR"},
                    {"type": "tool_use", "id": "toolu_a", "name": "get_weather", "input": {"city": "Paris"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_a", "content": "18C"},
                    {"type": "text", "text": "Thanks"}
                ]}
            ],
            "tools": [{
                "name": "get_weather",
                "description": "Look up weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
            }],
            "tool_choice": {"type": "tool", "name": "get_weather"},
            "output_config": {
                "effort": "high",
                "format": {"type": "json_schema", "schema": {"type": "object"}}
            },
            "cache_control": {"type": "ephemeral"},
            "stream": false
        })
    );

    assert_eq!(reply.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        reply.message.content,
        vec![
            text("Checking the weather."),
            ChatContent::ToolCall {
                call: ToolCall {
                    id: "toolu_fixture01".into(),
                    name: "get_weather".into(),
                    arguments: r#"{"city":"Paris"}"#.into(),
                },
            },
        ]
    );
    assert_eq!(
        reply.usage,
        Some(Usage {
            input_tokens: 60,
            output_tokens: 40,
            total_tokens: 100,
            reasoning_tokens: Some(25),
            cached_input_tokens: Some(30),
        })
    );
    assert_eq!(
        reply.reasoning_items,
        vec![
            ReasoningItem::new(
                Some("anthropic_thinking_msg_fixture01_0".into()),
                "SIGNATURE_FIXTURE_NOT_SECRET".into()
            )
            .unwrap(),
            ReasoningItem::new(
                Some("anthropic_redacted_thinking_msg_fixture01_1".into()),
                "REDACTED_FIXTURE_NOT_SECRET".into()
            )
            .unwrap(),
        ]
    );
}

#[test]
fn stop_reasons_map_and_unknown_fails() {
    let mut payload: Value = serde_json::from_str(MESSAGE).unwrap();
    // Without tool use, so a plain stop stays consistent.
    payload["content"].as_array_mut().unwrap().truncate(3);
    for (stop_reason, expected) in [
        ("end_turn", Some(FinishReason::Stop)),
        ("stop_sequence", Some(FinishReason::Stop)),
        ("max_tokens", Some(FinishReason::Length)),
        ("model_context_window_exceeded", Some(FinishReason::Length)),
        ("refusal", Some(FinishReason::ContentFilter)),
        ("tool_use", None),
        ("pause_turn", None),
    ] {
        payload["stop_reason"] = json!(stop_reason);
        let decoded = super::response::decode(
            payload.to_string().as_bytes(),
            ModelAlias("claude".into()),
            false,
        );
        match expected {
            Some(reason) => {
                let reply = decoded.unwrap();
                assert_eq!(reply.finish_reason, reason, "{stop_reason}");
                assert!(reply.reasoning_items.is_empty());
            }
            None => assert_eq!(decoded.unwrap_err().kind, ErrorKind::UpstreamFailure),
        }
    }
}

async fn stream_events(
    include_encrypted_reasoning: bool,
    body: &'static str,
) -> Vec<Result<NormalizedEvent, ErrorKind>> {
    let (adapter, config, server) = serve(200, "text/event-stream", body, "").await;
    let mut request = tool_turn();
    request.stream = true;
    request.include_encrypted_reasoning = include_encrypted_reasoning;
    let output = adapter.execute(routed(&config, request)).await.unwrap();
    let AdapterOutput::Events(events) = output else {
        panic!("event stream");
    };
    let events = tokio::time::timeout(
        Duration::from_secs(3),
        events
            .map(|event| event.map_err(|error| error.kind))
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    assert_eq!(server.await.unwrap().unwrap().body["stream"], true);
    events
}

#[tokio::test]
async fn stream_maps_thinking_text_and_tool_use_events() {
    let events = stream_events(true, STREAM).await;
    let started = Ok(NormalizedEvent::ChatStarted {
        model: ModelAlias("claude".into()),
    });
    let tool = |name: Option<&str>, arguments: &str| {
        Ok(NormalizedEvent::ChatToolCallDelta {
            call_id: "toolu_fixture02".into(),
            name: name.map(str::to_owned),
            arguments_delta: arguments.into(),
        })
    };
    let item = Ok(NormalizedEvent::ChatReasoningItem {
        item: ReasoningItem::new(
            Some("anthropic_thinking_msg_fixture02_0".into()),
            "SIGNATURE_STREAM_NOT_SECRET".into(),
        )
        .unwrap(),
    });
    let rest = [
        Ok(NormalizedEvent::ChatTextDelta {
            text: "Let me check.".into(),
        }),
        tool(Some("get_weather"), ""),
        tool(None, "{\"city\": "),
        tool(None, "\"Paris\"}"),
        Ok(NormalizedEvent::ChatCompleted {
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                input_tokens: 112,
                output_tokens: 42,
                total_tokens: 154,
                reasoning_tokens: None,
                cached_input_tokens: Some(100),
            }),
        }),
    ];
    let reasoning_delta = Ok(NormalizedEvent::ChatReasoningDelta {
        text: "Weather needs a lookup.".into(),
    });
    let mut expected = vec![started.clone(), reasoning_delta.clone(), item];
    expected.extend(rest.clone());
    assert_eq!(events, expected);

    // Without the client's request, no encrypted reasoning is surfaced.
    let mut expected = vec![started, reasoning_delta];
    expected.extend(rest);
    assert_eq!(stream_events(false, STREAM).await, expected);
}

#[tokio::test]
async fn mid_stream_error_and_truncated_stream_fail() {
    const ERROR: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_e\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    const TRUNCATED: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_t\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
    for body in [ERROR, TRUNCATED] {
        let events = stream_events(false, body).await;
        assert_eq!(
            events.last(),
            Some(&Err(ErrorKind::UpstreamFailure)),
            "{events:?}"
        );
    }
}

#[tokio::test]
async fn upstream_statuses_map_including_overloaded() {
    for (status, expected) in [
        (400, ErrorKind::InvalidRequest),
        (401, ErrorKind::UpstreamUnavailable),
        (404, ErrorKind::NotFound),
        (429, ErrorKind::RateLimited),
        (500, ErrorKind::UpstreamFailure),
        (529, ErrorKind::UpstreamUnavailable),
    ] {
        let (adapter, config, server) = serve(
            status,
            "application/json",
            r#"{"type":"error","error":{"type":"fixture_error","message":"fixture"}}"#,
            "",
        )
        .await;
        let error = adapter
            .execute(routed(
                &config,
                chat(vec![message(ChatRole::User, vec![text("Hi")])]),
            ))
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, expected, "{status}");
        server.await.unwrap().unwrap();
    }
}
