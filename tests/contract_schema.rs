use std::collections::BTreeMap;
use std::future::ready;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use futures_core::Stream;
use kanata::adapter::{Adapter, AdapterFuture, AdapterOutput};
use kanata::core::{
    Capabilities, CapabilityError, ErrorKind, ExtensionError, ExtensionKey, Extensions,
    GatewayError, MAX_EXTENSION_BYTES, MAX_EXTENSION_ENTRIES, ModelAlias, NormalizedEvent,
    Operation, Request, RequestContext, RequestValidationError, Response, RouteError,
    RouteIdentity, RoutedRequest, RoutedRequestError, ToolChoice, TranscriptionResponse, TrustZone,
};

fn fixture(name: &str) -> &'static str {
    match name {
        "chat_request" => include_str!("fixtures/contracts/chat_request.json"),
        "chat_response" => include_str!("fixtures/contracts/chat_response.json"),
        "chat_events" => include_str!("fixtures/contracts/chat_events.json"),
        "transcription_request" => include_str!("fixtures/contracts/transcription_request.json"),
        _ => unreachable!("known fixture"),
    }
}

#[test]
fn sanitized_contract_fixtures_round_trip() {
    let chat: Request = serde_json::from_str(fixture("chat_request")).expect("chat fixture parses");
    let response: Response =
        serde_json::from_str(fixture("chat_response")).expect("response fixture parses");
    let events: Vec<NormalizedEvent> =
        serde_json::from_str(fixture("chat_events")).expect("event fixture parses");
    let transcription: Request = serde_json::from_str(fixture("transcription_request"))
        .expect("transcription fixture parses");

    assert_eq!(
        serde_json::from_value::<Request>(serde_json::to_value(&chat).expect("chat serializes"))
            .expect("chat round trips"),
        chat
    );
    assert_eq!(
        serde_json::from_value::<Response>(
            serde_json::to_value(&response).expect("response serializes")
        )
        .expect("response round trips"),
        response
    );
    assert_eq!(
        serde_json::from_value::<Vec<NormalizedEvent>>(
            serde_json::to_value(&events).expect("events serialize")
        )
        .expect("events round trip"),
        events
    );
    assert_eq!(
        serde_json::from_value::<Request>(
            serde_json::to_value(&transcription).expect("transcription serializes")
        )
        .expect("transcription round trips"),
        transcription
    );
    assert_eq!(chat.operation(), Operation::Chat);
    assert_eq!(transcription.operation(), Operation::Transcription);
}

#[test]
fn extensions_are_namespaced_and_reject_invalid_keys() {
    assert!(ExtensionKey::parse("io.kanata.trace").is_ok());
    assert!(ExtensionKey::parse("trace").is_err());
    assert!(
        serde_json::from_str::<Request>(
            r#"{"operation":"chat","model":"chat-demo","messages":[],"extensions":{"trace":true}}"#,
        )
        .is_err()
    );
}

#[test]
fn extensions_enforce_entry_byte_and_depth_bounds() {
    let mut extensions = Extensions::default();
    for index in 0..MAX_EXTENSION_ENTRIES {
        extensions
            .insert(
                ExtensionKey::parse(format!("io.kanata.key{index}")).expect("key is valid"),
                serde_json::json!(index),
            )
            .expect("entry is within the bound");
    }
    assert_eq!(
        extensions.insert(
            ExtensionKey::parse("io.kanata.extra").expect("key is valid"),
            serde_json::json!(true),
        ),
        Err(ExtensionError::TooManyEntries)
    );

    let oversized = BTreeMap::from([(
        ExtensionKey::parse("io.kanata.large").expect("key is valid"),
        serde_json::json!("x".repeat(MAX_EXTENSION_BYTES)),
    )]);
    assert_eq!(
        Extensions::try_from_map(oversized),
        Err(ExtensionError::TooLarge)
    );

    let deep = BTreeMap::from([(
        ExtensionKey::parse("io.kanata.deep").expect("key is valid"),
        serde_json::json!({ "a": { "b": { "c": { "d": { "e": true } } } } }),
    )]);
    assert_eq!(Extensions::try_from_map(deep), Err(ExtensionError::TooDeep));
}

