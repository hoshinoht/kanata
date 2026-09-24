use std::fmt;

use crate::core::{ErrorKind, GatewayError, TimeoutPhase};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Endpoint {
    Chat,
    Transcription,
    Models,
    Other,
}

impl Endpoint {
    pub(crate) const ALL: [Self; 4] = [Self::Chat, Self::Transcription, Self::Models, Self::Other];

    pub(crate) fn from_path(path: &str) -> Self {
        match path {
            "/v1/chat/completions" => Self::Chat,
            "/v1/audio/transcriptions" => Self::Transcription,
            "/v1/models" => Self::Models,
            _ => Self::Other,
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Chat => 0,
            Self::Transcription => 1,
            Self::Models => 2,
            Self::Other => 3,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Transcription => "transcription",
            Self::Models => "models",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Outcome {
    Success,
    ClientError,
    UpstreamError,
    InternalError,
    Timeout,
    Cancelled,
    Draining,
}

impl Outcome {
    pub(crate) const COUNT: usize = 7;

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Success => 0,
            Self::ClientError => 1,
            Self::UpstreamError => 2,
            Self::InternalError => 3,
            Self::Timeout => 4,
            Self::Cancelled => 5,
            Self::Draining => 6,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ClientError => "client_error",
            Self::UpstreamError => "upstream_error",
            Self::InternalError => "internal_error",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Draining => "draining",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum StatusClass {
    Unknown,
    Informational,
    Success,
    Redirection,
    ClientError,
    ServerError,
}

impl StatusClass {
    pub(crate) const COUNT: usize = 6;
    pub(crate) const ALL: [Self; Self::COUNT] = [
        Self::Unknown,
        Self::Informational,
        Self::Success,
        Self::Redirection,
        Self::ClientError,
        Self::ServerError,
    ];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Unknown => 0,
            Self::Informational => 1,
            Self::Success => 2,
            Self::Redirection => 3,
            Self::ClientError => 4,
            Self::ServerError => 5,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Informational => "1xx",
            Self::Success => "2xx",
            Self::Redirection => "3xx",
            Self::ClientError => "4xx",
            Self::ServerError => "5xx",
        }
    }

    pub(crate) fn from_status(status: u16) -> Self {
        match status / 100 {
            1 => Self::Informational,
            2 => Self::Success,
            3 => Self::Redirection,
            4 => Self::ClientError,
            5 => Self::ServerError,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Phase {
    None,
    Queue,
    Connect,
    Headers,
    FirstByte,
    Idle,
    Overall,
}

impl Phase {
    pub(crate) const COUNT: usize = 7;

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::None => 0,
            Self::Queue => 1,
            Self::Connect => 2,
            Self::Headers => 3,
            Self::FirstByte => 4,
            Self::Idle => 5,
            Self::Overall => 6,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Queue => "queue",
            Self::Connect => "connect",
            Self::Headers => "headers",
            Self::FirstByte => "first_byte",
            Self::Idle => "idle",
            Self::Overall => "overall",
        }
    }

    pub(crate) const fn from_timeout(value: TimeoutPhase) -> Self {
        match value {
            TimeoutPhase::Queue => Self::Queue,
            TimeoutPhase::Connect => Self::Connect,
            TimeoutPhase::Headers => Self::Headers,
            TimeoutPhase::FirstByte => Self::FirstByte,
            TimeoutPhase::Idle => Self::Idle,
            TimeoutPhase::Overall => Self::Overall,
        }
    }
}

pub(crate) fn error_labels(error: GatewayError) -> (Outcome, Phase) {
    match error.kind {
        ErrorKind::Timeout { phase } => (Outcome::Timeout, Phase::from_timeout(phase)),
        ErrorKind::Cancelled => (Outcome::Cancelled, Phase::None),
        ErrorKind::UpstreamUnavailable | ErrorKind::UpstreamFailure => {
            (Outcome::UpstreamError, Phase::None)
        }
        ErrorKind::Internal => (Outcome::InternalError, Phase::None),
        ErrorKind::InvalidRequest
        | ErrorKind::Unauthorized
        | ErrorKind::Forbidden
        | ErrorKind::NotFound
        | ErrorKind::Conflict
        | ErrorKind::RateLimited
        | ErrorKind::UnsupportedOperation => (Outcome::ClientError, Phase::None),
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for StatusClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
