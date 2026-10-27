//! The `Adapter` trait and shared upstream error helpers.

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;

use crate::{Capabilities, ErrorKind, GatewayError, NormalizedEvent, Response, RoutedRequest};

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
pub fn status_error(status: u16) -> GatewayError {
    GatewayError::from(match status {
        400 | 422 => ErrorKind::InvalidRequest,
        404 => ErrorKind::NotFound,
        401 | 403 | 408 | 503 | 504 => ErrorKind::UpstreamUnavailable,
        429 => ErrorKind::RateLimited,
        _ => ErrorKind::UpstreamFailure,
    })
}

pub fn upstream_failure() -> GatewayError {
    ErrorKind::UpstreamFailure.into()
}

pub fn internal_error() -> GatewayError {
    ErrorKind::Internal.into()
}

pub fn invalid_request() -> GatewayError {
    ErrorKind::InvalidRequest.into()
}

pub fn unsupported_operation() -> GatewayError {
    ErrorKind::UnsupportedOperation.into()
}
