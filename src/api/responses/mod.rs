mod output;
mod streaming;
mod wire;

use axum::{
    extract::{Request, State},
    response::Response,
};

use crate::server::{Authenticated, ClientState};

pub(super) use output::{Options, complete};
pub(super) use streaming::stream_response;
pub(super) use wire::parse;

pub(super) async fn create(
    auth: Authenticated,
    State(state): State<ClientState>,
    request: Request,
) -> Response {
    super::chat::handle_chat(auth, state, request, super::chat::OutputFormat::Responses).await
}
