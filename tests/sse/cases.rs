use std::future::Future;
use std::task::{Context, Poll};

use axum::http::{StatusCode, header};
use futures_util::StreamExt;
use kanata::core::{ErrorKind, FinishReason, GatewayError, ModelAlias, NormalizedEvent, Usage};
use serde_json::Value;

use crate::support;

const BASIC: &str = include_str!("../fixtures/openai/sse-basic.json");
const TOOLS: &str = include_str!("../fixtures/openai/sse-tools.json");
const MODEL: &str = "private-chat";

fn started() -> support::Event {
    Ok(NormalizedEvent::ChatStarted {
        model: ModelAlias(MODEL.into()),
    })
}

fn text(value: &str) -> support::Event {
    Ok(NormalizedEvent::ChatTextDelta { text: value.into() })
}

fn tool(call_id: &str, name: Option<&str>, arguments: &str) -> support::Event {
    Ok(NormalizedEvent::ChatToolCallDelta {
        call_id: call_id.into(),
        name: name.map(str::to_owned),
        arguments_delta: arguments.into(),
    })
}

fn completed(reason: FinishReason, usage: Option<Usage>) -> support::Event {
    Ok(NormalizedEvent::ChatCompleted {
        finish_reason: reason,
        usage,
    })
}

fn error(kind: ErrorKind) -> support::Event {
    Err(GatewayError { kind })
}

fn usage() -> Usage {
    Usage {
        input_tokens: 11,
        output_tokens: 7,
        total_tokens: 18,
    }
}

fn records(body: &[u8], split: usize) -> Vec<String> {
    assert!(split > 0);
    let mut pending = Vec::new();
    let mut output = Vec::new();
    for part in body.chunks(split) {
        pending.extend_from_slice(part);
        while let Some(end) = pending.windows(2).position(|window| window == b"\n\n") {
            let record: Vec<_> = pending.drain(..end + 2).collect();
            let record = std::str::from_utf8(&record).expect("complete SSE record is UTF-8");
            let data = record
                .strip_suffix("\n\n")
                .and_then(|value| value.strip_prefix("data: "))
                .expect("single-line data record");
            output.push(data.to_owned());
        }
    }
    assert!(pending.is_empty(), "unterminated SSE record");
    output
}

fn json_records(body: &[u8]) -> Vec<Value> {
    records(body, 1)
        .into_iter()
        .filter(|record| record != "[DONE]")
        .map(|record| serde_json::from_str(&record).expect("JSON SSE data"))
        .collect()
}

fn assert_common_chunk(chunk: &Value, id: &str) {
    assert_eq!(chunk["id"], id);
    assert_eq!(chunk["object"], "chat.completion.chunk");
    assert_eq!(chunk["created"], 0);
    assert_eq!(chunk["model"], MODEL);
    assert_eq!(chunk["system_fingerprint"], Value::Null);
}

#[tokio::test]
async fn text_stream_is_valid_when_records_are_reconstructed_from_byte_splits() {
    let (server, probe) = support::server(vec![
        started(),
        text("hello "),
        text("🌍"),
        completed(FinishReason::Stop, Some(usage())),
    ]);
    let response = server
        .client_oneshot(support::request(BASIC))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    let body = support::response_body(response).await;

    let expected = records(&body, 1);
    for split in [1, 2, 3, 7, 13] {
        assert_eq!(records(&body, split), expected, "split size {split}");
    }
    assert_eq!(expected.last().map(String::as_str), Some("[DONE]"));
    assert_eq!(expected.len(), 5);

    let chunks = json_records(&body);
    let id = chunks[0]["id"].as_str().expect("chunk id");
    assert!(id.starts_with("chatcmpl_kanata_"));
    for chunk in &chunks {
        assert_common_chunk(chunk, id);
        assert_eq!(chunk["usage"], Value::Null);
    }
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "hello ");
    assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "🌍");
    assert_eq!(
        chunks[3]["choices"][0]["delta"],
        Value::Object(Default::default())
    );
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
    assert_eq!(probe.dispatches(), 1);
}

