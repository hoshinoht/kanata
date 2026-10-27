//! Model discovery: `GET /v1/models` and `GET /v1/models/{id}`.

use std::collections::{BTreeMap, BTreeSet};

use axum::{
    Json,
    extract::{Path, Request, State},
    response::{IntoResponse, Response},
};
use kanata_contract::limits::{MAX_RESPONSES_OUTPUT_BYTES, MAX_SPEECH_SPEED, MIN_SPEECH_SPEED};
use kanata_contract::{
    AdmissionSnapshot, EmbeddingsMetadata, ImageInputMetadata, MessagesEndpoint, ModelList,
    ModelMetadata, ModelObject, ResponsesEndpoint, SpeechMetadata, SpeedRange,
};
use serde::Serialize;

use super::errors::gateway_error_observed;
use crate::core::{Capabilities, ErrorKind, GatewayError, Operation, ReasoningEffort};
use crate::routing::RouteEntry;
use crate::server::{Authenticated, ClientState};
use crate::telemetry::{Listener, Observer};

pub(super) async fn list(auth: Authenticated, State(state): State<ClientState>) -> Response {
    let data: Vec<_> = aliases(&auth, &state)
        .into_iter()
        .map(|(alias, routes)| entry(&state, alias, &routes))
        .collect();
    json_body(ModelList {
        object: "list".into(),
        data,
    })
}

/// The listing's entry for `id`; unknown and unauthorized ids get the same 404.
pub(super) async fn retrieve(
    auth: Authenticated,
    State(state): State<ClientState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    match aliases(&auth, &state).remove(&id) {
        Some(routes) => json_body(entry(&state, id, &routes)),
        None => gateway_error_observed(
            GatewayError::from(ErrorKind::NotFound),
            request.extensions().get::<Observer>(),
        ),
    }
}

/// Listed ids, each with its authorized, bound routes; reasoning families share their base id.
fn aliases<'a>(
    auth: &Authenticated,
    state: &'a ClientState,
) -> BTreeMap<String, Vec<&'a RouteEntry>> {
    let mut aliases: BTreeMap<_, Vec<&RouteEntry>> = BTreeMap::new();
    for route in state
        .registry()
        .routes()
        .filter(|route| auth.authorize(&route.identity.selector).is_ok())
        .filter(|route| state.route_is_bound(route))
    {
        aliases
            .entry(if route.capabilities.reasoning_control {
                route.model_family_alias().to_owned()
            } else {
                route.identity.selector.model_alias.0.clone()
            })
            .or_default()
            .push(route);
    }
    aliases
}

/// One model object; `routes` are the id's authorized, bound routes.
fn entry(state: &ClientState, alias: String, routes: &[&RouteEntry]) -> ModelObject {
    // Capacity settings and live load are published to private clients only.
    let publish_admission = state.listener() == Listener::Private;
    let limits = state.admission().limits();
    let mut kanata = model_capabilities(routes, state.max_body_bytes());
    if let Some(route) = routes
        .iter()
        .find(|route| route.identity.selector.operation == Operation::Chat)
    {
        kanata.responses = Some(ResponsesEndpoint {
            endpoint: "/v1/responses".into(),
            operation: Operation::Chat,
            stateless: true,
            input: vec!["text".into()],
            function_tools: route.capabilities.function_tools,
            streaming: route.capabilities.streaming_chat,
            max_output_bytes: MAX_RESPONSES_OUTPUT_BYTES,
        });
        kanata.messages = Some(MessagesEndpoint {
            endpoint: "/v1/messages".into(),
            operation: Operation::Chat,
            stateless: true,
            function_tools: route.capabilities.function_tools,
            streaming: route.capabilities.streaming_chat,
        });
    }
    if routes
        .iter()
        .any(|route| route.identity.selector.operation == Operation::Embeddings)
    {
        kanata.embeddings = Some(EmbeddingsMetadata {
            input: vec!["string".into(), "string_array".into()],
            encoding_formats: vec!["float".into(), "base64".into()],
            max_inputs: crate::core::MAX_EMBEDDING_INPUTS,
            max_dimensions: crate::core::MAX_EMBEDDING_DIMENSIONS,
            max_total_values: crate::core::MAX_EMBEDDING_VALUES,
        });
    }
    if let Some(policy) = routes.iter().find_map(|route| route.speech.as_ref()) {
        kanata.speech = Some(SpeechMetadata {
            voices: policy.voices.iter().cloned().collect(),
            formats: policy.formats.iter().copied().collect(),
            max_input_characters: crate::core::MAX_SPEECH_CHARACTERS,
            max_input_bytes: crate::core::MAX_SPEECH_INPUT_BYTES,
            max_response_bytes: crate::core::MAX_SPEECH_RESPONSE_BYTES,
            speed: SpeedRange {
                min: MIN_SPEECH_SPEED,
                max: MAX_SPEECH_SPEED,
            },
        });
    }
    if publish_admission {
        let load = routes
            .iter()
            .find(|route| route.identity.selector.operation == Operation::Chat)
            .or(routes.first())
            .and_then(|route| state.admission().route_load(route));
        kanata.admission = Some(AdmissionSnapshot {
            max_in_flight: limits.max_in_flight,
            max_queue: limits.max_queue,
            queue_ms: limits.queue_ms,
            adapter_max_in_flight: load.and_then(|load| load.adapter_max_in_flight),
            in_flight: load.map(|load| load.in_flight),
            queued: load.map(|load| load.queued),
            adapter_in_flight: load.and_then(|load| load.adapter_in_flight),
            breaker: load.map(|load| load.breaker.as_str().to_owned()),
        });
    }
    ModelObject {
        id: alias,
        object: "model".into(),
        created: 0,
        owned_by: "kanata".into(),
        kanata,
    }
}

