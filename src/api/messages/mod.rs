//! Stateless `POST /v1/messages`: a subset of the Messages protocol served by chat
//! routes, with the same key scopes, admission, deadlines and quotas as chat.

mod output;
mod streaming;
mod wire;

use axum::{
    extract::{Request, State},
    response::Response,
};

use crate::server::{Authenticated, ClientState};

pub(super) use output::complete;
pub(super) use streaming::stream_start;
pub(super) use wire::parse;

pub(super) async fn create(
    auth: Authenticated,
    State(state): State<ClientState>,
    request: Request,
) -> Response {
    super::chat::handle_chat(auth, state, request, super::chat::OutputFormat::Messages).await
}

/// Route layer: every JSON error on this route, extractor rejections included, uses the
/// Messages envelope. Public-listener authentication runs before routing and keeps the
/// default envelope; `GET /v1/messages` gets the router's bodiless 405 with `Allow: POST`.
pub(super) async fn error_envelope(response: Response) -> Response {
    super::errors::messages_error(response)
}
