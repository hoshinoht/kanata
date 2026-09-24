use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::oneshot};

use crate::{
    adapter::{Adapter, AdapterOutput},
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
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("fixture listener"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("fixture address"));
    let (certificate, acceptor) = tls_fixture();
    let config = config_with_streaming(FIXTURE_HOST, address.port(), true);
    let adapter = adapter_for(&config, certificate, address);
    let server = tokio::spawn(serve_chunked_response(listener, acceptor, chunks));
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        adapter.execute(routed(&config, text_request_with_stream(true))),
    )
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
    let record = server
        .await
        .unwrap_or_else(|_| panic!("fixture server task"))
        .unwrap_or_else(|_| panic!("fixture request"));
    (events, error, record)
}

async fn serve_chunked_response(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    chunks: Vec<Vec<u8>>,
) -> io::Result<RequestRecord> {
    let (socket, _) = listener.accept().await?;
    let mut socket = acceptor.accept(socket).await.map_err(io::Error::other)?;
    let record = read_request_inner(&mut socket).await?;
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
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("fixture listener"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("fixture address"));
    let (certificate, acceptor) = tls_fixture();
    let config = config_with_streaming(FIXTURE_HOST, address.port(), true);
    let adapter = adapter_for(&config, certificate, address);
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_for_server = accepted.clone();
    let (request_tx, request_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener
            .accept()
            .await
            .unwrap_or_else(|_| panic!("fixture accept"));
        accepted_for_server.fetch_add(1, Ordering::SeqCst);
        let mut socket = acceptor
            .accept(socket)
            .await
            .unwrap_or_else(|_| panic!("fixture TLS accept"));
        let record = read_request_inner(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("fixture request"));
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 4096\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap_or_else(|_| panic!("fixture headers"));
        request_tx
            .send(record)
            .unwrap_or_else(|_| panic!("request signal"));
        wait_for_close(&mut socket).await;
        let _ = closed_tx.send(());
        if let Ok(Ok((socket, _))) =
            tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
        {
            accepted_for_server.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });

    let output = tokio::time::timeout(
        Duration::from_secs(3),
        adapter.execute(routed(&config, text_request_with_stream(true))),
    )
    .await
    .unwrap_or_else(|_| panic!("stream setup timeout"))
    .unwrap_or_else(|error| panic!("stream setup: {error:?}"));
    let AdapterOutput::Events(events) = output else {
        panic!("expected stream")
    };
    let record = tokio::time::timeout(Duration::from_secs(2), request_rx)
        .await
        .unwrap_or_else(|_| panic!("request record timeout"))
        .unwrap_or_else(|_| panic!("request record"));
    assert_eq!(record.path, "/api/v1/chat/completions");
    drop(events);
    tokio::time::timeout(Duration::from_secs(2), closed_rx)
        .await
        .unwrap_or_else(|_| panic!("upstream remained open after stream drop"))
        .unwrap_or_else(|_| panic!("upstream close signal"));
    server
        .await
        .unwrap_or_else(|_| panic!("fixture server task"));
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}