#[tokio::test]
async fn tool_deltas_keep_indices_and_emit_requested_usage_only() {
    let (server, _) = support::server(vec![
        started(),
        tool("call_one", Some("lookup"), "{\"q\":\"ka"),
        text("interleaved 🌙"),
        tool("call_two", Some("lookup"), "{\"q\":\"ru"),
        tool("call_one", None, "nata\"}"),
        tool("call_two", None, "st\"}"),
        completed(FinishReason::ToolCalls, Some(usage())),
    ]);
    let response = server
        .client_oneshot(support::request(TOOLS))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = support::response_body(response).await;
    let data = records(&body, 2);
    assert_eq!(data.last().map(String::as_str), Some("[DONE]"));

    let chunks = json_records(&body);
    let id = chunks[0]["id"].as_str().expect("chunk id");
    for chunk in &chunks {
        assert_common_chunk(chunk, id);
    }
    let first = &chunks[1]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(first["index"], 0);
    assert_eq!(first["id"], "call_one");
    assert_eq!(first["type"], "function");
    assert_eq!(first["function"]["name"], "lookup");
    assert_eq!(first["function"]["arguments"], "{\"q\":\"ka");

    let second = &chunks[3]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(second["index"], 1);
    assert_eq!(second["id"], "call_two");
    assert_eq!(second["function"]["name"], "lookup");
    let continuation = &chunks[4]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(continuation["index"], 0);
    assert!(continuation.get("id").is_none());
    assert!(continuation.get("type").is_none());
    assert!(continuation["function"].get("name").is_none());
    assert_eq!(continuation["function"]["arguments"], "nata\"}");
    assert_eq!(
        chunks[5]["choices"][0]["delta"]["tool_calls"][0]["index"],
        1
    );

    let usage_chunk = chunks.last().expect("usage chunk");
    assert_eq!(usage_chunk["choices"], Value::Array(Vec::new()));
    assert_eq!(usage_chunk["usage"]["prompt_tokens"], 11);
    assert_eq!(usage_chunk["usage"]["completion_tokens"], 7);
    assert_eq!(usage_chunk["usage"]["total_tokens"], 18);
}

#[tokio::test]
async fn finish_reason_matches_streamed_tool_calls_except_interruptions() {
    let cases = [
        (
            "stop-after-tool",
            vec![
                started(),
                tool("call_stop", Some("lookup"), ""),
                completed(FinishReason::Stop, None),
            ],
            false,
        ),
        (
            "tool-calls-without-tool",
            vec![started(), completed(FinishReason::ToolCalls, None)],
            false,
        ),
        (
            "length-after-tool",
            vec![
                started(),
                tool("call_length", Some("lookup"), ""),
                completed(FinishReason::Length, None),
            ],
            true,
        ),
        (
            "content-filter-after-tool",
            vec![
                started(),
                tool("call_filter", Some("lookup"), ""),
                completed(FinishReason::ContentFilter, None),
            ],
            true,
        ),
    ];
    for (name, events, succeeds) in cases {
        let (server, _) = support::server(events);
        let response = server
            .client_oneshot(support::request(TOOLS))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        let body = support::response_body(response).await;
        let data = records(&body, 2);
        if succeeds {
            assert_eq!(data.last().map(String::as_str), Some("[DONE]"), "{name}");
            let chunks = json_records(&body);
            assert!(
                chunks
                    .iter()
                    .any(|chunk| chunk["choices"][0]["finish_reason"]
                        == if name == "length-after-tool" {
                            "length"
                        } else {
                            "content_filter"
                        }),
                "{name} missing finish"
            );
        } else {
            assert!(!data.contains(&"[DONE]".to_owned()), "{name} emitted done");
            let error = serde_json::from_str::<Value>(data.last().expect("error record"))
                .expect("error data");
            assert_eq!(error["error"]["code"], "upstream_failure", "{name}");
            let chunks = json_records(&body);
            assert!(
                chunks
                    .iter()
                    .all(|chunk| chunk["choices"][0]["finish_reason"].is_null()),
                "{name} emitted finish"
            );
        }
    }
}

