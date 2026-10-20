use std::{io, time::Duration};

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpListener};

use crate::{
    adapter::AdapterOutput,
    core::{ErrorKind, FinishReason, ModelAlias, NormalizedEvent, Usage},
};

use super::*;

const TEXT_STREAM: &str =
    include_str!("../../../../tests/fixtures/openrouter/chat-stream-text.sse");
const ERROR_BEFORE_STREAM: &str =
    include_str!("../../../../tests/fixtures/openrouter/chat-stream-error-before.sse");
const ERROR_AFTER_STREAM: &str =
    include_str!("../../../../tests/fixtures/openrouter/chat-stream-error-after.sse");
const TOOL_STREAM: &str =
    include_str!("../../../../tests/fixtures/openrouter/chat-stream-tool-delta.sse");
const MALFORMED_STREAM: &str =
    include_str!("../../../../tests/fixtures/openrouter/chat-stream-malformed.sse");
const UNKNOWN_FINISH_STREAM: &str =
    include_str!("../../../../tests/fixtures/openrouter/chat-stream-unknown-finish.sse");

async fn run_stream(
    chunks: Vec<Vec<u8>>,
) -> (
    Vec<NormalizedEvent>,
    Option<crate::core::GatewayError>,
    RequestRecord,
) {
    run_stream_with(chunks, text_request_with_stream(true)).await
}

async fn run_stream_with(
    chunks: Vec<Vec<u8>>,
    request: CoreRequest,
) -> (
    Vec<NormalizedEvent>,
    Option<crate::core::GatewayError>,
    RequestRecord,
) {
    let (client, listener, acceptor) = Client::new(FIXTURE_HOST, STREAMING).await;
    let server = tokio::spawn(serve_chunked_response(listener, acceptor, chunks));
    let output = tokio::time::timeout(Duration::from_secs(3), client.execute(request))
        .await
        .unwrap_or_else(|_| panic!("OpenRouter stream setup timeout"))
        .unwrap_or_else(|error| panic!("OpenRouter stream setup: {error:?}"));
    let AdapterOutput::Events(mut stream) = output else {
        panic!("expected normalized event stream")
    };
    let mut events = Vec::new();
    let mut error = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => events.push(event),
            Err(stream_error) => {
                error = Some(stream_error);
                break;
            }
        }
    }
    drop(stream);
    (events, error, recorded(server).await)
}

async fn serve_chunked_response(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    chunks: Vec<Vec<u8>>,
) -> io::Result<RequestRecord> {
    let (socket, _) = listener.accept().await?;
    let mut socket = acceptor.accept(socket).await.map_err(io::Error::other)?;
    let record = read_request(&mut socket).await?;
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        )
        .await?;
    for chunk in chunks {
        if chunk.is_empty() {
            continue;
        }
        let header = format!("{:x}\r\n", chunk.len());
        if socket.write_all(header.as_bytes()).await.is_err()
            || socket.write_all(&chunk).await.is_err()
            || socket.write_all(b"\r\n").await.is_err()
        {
            return Ok(record);
        }
        let _ = socket.flush().await;
    }
    let _ = socket.write_all(b"0\r\n\r\n").await;
    Ok(record)
}

fn fixture_chunks(body: &str) -> Vec<Vec<u8>> {
    vec![body.as_bytes().to_vec()]
}

