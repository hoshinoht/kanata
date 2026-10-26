use super::*;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutPhase {
    Upload,
    Queue,
    Connect,
    Headers,
    FirstByte,
    Idle,
    Overall,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    RateLimited,
    Timeout { phase: TimeoutPhase },
    Cancelled,
    UpstreamUnavailable,
    UpstreamFailure,
    UnsupportedOperation,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorMapping {
    pub status: u16,
    pub code: &'static str,
    pub error_type: &'static str,
}

impl ErrorKind {
    pub const fn mapping(self) -> ErrorMapping {
        match self {
            Self::InvalidRequest => ErrorMapping {
                status: 400,
                code: "invalid_request",
                error_type: "invalid_request_error",
            },
            Self::Unauthorized => ErrorMapping {
                status: 401,
                code: "invalid_api_key",
                error_type: "authentication_error",
            },
            Self::Forbidden => ErrorMapping {
                status: 403,
                code: "permission_denied",
                error_type: "permission_error",
            },
            Self::NotFound => ErrorMapping {
                status: 404,
                code: "not_found",
                error_type: "invalid_request_error",
            },
            Self::Conflict => ErrorMapping {
                status: 409,
                code: "conflict",
                error_type: "invalid_request_error",
            },
            Self::RateLimited => ErrorMapping {
                status: 429,
                code: "rate_limit_exceeded",
                error_type: "rate_limit_error",
            },
            Self::Timeout {
                phase: TimeoutPhase::Upload,
            } => ErrorMapping {
                status: 408,
                code: "request_upload_timeout",
                error_type: "invalid_request_error",
            },
            Self::Timeout { .. } => ErrorMapping {
                status: 504,
                code: "upstream_timeout",
                error_type: "api_error",
            },
            Self::Cancelled => ErrorMapping {
                status: 408,
                code: "request_cancelled",
                error_type: "api_error",
            },
            Self::UpstreamUnavailable => ErrorMapping {
                status: 503,
                code: "upstream_unavailable",
                error_type: "api_error",
            },
            Self::UpstreamFailure => ErrorMapping {
                status: 502,
                code: "upstream_failure",
                error_type: "api_error",
            },
            Self::UnsupportedOperation => ErrorMapping {
                status: 400,
                code: "unsupported_operation",
                error_type: "invalid_request_error",
            },
            Self::Internal => ErrorMapping {
                status: 500,
                code: "internal_error",
                error_type: "api_error",
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GatewayError {
    pub kind: ErrorKind,
}

impl From<ErrorKind> for GatewayError {
    fn from(kind: ErrorKind) -> Self {
        Self { kind }
    }
}
