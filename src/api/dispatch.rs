//! Handler steps shared by every endpoint, in the order `AGENTS.md` lists.

use std::sync::Arc;

use axum::{
    body::{Bytes, to_bytes},
    extract::Request,
    http::{HeaderMap, header},
    response::Response,
};

use super::{deadline::RequestDeadline, errors::*};
use crate::{
    adapter::{Adapter, AdapterOutput},
    core::{
        Extensions, GatewayError, Request as CoreRequest, RequestContext, RouteSelector,
        RoutedRequest,
    },
    routing::{RouteEntry, admission::AdmissionPermit, uploads::Reservation},
    server::{Authenticated, ClientState},
    telemetry::Observer,
};

/// Telemetry handle and overall deadline of one request.
pub(super) struct Inbound {
    pub(super) observer: Option<Observer>,
    pub(super) deadline: RequestDeadline,
}

/// Drain check, overall deadline and request id.
pub(super) fn inbound(
    state: &ClientState,
    request: &Request,
) -> Result<(Inbound, String), Box<Response>> {
    let observer = request.extensions().get::<Observer>().cloned();
    if state.admission().is_closed() {
        return Err(Box::new(server_draining_observed(observer.as_ref())));
    }
    let deadline = RequestDeadline::new(state.overall_ms())
        .map_err(|error| Box::new(gateway_error_observed(error, observer.as_ref())))?;
    let request_id = request_id(request.headers(), state).ok_or_else(|| Box::new(invalid()))?;
    annotate_client_request_id(observer.as_ref(), request.headers(), &request_id);
    Ok((Inbound { observer, deadline }, request_id))
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    value
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

pub(super) fn routed(
    route: &RouteEntry,
    request_id: String,
    extensions: Extensions,
    request: CoreRequest,
) -> Result<RoutedRequest, Box<Response>> {
    let context = RequestContext {
        request_id,
        route: route.identity.clone(),
        trust_zone: route.trust_zone,
        extensions,
    };
    RoutedRequest::new(context, request).map_err(|_| Box::new(invalid()))
}

impl Inbound {
    pub(super) fn observer(&self) -> Option<&Observer> {
        self.observer.as_ref()
    }

    /// JSON content type, upload reservation and bounded body read; keep the reservation
    /// until the response is built.
    pub(super) async fn read_json(
        &self,
        state: &ClientState,
        request: Request,
        maximum: usize,
    ) -> Result<(Reservation, Bytes), Box<Response>> {
        if !has_json_content_type(request.headers()) {
            return Err(Box::new(invalid()));
        }
        let (reservation, limit) = super::upload::reserve(state, request.headers(), maximum)?;
        let body = to_bytes(request.into_body(), limit);
        match super::upload::read(self.deadline, state.upload_ms(), body).await {
            Ok(Ok(bytes)) => Ok((reservation, bytes)),
            Ok(Err(_)) => Err(Box::new(body_too_large())),
            Err(error) => Err(Box::new(gateway_error_observed(error, self.observer()))),
        }
    }

    /// Exact route and its adapter; a configured route is annotated before authorization.
    pub(super) fn resolve<'a>(
        &self,
        state: &'a ClientState,
        auth: &Authenticated,
        selector: &RouteSelector,
    ) -> Result<(&'a RouteEntry, Arc<dyn Adapter>), Box<Response>> {
        let route = state.registry().resolve(selector);
        if let (Some(observer), Some(_)) = (self.observer(), route) {
            observer.annotate_route(&selector.model_alias.0, selector.operation, false);
        }
        if auth.authorize(selector).is_err() {
            return Err(Box::new(forbidden()));
        }
        let route = route.ok_or_else(|| Box::new(invalid()))?;
        let adapter = state
            .adapter(&route.adapter_id)
            .ok_or_else(|| Box::new(unavailable()))?;
        Ok((route, adapter))
    }

    /// Admission, daily quota and the upstream call, each under the deadline. The permit
    /// has seen the outcome of a call that finished in time.
    pub(super) async fn execute(
        &self,
        auth: &Authenticated,
        state: &ClientState,
        route: &RouteEntry,
        adapter: &dyn Adapter,
        routed: RoutedRequest,
    ) -> Result<(AdmissionPermit, Result<AdapterOutput, GatewayError>), Box<Response>> {
        let (deadline, observer) = (self.deadline, self.observer());
        if let Some(observer) = observer {
            observer.begin_queue();
        }
        let admission = deadline
            .run(|| state.admission().acquire(route, auth.key_limits()))
            .await;
        if let Some(observer) = observer {
            observer.end_queue();
        }
        let mut permit = match admission {
            Ok(Ok(permit)) => permit,
            Ok(Err(error)) => {
                let retry_after = state.admission().retry_after_secs(error);
                let response = admission_rejected_observed(error, retry_after, observer);
                return Err(Box::new(response));
            }
            Err(error) => return Err(Box::new(gateway_error_observed(error, observer))),
        };
        match deadline.run(|| reserve_usage(auth, observer)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(Box::new(quota_rejected(error, observer))),
            Err(error) => return Err(Box::new(gateway_error_observed(error, observer))),
        }
        if let Some(observer) = observer {
            observer.begin_upstream();
            observer.annotate_adapter(&route.adapter_id, route.provider_kind.label());
        }
        let result = deadline.run(|| adapter.execute(routed)).await;
        // A stream's upstream phase ends with its terminal event.
        if !matches!(result, Ok(Ok(AdapterOutput::Events(_))))
            && let Some(observer) = observer
        {
            observer.end_upstream();
        }
        let result = match result {
            Ok(outcome) => {
                permit.record_outcome(&outcome);
                outcome
            }
            Err(error) => Err(error),
        };
        Ok((permit, result))
    }
}