#[tokio::test]
async fn verified_https_stream_normalizes_fragmented_sse_usage_and_utf8() {
    let chunks = TEXT_STREAM
        .as_bytes()
        .iter()
        .map(|byte| vec![*byte])
        .collect();
    let (events, error, record) = run_stream(chunks).await;

    assert!(error.is_none());
    assert_eq!(
        events,
        vec![
            NormalizedEvent::ChatStarted {
                model: ModelAlias(PUBLIC_MODEL.into())
            },
            NormalizedEvent::ChatTextDelta {
                text: "Hello ".into()
            },
            NormalizedEvent::ChatTextDelta {
                text: "café 🌍".into()
            },
            NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 11,
                    output_tokens: 7,
                    total_tokens: 18,
                    reasoning_tokens: Some(0),
                    cached_input_tokens: Some(2),
                })
            }
        ]
    );
    assert_eq!(record.method, "POST");
    assert_eq!(record.path, "/api/v1/chat/completions");
    assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(record.headers["accept"], "text/event-stream");
    let payload: Value =
        serde_json::from_slice(&record.body).unwrap_or_else(|_| panic!("stream request json"));
    assert_eq!(payload["model"], UPSTREAM_MODEL);
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["provider"], json!({"allow_fallbacks": false}));
    assert_eq!(payload.as_object().map(serde_json::Map::len), Some(4));
    assert!(
        !payload
            .as_object()
            .is_some_and(|object| object.contains_key("stream_options"))
    );
}

#[tokio::test]
async fn reasoning_items_and_cache_fields_are_never_forwarded() {
    let CoreRequest::Chat(mut chat) = text_request_with_stream(true) else {
        panic!("chat request")
    };
    let reasoning = || crate::core::ChatContent::Reasoning {
        item: crate::core::ReasoningItem::new(Some("rs_fixture".into()), "ENCRYPTED_MARKER".into())
            .unwrap_or_else(|_| panic!("reasoning item")),
    };
    chat.messages[2].content.insert(0, reasoning());
    chat.messages.insert(
        2,
        crate::core::ChatMessage {
            role: crate::core::ChatRole::Assistant,
            content: vec![reasoning()],
        },
    );
    chat.prompt_cache_key = Some(
        crate::core::PromptCacheKey::new("cache-key-fixture".into())
            .unwrap_or_else(|_| panic!("cache key")),
    );
    chat.include_encrypted_reasoning = true;
    let (_, error, record) =
        run_stream_with(fixture_chunks(TEXT_STREAM), CoreRequest::Chat(chat)).await;
    assert!(error.is_none());
    let body: Value =
        serde_json::from_slice(&record.body).unwrap_or_else(|_| panic!("JSON request"));
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(4));
    assert_eq!(body["messages"][2]["content"], "2");
    for absent in ["prompt_cache_key", "include", "reasoning"] {
        assert!(body.get(absent).is_none(), "{absent} forwarded");
    }
    assert!(!String::from_utf8_lossy(&record.body).contains("ENCRYPTED_MARKER"));
}

#[tokio::test]
async fn done_ignores_trailing_records_when_coalesced_or_split() {
    let marker = "data: {not-json}";
    let trailing = TEXT_STREAM
        .find(marker)
        .unwrap_or_else(|| panic!("trailing fixture record"));
    let after_done = TEXT_STREAM.as_bytes()[..trailing]
        .windows(b"data: [DONE]\n\n".len())
        .position(|window| window == b"data: [DONE]\n\n")
        .map(|done| done + b"data: [DONE]\n\n".len())
        .unwrap_or_else(|| panic!("DONE fixture record"));
    let body = TEXT_STREAM.as_bytes();
    let cases = [
        fixture_chunks(TEXT_STREAM),
        vec![body[..after_done].to_vec(), body[after_done..].to_vec()],
    ];
    let mut results = Vec::new();
    for chunks in cases {
        let (events, error, _) = run_stream(chunks).await;
        assert!(error.is_none());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
                .count(),
            1
        );
        results.push(events);
    }
    assert_eq!(results[0], results[1]);
}

#[tokio::test]
async fn malformed_tool_and_unrecognized_finish_frames_fail_without_done() {
    for body in [TOOL_STREAM, MALFORMED_STREAM, UNKNOWN_FINISH_STREAM] {
        let (events, error, _) = run_stream(fixture_chunks(body)).await;
        let error = error.unwrap_or_else(|| panic!("malformed stream was accepted"));
        assert_eq!(error.kind, ErrorKind::UpstreamFailure);
        assert!(!format!("{error:?}").contains("fixture-private-marker"));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, NormalizedEvent::ChatToolCallDelta { .. }))
        );
    }
}

