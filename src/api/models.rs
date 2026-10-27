//! Model discovery: `GET /v1/models` and `GET /v1/models/{id}`.

use std::collections::{BTreeMap, BTreeSet};

use axum::{
    Json,
    extract::{Path, Request, State},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

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
    Json(json!({"object": "list", "data": data})).into_response()
}

/// The listing's entry for `id`; unknown and unauthorized ids get the same 404.
pub(super) async fn retrieve(
    auth: Authenticated,
    State(state): State<ClientState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    match aliases(&auth, &state).remove(&id) {
        Some(routes) => Json(entry(&state, id, &routes)).into_response(),
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
fn entry(state: &ClientState, alias: String, routes: &[&RouteEntry]) -> Value {
    // Capacity settings and live load are published to private clients only.
    let publish_admission = state.listener() == Listener::Private;
    let limits = state.admission().limits();
    let mut kanata = model_capabilities(routes);
    if kanata["input_images"] == true {
        kanata["images"]["max_json_body_bytes_without_audio"] = json!(state.max_body_bytes());
    }
    if let Some(route) = routes
        .iter()
        .find(|route| route.identity.selector.operation == Operation::Chat)
    {
        kanata["responses"] = json!({
            "endpoint": "/v1/responses", "operation": "chat", "stateless": true,
            "input": ["text"], "function_tools": route.capabilities.function_tools,
            "streaming": route.capabilities.streaming_chat, "max_output_bytes": 8 * 1024 * 1024,
        });
        kanata["messages"] = json!({
            "endpoint": "/v1/messages", "operation": "chat", "stateless": true,
            "function_tools": route.capabilities.function_tools,
            "streaming": route.capabilities.streaming_chat,
        });
    }
    if routes
        .iter()
        .any(|route| route.identity.selector.operation == Operation::Embeddings)
    {
        kanata["embeddings"] = json!({
            "input": ["string", "string_array"],
            "encoding_formats": ["float", "base64"],
            "max_inputs": crate::core::MAX_EMBEDDING_INPUTS,
            "max_dimensions": crate::core::MAX_EMBEDDING_DIMENSIONS,
            "max_total_values": crate::core::MAX_EMBEDDING_VALUES,
        });
    }
    if let Some(policy) = routes.iter().find_map(|route| route.speech.as_ref()) {
        kanata["speech"] = json!({
            "voices": policy.voices, "formats": policy.formats,
            "max_input_characters": crate::core::MAX_SPEECH_CHARACTERS,
            "max_input_bytes": crate::core::MAX_SPEECH_INPUT_BYTES,
            "max_response_bytes": crate::core::MAX_SPEECH_RESPONSE_BYTES,
            "speed": {"min": 0.25, "max": 4.0},
        });
    }
    if publish_admission {
        let load = routes
            .iter()
            .find(|route| route.identity.selector.operation == Operation::Chat)
            .or(routes.first())
            .and_then(|route| state.admission().route_load(route));
        kanata["admission"] = json!({
            "max_in_flight": limits.max_in_flight,
            "max_queue": limits.max_queue,
            "queue_ms": limits.queue_ms,
            "adapter_max_in_flight": load.and_then(|load| load.adapter_max_in_flight),
            "in_flight": load.map(|load| load.in_flight),
            "queued": load.map(|load| load.queued),
            "adapter_in_flight": load.and_then(|load| load.adapter_in_flight),
            "breaker": load.map(|load| load.breaker.as_str()),
        });
    }
    json!({
        "id": alias,
        "object": "model",
        "created": 0,
        "owned_by": "kanata",
        "kanata": kanata,
    })
}

/// Kanata extension for a model entry; `routes` are the alias's authorized, bound routes.
fn model_capabilities(routes: &[&RouteEntry]) -> Value {
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
    json!({
        "operations": operations,
        "structured_output": flag(|c| c.structured_output),
        "sampling_controls": flag(|c| c.sampling_controls),
        "reasoning_control": reasoning_control,
        "function_tools": flag(|c| c.function_tools),
        "streaming": flag(|c| c.streaming_chat),
        "input_audio": flag(|c| c.input_audio),
        "input_images": flag(|c| c.input_images),
        "images": flag(|c| c.input_images).then(|| json!({
            "transport": "data_url", "media_types": ["image/png", "image/jpeg"],
            "max_count": crate::core::MAX_INPUT_IMAGES,
            "max_bytes_per_image": crate::core::MAX_IMAGE_BYTES,
            "max_total_bytes": crate::core::MAX_IMAGE_TOTAL_BYTES,
            "max_base64_bytes_per_image": crate::core::MAX_IMAGE_BASE64_BYTES,
            "max_dimension": crate::core::MAX_IMAGE_DIMENSION,
            "max_total_pixels": crate::core::MAX_IMAGE_TOTAL_PIXELS,
        })),
        "trust_zone": trust_zone,
        "reasoning_efforts": reasoning_efforts,
        "context_tokens": chat.and_then(|route| route.context_tokens),
        "max_output_tokens": chat.and_then(|route| route.max_output_tokens),
    })
}
