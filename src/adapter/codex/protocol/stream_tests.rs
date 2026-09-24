use crate::core::{
    ChatContent, ChatRole, FinishReason, ModelAlias, NormalizedEvent, ToolCall, Usage,
};

use super::{ResponsesStreamError, ResponsesStreamParser};

const RESPONSES_EVENTS: &str =
    include_str!("../../../../tests/fixtures/codex/responses-events.sse");
const FAILED_EVENTS: &str =
    include_str!("../../../../tests/fixtures/codex/responses-events-failed.sse");
const INCOMPLETE_EVENTS: &str =
    include_str!("../../../../tests/fixtures/codex/responses-events-incomplete.sse");
const TEXT_LIFECYCLE_EVENTS: &str =
    include_str!("../../../../tests/fixtures/codex/responses-events-text-lifecycle.sse");
const NAMED_RESPONSES_EVENTS: &str =
    include_str!("../../../../tests/fixtures/codex/responses-events-named.sse");
const IN_PROGRESS_NAMED: &str =
    include_str!("../../../../tests/fixtures/codex/responses-events-in-progress-named.sse");
const IN_PROGRESS_UNNAMED: &str =
    include_str!("../../../../tests/fixtures/codex/responses-events-in-progress-unnamed.sse");
const LIVE_SHAPE_EVENTS: &str =
    include_str!("../../../../tests/fixtures/codex/responses-events-live-shape.sse");

fn new_parser() -> ResponsesStreamParser {
    ResponsesStreamParser::new(ModelAlias("codex-chat".into()))
}

#[test]
fn fixture_translates_fragmented_multiline_text_tools_and_collection() {
    let multiline = RESPONSES_EVENTS.replace(
        "data: {\"type\":\"response.created\",\"response\":",
        "data: {\"type\":\"response.created\",\ndata: \"response\":",
    );
    let completed_offset = multiline
        .find("data: {\"type\":\"response.completed\"")
        .expect("fixture completion event");
    let (before_completed, completed) = multiline.split_at(completed_offset);
    let mut parser = new_parser();
    let mut events = Vec::new();

    for byte in before_completed.as_bytes() {
        events.extend(
            parser
                .feed(std::slice::from_ref(byte))
                .expect("event parses"),
        );
    }
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, NormalizedEvent::ChatCompleted { .. }))
    );
    assert!(parser.take_chat_response().is_none());

    for byte in completed.as_bytes() {
        events.extend(
            parser
                .feed(std::slice::from_ref(byte))
                .expect("completion and optional DONE parse"),
        );
    }
    parser.finish_input().expect("complete SSE body");

    assert_eq!(
        events,
        vec![
            NormalizedEvent::ChatStarted {
                model: ModelAlias("codex-chat".into()),
            },
            NormalizedEvent::ChatTextDelta {
                text: "TEST_ONLY_TEXT_NOT_SECRET_0001".into(),
            },
            NormalizedEvent::ChatToolCallDelta {
                call_id: "TEST_ONLY_CALL_ID_NOT_SECRET_0001".into(),
                name: Some("lookup".into()),
                arguments_delta: String::new(),
            },
            NormalizedEvent::ChatToolCallDelta {
                call_id: "TEST_ONLY_CALL_ID_NOT_SECRET_0001".into(),
                name: None,
                arguments_delta: "{\"query\":".into(),
            },
            NormalizedEvent::ChatToolCallDelta {
                call_id: "TEST_ONLY_CALL_ID_NOT_SECRET_0001".into(),
                name: None,
                arguments_delta: "\"TEST_ONLY_QUERY_NOT_SECRET_0001\"}".into(),
            },
            NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::ToolCalls,
                usage: Some(Usage {
                    input_tokens: 4,
                    output_tokens: 3,
                    total_tokens: 7,
                }),
            },
        ]
    );

    let response = parser.take_chat_response().expect("completed response");
    assert_eq!(response.model, ModelAlias("codex-chat".into()));
    assert_eq!(response.message.role, ChatRole::Assistant);
    assert_eq!(
        response.message.content,
        vec![
            ChatContent::Text {
                text: "TEST_ONLY_TEXT_NOT_SECRET_0001".into(),
            },
            ChatContent::ToolCall {
                call: ToolCall {
                    id: "TEST_ONLY_CALL_ID_NOT_SECRET_0001".into(),
                    name: "lookup".into(),
                    arguments: "{\"query\":\"TEST_ONLY_QUERY_NOT_SECRET_0001\"}".into(),
                },
            },
        ]
    );
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Some(Usage {
            input_tokens: 4,
            output_tokens: 3,
            total_tokens: 7,
        })
    );
    assert!(parser.take_chat_response().is_none());
}

