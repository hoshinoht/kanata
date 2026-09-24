use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::core::{ErrorKind, GatewayError};
use crate::server::{ClientState, error_response};
use crate::telemetry::{Observer, observe_draining, observe_error};

pub(super) fn request_id(headers: &HeaderMap, state: &ClientState) -> Option<String> {
    let mut values = headers.get_all("x-request-id").iter();
    let Some(value) = values.next() else {
        return Some(state.next_request_id());
    };
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?;
    (value.len() <= 128
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| value.into())
}

pub(super) fn gateway_error(error: GatewayError) -> Response {
    let mapping = error.kind.mapping();
    let status = StatusCode::from_u16(mapping.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    error_response(
        status,
        gateway_message(error.kind),
        mapping.error_type,
        mapping.code,
    )
}

pub(super) fn gateway_error_observed(error: GatewayError, observer: Option<&Observer>) -> Response {
    observe_error(observer, error);
    gateway_error(error)
}

fn gateway_message(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidRequest => "Invalid request",
        ErrorKind::Unauthorized => "Invalid authentication credentials",
        ErrorKind::Forbidden => "Permission denied",
        ErrorKind::NotFound => "Not found",
        ErrorKind::Conflict => "Conflict",
        ErrorKind::RateLimited => "Rate limit exceeded",
        ErrorKind::Timeout { .. } => "Upstream timeout",
        ErrorKind::Cancelled => "Request cancelled",
        ErrorKind::UpstreamUnavailable => "Upstream unavailable",
        ErrorKind::UpstreamFailure => "Upstream failure",
        ErrorKind::UnsupportedOperation => "Unsupported operation",
        ErrorKind::Internal => "Internal server error",
    }
}

pub(super) fn sse_error_data(error: GatewayError) -> Value {
    let mapping = error.kind.mapping();
    json!({
        "error": {
            "message": gateway_message(error.kind),
            "type": mapping.error_type,
            "param": null,
            "code": mapping.code
        }
    })
}

pub(super) fn invalid() -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "Invalid request",
        "invalid_request_error",
        "invalid_request",
    )
}

pub(super) fn invalid_param(param: &'static str) -> Response {
    let mut response = (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": {
            "message": "Invalid request",
            "type": "invalid_request_error",
            "param": param,
            "code": "invalid_request"
        }})),
    )
        .into_response();
    response
        .extensions_mut()
        .insert(crate::telemetry::ErrorCode("invalid_request"));
    response
}

pub(super) fn forbidden() -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        "Permission denied",
        "permission_error",
        "permission_denied",
    )
}

pub(super) fn unavailable() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Upstream unavailable",
        "api_error",
        "upstream_unavailable",
    )
}

pub(super) fn server_draining() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Server is draining",
        "api_error",
        "server_draining",
    )
}

pub(super) fn server_draining_observed(observer: Option<&Observer>) -> Response {
    observe_draining(observer);
    server_draining()
}

pub(super) fn upstream_failure() -> Response {
    error_response(
        StatusCode::BAD_GATEWAY,
        "Upstream failure",
        "api_error",
        "upstream_failure",
    )
}

pub(super) fn upstream_failure_observed(observer: Option<&Observer>) -> Response {
    observe_error(
        observer,
        GatewayError {
            kind: ErrorKind::UpstreamFailure,
        },
    );
    upstream_failure()
}

pub(super) fn body_too_large() -> Response {
    error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Request body too large",
        "invalid_request_error",
        "invalid_request",
    )
}
