use axum::{
    Json,
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    adapter::AdapterOutput,
    core::{
        EmbeddingEncoding, EmbeddingRequest, Extensions, ModelAlias, Operation,
        Request as CoreRequest, Response as CoreResponse, RouteSelector,
    },
    server::{Authenticated, ClientState},
};

use super::{check_supported, dispatch, errors::*};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    model: String,
    input: Value,
    #[serde(default)]
    encoding_format: EmbeddingEncoding,
    #[serde(default)]
    dimensions: Option<usize>,
}

pub(super) async fn embeddings(
    auth: Authenticated,
    State(state): State<ClientState>,
    request: Request,
) -> Response {
    let (inbound, request_id) = try_step!(dispatch::inbound(&state, &request));
    let max_body_bytes = state.max_body_bytes();
    let (_reservation, bytes) = try_step!(inbound.read_json(&state, request, max_body_bytes).await);
    let wire: Wire = match serde_json::from_slice(&bytes) {
        Ok(wire) => wire,
        Err(_) => return invalid(),
    };
    drop(bytes);
    let input = match wire.input {
        Value::String(text) => vec![text],
        Value::Array(items) => match items
            .into_iter()
            .map(|item| match item {
                Value::String(text) => Some(text),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
        {
            Some(items) => items,
            None => return invalid_param("input"),
        },
        _ => return invalid_param("input"),
    };
    let embedding = EmbeddingRequest {
        model: ModelAlias(wire.model),
        input,
        dimensions: wire.dimensions,
    };
    if let Err(param) = embedding.validate() {
        return invalid_param(param);
    }
    let selector = RouteSelector {
        model_alias: embedding.model.clone(),
        operation: Operation::Embeddings,
    };
    let (route, adapter) = try_step!(inbound.resolve(&state, &auth, &selector));
    let request = CoreRequest::Embeddings(embedding.clone());
    if check_supported(&route.capabilities, adapter.capabilities(), &request).is_err() {
        return invalid();
    }
    let routed = try_step!(dispatch::routed(
        route,
        request_id,
        Extensions::default(),
        request
    ));
    let execution = inbound.execute(&auth, &state, route, &*adapter, routed);
    let (_permit, result) = try_step!(execution.await);
    let observer = inbound.observer();
    match result {
        Ok(AdapterOutput::Complete(CoreResponse::Embeddings(response)))
            if response.valid_for(&embedding) =>
        {
            if let Some(observer) = observer {
                observer.first_content();
                observer.record_usage(response.usage.clone());
            }
            let data: Vec<_> = response
                .vectors
                .into_iter()
                .enumerate()
                .map(|(index, vector)| {
                    let vector = match wire.encoding_format {
                        EmbeddingEncoding::Float => json!(vector),
                        EmbeddingEncoding::Base64 => {
                            let bytes: Vec<_> = vector
                                .iter()
                                .flat_map(|value| value.to_le_bytes())
                                .collect();
                            json!(STANDARD.encode(bytes))
                        }
                    };
                    json!({"object": "embedding", "index": index, "embedding": vector})
                })
                .collect();
            let mut body = json!({"object": "list", "data": data, "model": embedding.model.0});
            if let Some(usage) = response.usage {
                body["usage"] = json!({"prompt_tokens": usage.input_tokens, "total_tokens": usage.total_tokens});
            }
            Json(body).into_response()
        }
        Ok(_) => upstream_failure_observed(observer),
        Err(error) => gateway_error_observed(error, observer),
    }
}
