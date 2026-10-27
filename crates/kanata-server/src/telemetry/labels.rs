use crate::core::{ErrorKind, GatewayError, TimeoutPhase};

str_enum! {
    #[derive(Clone, Copy, Eq, PartialEq)]
    pub(crate) enum Endpoint {
        Chat = 0 => "chat",
        Transcription = 1 => "transcription",
        Models = 2 => "models",
        Embeddings = 4 => "embeddings",
        Responses = 5 => "responses",
        Speech = 6 => "speech",
        Messages = 7 => "messages",
        Other = 3 => "other",
    }
    const ALL;
}

impl Endpoint {
    pub(crate) fn from_path(path: &str) -> Self {
        match path {
            "/v1/chat/completions" => Self::Chat,
            "/v1/audio/transcriptions" => Self::Transcription,
            "/v1/models" => Self::Models,
            "/v1/embeddings" => Self::Embeddings,
            "/v1/responses" => Self::Responses,
            "/v1/audio/speech" => Self::Speech,
            "/v1/messages" => Self::Messages,
            _ if path.starts_with("/v1/models/") => Self::Models,
            _ => Self::Other,
        }
    }
}

str_enum! {
    #[derive(Clone, Copy, Eq, PartialEq)]
    pub(crate) enum Outcome {
        Success = 0 => "success",
        ClientError = 1 => "client_error",
        UpstreamError = 2 => "upstream_error",
        InternalError = 3 => "internal_error",
        Timeout = 4 => "timeout",
        Cancelled = 5 => "cancelled",
        Draining = 6 => "draining",
    }
}

str_enum! {
    #[derive(Clone, Copy, Eq, PartialEq)]
    pub(crate) enum StatusClass {
        Unknown = 0 => "unknown",
        Informational = 1 => "1xx",
        Success = 2 => "2xx",
        Redirection = 3 => "3xx",
        ClientError = 4 => "4xx",
        ServerError = 5 => "5xx",
    }
    const ALL;
}

impl StatusClass {
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

str_enum! {
    #[derive(Clone, Copy, Eq, PartialEq)]
    pub(crate) enum Phase {
        Upload = 7 => "upload",
        None = 0 => "none",
        Queue = 1 => "queue",
        Connect = 2 => "connect",
        Headers = 3 => "headers",
        FirstByte = 4 => "first_byte",
        Idle = 5 => "idle",
        Overall = 6 => "overall",
    }
}

impl Phase {
    pub(crate) const fn from_timeout(value: TimeoutPhase) -> Self {
        match value {
            TimeoutPhase::Upload => Self::Upload,
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
