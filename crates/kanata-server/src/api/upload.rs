use super::{
    deadline::{RequestDeadline, timeout},
    errors,
};
use crate::{core::TimeoutPhase, server::ClientState};
use axum::{
    http::{HeaderMap, header},
    response::Response,
};
use std::{future::Future, time::Duration};

pub(super) fn reserve(
    state: &ClientState,
    headers: &HeaderMap,
    maximum: usize,
) -> Result<(crate::routing::uploads::Reservation, usize), Box<Response>> {
    let mut values = headers.get_all(header::CONTENT_LENGTH).iter();
    let limit = match values.next() {
        Some(value) => {
            if values.next().is_some() {
                return Err(Box::new(errors::invalid()));
            }
            let length = value
                .to_str()
                .ok()
                .and_then(|text| text.parse::<usize>().ok())
                .ok_or_else(|| Box::new(errors::invalid()))?;
            if length > maximum {
                return Err(Box::new(errors::body_too_large()));
            }
            length
        }
        None => maximum,
    };
    state
        .uploads()
        .acquire(limit)
        .map(|permit| (permit, limit))
        .ok_or_else(|| Box::new(errors::upload_busy()))
}

pub(super) async fn read<F: Future>(
    deadline: RequestDeadline,
    milliseconds: u64,
    future: F,
) -> Result<F::Output, crate::core::GatewayError> {
    deadline
        .run(|| async {
            tokio::time::timeout(Duration::from_millis(milliseconds), future)
                .await
                .map_err(|_| timeout(TimeoutPhase::Upload))
        })
        .await?
}
