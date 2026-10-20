//! OpenAI-compatible Chat Completions wire handling shared by the vLLM (and CLIProxyAPI),
//! Ollama (and Apple FM) and OpenRouter adapters. Provider differences are explicit
//! parameters: `request::MessageRules`, `stream_state::StreamDialect`, strict wire variants
//! in `stream_wire`, and the per-provider response envelopes.

pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod stream;
pub(crate) mod stream_state;
pub(crate) mod stream_wire;
pub(crate) mod validation;

use futures_util::StreamExt;
use http::Method;
use serde::Serialize;

use crate::{
    adapter::{
        diagnostics::UpstreamLabel,
        status_error,
        transport::{
            Accept, COMPLETE_RESPONSE_BYTES, CredentialHeader, Endpoint, ResponseBody,
            ResponseContentType, STREAM_RESPONSE_BYTES, Transport, TransportRequest,
            TransportResponse,
        },
        upstream_failure,
    },
    core::GatewayError,
};

pub(crate) const MAX_TOOL_CALLS: usize = 64;
const MAX_TOOL_CALL_ID_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 64;

pub(crate) fn valid_call_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_CALL_ID_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

pub(crate) fn valid_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// `POST chat/completions`, accepting an event stream when `stream` is set and JSON otherwise.
pub(crate) fn chat_request(
    payload: &impl Serialize,
    credential: Option<CredentialHeader>,
    stream: bool,
    request_budget: usize,
) -> Result<TransportRequest, GatewayError> {
    let (accept, response_budget) = if stream {
        (Accept::EventStream, STREAM_RESPONSE_BYTES)
    } else {
        (Accept::Json, COMPLETE_RESPONSE_BYTES)
    };
    TransportRequest::json(
        Method::POST,
        Endpoint::new(&["chat", "completions"])?,
        payload,
        credential,
        Some(accept),
        request_budget,
        response_budget,
    )
}

pub(crate) async fn post_json(
    transport: &Transport,
    request: TransportRequest,
    label: &UpstreamLabel,
) -> Result<Vec<u8>, GatewayError> {
    json_body(transport.execute(request).await?, label).await
}

pub(crate) async fn post_events(
    transport: &Transport,
    request: TransportRequest,
    label: &UpstreamLabel,
) -> Result<ResponseBody, GatewayError> {
    event_body(transport.execute(request).await?, label).await
}

/// A 200 JSON body of at most `COMPLETE_RESPONSE_BYTES`.
pub(crate) async fn json_body(
    mut response: TransportResponse,
    label: &UpstreamLabel,
) -> Result<Vec<u8>, GatewayError> {
    if response.status != 200 {
        label.status(response.status, &mut response.body).await;
        return Err(status_error(response.status));
    }
    if !matches!(response.content_type, Some(ResponseContentType::Json)) {
        label.content_type(response.status, response.media_type.as_deref());
        return Err(upstream_failure());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > COMPLETE_RESPONSE_BYTES {
            return Err(upstream_failure());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The body of a 200 event-stream response.
pub(crate) async fn event_body(
    mut response: TransportResponse,
    label: &UpstreamLabel,
) -> Result<ResponseBody, GatewayError> {
    if response.status != 200 {
        label.status(response.status, &mut response.body).await;
        return Err(status_error(response.status));
    }
    if !matches!(
        response.content_type,
        Some(ResponseContentType::EventStream)
    ) {
        label.content_type(response.status, response.media_type.as_deref());
        return Err(upstream_failure());
    }
    Ok(response.body)
}
