#[path = "support/gateway.rs"]
mod support;

use kanata::{
    adapter::{Adapter, ollama::OllamaAdapter, vllm::VllmAdapter},
    config,
    core::{
        AudioValidationError, Capabilities, CapabilityError, ChatContent, ChatMessage, ChatRequest,
        ChatRole, ErrorKind, Extensions, FunctionTool, InputAudioFormat, MAX_INPUT_AUDIO_BYTES,
        ModelAlias, Operation, Request, RequestContext, RequestValidationError, RouteIdentity,
        RoutedRequest, RoutedRequestError, ToolCall, ToolChoice, TrustZone, ValidatedAudio,
    },
};

fn audio() -> ValidatedAudio {
    ValidatedAudio::new(InputAudioFormat::Wav, b"audio-fixture".to_vec())
        .expect("audio fixture is bounded")
}

fn chat_request(has_audio: bool, stream: bool, with_tools: bool) -> Request {
    let mut content = vec![ChatContent::Text {
        text: "transcribe this".into(),
    }];
    if has_audio {
        content.push(ChatContent::InputAudio { audio: audio() });
    }
    Request::Chat(ChatRequest {
        model: ModelAlias("audio-chat".into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content,
        }],
        tools: with_tools
            .then_some(FunctionTool {
                name: "lookup".into(),
                description: None,
                parameters: serde_json::json!({"type":"object"}),
            })
            .into_iter()
            .collect(),
        tool_choice: ToolChoice::Auto,
        stream,
        extensions: Extensions::default(),
        options: Default::default(),
    })
}

fn context(route: RouteIdentity) -> RequestContext {
    RequestContext {
        request_id: "audio-contract-test".into(),
        route,
        trust_zone: TrustZone::Local,
        extensions: Extensions::default(),
    }
}

#[test]
fn validated_audio_is_nonempty_bounded_typed_and_debug_redacted() {
    assert_eq!(
        ValidatedAudio::with_max_bytes(InputAudioFormat::Wav, Vec::new(), 32),
        Err(AudioValidationError::EmptyBytes)
    );
    assert_eq!(
        ValidatedAudio::with_max_bytes(InputAudioFormat::Mp3, vec![1, 2], 1),
        Err(AudioValidationError::TooLarge)
    );
    let audio =
        ValidatedAudio::new(InputAudioFormat::Mp3, b"AUDIO_SECRET".to_vec()).expect("valid audio");
    let debug = format!("{audio:?}");
    assert!(debug.contains("byte_len: 12"));
    assert!(!debug.contains("AUDIO_SECRET"));

    for invalid in [
        r#"{"format":"flac","bytes":[1]}"#,
        r#"{"format":"wav","bytes":[]}"#,
    ] {
        assert!(serde_json::from_str::<ValidatedAudio>(invalid).is_err());
    }
}

#[test]
fn audio_ir_is_user_only_and_enforces_an_aggregate_decoded_limit() {
    let mut assistant_audio = match chat_request(true, false, false) {
        Request::Chat(chat) => chat,
        _ => unreachable!(),
    };
    assistant_audio.messages[0].role = ChatRole::Assistant;
    let role_error = RoutedRequest::new(
        context(RouteIdentity::new(
            "audio-route",
            "audio-upstream",
            ModelAlias("audio-chat".into()),
            Operation::Chat,
        )),
        Request::Chat(assistant_audio),
    );
    assert!(matches!(
        role_error,
        Err(RoutedRequestError::Request(
            RequestValidationError::InputAudioRoleMismatch
        ))
    ));

    let half_plus_one = MAX_INPUT_AUDIO_BYTES / 2 + 1;
    let chunk = || {
        ValidatedAudio::with_max_bytes(
            InputAudioFormat::Wav,
            vec![0; half_plus_one],
            MAX_INPUT_AUDIO_BYTES,
        )
        .expect("each audio part is under the per-part ceiling")
    };
    let request = Request::Chat(ChatRequest {
        model: ModelAlias("audio-chat".into()),
        messages: [
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatContent::InputAudio { audio: chunk() }],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatContent::InputAudio { audio: chunk() }],
            },
        ]
        .into(),
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    });
    assert!(matches!(
        RoutedRequest::new(
            context(RouteIdentity::new(
                "audio-route",
                "audio-upstream",
                ModelAlias("audio-chat".into()),
                Operation::Chat,
            )),
            request,
        ),
        Err(RoutedRequestError::Request(
            RequestValidationError::InputAudioTooLarge
        ))
    ));
}

