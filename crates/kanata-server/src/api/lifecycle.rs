use std::time::Duration;

use axum::body::Body;
use futures_util::stream;

use crate::routing::admission::AdmissionPermit;

use super::keepalive::{COMMENT, keepalive};
use super::sse::state::StreamState;

pub(super) fn stream_body(state: StreamState, permit: AdmissionPermit, interval: Duration) -> Body {
    let body = stream::unfold(Some((state, permit)), |state| async move {
        let (state, permit) = state?;
        let (item, state) = state.next().await?;
        let next = (!state.releases_permit_after_frame()).then_some((state, permit));
        Some((item, next))
    });
    Body::from_stream(keepalive(body, interval, COMMENT))
}