#[test]
fn failure_events_are_redacted_and_distinguish_pre_and_post_output() {
    for fixture in [FAILED_EVENTS, INCOMPLETE_EVENTS] {
        let mut parser = new_parser();
        let error = parser
            .feed(fixture.as_bytes())
            .expect_err("terminal error event");
        assert_eq!(error, ResponsesStreamError::BeforeOutput);
        assert!(!format!("{error:?}").contains("TEST_ONLY_"));
    }

    let output_end = RESPONSES_EVENTS
        .find("data: {\"type\":\"response.output_item.added\"")
        .expect("fixture tool event");
    let mut parser = new_parser();
    let events = parser
        .feed(&RESPONSES_EVENTS.as_bytes()[..output_end])
        .expect("text output prefix");
    assert!(matches!(
        events.as_slice(),
        [
            NormalizedEvent::ChatStarted { .. },
            NormalizedEvent::ChatTextDelta { .. }
        ]
    ));
    let error = parser
        .feed(FAILED_EVENTS.as_bytes())
        .expect_err("failure after output");
    assert_eq!(error, ResponsesStreamError::AfterOutput);
    assert!(!format!("{error:?}").contains("TEST_ONLY_FAILURE_PAYLOAD_NOT_SECRET_0001"));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
    );
}

#[test]
fn contradictory_malformed_and_incomplete_streams_never_complete() {
    let mut malformed = new_parser();
    assert_eq!(
        malformed.feed(b"data: {not-json}\n\n").unwrap_err(),
        ResponsesStreamError::BeforeOutput
    );

    let completed_at = RESPONSES_EVENTS
        .find("data: {\"type\":\"response.completed\"")
        .expect("fixture completion event");
    let (prefix, completed) = RESPONSES_EVENTS.split_at(completed_at);
    let mut parser = new_parser();
    let events = parser.feed(prefix.as_bytes()).expect("valid output prefix");
    assert!(!events.is_empty());

    let contradictory = completed.replacen("\"total_tokens\":7", "\"total_tokens\":8", 1);
    let error = parser
        .feed(contradictory.as_bytes())
        .expect_err("contradictory usage");
    assert_eq!(error, ResponsesStreamError::AfterOutput);
    assert!(!format!("{error:?}").contains("TEST_ONLY_"));

    let completed_at = RESPONSES_EVENTS
        .find("data: {\"type\":\"response.completed\"")
        .expect("fixture completion event");
    let mut parser = new_parser();
    let events = parser
        .feed(&RESPONSES_EVENTS.as_bytes()[..completed_at])
        .expect("fixture truncated before completion");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NormalizedEvent::ChatTextDelta { .. }))
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, NormalizedEvent::ChatCompleted { .. }))
    );
    assert_eq!(
        parser.finish_input().unwrap_err(),
        ResponsesStreamError::AfterOutput
    );
    assert!(parser.take_chat_response().is_none());
}

#[test]
fn terminal_marker_completes_once_and_rejects_reuse() {
    let mut parser = new_parser();
    let events = parser
        .feed(RESPONSES_EVENTS.as_bytes())
        .expect("fixture stream");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
            .count(),
        1
    );
    assert_eq!(
        parser.feed(b"data: [DONE]\n\n").unwrap_err(),
        ResponsesStreamError::AfterOutput
    );
}

