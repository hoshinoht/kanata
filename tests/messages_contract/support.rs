//! A server whose chat routes answer from a fixture adapter: a tool call while the
//! transcript does not end in a tool result, text otherwise; usage always reports a
//! cached prefix.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    body::Body,
    http::{Request, header},
};
use futures_util::stream;
use kanata::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    config::{ValidatedConfig, load},
    core::{
        Capabilities, ChatContent, ChatMessage, ChatRequest, ChatResponse, ChatRole, FinishReason,
        NormalizedEvent, ReasoningItem, Request as CoreRequest, Response as CoreResponse,
        RoutedRequest, ToolCall, Usage,
    },
    server::{Readiness, TwoPlaneServer},
};
use serde_json::Value;

pub const INPUT_TOKENS: u64 = 2048;
pub const CACHED_TOKENS: u64 = 1024;
pub const CALL_ID: &str = "call_fixture_1";
pub const NEXT_ENCRYPTED: &str = "NEXT_ENCRYPTED_FIXTURE";

pub type Recorded = Arc<Mutex<Vec<ChatRequest>>>;

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

struct FixtureAdapter {
    id: &'static str,
    capabilities: Capabilities,
    recorded: Recorded,
}

fn usage() -> Usage {
    Usage {
        input_tokens: INPUT_TOKENS,
        output_tokens: 3,
        total_tokens: INPUT_TOKENS + 3,
        reasoning_tokens: None,
        cached_input_tokens: None,
    }
    .with_cached_input_tokens(CACHED_TOKENS)
}

impl Adapter for FixtureAdapter {
    fn id(&self) -> &str {
        self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
        let (_, request) = routed.into_parts();
        let CoreRequest::Chat(chat) = request else {
            panic!("chat request");
        };
        let model = chat.model.clone();
        let tool_turn = !chat.tools.is_empty()
            && chat
                .messages
                .last()
                .is_none_or(|message| message.role != ChatRole::Tool);
        let reasoning_items = if chat.include_encrypted_reasoning {
            vec![ReasoningItem::new(Some("rs_fixture_next".into()), NEXT_ENCRYPTED.into()).unwrap()]
        } else {
            Vec::new()
        };
        let stream = chat.stream;
        self.recorded.lock().expect("record lock").push(chat);
        let finish_reason = if tool_turn {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };
        Box::pin(async move {
            if stream {
                let mut events = vec![Ok(NormalizedEvent::ChatStarted { model })];
                events.extend(
                    reasoning_items
                        .into_iter()
                        .map(|item| Ok(NormalizedEvent::ChatReasoningItem { item })),
                );
                if tool_turn {
                    events.push(Ok(NormalizedEvent::ChatToolCallDelta {
                        call_id: CALL_ID.into(),
                        name: Some("Bash".into()),
                        arguments_delta: r#"{"command":"#.into(),
                    }));
                    events.push(Ok(NormalizedEvent::ChatToolCallDelta {
                        call_id: CALL_ID.into(),
                        name: None,
                        arguments_delta: r#""ls"}"#.into(),
                    }));
                } else {
                    for text in ["ans", "wer"] {
                        events.push(Ok(NormalizedEvent::ChatTextDelta { text: text.into() }));
                    }
                }
                events.push(Ok(NormalizedEvent::ChatCompleted {
                    finish_reason,
                    usage: Some(usage()),
                }));
                return Ok(AdapterOutput::Events(Box::pin(stream::iter(events))));
            }
            let content = if tool_turn {
                ChatContent::ToolCall {
                    call: ToolCall {
                        id: CALL_ID.into(),
                        name: "Bash".into(),
                        arguments: r#"{"command":"ls"}"#.into(),
                    },
                }
            } else {
                ChatContent::Text {
                    text: "answer".into(),
                }
            };
            Ok(AdapterOutput::Complete(CoreResponse::Chat(ChatResponse {
                model,
                message: ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![content],
                },
                finish_reason,
                usage: Some(usage()),
                reasoning: None,
                reasoning_items,
            })))
        })
    }
}

/// The example fixture with a public listener publishing `private-chat`, sampling
/// controls on its route (Messages always sends `max_tokens`), and `edits` applied.
pub fn config_with(edits: &[(&str, &str)]) -> ValidatedConfig {
    let mut contents =
        std::fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
    for (from, to) in [
        (
            "[listeners.admin]",
            "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
        ),
        (
            "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]",
            "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]\npublic_routes = [{ model_alias = \"private-chat\", operation = \"chat\" }]",
        ),
        (
            "operations = [\"chat\", \"transcription\"]\n",
            "operations = [\"chat\", \"transcription\"]\nsampling_controls = true\n",
        ),
    ]
    .iter()
    .chain(edits)
    {
        assert!(contents.contains(from), "fixture contains {from}");
        contents = contents.replacen(from, to, 1);
    }
    let path = std::env::temp_dir().join(format!(
        "kanata-messages-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, contents).expect("fixture writes");
    let result = load(&path);
    std::fs::remove_file(path).expect("fixture removes");
    result.expect("config validates")
}

/// `private-chat` (public and private), `local-chat` and `codex-chat` (private only).
pub fn server_with(config: &ValidatedConfig) -> (TwoPlaneServer, Recorded) {
    let recorded = Recorded::default();
    let adapters: Vec<Arc<dyn Adapter>> = ["ollama-local", "vllm-private", "codex-private"]
        .into_iter()
        .map(|id| {
            Arc::new(FixtureAdapter {
                id,
                capabilities: crate::gateway::capabilities(config, id),
                recorded: recorded.clone(),
            }) as Arc<dyn Adapter>
        })
        .collect();
    let server = TwoPlaneServer::from_validated_with_adapters(
        config,
        &crate::gateway::Resolver,
        Readiness::new(true),
        adapters,
    )
    .expect("server");
    (server, recorded)
}

pub fn server() -> (TwoPlaneServer, Recorded) {
    server_with(&config_with(&[]))
}

pub fn post(path: &str, body: &Value) -> Request<Body> {
    post_with(path, body, &[("authorization", "Bearer test-key")])
}

pub fn post_with(path: &str, body: &Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body.to_string())).expect("request")
}

pub fn last_request(recorded: &Recorded) -> ChatRequest {
    recorded
        .lock()
        .expect("record lock")
        .last()
        .cloned()
        .expect("recorded request")
}

/// `(event, data)` records of an event stream; `event` is empty for data-only records.
pub fn sse_records(body: &[u8]) -> Vec<(String, Value)> {
    std::str::from_utf8(body)
        .expect("utf8 stream")
        .split("\n\n")
        .filter(|record| !record.is_empty() && !record.starts_with(':'))
        .filter_map(|record| {
            let mut event = String::new();
            let mut data = None;
            for line in record.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    event = value.to_owned();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data = Some(value);
                }
            }
            let data = data.filter(|data| *data != "[DONE]")?;
            Some((event, serde_json::from_str(data).expect("json data")))
        })
        .collect()
}

pub fn claude_fixture() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string("tests/fixtures/harness/claude.json").expect("fixture reads"),
    )
    .expect("fixture json")
}