#[tokio::test]
async fn stream_ids_are_unique_per_response_and_stable_with_usage() {
    let events = vec![
        started(),
        text("one"),
        completed(FinishReason::Stop, Some(usage())),
    ];
    let (server, _) = support::server(events);
    let first = server
        .client_oneshot(support::request(TOOLS))
        .await
        .expect("first response");
    let first = support::response_body(first).await;
    let second = server
        .client_oneshot(support::request(TOOLS))
        .await
        .expect("second response");
    let second = support::response_body(second).await;
    let first_chunks = json_records(&first);
    let second_chunks = json_records(&second);
    let first_id = first_chunks[0]["id"].as_str().expect("first id");
    let second_id = second_chunks[0]["id"].as_str().expect("second id");
    assert_ne!(first_id, second_id);
    assert!(first_chunks.iter().all(|chunk| chunk["id"] == first_id));
    assert!(second_chunks.iter().all(|chunk| chunk["id"] == second_id));
    assert!(first_chunks.iter().all(|chunk| chunk["model"] == MODEL));
}

#[tokio::test]
async fn preflight_requires_one_exact_chat_start_before_headers() {
    let cases = [
        ("empty", Vec::new(), StatusCode::BAD_GATEWAY, None),
        (
            "text-first",
            vec![text("UPSTREAM_TEXT_MARKER")],
            StatusCode::BAD_GATEWAY,
            None,
        ),
        (
            "wrong-model",
            vec![Ok(NormalizedEvent::ChatStarted {
                model: ModelAlias("UPSTREAM_MODEL_MARKER".into()),
            })],
            StatusCode::BAD_GATEWAY,
            None,
        ),
        (
            "mapped-error",
            vec![error(ErrorKind::RateLimited)],
            StatusCode::TOO_MANY_REQUESTS,
            Some("rate_limit_exceeded"),
        ),
    ];
    for (name, events, status, code) in cases {
        let (server, probe) = support::server(events);
        let response = server
            .client_oneshot(support::request(BASIC))
            .await
            .expect("response");
        assert_eq!(response.status(), status, "{name}");
        let body = support::response_body(response).await;
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("UPSTREAM_"), "{name} leaked upstream data");
        assert!(!body.contains("[DONE]"), "{name} emitted done");
        if let Some(code) = code {
            assert!(
                body.contains(code),
                "{name} did not preserve safe error code"
            );
        }
        assert_eq!(probe.dispatches(), 1, "{name} dispatch count");
        assert_eq!(probe.polls(), 1, "{name} preflight poll count");
    }
}

#[tokio::test]
async fn post_start_errors_are_one_sanitized_event_without_success_termination() {
    let cases = [
        ("eof", vec![started()], None),
        (
            "adapter-error",
            vec![started(), error(ErrorKind::RateLimited)],
            Some("rate_limit_exceeded"),
        ),
        (
            "duplicate-start",
            vec![
                started(),
                Ok(NormalizedEvent::ChatStarted {
                    model: ModelAlias("UPSTREAM_MODEL_MARKER".into()),
                }),
            ],
            None,
        ),
        (
            "missing-initial-name",
            vec![started(), tool("TOOL_ID_MARKER", None, "{}")],
            None,
        ),
        (
            "changed-name",
            vec![
                started(),
                tool("call_one", Some("lookup"), "{"),
                tool("call_one", Some("TOOL_NAME_MARKER"), "}"),
            ],
            None,
        ),
        (
            "unknown-continuation",
            vec![started(), tool("UNKNOWN_CALL_MARKER", None, "}")],
            None,
        ),
    ];
    for (name, events, code) in cases {
        let (server, _) = support::server(events);
        let response = server
            .client_oneshot(support::request(BASIC))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        let body = support::response_body(response).await;
        let data = records(&body, 3);
        assert!(data.len() >= 2, "{name} should have role and error");
        assert!(!data.contains(&"[DONE]".to_owned()), "{name} emitted done");
        let error =
            serde_json::from_str::<Value>(data.last().expect("error record")).expect("error data");
        assert!(
            error.get("error").is_some(),
            "{name} missing error envelope"
        );
        assert!(
            !String::from_utf8_lossy(&body).contains("MARKER"),
            "{name} leaked marker"
        );
        if let Some(code) = code {
            assert_eq!(error["error"]["code"], code, "{name} error code");
        }
    }
}