#[test]
fn canonical_ir_rejects_unknown_fields_and_supports_tool_choice() {
    let request: Request =
        serde_json::from_str(fixture("chat_request")).expect("chat fixture parses");
    let Request::Chat(chat) = request else {
        panic!("fixture is chat");
    };
    assert_eq!(
        chat.tool_choice,
        ToolChoice::Function {
            name: "lookup".into()
        }
    );
    assert_eq!(
        serde_json::to_value(&chat.tool_choice).expect("tool choice serializes"),
        serde_json::json!({ "kind": "function", "name": "lookup" })
    );
    assert!(
        serde_json::from_str::<Request>(
            r#"{"operation":"chat","model":"chat-demo","messages":[],"unsupported":true}"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<ToolChoice>(r#"{"kind":"unsupported"}"#).is_err());
    assert!(
        serde_json::from_str::<ToolChoice>(
            r#"{"kind":"function","name":"lookup","unsupported":true}"#
        )
        .is_err()
    );
}

#[test]
fn transcription_file_deserialization_validates_owned_metadata_and_bytes() {
    for file in [
        r#"{"file_name":" ","media_type":"audio/wav","bytes":[1]}"#,
        r#"{"file_name":"sample.wav","media_type":"invalid","bytes":[1]}"#,
        r#"{"file_name":"sample.wav","media_type":"audio/wav/extra","bytes":[1]}"#,
        r#"{"file_name":"sample.wav","media_type":"audio//wav","bytes":[1]}"#,
        r#"{"file_name":"sample.wav","media_type":"audio/wav","bytes":[]}"#,
    ] {
        let request = format!(
            r#"{{"operation":"transcription","model":"transcription-demo","file":{file}}}"#
        );
        assert!(serde_json::from_str::<Request>(&request).is_err());
    }
}

#[test]
fn routed_request_rejects_invalid_tool_choice_semantics() {
    let required: Request = serde_json::from_str(
        r#"{"operation":"chat","model":"chat-demo","messages":[],"tool_choice":{"kind":"required"}}"#,
    )
    .expect("request parses");
    assert!(matches!(
        RoutedRequest::new(chat_context(), required),
        Err(RoutedRequestError::Request(
            RequestValidationError::RequiredToolChoiceWithoutTools
        ))
    ));

    let named: Request = serde_json::from_str(
        r#"{"operation":"chat","model":"chat-demo","messages":[],"tools":[{"name":"lookup","parameters":{}}],"tool_choice":{"kind":"function","name":"other"}}"#,
    )
    .expect("request parses");
    assert!(matches!(
        RoutedRequest::new(chat_context(), named),
        Err(RoutedRequestError::Request(
            RequestValidationError::UndeclaredToolChoice { name }
        )) if name == "other"
    ));

    for tool_choice in [r#"{"kind":"none"}"#, r#"{"kind":"auto"}"#] {
        let request: Request = serde_json::from_str(&format!(
            r#"{{"operation":"chat","model":"chat-demo","messages":[],"tool_choice":{tool_choice}}}"#
        ))
        .expect("request parses");
        assert!(RoutedRequest::new(chat_context(), request).is_ok());
    }
}

#[test]
fn capabilities_check_request_features_in_a_stable_order() {
    let request: Request =
        serde_json::from_str(fixture("chat_request")).expect("chat fixture parses");
    let capabilities = Capabilities::new([Operation::Chat]);
    assert_eq!(
        capabilities.check_request(&request),
        Err(CapabilityError::StreamingUnavailable)
    );

    let mut capabilities = Capabilities::new([Operation::Chat]);
    capabilities.streaming_chat = true;
    assert_eq!(
        capabilities.check_request(&request),
        Err(CapabilityError::FunctionToolsUnavailable)
    );
}

#[test]
fn errors_have_stable_wire_mapping_and_timeout_phase() {
    let mapping = ErrorKind::Timeout {
        phase: kanata::core::TimeoutPhase::FirstByte,
    }
    .mapping();
    assert_eq!(
        (mapping.status, mapping.code, mapping.error_type),
        (504, "upstream_timeout", "api_error")
    );
}

struct MockAdapter {
    capabilities: Capabilities,
    events: bool,
}

struct EmptyEvents;

impl Stream for EmptyEvents {
    type Item = Result<NormalizedEvent, GatewayError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

impl Adapter for MockAdapter {
    fn id(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, _request: RoutedRequest) -> AdapterFuture {
        if self.events {
            return Box::pin(ready(Ok(AdapterOutput::Events(Box::pin(EmptyEvents)))));
        }
        Box::pin(ready(Ok(AdapterOutput::Complete(Response::Transcription(
            TranscriptionResponse {
                text: "fixture".into(),
            },
        )))))
    }
}

fn accepts_dyn_adapter(_: &dyn Adapter) {}

fn transcription_context() -> RequestContext {
    RequestContext {
        request_id: "request-fixture".into(),
        route: RouteIdentity::new(
            "route-transcription",
            "upstream-transcription-01",
            ModelAlias("transcription-demo".into()),
            Operation::Transcription,
        ),
        trust_zone: TrustZone::PrivateNetwork,
        extensions: Default::default(),
    }
}

fn chat_context() -> RequestContext {
    RequestContext {
        request_id: "request-fixture".into(),
        route: RouteIdentity::new(
            "route-chat",
            "upstream-chat-01",
            ModelAlias("chat-demo".into()),
            Operation::Chat,
        ),
        trust_zone: TrustZone::PrivateNetwork,
        extensions: Default::default(),
    }
}

#[test]
fn adapter_is_object_safe_and_returns_boxed_future() {
    let adapter: Box<dyn Adapter> = Box::new(MockAdapter {
        capabilities: Capabilities::new([Operation::Transcription]),
        events: false,
    });
    accepts_dyn_adapter(adapter.as_ref());
    assert_eq!(adapter.id(), "mock");
    assert!(
        adapter
            .capabilities()
            .operations
            .contains(&Operation::Transcription)
    );

    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let request: Request =
        serde_json::from_str(fixture("transcription_request")).expect("fixture parses");
    let routed = RoutedRequest::new(transcription_context(), request).expect("route is valid");
    let mut future = adapter.execute(routed);
    assert!(matches!(
        future.as_mut().poll(&mut context),
        Poll::Ready(Ok(AdapterOutput::Complete(_)))
    ));

    let stream_adapter: Box<dyn Adapter> = Box::new(MockAdapter {
        capabilities: Capabilities::new([Operation::Chat]),
        events: true,
    });
    let request: Request = serde_json::from_str(fixture("chat_request")).expect("fixture parses");
    let routed = RoutedRequest::new(chat_context(), request).expect("route is valid");
    let mut future = stream_adapter.execute(routed);
    let Poll::Ready(Ok(AdapterOutput::Events(mut events))) = future.as_mut().poll(&mut context)
    else {
        panic!("adapter should return a boxed event stream");
    };
    assert!(matches!(
        events.as_mut().poll_next(&mut context),
        Poll::Ready(None)
    ));
}

#[test]
fn context_carries_exact_route_identity_without_auth_material() {
    let context = RequestContext {
        request_id: "request-fixture".into(),
        route: RouteIdentity::new(
            "route-chat",
            "upstream-chat-01",
            ModelAlias("chat-demo".into()),
            Operation::Chat,
        ),
        trust_zone: TrustZone::PrivateNetwork,
        extensions: Default::default(),
    };
    assert_eq!(context.route.selector.model_alias.0, "chat-demo");
    assert_eq!(context.route.selector.operation, Operation::Chat);
    let chat: Request = serde_json::from_str(fixture("chat_request")).expect("fixture parses");
    let transcription: Request =
        serde_json::from_str(fixture("transcription_request")).expect("fixture parses");
    assert_eq!(context.check_request(&chat), Ok(()));
    assert_eq!(
        context.check_request(&transcription),
        Err(RouteError::OperationMismatch {
            route_operation: Operation::Chat,
            request_operation: Operation::Transcription,
        })
    );
    let mismatched_model_context = RequestContext {
        route: RouteIdentity::new(
            "route-other-chat",
            "upstream-chat-02",
            ModelAlias("other-chat-demo".into()),
            Operation::Chat,
        ),
        ..context.clone()
    };
    assert_eq!(
        mismatched_model_context.check_request(&chat),
        Err(RouteError::ModelAliasMismatch {
            route_model_alias: ModelAlias("other-chat-demo".into()),
            request_model_alias: ModelAlias("chat-demo".into()),
        })
    );
    let routed = RoutedRequest::new(context, chat).expect("exact route is valid");
    assert_eq!(routed.context().route.selector.operation, Operation::Chat);
}