#[test]
fn named_events_complete_without_done_and_labels_must_match_json_type() {
    assert!(!NAMED_RESPONSES_EVENTS.contains("[DONE]"));
    let mut parser = new_parser();
    let mut events = Vec::new();
    for byte in NAMED_RESPONSES_EVENTS.as_bytes() {
        events.extend(
            parser
                .feed(std::slice::from_ref(byte))
                .expect("named SSE record agrees with JSON type"),
        );
    }
    parser
        .finish_input()
        .expect("response.completed is terminal without DONE");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
            .count(),
        1
    );
    let response = parser.take_chat_response().expect("collected response");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.message.content,
        vec![
            ChatContent::Text {
                text: "TEST_ONLY_TEXT_NOT_SECRET_0001".into(),
            },
            ChatContent::ToolCall {
                call: ToolCall {
                    id: "TEST_ONLY_CALL_ID_NOT_SECRET_0001".into(),
                    name: "lookup".into(),
                    arguments: "{\"query\":\"TEST_ONLY_QUERY_NOT_SECRET_0001\"}".into(),
                },
            },
        ]
    );

    let mut parser = new_parser();
    parser
        .feed(NAMED_RESPONSES_EVENTS.as_bytes())
        .expect("named stream completes");
    assert!(parser.feed(b"data: [DONE]\n\n").unwrap().is_empty());
    assert_eq!(
        parser.feed(b"data: [DONE]\n\n").unwrap_err(),
        ResponsesStreamError::AfterOutput
    );

    let unknown = "event: response.unknown\ndata: {\"type\":\"response.unknown\"}\n\n";
    let mut parser = new_parser();
    parser
        .feed(format!("{unknown}{NAMED_RESPONSES_EVENTS}").as_bytes())
        .expect("unknown event type is skipped");
    assert!(parser.take_chat_response().is_some());

    for body in [
        NAMED_RESPONSES_EVENTS.replacen("event: response.created", "event: response.completed", 1),
        NAMED_RESPONSES_EVENTS.replacen("event: response.created", "event: response.unknown", 1),
    ] {
        let mut parser = new_parser();
        let error = parser
            .feed(body.as_bytes())
            .expect_err("event line must match JSON type");
        assert_eq!(error, ResponsesStreamError::BeforeOutput);
        assert!(!format!("{error:?}").contains("TEST_ONLY_"));
    }
}

#[test]
fn records_after_completion_are_ignored() {
    let trailing = "event: response.unsupported\ndata: {\"type\":\"response.unsupported\"}\n\n";
    let mut parser = new_parser();
    let body = format!("{NAMED_RESPONSES_EVENTS}{trailing}");
    parser.feed(body.as_bytes()).expect("completed stream");
    parser.finish_input().expect("terminal event seen");
    assert!(parser.take_chat_response().is_some());
}

#[test]
fn event_frames_and_collected_output_are_bounded() {
    let mut oversized_frame = new_parser();
    let oversized = format!("data: {}\n\n", "TEST_ONLY_OVERSIZED_".repeat(250_000));
    let error = oversized_frame
        .feed(oversized.as_bytes())
        .expect_err("oversized SSE line");
    assert_eq!(error, ResponsesStreamError::BeforeOutput);
    assert!(!format!("{error:?}").contains("TEST_ONLY_OVERSIZED_"));

    let mut oversized_output = new_parser();
    let created = RESPONSES_EVENTS
        .lines()
        .next()
        .expect("created fixture event");
    oversized_output
        .feed(format!("{created}\n\n").as_bytes())
        .expect("response created");
    let delta = format!(
        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}\n\n",
        "x".repeat(400_000)
    );
    for _ in 0..2 {
        oversized_output
            .feed(delta.as_bytes())
            .expect("output remains within aggregate bound");
    }
    assert_eq!(
        oversized_output.feed(delta.as_bytes()).unwrap_err(),
        ResponsesStreamError::AfterOutput
    );
}