/// Kanata extension for a model entry; `routes` are the alias's authorized, bound routes.
fn model_capabilities(routes: &[&RouteEntry], max_body_bytes: usize) -> ModelMetadata {
    let mut operations: Vec<_> = routes
        .iter()
        .map(|route| route.identity.selector.operation)
        .collect();
    operations.sort();
    operations.dedup();
    let chat = routes
        .iter()
        .find(|route| route.identity.selector.operation == Operation::Chat);
    let caps = chat.map(|route| &route.capabilities);
    let flag = |get: fn(&Capabilities) -> bool| caps.is_some_and(get);
    let reasoning_control = flag(|c| c.reasoning_control);
    let reasoning_efforts =
        if reasoning_control && chat.is_some_and(|route| route.pinned_reasoning_effort.is_some()) {
            let efforts: BTreeSet<_> = routes
                .iter()
                .filter(|_| reasoning_control)
                .filter_map(|route| {
                    route
                        .pinned_reasoning_effort
                        .and_then(ReasoningEffort::parse)
                })
                .collect();
            Some(efforts.into_iter().collect())
        } else {
            chat.filter(|_| reasoning_control)
                .and_then(|route| route.provider_kind.restricted_reasoning_efforts())
        };
    let trust_zone = chat.or(routes.first()).map(|route| route.trust_zone);
    ModelMetadata {
        operations,
        structured_output: flag(|c| c.structured_output),
        sampling_controls: flag(|c| c.sampling_controls),
        reasoning_control,
        function_tools: flag(|c| c.function_tools),
        streaming: flag(|c| c.streaming_chat),
        input_audio: flag(|c| c.input_audio),
        input_images: flag(|c| c.input_images),
        images: flag(|c| c.input_images).then(|| ImageInputMetadata {
            transport: "data_url".into(),
            media_types: vec!["image/png".into(), "image/jpeg".into()],
            max_count: crate::core::MAX_INPUT_IMAGES,
            max_bytes_per_image: crate::core::MAX_IMAGE_BYTES,
            max_total_bytes: crate::core::MAX_IMAGE_TOTAL_BYTES,
            max_base64_bytes_per_image: crate::core::MAX_IMAGE_BASE64_BYTES,
            max_dimension: crate::core::MAX_IMAGE_DIMENSION,
            max_total_pixels: crate::core::MAX_IMAGE_TOTAL_PIXELS,
            max_json_body_bytes_without_audio: max_body_bytes,
        }),
        trust_zone,
        reasoning_efforts,
        context_tokens: chat.and_then(|route| route.context_tokens),
        max_output_tokens: chat.and_then(|route| route.max_output_tokens),
        responses: None,
        messages: None,
        embeddings: None,
        speech: None,
        admission: None,
    }
}

/// Serializes through `Value` so object keys keep their sorted wire order.
fn json_body(body: impl Serialize) -> Response {
    Json(serde_json::to_value(body).unwrap_or_default()).into_response()
}
