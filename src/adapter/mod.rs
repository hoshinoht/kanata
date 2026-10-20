use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;

use crate::core::{
    Capabilities, ErrorKind, GatewayError, NormalizedEvent, Response, RoutedRequest,
};

pub mod anthropic;
mod auth_files;
mod chat_completions;
pub mod chatgpt;
pub mod codex;
pub(crate) mod diagnostics;
pub mod ollama;
pub mod openrouter;
pub(crate) mod probe;
pub mod speech;
pub mod vllm;

#[allow(dead_code)]
pub(crate) mod transport;

pub type AdapterFuture =
    Pin<Box<dyn Future<Output = Result<AdapterOutput, GatewayError>> + Send + 'static>>;
pub type EventStream =
    Pin<Box<dyn Stream<Item = Result<NormalizedEvent, GatewayError>> + Send + 'static>>;

pub enum AdapterOutput {
    Complete(Response),
    Events(EventStream),
}

pub trait Adapter: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> &Capabilities;
    fn execute(&self, request: RoutedRequest) -> AdapterFuture;
}

/// Shared upstream HTTP status mapping.
pub(crate) fn status_error(status: u16) -> GatewayError {
    GatewayError::from(match status {
        400 | 422 => ErrorKind::InvalidRequest,
        404 => ErrorKind::NotFound,
        401 | 403 | 408 | 503 | 504 => ErrorKind::UpstreamUnavailable,
        429 => ErrorKind::RateLimited,
        _ => ErrorKind::UpstreamFailure,
    })
}

pub(crate) fn upstream_failure() -> GatewayError {
    ErrorKind::UpstreamFailure.into()
}

pub(crate) fn internal_error() -> GatewayError {
    ErrorKind::Internal.into()
}

pub(crate) fn invalid_request() -> GatewayError {
    ErrorKind::InvalidRequest.into()
}

pub(crate) fn unsupported_operation() -> GatewayError {
    ErrorKind::UnsupportedOperation.into()
}