#[test]
fn full_text_item_lifecycle_translates_and_collects_fragmented_multiline_sse() {
    let completed_offset = TEXT_LIFECYCLE_EVENTS
        .find("data: {\"type\":\"response.completed\"")
        .expect("fixture completion event");
    let mut parser = new_parser();
    let mut events = Vec::new();

    for byte in &TEXT_LIFECYCLE_EVENTS.as_bytes()[..completed_offset] {
        events.extend(
            parser
                .feed(std::slice::from_ref(byte))
                .expect("lifecycle event parses"),
        );
    }
    assert_eq!(
        events,
        vec![
            NormalizedEvent::ChatStarted {
                model: ModelAlias("codex-chat".into()),
            },
            NormalizedEvent::ChatTextDelta {
                text: "TEST_ONLY_LIFECYCLE_TEXT_NOT_SECRET_0001".into(),
            },
        ]
    );
    assert!(parser.take_chat_response().is_none());

    for byte in &TEXT_LIFECYCLE_EVENTS.as_bytes()[completed_offset..] {
        events.extend(
            parser
                .feed(std::slice::from_ref(byte))
                .expect("terminal event parses"),
        );
    }
    parser.finish_input().expect("complete lifecycle stream");
    assert_eq!(
        events.last(),
        Some(&NormalizedEvent::ChatCompleted {
            finish_reason: FinishReason::Stop,
            usage: Some(Usage {
                input_tokens: 5,
                output_tokens: 2,
                total_tokens: 7,
            }),
        })
    );

    let response = parser
        .take_chat_response()
        .expect("collected text response");
    assert_eq!(response.model, ModelAlias("codex-chat".into()));
    assert_eq!(response.message.role, ChatRole::Assistant);
    assert_eq!(
        response.message.content,
        vec![ChatContent::Text {
            text: "TEST_ONLY_LIFECYCLE_TEXT_NOT_SECRET_0001".into(),
        }]
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.usage,
        Some(Usage {
            input_tokens: 5,
            output_tokens: 2,
            total_tokens: 7,
        })
    );
}

#[test]
fn in_progress_events_are_ignored_metadata() {
    let created_end = RESPONSES_EVENTS
        .find("\n\n")
        .expect("created event boundary")
        + 2;
    let (created, remaining) = RESPONSES_EVENTS.split_at(created_end);
    let mut baseline = new_parser();
    let expected_events = baseline
        .feed(RESPONSES_EVENTS.as_bytes())
        .expect("baseline fixture stream");
    let expected_response = baseline.take_chat_response().expect("baseline collection");

    for fixture in [IN_PROGRESS_NAMED, IN_PROGRESS_UNNAMED] {
        let mut parser = new_parser();
        assert!(
            parser
                .feed(created.as_bytes())
                .expect("response created")
                .is_empty()
        );
        let progress_events = parser
            .feed(fixture.as_bytes())
            .expect("in-progress metadata matches created response");
        assert!(progress_events.is_empty());
        assert!(parser.take_chat_response().is_none());

        let events = parser
            .feed(remaining.as_bytes())
            .expect("remaining output completes normally");
        parser
            .finish_input()
            .expect("completed response is terminal");
        assert_eq!(events, expected_events);
        assert_eq!(
            parser.take_chat_response().expect("completed response"),
            expected_response
        );
    }
}

#[test]
fn tool_delta_metadata_matches_the_added_item_and_sequence() {
    let with_metadata = RESPONSES_EVENTS
        .replacen(
            "\"type\":\"response.created\",\"response\"",
            "\"type\":\"response.created\",\"sequence_number\":0,\"response\"",
            1,
        )
        .replacen(
            "\"type\":\"response.output_item.added\",\"output_index\":0",
            "\"type\":\"response.output_item.added\",\"output_index\":0,\"sequence_number\":1",
            1,
        )
        .replacen(
            "\"type\":\"response.function_call_arguments.delta\",\"output_index\":0",
            "\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"TEST_ONLY_FUNCTION_CALL_ID_NOT_SECRET_0001\",\"sequence_number\":2",
            1,
        )
        .replacen(
            "\"type\":\"response.function_call_arguments.done\",\"output_index\":0",
            "\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"item_id\":\"TEST_ONLY_FUNCTION_CALL_ID_NOT_SECRET_0001\",\"sequence_number\":3",
            1,
        )
        .replacen(
            "\"type\":\"response.completed\",\"response\"",
            "\"type\":\"response.completed\",\"sequence_number\":4,\"response\"",
            1,
        );
    let mut parser = new_parser();
    let events = parser
        .feed(with_metadata.as_bytes())
        .expect("source-backed tool metadata parses");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
            .count(),
        1
    );

    let wrong_item = with_metadata.replacen(
        "\"item_id\":\"TEST_ONLY_FUNCTION_CALL_ID_NOT_SECRET_0001\"",
        "\"item_id\":\"TEST_ONLY_OTHER_CALL_ID_NOT_SECRET_0001\"",
        1,
    );
    let mut parser = new_parser();
    let error = parser
        .feed(wrong_item.as_bytes())
        .expect_err("mismatched function item identity");
    assert_eq!(error, ResponsesStreamError::BeforeOutput);
    assert!(!format!("{error:?}").contains("TEST_ONLY_"));
}

