use kanata::{
    adapter::{Adapter, AdapterOutput, openrouter::OpenRouterAdapter},
    core::{
        ChatContent, ChatMessage, ChatRole, ErrorKind, Extensions, FunctionTool, InputAudioFormat,
        ModelAlias, Operation, Request as CoreRequest, ToolCall, ToolChoice, TrustZone,
        ValidatedAudio,
    },
};

use crate::support::{
    ADAPTER_ID, ROUTE_ID, SyntheticResolver, adapter, config_for, config_with, routed, routed_with,
    text_request, transcription_route,
};

fn error_kind(result: Result<AdapterOutput, kanata::core::GatewayError>) -> ErrorKind {
    match result {
        Err(error) => error.kind,
        Ok(AdapterOutput::Complete(_)) | Ok(AdapterOutput::Events(_)) => {
            panic!("unexpected adapter success")
        }
    }
}

#[test]
fn constructor_declares_only_fixture_proven_nonstream_text_chat() {
    let config = config_for("https://openrouter.invalid/api/v1");
    let adapter = adapter(&config);

    assert_eq!(adapter.id(), ADAPTER_ID);
    assert_eq!(adapter.configured_id(), ADAPTER_ID);
    assert_eq!(
        adapter.capabilities().operations,
        [Operation::Chat].into_iter().collect()
    );
    assert!(!adapter.capabilities().streaming_chat);
    assert!(!adapter.capabilities().function_tools);
    assert!(!adapter.capabilities().input_audio);
    assert!(!format!("{adapter:?}").contains("fixture-openrouter-key"));
}

#[test]
fn constructor_declares_configured_text_streaming_without_other_features() {
    let config = config_with(
        "https://openrouter.invalid/api/v1",
        "chat",
        "chat",
        true,
        false,
        false,
    );
    let adapter = adapter(&config);

    assert!(adapter.capabilities().streaming_chat);
    assert!(!adapter.capabilities().function_tools);
    assert!(!adapter.capabilities().input_audio);
}

#[test]
fn constructor_rejects_function_tools() {
    let config = config_with(
        "https://openrouter.invalid/api/v1",
        "chat",
        "chat",
        false,
        true,
        false,
    );
    assert!(
        OpenRouterAdapter::from_config(&config, ADAPTER_ID, ROUTE_ID, &SyntheticResolver).is_err()
    );
}

#[test]
fn constructor_accepts_input_audio_and_transcription() {
    let audio = config_with(
        "https://openrouter.invalid/api/v1",
        "chat",
        "chat",
        false,
        false,
        true,
    );
    assert!(adapter(&audio).capabilities().input_audio);
    let transcription = config_with(
        "https://openrouter.invalid/api/v1",
        "transcription",
        "transcription",
        false,
        false,
        false,
    );
    assert!(
        adapter(&transcription)
            .capabilities()
            .operations
            .contains(&Operation::Transcription)
    );
}

#[tokio::test]
async fn unsupported_requests_extensions_and_unbound_routes_reject_before_dispatch() {
    let config = config_for("https://openrouter.invalid/api/v1");
    let adapter = adapter(&config);
    let route = config.routes()[0].identity().clone();

    let mut streaming = text_request();
    let CoreRequest::Chat(chat) = &mut streaming else {
        panic!("chat request")
    };
    chat.stream = true;

    let mut with_tools = text_request();
    let CoreRequest::Chat(chat) = &mut with_tools else {
        panic!("chat request")
    };
    chat.tools.push(FunctionTool {
        name: "lookup".into(),
        description: None,
        parameters: serde_json::json!({"type":"object"}),
    });

    let tool_history = CoreRequest::Chat(kanata::core::ChatRequest {
        model: ModelAlias("openrouter-public".into()),
        messages: vec![ChatMessage {
            role: ChatRole::Assistant,
            content: vec![ChatContent::ToolCall {
                call: ToolCall {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{}".into(),
                },
            }],
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    });

    let audio = CoreRequest::Chat(kanata::core::ChatRequest {
        model: ModelAlias("openrouter-public".into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatContent::InputAudio {
                audio: ValidatedAudio::new(InputAudioFormat::Wav, vec![1, 2, 3])
                    .unwrap_or_else(|_| panic!("audio")),
            }],
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    });

    let mut with_extension = text_request();
    let CoreRequest::Chat(chat) = &mut with_extension else {
        panic!("chat request")
    };
    chat.extensions
        .insert(
            kanata::core::ExtensionKey::parse("fixture.marker")
                .unwrap_or_else(|_| panic!("extension key")),
            serde_json::json!("value"),
        )
        .unwrap_or_else(|_| panic!("extension"));

    let transcription = CoreRequest::Transcription(kanata::core::TranscriptionRequest {
        model: ModelAlias("openrouter-public".into()),
        file: kanata::core::ValidatedFile::new("audio.wav", "audio/wav", vec![1])
            .unwrap_or_else(|_| panic!("file")),
        language: None,
        prompt: None,
        extensions: Extensions::default(),
    });

    let cases = [
        (ErrorKind::UnsupportedOperation, routed(&config, streaming)),
        (ErrorKind::UnsupportedOperation, routed(&config, with_tools)),
        (
            ErrorKind::UnsupportedOperation,
            routed(&config, tool_history),
        ),
        (ErrorKind::UnsupportedOperation, routed(&config, audio)),
        (ErrorKind::InvalidRequest, routed(&config, with_extension)),
        (
            ErrorKind::UnsupportedOperation,
            routed_with(
                transcription_route(),
                TrustZone::External,
                Extensions::default(),
                transcription,
            ),
        ),
        (
            ErrorKind::InvalidRequest,
            routed_with(
                kanata::core::RouteIdentity::new(
                    "other-route",
                    "openai/gpt-4o-mini",
                    ModelAlias("openrouter-public".into()),
                    Operation::Chat,
                ),
                TrustZone::External,
                Extensions::default(),
                text_request(),
            ),
        ),
        (
            ErrorKind::InvalidRequest,
            routed_with(
                route,
                TrustZone::Local,
                Extensions::default(),
                text_request(),
            ),
        ),
    ];

    for (expected, request) in cases {
        assert_eq!(error_kind(adapter.execute(request).await), expected);
    }
}
