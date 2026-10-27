//! The JSON error envelope and its `code` and `type` strings.

use serde::{Deserialize, Serialize};

/// `error.code` values.
pub mod code {
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const INVALID_API_KEY: &str = "invalid_api_key";
    pub const PERMISSION_DENIED: &str = "permission_denied";
    pub const NOT_FOUND: &str = "not_found";
    pub const CONFLICT: &str = "conflict";
    pub const RATE_LIMIT_EXCEEDED: &str = "rate_limit_exceeded";
    pub const REQUEST_UPLOAD_TIMEOUT: &str = "request_upload_timeout";
    pub const UPSTREAM_TIMEOUT: &str = "upstream_timeout";
    pub const REQUEST_CANCELLED: &str = "request_cancelled";
    pub const UPSTREAM_UNAVAILABLE: &str = "upstream_unavailable";
    pub const UPSTREAM_FAILURE: &str = "upstream_failure";
    pub const UNSUPPORTED_OPERATION: &str = "unsupported_operation";
    pub const INTERNAL_ERROR: &str = "internal_error";
    pub const KEY_EXPIRED: &str = "key_expired";
    pub const SERVER_DRAINING: &str = "server_draining";
    pub const GATEWAY_QUEUE_FULL: &str = "gateway_queue_full";
    pub const GATEWAY_BUSY: &str = "gateway_busy";
    pub const GATEWAY_KEY_BUSY: &str = "gateway_key_busy";
    pub const GATEWAY_KEY_RATE_LIMITED: &str = "gateway_key_rate_limited";
    pub const GATEWAY_UPLOAD_BUSY: &str = "gateway_upload_busy";
    pub const DAILY_QUOTA_EXCEEDED: &str = "daily_quota_exceeded";
    pub const MONTHLY_QUOTA_EXCEEDED: &str = "monthly_quota_exceeded";
    pub const QUOTA_UNAVAILABLE: &str = "quota_unavailable";
}

/// `error.type` values.
pub mod kind {
    pub const INVALID_REQUEST_ERROR: &str = "invalid_request_error";
    pub const AUTHENTICATION_ERROR: &str = "authentication_error";
    pub const PERMISSION_ERROR: &str = "permission_error";
    pub const RATE_LIMIT_ERROR: &str = "rate_limit_error";
    pub const API_ERROR: &str = "api_error";
    pub const QUOTA_ERROR: &str = "quota_error";
    /// Messages envelope (`{"type": "error", ...}`) only.
    pub const NOT_FOUND_ERROR: &str = "not_found_error";
    pub const TIMEOUT_ERROR: &str = "timeout_error";
    pub const REQUEST_TOO_LARGE: &str = "request_too_large";
    pub const OVERLOADED_ERROR: &str = "overloaded_error";
}

/// `{"error": {...}}`: the body of every non-Messages JSON error and of SSE error frames.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

/// Fields in serialized (alphabetical) order; `param` is `null` when no member is blamed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub param: Option<String>,
    #[serde(rename = "type")]
    pub error_type: String,
}

impl ErrorEnvelope {
    pub fn new(message: &str, error_type: &str, code: &str, param: Option<&str>) -> Self {
        Self {
            error: ErrorBody {
                code: code.to_owned(),
                message: message.to_owned(),
                param: param.map(str::to_owned),
                error_type: error_type.to_owned(),
            },
        }
    }
}
