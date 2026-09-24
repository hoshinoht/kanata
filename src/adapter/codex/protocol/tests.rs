use serde_json::{Value, json};

use crate::config::{self, CodexReasoningEffort};
use crate::core::{
    ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, Extensions, FunctionTool,
    InputAudioFormat, ModelAlias, Operation, Request, RequestContext, RouteIdentity, RoutedRequest,
    ToolCall, ToolChoice, TrustZone, ValidatedAudio, ValidatedFile,
};

use super::to_responses_request as map_responses_request;

const SANCTIONED_REQUEST: &str =
    include_str!("../../../../tests/fixtures/codex/responses-request.json");

fn context(trust_zone: TrustZone, upstream_id: &str) -> RequestContext {
    RequestContext {
        request_id: "TEST_ONLY_REQUEST_ID_NOT_SECRET_0001".into(),
        route: RouteIdentity::new(
            "route-codex-chat",
            upstream_id,
            ModelAlias("codex-chat".into()),
            Operation::Chat,
        ),
        trust_zone,
        extensions: Extensions::default(),
    }
}

fn chat(messages: Vec<ChatMessage>) -> ChatRequest {
    ChatRequest {
        model: ModelAlias("codex-chat".into()),
        messages,
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    }
}

fn message(role: ChatRole, text: &str) -> ChatMessage {
    ChatMessage {
        role,
        content: vec![ChatContent::Text { text: text.into() }],
    }
}

fn route(chat: ChatRequest, context: RequestContext) -> RoutedRequest {
    RoutedRequest::new(context, Request::Chat(chat)).expect("test route is valid")
}

fn to_responses_request(routed: &RoutedRequest) -> Result<Value, crate::core::GatewayError> {
    map_responses_request(routed, CodexReasoningEffort::Medium)
}

fn error_kind(result: Result<Value, crate::core::GatewayError>) -> ErrorKind {
    result.expect_err("request should be rejected").kind
}

#[test]
fn simple_text_and_function_tool_match_sanitized_private_request_fixture() {
    let mut chat = chat(vec![message(
        ChatRole::User,
        "TEST_ONLY_PROMPT_NOT_SECRET_0001",
    )]);
    chat.tools.push(FunctionTool {
        name: "lookup".into(),
        description: Some("TEST_ONLY_TOOL_DESCRIPTION_NOT_SECRET_0001".into()),
        parameters: json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"],
            "additionalProperties": false,
        }),
    });
    chat.tool_choice = ToolChoice::Function {
        name: "lookup".into(),
    };

    let fixture: Value = serde_json::from_str(SANCTIONED_REQUEST).expect("fixture parses");
    let routed = route(chat, context(TrustZone::External, "fixture-codex-model"));
    let config =
        config::load("tests/fixtures/config/example.toml").expect("baseline config validates");
    let reasoning_effort = config
        .routes()
        .iter()
        .find(|route| route.identity().route_id == "codex-chat")
        .and_then(|route| route.codex_reasoning_effort())
        .expect("unsuffixed Codex route defaults to medium");
    let actual = map_responses_request(&routed, reasoning_effort).expect("text chat maps");
    assert_eq!(actual, fixture["body"]);
}

#[test]
fn configured_low_alias_maps_effort_and_explicit_upstream_id_verbatim() {
    let config = config::load("config/personal.example.toml").expect("personal config validates");
    let configured_route = config
        .routes()
        .iter()
        .find(|route| route.identity().route_id == "codex-gpt-6-luna-low")
        .expect("low-effort route is configured");
    let reasoning_effort = configured_route
        .codex_reasoning_effort()
        .expect("validated route carries effort");
    let mut request_context = context(
        TrustZone::External,
        &configured_route.identity().upstream_id,
    );
    request_context.route = configured_route.identity().clone();
    let mut chat = chat(vec![message(ChatRole::User, "question")]);
    chat.model = ModelAlias("gpt-6-luna:low".into());

    let actual = map_responses_request(&route(chat, request_context), reasoning_effort)
        .expect("low-effort chat maps");

    assert_eq!(actual["model"], "gpt-6-luna");
    assert_eq!(actual["reasoning"]["effort"], "low");
    assert_ne!(actual["model"], "gpt-6-luna:low");
}

#[test]
fn model_id_is_not_rewritten_and_instructions_preserve_message_order() {
    let mut chat = chat(vec![
        message(ChatRole::System, "first"),
        message(ChatRole::User, "question"),
        message(ChatRole::Developer, "second"),
        message(ChatRole::Assistant, "prior answer"),
    ]);
    chat.model = ModelAlias("codex-chat".into());
    let actual = to_responses_request(&route(
        chat,
        context(TrustZone::External, "openai/model-1m-fast"),
    ))
    .expect("chat maps");

    assert_eq!(actual["model"], "openai/model-1m-fast");
    assert_eq!(actual["instructions"], "first\n\nsecond");
    assert_eq!(actual["input"][0]["role"], "user");
    assert_eq!(actual["input"][1]["role"], "assistant");
}