#[test]
fn audio_stream_and_tool_history_need_independent_capabilities() {
    let mut capabilities = Capabilities::new([Operation::Chat]);
    capabilities.streaming_chat = true;
    capabilities.function_tools = true;
    assert_eq!(
        capabilities.check_request(&chat_request(true, false, false)),
        Err(CapabilityError::InputAudioUnavailable)
    );

    capabilities.input_audio = true;
    assert_eq!(
        capabilities.check_request(&chat_request(true, true, false)),
        Err(CapabilityError::AudioStreamingUnavailable)
    );
    assert_eq!(
        capabilities.check_request(&chat_request(true, false, true)),
        Err(CapabilityError::AudioFunctionToolsUnavailable)
    );

    let mut history = match chat_request(true, false, false) {
        Request::Chat(chat) => chat,
        _ => unreachable!(),
    };
    history.messages.push(ChatMessage {
        role: ChatRole::Assistant,
        content: vec![ChatContent::ToolCall {
            call: ToolCall {
                id: "call-1".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
            },
        }],
    });
    assert_eq!(
        capabilities.check_request(&Request::Chat(history)),
        Err(CapabilityError::AudioFunctionToolsUnavailable)
    );

    assert_eq!(
        capabilities.check_request(&chat_request(false, true, true)),
        Ok(())
    );
}

#[tokio::test]
async fn text_only_ollama_and_vllm_reject_audio_before_network_dispatch() {
    let config = config::load("config/personal.example.toml").expect("text-only config");
    let routes = ["local-chat", "private-chat-a"];
    for (adapter_index, route_alias) in [(0, routes[0]), (1, routes[1])] {
        let adapter_config = &config.adapters()[adapter_index];
        let route = config
            .routes()
            .iter()
            .find(|route| route.identity().selector.model_alias.0 == route_alias)
            .expect("configured route");
        let adapter: Box<dyn Adapter> = match adapter_config.kind() {
            config::ProviderKind::Ollama => Box::new(
                OllamaAdapter::new_for_route(
                    adapter_config,
                    route,
                    config.timeouts(),
                    config.limits(),
                )
                .expect("text-only Ollama adapter"),
            ),
            config::ProviderKind::Vllm => Box::new(
                VllmAdapter::new_for_route(
                    adapter_config,
                    route,
                    config.timeouts(),
                    config.limits(),
                )
                .expect("text-only vLLM adapter"),
            ),
            _ => unreachable!("selected adapters are local text providers"),
        };
        let mut request = match chat_request(true, false, false) {
            Request::Chat(chat) => chat,
            _ => unreachable!(),
        };
        request.model = route.identity().selector.model_alias.clone();
        let routed = RoutedRequest::new(context(route.identity().clone()), Request::Chat(request))
            .expect("audio request has valid core shape");
        let error = match adapter.execute(routed).await {
            Err(error) => error,
            Ok(_) => panic!("text-only adapter unexpectedly accepted audio"),
        };
        assert_eq!(error.kind, ErrorKind::UnsupportedOperation);
    }
}

#[tokio::test]
async fn api_wire_rejects_input_audio_without_dispatch_or_echoing_payload() {
    let config = support::config();
    let adapters = config
        .adapters()
        .iter()
        .map(|adapter| {
            support::adapter_spec(
                adapter.id(),
                support::capabilities(&config, adapter.id()),
                support::chat_outcome("text only"),
            )
        })
        .collect();
    let (server, requests) = support::server_with(&config, adapters);
    let response = server
        .client_oneshot(support::chat_request(
            r#"{"model":"local-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AUDIO_SECRET_MARKER","format":"wav"}}]}]}"#,
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
    let body = support::response_body(response).await;
    assert!(!String::from_utf8_lossy(&body).contains("AUDIO_SECRET_MARKER"));
    assert_eq!(support::recorded_len(&requests), 0);
}