#[test]
fn text_lifecycle_rejects_contradictory_metadata() {
    let wrong_item = TEXT_LIFECYCLE_EVENTS.replacen(
        "\"type\":\"response.output_text.delta\",\"item_id\":\"TEST_ONLY_TEXT_ITEM_ID_NOT_SECRET_0001\"",
        "\"type\":\"response.output_text.delta\",\"item_id\":\"TEST_ONLY_OTHER_ITEM_ID_NOT_SECRET_0001\"",
        1,
    );
    let wrong_output_index = TEXT_LIFECYCLE_EVENTS.replacen(
        "\"type\":\"response.output_text.delta\",\"item_id\":\"TEST_ONLY_TEXT_ITEM_ID_NOT_SECRET_0001\",\"output_index\":0",
        "\"type\":\"response.output_text.delta\",\"item_id\":\"TEST_ONLY_TEXT_ITEM_ID_NOT_SECRET_0001\",\"output_index\":1",
        1,
    );
    let out_of_order =
        TEXT_LIFECYCLE_EVENTS.replacen("\"sequence_number\":3", "\"sequence_number\":2", 1);
    let item_done_at = TEXT_LIFECYCLE_EVENTS
        .find("data: {\"type\":\"response.output_item.done\"")
        .expect("output item done event");
    let (before_item_done, item_done_and_after) = TEXT_LIFECYCLE_EVENTS.split_at(item_done_at);
    let mismatched_item_text = format!(
        "{before_item_done}{}",
        item_done_and_after.replacen(
            "TEST_ONLY_LIFECYCLE_TEXT_NOT_SECRET_0001",
            "TEST_ONLY_DIFFERENT_ITEM_TEXT_NOT_SECRET_0001",
            1,
        )
    );

    for body in [
        wrong_item,
        wrong_output_index,
        out_of_order,
        mismatched_item_text,
    ] {
        let mut parser = new_parser();
        let error = parser
            .feed(body.as_bytes())
            .expect_err("contradictory lifecycle must fail closed");
        assert_eq!(error, ResponsesStreamError::BeforeOutput);
        assert!(!format!("{error:?}").contains("TEST_ONLY_"));
    }
}

#[test]
fn live_shape_stream_skips_extra_fields_and_events_and_tolerates_snapshot_model() {
    assert!(LIVE_SHAPE_EVENTS.contains("\"model\":\"gpt-6-luna-2026-09-01\""));
    let mut parser = new_parser();
    let mut events = Vec::new();
    for chunk in LIVE_SHAPE_EVENTS.as_bytes().chunks(97) {
        events.extend(parser.feed(chunk).expect("live-shape event parses"));
    }
    parser.finish_input().expect("completed without DONE");

    let call_id = "call_TEST_ONLY_CALL_ID_NOT_SECRET_0003";
    let usage = Some(Usage {
        input_tokens: 120,
        output_tokens: 48,
        total_tokens: 168,
    });
    assert_eq!(
        events,
        vec![
            NormalizedEvent::ChatStarted {
                model: ModelAlias("codex-chat".into()),
            },
            NormalizedEvent::ChatTextDelta {
                text: "TEST_ONLY_LIVE_TEXT_".into(),
            },
            NormalizedEvent::ChatTextDelta {
                text: "NOT_SECRET_0001".into(),
            },
            NormalizedEvent::ChatToolCallDelta {
                call_id: call_id.into(),
                name: Some("lookup".into()),
                arguments_delta: String::new(),
            },
            NormalizedEvent::ChatToolCallDelta {
                call_id: call_id.into(),
                name: None,
                arguments_delta: "{\"query\":".into(),
            },
            NormalizedEvent::ChatToolCallDelta {
                call_id: call_id.into(),
                name: None,
                arguments_delta: "\"TEST_ONLY_LIVE_QUERY_NOT_SECRET_0001\"}".into(),
            },
            NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::ToolCalls,
                usage: usage.clone(),
            },
        ]
    );
    assert!(!format!("{events:?}").contains("SUMMARY"));

    let response = parser.take_chat_response().expect("collected response");
    assert_eq!(response.model, ModelAlias("codex-chat".into()));
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.usage, usage);
    assert_eq!(
        response.message.content,
        vec![
            ChatContent::Text {
                text: "TEST_ONLY_LIVE_TEXT_NOT_SECRET_0001".into(),
            },
            ChatContent::ToolCall {
                call: ToolCall {
                    id: call_id.into(),
                    name: "lookup".into(),
                    arguments: "{\"query\":\"TEST_ONLY_LIVE_QUERY_NOT_SECRET_0001\"}".into(),
                },
            },
        ]
    );
}