#[test]
fn assistant_tool_history_and_results_preserve_call_ids_and_order() {
    let mut chat = chat(vec![
        message(ChatRole::User, "look this up"),
        ChatMessage {
            role: ChatRole::Assistant,
            content: vec![
                ChatContent::Text {
                    text: "checking".into(),
                },
                ChatContent::ToolCall {
                    call: ToolCall {
                        id: "call_lookup_1".into(),
                        name: "lookup".into(),
                        arguments: "{\"q\":\"kanata\"}".into(),
                    },
                },
            ],
        },
        ChatMessage {
            role: ChatRole::Tool,
            content: vec![ChatContent::ToolResult {
                call_id: "call_lookup_1".into(),
                content: "result".into(),
            }],
        },
    ]);
    chat.tools.push(FunctionTool {
        name: "lookup".into(),
        description: None,
        parameters: json!({ "type": "object" }),
    });

    let actual = to_responses_request(&route(chat, context(TrustZone::External, "codex-upstream")))
        .expect("tool history maps");
    assert_eq!(actual["input"][1]["content"][0]["text"], "checking");
    assert_eq!(
        actual["input"][2],
        json!({
            "type": "function_call",
            "call_id": "call_lookup_1",
            "name": "lookup",
            "arguments": "{\"q\":\"kanata\"}",
        })
    );
    assert_eq!(
        actual["input"][3],
        json!({
            "type": "function_call_output",
            "call_id": "call_lookup_1",
            "output": "result",
        })
    );
}

#[test]
fn assistant_text_and_tool_calls_preserve_content_order() {
    let history = chat(vec![
        message(ChatRole::User, "question"),
        ChatMessage {
            role: ChatRole::Assistant,
            content: vec![
                ChatContent::Text {
                    text: "before".into(),
                },
                ChatContent::Text {
                    text: " text".into(),
                },
                ChatContent::ToolCall {
                    call: ToolCall {
                        id: "call_first".into(),
                        name: "lookup".into(),
                        arguments: "{}".into(),
                    },
                },
                ChatContent::Text {
                    text: "after".into(),
                },
                ChatContent::Text {
                    text: " call".into(),
                },
                ChatContent::ToolCall {
                    call: ToolCall {
                        id: "call_second".into(),
                        name: "lookup_more".into(),
                        arguments: "{}".into(),
                    },
                },
                ChatContent::Text {
                    text: "tail".into(),
                },
            ],
        },
        ChatMessage {
            role: ChatRole::Tool,
            content: vec![ChatContent::ToolResult {
                call_id: "call_first".into(),
                content: "first result".into(),
            }],
        },
        ChatMessage {
            role: ChatRole::Tool,
            content: vec![ChatContent::ToolResult {
                call_id: "call_second".into(),
                content: "second result".into(),
            }],
        },
    ]);
    let actual = to_responses_request(&route(
        history,
        context(TrustZone::External, "codex-upstream"),
    ))
    .expect("mixed assistant history maps");

    assert_eq!(actual["input"][1]["content"][0]["text"], "before text");
    assert_eq!(actual["input"][2]["type"], "function_call");
    assert_eq!(actual["input"][2]["call_id"], "call_first");
    assert_eq!(actual["input"][3]["content"][0]["text"], "after call");
    assert_eq!(actual["input"][4]["type"], "function_call");
    assert_eq!(actual["input"][4]["call_id"], "call_second");
    assert_eq!(actual["input"][5]["content"][0]["text"], "tail");
    assert_eq!(
        actual["input"][6],
        json!({
            "type": "function_call_output",
            "call_id": "call_first",
            "output": "first result",
        })
    );
    assert_eq!(
        actual["input"][7],
        json!({
            "type": "function_call_output",
            "call_id": "call_second",
            "output": "second result",
        })
    );
}

#[test]
fn every_ir_tool_choice_maps_to_its_private_shape() {
    let cases = [
        (ToolChoice::None, json!("none")),
        (ToolChoice::Auto, json!("auto")),
        (ToolChoice::Required, json!("required")),
        (
            ToolChoice::Function {
                name: "lookup".into(),
            },
            json!({ "type": "function", "name": "lookup" }),
        ),
    ];
    for (choice, expected) in cases {
        let mut chat = chat(vec![message(ChatRole::User, "question")]);
        chat.tools.push(FunctionTool {
            name: "lookup".into(),
            description: None,
            parameters: json!({ "type": "object" }),
        });
        chat.tool_choice = choice;
        let actual =
            to_responses_request(&route(chat, context(TrustZone::External, "codex-upstream")))
                .expect("choice maps");
        assert_eq!(actual["tool_choice"], expected);
    }
}

#[test]
fn default_auto_without_tools_omits_optional_tool_fields() {
    let actual = to_responses_request(&route(
        chat(vec![message(ChatRole::User, "question")]),
        context(TrustZone::External, "codex-upstream"),
    ))
    .expect("text chat maps");
    assert!(actual.get("tools").is_none());
    assert!(actual.get("tool_choice").is_none());
}

