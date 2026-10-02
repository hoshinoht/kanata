use axum::{
    Json,
    body::to_bytes,
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
        Request as CoreRequest, RequestContext, Response as CoreResponse, RouteSelector,
        RoutedRequest,
    },
    server::{Authenticated, ClientState},
    telemetry::Observer,
};

use super::{check_supported, deadline::RequestDeadline, errors::*};

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
    let observer = request.extensions().get::<Observer>().cloned();
    if state.admission().is_closed() {
        return server_draining_observed(observer.as_ref());
    }
    let deadline = match RequestDeadline::new(state.overall_ms()) {
        Ok(deadline) => deadline,
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    };
    let Some(request_id) = request_id(request.headers(), &state) else {
        return invalid();
    };
    annotate_client_request_id(observer.as_ref(), request.headers(), &request_id);
    if !super::chat::has_json_content_type(request.headers()) {
        return invalid();
    }
    let (_reservation, max_body_bytes) =
        match super::upload::reserve(&state, request.headers(), state.max_body_bytes()) {
            Ok(value) => value,
            Err(response) => return *response,
        };
    let bytes = match super::upload::read(
        deadline,
        state.upload_ms(),
        to_bytes(request.into_body(), max_body_bytes),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return body_too_large(),
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    };
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
    if let (Some(observer), Some(_)) = (observer.as_ref(), state.registry().resolve(&selector)) {
        observer.annotate_route(&embedding.model.0, Operation::Embeddings, false);
    }
    if auth.authorize(&selector).is_err() {
        return forbidden();
    }
    let Some(route) = state.registry().resolve(&selector) else {
        return invalid();
    };
    let Some(adapter) = state.adapter(&route.adapter_id) else {
        return unavailable();
    };
    let request = CoreRequest::Embeddings(embedding.clone());
    if check_supported(&route.capabilities, adapter.capabilities(), &request).is_err() {
        return invalid();
    }
    let context = RequestContext {
        request_id,
        route: route.identity.clone(),
        trust_zone: route.trust_zone,
        extensions: Extensions::default(),
    };
    let routed = match RoutedRequest::new(context, request) {
        Ok(request) => request,
        Err(_) => return invalid(),
    };
    if let Some(observer) = observer.as_ref() {
        observer.begin_queue();
    }
    let admission = deadline
        .run(|| state.admission().acquire(route, auth.key_limits()))
        .await;
    if let Some(observer) = observer.as_ref() {
        observer.end_queue();
    }
    let mut permit = match admission {
        Ok(Ok(permit)) => permit,
        Ok(Err(error)) => {
            return admission_rejected_observed(
                error,
                state.admission().retry_after_secs(error),
                observer.as_ref(),
            );
        }
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    };
    match deadline
        .run(|| reserve_usage(&auth, observer.as_ref()))
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return quota_rejected(error, observer.as_ref()),
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    }
    if let Some(observer) = observer.as_ref() {
        observer.begin_upstream();
        observer.annotate_adapter(&route.adapter_id, route.provider_kind.label());
    }
    let result = deadline.run(move || adapter.execute(routed)).await;
    if let Some(observer) = observer.as_ref() {
        observer.end_upstream();
    }
    if let Ok(outcome) = &result {
        permit.record_outcome(outcome);
    }
    match result {
        Ok(Ok(AdapterOutput::Complete(CoreResponse::Embeddings(response))))
            if response.valid_for(&embedding) =>
        {
            if let Some(observer) = observer.as_ref() {
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
        Ok(Ok(_)) => upstream_failure_observed(observer.as_ref()),
        Ok(Err(error)) | Err(error) => gateway_error_observed(error, observer.as_ref()),
    }
}
