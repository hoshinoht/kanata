use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use futures_util::StreamExt;
use kanata::{
    adapter::{Adapter, AdapterOutput},
    core::{
        ChatContent, ErrorKind, FinishReason, GatewayError, ModelAlias, NormalizedEvent,
        Request as CoreRequest, ToolChoice,
    },
};
use serde_json::{Value, json};

use crate::support::{
    MockServer, ResponseSpec, adapter, config_for, error_kind, routed, text_request_with_stream,
    tool_request,
};

#[tokio::test]
async fn tool_results_require_one_previously_outstanding_call() {
    let mut unknown = tool_request(ToolChoice::Auto);
    set_tool_result_call_id(&mut unknown, "call_unknown");

    let mut repeated = tool_request(ToolChoice::Auto);
    let CoreRequest::Chat(chat) = &mut repeated else {
        panic!("chat request")
    };
    let repeated_result = chat.messages[2].clone();
    chat.messages.push(repeated_result);

    let mut forward = tool_request(ToolChoice::Auto);
    let CoreRequest::Chat(chat) = &mut forward else {
        panic!("chat request")
    };
    chat.messages.swap(1, 2);

    for (name, request) in [
        ("unknown", unknown),
        ("repeated", repeated),
        ("forward", forward),
    ] {
        let mock = MockServer::once(ResponseSpec::json(crate::support::TEXT_RESPONSE)).await;
        let config = config_for(&mock.address, false, true);
        let adapter = adapter(&config);
        assert_eq!(
            error_kind(adapter.execute(routed(&config, request)).await),
            ErrorKind::InvalidRequest,
            "{name} result"
        );
        assert_eq!(mock.once.load(Ordering::SeqCst), 0, "{name} result");
    }
}

fn set_tool_result_call_id(request: &mut CoreRequest, value: &str) {
    let CoreRequest::Chat(chat) = request else {
        panic!("chat request")
    };
    let ChatContent::ToolResult { call_id, .. } = &mut chat.messages[2].content[0] else {
        panic!("tool result")
    };
    *call_id = value.into();
}

type StreamOutcome = Result<Vec<NormalizedEvent>, (Vec<NormalizedEvent>, ErrorKind)>;

async fn stream_outcome(output: Result<AdapterOutput, GatewayError>) -> StreamOutcome {
    let output = match output {
        Ok(output) => output,
        Err(error) => return Err((Vec::new(), error.kind)),
    };
    let AdapterOutput::Events(mut stream) = output else {
        panic!("expected event stream")
    };
    let mut collected = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => collected.push(event),
            Err(error) => return Err((collected, error.kind)),
        }
    }
    Ok(collected)
}

fn sse_record(payload: Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(&payload).unwrap_or_else(|_| panic!("stream json"))
    )
}

fn choice_chunk(delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }]
    })
}

#[tokio::test]
async fn stream_completion_requires_visible_text_or_a_tool_call() {
    for delta in [
        json!({"role":"assistant"}),
        json!({"role":"assistant","reasoning":"hidden"}),
        json!({"role":"assistant","reasoning_content":"hidden"}),
    ] {
        let body = format!(
            "{}{}data: [DONE]\n\n",
            sse_record(choice_chunk(delta, None)),
            sse_record(choice_chunk(json!({}), Some("stop")))
        );
        let mock = MockServer::once(ResponseSpec::event_stream(&body, body.len())).await;
        let config = config_for(&mock.address, true, true);
        let adapter = adapter(&config);
        let (events, kind) = match stream_outcome(
            adapter
                .execute(routed(&config, text_request_with_stream(true)))
                .await,
        )
        .await
        {
            Err(error) => error,
            Ok(events) => panic!("empty stream completed: {events:?}"),
        };
        assert_eq!(kind, ErrorKind::UpstreamFailure);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
        );
    }
}

#[tokio::test]
async fn stream_length_stop_without_visible_text_completes_empty() {
    let body = format!(
        "{}{}data: [DONE]\n\n",
        sse_record(choice_chunk(
            json!({"role":"assistant","reasoning_content":"hidden"}),
            None
        )),
        sse_record(choice_chunk(json!({}), Some("length")))
    );
    let mock = MockServer::once(ResponseSpec::event_stream(&body, body.len())).await;
    let config = config_for(&mock.address, true, true);
    let adapter = adapter(&config);
    let events = stream_outcome(
        adapter
            .execute(routed(&config, text_request_with_stream(true)))
            .await,
    )
    .await
    .unwrap_or_else(|(events, kind)| panic!("length stop failed: {kind:?} {events:?}"));
    assert!(matches!(
        events.as_slice(),
        [
            NormalizedEvent::ChatStarted { .. },
            NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::Length,
                ..
            }
        ]
    ));
}

