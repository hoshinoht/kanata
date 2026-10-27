use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use kanata_contract::errors::{code, kind};
use kanata_contract::{ErrorEnvelope, events};
use serde_json::{Value, json};

use crate::core::{ErrorKind, GatewayError};
use crate::routing::admission::AdmissionError;
use crate::server::{ClientState, ErrorDetails, error_response, error_response_with_param};
use crate::telemetry::{Listener, Observer, observe_draining, observe_error};

/// Upstream request id: the caller's valid `x-request-id` on the private listener, else
/// a gateway sequence id; `None` rejects a repeated or unsafe private caller id.
pub(super) fn request_id(headers: &HeaderMap, state: &ClientState) -> Option<String> {
    if state.listener() == Listener::Public {
        return Some(state.next_request_id());
    }
    match crate::telemetry::sanitize::request_id(headers) {
        Ok(Some(id)) => Some(id.to_owned()),
        Ok(None) => Some(state.next_request_id()),
        Err(()) => None,
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
    serde_json::to_value(ErrorEnvelope::new(
        gateway_message(error.kind),
        mapping.error_type,
        mapping.code,
        None,
    ))
    .unwrap_or_default()
}

pub(super) fn invalid() -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "Invalid request",
        kind::INVALID_REQUEST_ERROR,
        code::INVALID_REQUEST,
    )
}

pub(super) fn invalid_param(param: &str) -> Response {
    error_response_with_param(
        StatusCode::BAD_REQUEST,
        "Invalid request",
        kind::INVALID_REQUEST_ERROR,
        code::INVALID_REQUEST,
        Some(param),
    )
}

/// Messages-protocol error type for a Kanata status; Kanata codes are kept beside it.
fn messages_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => kind::AUTHENTICATION_ERROR,
        403 => kind::PERMISSION_ERROR,
        404 => kind::NOT_FOUND_ERROR,
        408 | 504 => kind::TIMEOUT_ERROR,
        413 => kind::REQUEST_TOO_LARGE,
        429 => kind::RATE_LIMIT_ERROR,
        503 | 529 => kind::OVERLOADED_ERROR,
        500..=599 => kind::API_ERROR,
        _ => kind::INVALID_REQUEST_ERROR,
    }
}

fn messages_error_body(
    status: StatusCode,
    message: &str,
    code: Option<&str>,
    param: Option<&str>,
) -> Value {
    json!({"type":events::messages::ERROR,"error":{
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
        kind::PERMISSION_ERROR,
        code::PERMISSION_DENIED,
    )
}

pub(super) fn unavailable() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Upstream unavailable",
        kind::API_ERROR,
        code::UPSTREAM_UNAVAILABLE,
    )
}

pub(super) fn server_draining() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Server is draining",
        kind::API_ERROR,
        code::SERVER_DRAINING,
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
            kind::RATE_LIMIT_ERROR,
            code::GATEWAY_QUEUE_FULL,
        ),
        AdmissionError::QueueTimeout => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Gateway is busy",
            kind::API_ERROR,
            code::GATEWAY_BUSY,
        ),
        AdmissionError::KeyBusy => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many concurrent requests for this key",
            kind::RATE_LIMIT_ERROR,
            code::GATEWAY_KEY_BUSY,
        ),
        AdmissionError::KeyRateLimited { .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            "Request rate limit exceeded for this key",
            kind::RATE_LIMIT_ERROR,
            code::GATEWAY_KEY_RATE_LIMITED,
        ),
        AdmissionError::CircuitOpen { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Upstream unavailable",
            kind::API_ERROR,
            code::UPSTREAM_UNAVAILABLE,
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
        kind::API_ERROR,
        code::UPSTREAM_FAILURE,
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
        kind::INVALID_REQUEST_ERROR,
        code::INVALID_REQUEST,
    )
}

pub(super) fn upload_busy() -> Response {
    let mut response = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "Request buffer capacity exhausted",
        kind::API_ERROR,
        code::GATEWAY_UPLOAD_BUSY,
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
    use crate::keys::quota::{QuotaError, QuotaPeriod};
    let (status, code, message) = match error {
        QuotaError::Exhausted {
            period: QuotaPeriod::Day,
            ..
        } => (
            StatusCode::TOO_MANY_REQUESTS,
            code::DAILY_QUOTA_EXCEEDED,
            "Daily quota exceeded",
        ),
        QuotaError::Exhausted {
            period: QuotaPeriod::Month,
            ..
        } => (
            StatusCode::TOO_MANY_REQUESTS,
            code::MONTHLY_QUOTA_EXCEEDED,
            "Monthly quota exceeded",
        ),
        QuotaError::ClockRollback => (
            StatusCode::SERVICE_UNAVAILABLE,
            code::QUOTA_UNAVAILABLE,
            "Daily quota clock unavailable",
        ),
        QuotaError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            code::QUOTA_UNAVAILABLE,
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
    let mut response = error_response(status, message, kind::QUOTA_ERROR, code);
    if let QuotaError::Exhausted { retry_after, .. } = error {
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
        Some(observer) => observer.reserve_usage(auth.quotas()).await,
        None if auth.quotas().is_some() => Err(crate::keys::quota::QuotaError::Unavailable),
        None => Ok(()),
    }
}
