use super::*;
use kanata_contract::errors::{code, kind};

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
                code: code::INVALID_REQUEST,
                error_type: kind::INVALID_REQUEST_ERROR,
            },
            Self::Unauthorized => ErrorMapping {
                status: 401,
                code: code::INVALID_API_KEY,
                error_type: kind::AUTHENTICATION_ERROR,
            },
            Self::Forbidden => ErrorMapping {
                status: 403,
                code: code::PERMISSION_DENIED,
                error_type: kind::PERMISSION_ERROR,
            },
            Self::NotFound => ErrorMapping {
                status: 404,
                code: code::NOT_FOUND,
                error_type: kind::INVALID_REQUEST_ERROR,
            },
            Self::Conflict => ErrorMapping {
                status: 409,
                code: code::CONFLICT,
                error_type: kind::INVALID_REQUEST_ERROR,
            },
            Self::RateLimited => ErrorMapping {
                status: 429,
                code: code::RATE_LIMIT_EXCEEDED,
                error_type: kind::RATE_LIMIT_ERROR,
            },
            Self::Timeout {
                phase: TimeoutPhase::Upload,
            } => ErrorMapping {
                status: 408,
                code: code::REQUEST_UPLOAD_TIMEOUT,
                error_type: kind::INVALID_REQUEST_ERROR,
            },
            Self::Timeout { .. } => ErrorMapping {
                status: 504,
                code: code::UPSTREAM_TIMEOUT,
                error_type: kind::API_ERROR,
            },
            Self::Cancelled => ErrorMapping {
                status: 408,
                code: code::REQUEST_CANCELLED,
                error_type: kind::API_ERROR,
            },
            Self::UpstreamUnavailable => ErrorMapping {
                status: 503,
                code: code::UPSTREAM_UNAVAILABLE,
                error_type: kind::API_ERROR,
            },
            Self::UpstreamFailure => ErrorMapping {
                status: 502,
                code: code::UPSTREAM_FAILURE,
                error_type: kind::API_ERROR,
            },
            Self::UnsupportedOperation => ErrorMapping {
                status: 400,
                code: code::UNSUPPORTED_OPERATION,
                error_type: kind::INVALID_REQUEST_ERROR,
            },
            Self::Internal => ErrorMapping {
                status: 500,
                code: code::INTERNAL_ERROR,
                error_type: kind::API_ERROR,
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
