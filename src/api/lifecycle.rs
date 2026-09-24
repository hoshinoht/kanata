use axum::body::Body;
use futures_util::stream;

use crate::routing::admission::AdmissionPermit;

use super::sse::state::StreamState;

pub(super) fn stream_body(state: StreamState, permit: AdmissionPermit) -> Body {
    let body = stream::unfold(Some((state, permit)), |state| async move {
        let (state, permit) = state?;
        let (item, state) = state.next().await?;
        let next = (!state.releases_permit_after_frame()).then_some((state, permit));
        Some((item, next))
    });
    Body::from_stream(body)
}
