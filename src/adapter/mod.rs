use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;

use crate::core::{Capabilities, GatewayError, NormalizedEvent, Response, RoutedRequest};

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