#[test]
fn output_items_without_deltas_are_emitted_from_done_items() {
    let only_done_items: String = LIVE_SHAPE_EVENTS
        .split_inclusive("\n\n")
        .filter(|record| {
            !record.contains("\"type\":\"response.output_text.delta\"")
                && !record.contains("\"type\":\"response.output_item.added\"")
                && !record.contains("\"type\":\"response.function_call_arguments.")
        })
        .collect();
    let mut parser = new_parser();
    let events = parser
        .feed(only_done_items.as_bytes())
        .expect("done items parse");
    parser.finish_input().expect("completed stream");
    assert_eq!(
        events,
        vec![
            NormalizedEvent::ChatStarted {
                model: ModelAlias("codex-chat".into()),
            },
            NormalizedEvent::ChatTextDelta {
                text: "TEST_ONLY_LIVE_TEXT_NOT_SECRET_0001".into(),
            },
            NormalizedEvent::ChatToolCallDelta {
                call_id: "call_TEST_ONLY_CALL_ID_NOT_SECRET_0003".into(),
                name: Some("lookup".into()),
                arguments_delta: "{\"query\":\"TEST_ONLY_LIVE_QUERY_NOT_SECRET_0001\"}".into(),
            },
            NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::ToolCalls,
                usage: Some(Usage {
                    input_tokens: 120,
                    output_tokens: 48,
                    total_tokens: 168,
                }),
            },
        ]
    );
}

#[test]
fn completed_response_id_must_match_created() {
    let body = LIVE_SHAPE_EVENTS.replacen(
        "\"type\":\"response.completed\",\"sequence_number\":23,\"response\":{\"id\":\"resp_TEST_ONLY_RESPONSE_ID_NOT_SECRET_0003\"",
        "\"type\":\"response.completed\",\"sequence_number\":23,\"response\":{\"id\":\"resp_TEST_ONLY_OTHER_ID_NOT_SECRET_0003\"",
        1,
    );
    assert_ne!(body, LIVE_SHAPE_EVENTS);
    let mut parser = new_parser();
    assert_eq!(
        parser.feed(body.as_bytes()).unwrap_err(),
        ResponsesStreamError::BeforeOutput
    );
    assert!(parser.take_chat_response().is_none());
}

#[test]
fn completed_requires_created_and_finished_items() {
    let created_end = LIVE_SHAPE_EVENTS.find("\n\n").expect("created boundary") + 2;
    let drop_records = |needles: &[&str]| -> String {
        LIVE_SHAPE_EVENTS
            .split_inclusive("\n\n")
            .filter(|record| !needles.iter().any(|needle| record.contains(needle)))
            .collect()
    };
    for body in [
        LIVE_SHAPE_EVENTS[created_end..].to_owned(),
        drop_records(&[
            "\"type\":\"response.function_call_arguments.done\"",
            "\"type\":\"response.output_item.done\",\"sequence_number\":21",
        ]),
        drop_records(&["\"type\":\"response.output_item.done\",\"sequence_number\":16"]),
    ] {
        let mut parser = new_parser();
        assert!(parser.feed(body.as_bytes()).is_err());
        assert_eq!(
            parser.failure().map(|failure| failure.event.as_str()),
            Some("response.completed")
        );
        assert!(parser.take_chat_response().is_none());
    }
}
