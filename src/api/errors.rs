use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};

use crate::core::{ErrorKind, GatewayError};
use crate::routing::admission::AdmissionError;
use crate::server::{ClientState, ErrorDetails, error_response, error_response_with_param};
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

/// Logs `id` only when the caller supplied it; `id` comes from [`request_id`].
pub(super) fn annotate_client_request_id(
    observer: Option<&Observer>,
    headers: &HeaderMap,
    id: &str,
) {
    if let Some(observer) = observer
        && headers.contains_key("x-request-id")
    {
        observer.annotate_client_request_id(id);
    }
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
        ErrorKind::Timeout {
            phase: crate::core::TimeoutPhase::Upload,
        } => "Request upload timed out",
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

pub(super) fn invalid_param(param: &str) -> Response {
    error_response_with_param(
        StatusCode::BAD_REQUEST,
        "Invalid request",
        "invalid_request_error",
        "invalid_request",
        Some(param),
    )
}

/// Messages-protocol error type for a Kanata status; Kanata codes are kept beside it.
fn messages_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        408 | 504 => "timeout_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        500..=599 => "api_error",
        _ => "invalid_request_error",
    }
}

fn messages_error_body(
    status: StatusCode,
    message: &str,
    code: Option<&str>,
    param: Option<&str>,
) -> Value {
    json!({"type":"error","error":{
        "type":messages_error_type(status),"message":message,"code":code,"param":param
    }})
}

/// Re-shapes a JSON error from the shared builder into the Messages envelope. Status,
/// headers (`Retry-After`, `WWW-Authenticate`) and extensions (the telemetry `ErrorCode`)
/// are kept. Bodiless router answers, such as the 405 for `GET /v1/messages`, pass
/// through unchanged.
pub(super) fn messages_error(response: Response) -> Response {
    let Some(details) = response.extensions().get::<ErrorDetails>().cloned() else {
        return response;
    };
    let code = response
        .extensions()
        .get::<crate::telemetry::ErrorCode>()
        .map(|code| code.0);
    let (mut parts, _) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    let body = messages_error_body(
        parts.status,
        details.message,
        code,
        details.param.as_deref(),
    );
    Response::from_parts(parts, Body::from(body.to_string()))
}

/// Mid-stream failure in the Messages envelope, for an `error` event.
pub(super) fn messages_sse_error(error: GatewayError) -> Value {
    let mapping = error.kind.mapping();
    let status = StatusCode::from_u16(mapping.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    messages_error_body(
        status,
        gateway_message(error.kind),
        Some(mapping.code),
        None,
    )
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

/// Gateway-side admission refusal; carries `Retry-After`.
pub(super) fn admission_rejected_observed(
    error: AdmissionError,
    retry_after_secs: u64,
    observer: Option<&Observer>,
) -> Response {
    let (status, message, error_type, code) = match error {
        AdmissionError::QueueFull => (
            StatusCode::TOO_MANY_REQUESTS,
            "Gateway queue is full",
            "rate_limit_error",
            "gateway_queue_full",
        ),
        AdmissionError::QueueTimeout => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Gateway is busy",
            "api_error",
            "gateway_busy",
        ),
        AdmissionError::KeyBusy => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many concurrent requests for this key",
            "rate_limit_error",
            "gateway_key_busy",
        ),
        AdmissionError::KeyRateLimited { .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            "Request rate limit exceeded for this key",
            "rate_limit_error",
            "gateway_key_rate_limited",
        ),
        AdmissionError::CircuitOpen { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Upstream unavailable",
            "api_error",
            "upstream_unavailable",
        ),
        AdmissionError::Closed => return server_draining_observed(observer),
        AdmissionError::UnknownRoute => {
            return gateway_error_observed(error.gateway_error(), observer);
        }
    };
    observe_error(observer, error.gateway_error());
    let mut response = error_response(status, message, error_type, code);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
    response
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
    observe_error(observer, GatewayError::from(ErrorKind::UpstreamFailure));
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

pub(super) fn upload_busy() -> Response {
    let mut response = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Request buffer capacity exhausted",
        "api_error",
        "gateway_upload_busy",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

pub(super) fn quota_rejected(
    error: crate::keys::quota::QuotaError,
    observer: Option<&Observer>,
) -> Response {
    use crate::keys::quota::QuotaError;
    let (status, code, message) = match error {
        QuotaError::Exhausted { .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            "daily_quota_exceeded",
            "Daily quota exceeded",
        ),
        QuotaError::ClockRollback => (
            StatusCode::SERVICE_UNAVAILABLE,
            "quota_unavailable",
            "Daily quota clock unavailable",
        ),
        QuotaError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "quota_unavailable",
            "Daily quota storage unavailable",
        ),
    };
    if let Some(observer) = observer {
        observer.record_error(GatewayError::from(
            if status == StatusCode::TOO_MANY_REQUESTS {
                ErrorKind::RateLimited
            } else {
                ErrorKind::Internal
            },
        ));
    }
    let mut response = error_response(status, message, "quota_error", code);
    if let QuotaError::Exhausted { retry_after } = error {
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&retry_after.to_string()).expect("numeric retry"),
        );
    }
    response
}

pub(super) async fn reserve_usage(
    auth: &crate::server::Authenticated,
    observer: Option<&Observer>,
) -> Result<(), crate::keys::quota::QuotaError> {
    match observer {
        Some(observer) => observer.reserve_usage(auth.daily_quota()).await,
        None if auth.daily_quota().is_some() => Err(crate::keys::quota::QuotaError::Unavailable),
        None => Ok(()),
    }
}