#[tokio::test]
async fn top_level_stream_errors_fail_redacted_before_and_after_output() {
    let (before, before_error, _) = run_stream(fixture_chunks(ERROR_BEFORE_STREAM)).await;
    let before_error = before_error.unwrap_or_else(|| panic!("pre-output error accepted"));
    assert_eq!(before_error.kind, ErrorKind::UpstreamFailure);
    assert!(before.is_empty());
    assert!(!format!("{before_error:?}").contains("fixture-error-marker"));

    let (after, after_error, _) = run_stream(fixture_chunks(ERROR_AFTER_STREAM)).await;
    let after_error = after_error.unwrap_or_else(|| panic!("mid-stream error accepted"));
    assert_eq!(after_error.kind, ErrorKind::UpstreamFailure);
    assert!(matches!(
        after.as_slice(),
        [
            NormalizedEvent::ChatStarted { .. },
            NormalizedEvent::ChatTextDelta { text }
        ] if text == "partial output"
    ));
    assert!(!format!("{after_error:?}").contains("fixture-error-marker"));
}

#[tokio::test]
async fn dropping_stream_closes_verified_https_upstream_without_replay() {
    let (client, listener, acceptor) = Client::new(FIXTURE_HOST, STREAMING).await;
    let mut upstream = Stalled::serve(
        listener,
        acceptor,
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 4096\r\nconnection: close\r\n\r\n",
    );

    let output = tokio::time::timeout(
        Duration::from_secs(3),
        client.execute(text_request_with_stream(true)),
    )
    .await
    .unwrap_or_else(|_| panic!("stream setup timeout"))
    .unwrap_or_else(|error| panic!("stream setup: {error:?}"));
    let AdapterOutput::Events(events) = output else {
        panic!("expected stream")
    };
    assert_eq!(upstream.request().await.path, "/api/v1/chat/completions");
    drop(events);
    upstream.assert_closed_without_replay().await;
}

#[test]
fn stop_or_length_without_text_completes_empty_and_content_filter_does_not() {
    let chunk = |delta: Value, finish: Value, usage: Value| {
        json!({
            "id":"gen-fixture","object":"chat.completion.chunk","created":1,"model":"m",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish,"native_finish_reason":null}],
            "usage":usage
        })
        .to_string()
    };
    let usage = json!({"prompt_tokens":1,"completion_tokens":2,"total_tokens":3});
    for (finish, expected) in [
        ("length", Some(FinishReason::Length)),
        ("stop", Some(FinishReason::Stop)),
        ("content_filter", None),
    ] {
        let mut state =
            super::super::stream_state::StreamState::new(ModelAlias(PUBLIC_MODEL.into()));
        let mut events = state
            .data(&chunk(
                json!({"role":"assistant"}),
                Value::Null,
                Value::Null,
            ))
            .unwrap_or_else(|_| panic!("role chunk"));
        events.extend(
            state
                .data(&chunk(
                    json!({"content":"","reasoning":"thinking"}),
                    Value::Null,
                    Value::Null,
                ))
                .unwrap_or_else(|_| panic!("reasoning chunk")),
        );
        let finished = state.data(&chunk(json!({}), json!(finish), Value::Null));
        let Some(expected) = expected else {
            assert!(finished.is_err());
            continue;
        };
        events.extend(finished.unwrap_or_else(|_| panic!("{finish} finish")));
        state
            .data(&chunk(json!({}), json!(finish), usage.clone()))
            .unwrap_or_else(|_| panic!("usage chunk"));
        events.push(state.done().unwrap_or_else(|_| panic!("done")));
        assert!(matches!(
            events.as_slice(),
            [
                NormalizedEvent::ChatStarted { .. },
                NormalizedEvent::ChatReasoningDelta { .. },
                NormalizedEvent::ChatCompleted { finish_reason, .. }
            ] if *finish_reason == expected
        ));
    }
}