#[tokio::test]
async fn cancellation_is_a_sanitized_failure_not_a_cancelled_finish_reason() {
    let (server, _) = support::server(vec![
        started(),
        completed(FinishReason::Cancelled, Some(usage())),
    ]);
    let response = server
        .client_oneshot(support::request(BASIC))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = support::response_body(response).await;
    let data = records(&body, 1);
    assert_eq!(data.len(), 2);
    assert!(data[1].starts_with('{'));
    assert!(!data.contains(&"[DONE]".to_owned()));
    assert!(!String::from_utf8_lossy(&body).contains("\"finish_reason\":\"cancelled\""));
    let error: Value = serde_json::from_str(&data[1]).expect("cancellation error");
    assert_eq!(error["error"]["code"], "request_cancelled");
}

#[tokio::test]
async fn completion_drops_upstream_and_does_not_poll_trailing_events() {
    let (server, probe) = support::server(vec![
        started(),
        completed(FinishReason::Stop, None),
        text("TRAILING_EVENT_MARKER"),
    ]);
    let response = server
        .client_oneshot(support::request(BASIC))
        .await
        .expect("response");
    let body = support::response_body(response).await;
    let data = records(&body, 1);
    assert_eq!(data.len(), 3);
    assert_eq!(data.last().map(String::as_str), Some("[DONE]"));
    assert!(!String::from_utf8_lossy(&body).contains("TRAILING_EVENT_MARKER"));
    assert_eq!(probe.polls(), 2, "preflight and completion only");
    assert_eq!(probe.drops(), 1, "upstream dropped at completion");
}

#[tokio::test]
async fn tool_bookkeeping_has_a_conservative_bound() {
    let mut events = vec![started()];
    for index in 0..65 {
        events.push(tool(&format!("call_{index}"), Some("lookup"), ""));
    }
    let (server, _) = support::server(events);
    let response = server
        .client_oneshot(support::request(TOOLS))
        .await
        .expect("response");
    let body = support::response_body(response).await;
    let data = records(&body, 5);
    assert_eq!(data.len(), 66, "role, 64 bounded calls, and error");
    assert!(data.last().is_some_and(|value| value.starts_with('{')));
    assert!(!data.contains(&"[DONE]".to_owned()));
    let chunks = json_records(&body);
    assert_eq!(chunks.len(), 66);
    assert!(chunks[65].get("error").is_some());
}

#[tokio::test]
async fn response_construction_is_lazy_and_body_drop_releases_upstream() {
    let (server, probe) = support::pending_after_first_server(vec![
        started(),
        text("never"),
        completed(FinishReason::Stop, None),
    ]);
    let response = server
        .client_oneshot(support::request(BASIC))
        .await
        .expect("response");
    assert_eq!(probe.polls(), 1, "only preflight poll before headers");
    assert_eq!(probe.drops(), 0);

    let mut body = response.into_body().into_data_stream();
    let role = body.next().await.expect("role frame").expect("body bytes");
    assert!(
        std::str::from_utf8(&role)
            .expect("role UTF-8")
            .contains("assistant")
    );
    assert_eq!(probe.polls(), 1, "start frame does not poll upstream");

    let mut pending = Box::pin(body.next());
    let waker = futures_util::task::noop_waker_ref();
    let mut context = Context::from_waker(waker);
    assert!(matches!(pending.as_mut().poll(&mut context), Poll::Pending));
    drop(pending);
    drop(body);
    assert_eq!(probe.polls(), 2);
    assert_eq!(probe.drops(), 1, "body drop releases upstream stream");
}

#[tokio::test]
async fn preflight_cancellation_drops_a_pending_upstream_stream() {
    let (server, probe) = support::pending_first_server(vec![started()]);
    let mut request = Box::pin(server.client_oneshot(support::request(BASIC)));
    let waker = futures_util::task::noop_waker_ref();
    let mut context = Context::from_waker(waker);
    assert!(matches!(request.as_mut().poll(&mut context), Poll::Pending));
    assert_eq!(probe.polls(), 1);
    drop(request);
    assert_eq!(probe.drops(), 1, "cancelled preflight drops upstream");
}