#[test]
fn non_external_trust_and_transcription_are_unsupported() {
    for zone in [TrustZone::Local, TrustZone::PrivateNetwork] {
        assert_eq!(
            error_kind(to_responses_request(&route(
                chat(vec![message(ChatRole::User, "question")]),
                context(zone, "codex-upstream"),
            ))),
            ErrorKind::UnsupportedOperation
        );
    }

    let transcription_context = RequestContext {
        route: RouteIdentity::new(
            "route-codex-transcription",
            "codex-upstream",
            ModelAlias("codex-chat".into()),
            Operation::Transcription,
        ),
        ..context(TrustZone::External, "codex-upstream")
    };
    let request = Request::Transcription(crate::core::TranscriptionRequest {
        model: ModelAlias("codex-chat".into()),
        file: ValidatedFile::new("fixture.wav", "audio/wav", vec![1]).expect("valid file"),
        language: None,
        prompt: None,
        extensions: Extensions::default(),
    });
    let routed = RoutedRequest::new(transcription_context, request).expect("route is valid");
    assert_eq!(
        error_kind(to_responses_request(&routed)),
        ErrorKind::UnsupportedOperation
    );
}

#[test]
fn inline_audio_and_extensions_are_rejected() {
    let audio = ValidatedAudio::new(InputAudioFormat::Wav, vec![1, 2]).expect("valid audio");
    let audio_chat = chat(vec![ChatMessage {
        role: ChatRole::User,
        content: vec![ChatContent::InputAudio { audio }],
    }]);
    assert_eq!(
        error_kind(to_responses_request(&route(
            audio_chat,
            context(TrustZone::External, "codex-upstream"),
        ))),
        ErrorKind::UnsupportedOperation
    );

    let mut chat_extension = chat(vec![message(ChatRole::User, "question")]);
    chat_extension
        .extensions
        .insert(
            crate::core::ExtensionKey::parse("io.test.marker").expect("valid extension key"),
            json!(true),
        )
        .expect("extension fits bounds");
    assert_eq!(
        error_kind(to_responses_request(&route(
            chat_extension,
            context(TrustZone::External, "codex-upstream"),
        ))),
        ErrorKind::InvalidRequest
    );

    let mut context_extension = context(TrustZone::External, "codex-upstream");
    context_extension
        .extensions
        .insert(
            crate::core::ExtensionKey::parse("io.test.marker").expect("valid extension key"),
            json!(true),
        )
        .expect("extension fits bounds");
    assert_eq!(
        error_kind(to_responses_request(&route(
            chat(vec![message(ChatRole::User, "question")]),
            context_extension,
        ))),
        ErrorKind::InvalidRequest
    );
}

#[test]
fn invalid_tool_declarations_calls_and_results_are_rejected() {
    let mut bad_name = chat(vec![message(ChatRole::User, "question")]);
    bad_name.tools.push(FunctionTool {
        name: "bad name".into(),
        description: None,
        parameters: json!({ "type": "object" }),
    });
    assert_eq!(
        error_kind(to_responses_request(&route(
            bad_name,
            context(TrustZone::External, "codex-upstream"),
        ))),
        ErrorKind::InvalidRequest
    );

    let mut bad_parameters = chat(vec![message(ChatRole::User, "question")]);
    bad_parameters.tools.push(FunctionTool {
        name: "lookup".into(),
        description: None,
        parameters: json!([]),
    });
    assert_eq!(
        error_kind(to_responses_request(&route(
            bad_parameters,
            context(TrustZone::External, "codex-upstream"),
        ))),
        ErrorKind::InvalidRequest
    );

    for call_id in ["", "contains space", &"x".repeat(129)] {
        let invalid_call = chat(vec![ChatMessage {
            role: ChatRole::Assistant,
            content: vec![ChatContent::ToolCall {
                call: ToolCall {
                    id: call_id.into(),
                    name: "lookup".into(),
                    arguments: "{}".into(),
                },
            }],
        }]);
        assert_eq!(
            error_kind(to_responses_request(&route(
                invalid_call,
                context(TrustZone::External, "codex-upstream"),
            ))),
            ErrorKind::InvalidRequest
        );
    }

    let orphan_result = chat(vec![ChatMessage {
        role: ChatRole::Tool,
        content: vec![ChatContent::ToolResult {
            call_id: "call_missing".into(),
            content: "result".into(),
        }],
    }]);
    assert_eq!(
        error_kind(to_responses_request(&route(
            orphan_result,
            context(TrustZone::External, "codex-upstream"),
        ))),
        ErrorKind::InvalidRequest
    );
}

#[test]
fn empty_upstream_id_is_rejected_without_rewriting_nonempty_values() {
    assert_eq!(
        error_kind(to_responses_request(&route(
            chat(vec![message(ChatRole::User, "question")]),
            context(TrustZone::External, " \t "),
        ))),
        ErrorKind::InvalidRequest
    );
}