#[tokio::test]
async fn plain_ollama_rejects_named_error_events() {
    let body = format!(
        "{}event: error\ndata: {{\"error\":{{\"message\":\"The model's safety guardrails were triggered.\"}}}}\n\n",
        sse_record(choice_chunk(
            json!({"role":"assistant","content":"hi"}),
            None
        )),
    );
    let mock = MockServer::once(ResponseSpec::event_stream(&body, body.len())).await;
    let config = config_for(&mock.address, true, true);
    let adapter = adapter(&config);
    let (_, kind) = stream_outcome(
        adapter
            .execute(routed(&config, text_request_with_stream(true)))
            .await,
    )
    .await
    .expect_err("named error event accepted");
    assert_eq!(kind, ErrorKind::UpstreamFailure);
}

#[tokio::test]
async fn first_tool_indices_must_be_contiguous_in_declaration_order() {
    for (first_index, first_id, second_index, second_id) in [
        (1, "call_first", 0, "call_second"),
        (0, "call_first", 2, "call_second"),
    ] {
        let mut body = String::new();
        for (index, id) in [(first_index, first_id), (second_index, second_id)] {
            body.push_str(&sse_record(choice_chunk(
                json!({
                    "tool_calls": [{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": {"name":"lookup","arguments":"{}"}
                    }]
                }),
                None,
            )));
        }
        body.push_str(&sse_record(choice_chunk(json!({}), Some("tool_calls"))));
        body.push_str("data: [DONE]\n\n");

        let mock = MockServer::once(ResponseSpec::event_stream(&body, body.len())).await;
        let config = config_for(&mock.address, true, true);
        let adapter = adapter(&config);
        let (events, kind) = match stream_outcome(
            adapter
                .execute(routed(&config, text_request_with_stream(true)))
                .await,
        )
        .await
        {
            Err(error) => error,
            Ok(events) => panic!("invalid tool indices completed: {events:?}"),
        };
        assert_eq!(kind, ErrorKind::UpstreamFailure);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
        );
    }
}

fn event_stream_chunks(chunks: Vec<Vec<u8>>) -> ResponseSpec {
    ResponseSpec {
        status: StatusCode::OK,
        content_type: Some("text/event-stream"),
        body: Vec::new(),
        chunks,
        wait_for_close: false,
        write_body_before_close: false,
    }
}

#[tokio::test]
async fn done_ignores_later_records_in_coalesced_and_split_chunks() {
    let mut body = String::new();
    body.push_str(&sse_record(choice_chunk(json!({"role":"assistant"}), None)));
    body.push_str(&sse_record(choice_chunk(
        json!({"content":"visible"}),
        None,
    )));
    body.push_str(&sse_record(choice_chunk(json!({}), Some("stop"))));
    body.push_str("data: [DONE]\n\n");
    body.push_str("data: not-json\n\n");
    let trailing_record = body
        .find("data: not-json")
        .unwrap_or_else(|| panic!("trailing record"));
    let body_bytes = body.as_bytes();
    let split = event_stream_chunks(vec![
        body_bytes[..trailing_record].to_vec(),
        body_bytes[trailing_record..].to_vec(),
    ]);
    let specs = [ResponseSpec::event_stream(&body, body.len()), split];
    let expected = vec![
        NormalizedEvent::ChatStarted {
            model: ModelAlias("local-chat".into()),
        },
        NormalizedEvent::ChatTextDelta {
            text: "visible".into(),
        },
        NormalizedEvent::ChatCompleted {
            finish_reason: FinishReason::Stop,
            usage: None,
        },
    ];
    let mut results = Vec::new();
    for spec in specs {
        let mut mock = MockServer::once(spec).await;
        let config = config_for(&mock.address, true, false);
        let adapter = adapter(&config);
        let events = match stream_outcome(
            adapter
                .execute(routed(&config, text_request_with_stream(true)))
                .await,
        )
        .await
        {
            Ok(events) => events,
            Err((events, error)) => panic!("stream failed after {events:?}: {error:?}"),
        };
        assert_eq!(events, expected);
        results.push(events);
        mock.finish().await;
    }
    assert_eq!(results[0], results[1]);
}
