/// Value of a `dispatch` step, or its error response returned from the handler.
macro_rules! try_step {
    ($step:expr) => {
        match $step {
            Ok(value) => value,
            Err(response) => return *response,
        }
    };
}

mod chat;
mod deadline;
mod dispatch;
mod embeddings;
mod errors;
mod extensions;
mod keepalive;
mod lifecycle;
mod messages;
mod multipart;
mod responses;
mod serialization;
mod speech;
mod sse;
mod stream_timeout;
mod transcription;
mod upload;
mod wire;

use axum::{Router, middleware::map_response, routing::post};

use crate::core::{Capabilities, ChatContent, Request as CoreRequest};
use crate::server::ClientState;

pub(super) use extensions::validate_extensions;

pub(crate) fn routes() -> Router<ClientState> {
    Router::new()
        .route("/chat/completions", post(chat::chat_completions))
        .route("/responses", post(responses::create))
        .route(
            "/messages",
            post(messages::create).route_layer(map_response(messages::error_envelope)),
        )
        .route("/embeddings", post(embeddings::embeddings))
        .route("/audio/speech", post(speech::speech))
        .route("/audio/transcriptions", post(transcription::transcriptions))
}

/// On rejection, yields the wire parameter to blame when one is identifiable.
pub(super) fn check_supported(
    configured: &Capabilities,
    actual: &Capabilities,
    request: &CoreRequest,
) -> Result<(), Option<&'static str>> {
    configured
        .check_request(request)
        .and_then(|()| actual.check_request(request))
        .map_err(|error| error.param())?;
    if request_has_tool_history(request) && (!configured.function_tools || !actual.function_tools) {
        return Err(None);
    }
    Ok(())
}

fn request_has_tool_history(request: &CoreRequest) -> bool {
    let CoreRequest::Chat(chat) = request else {
        return false;
    };
    chat.messages.iter().any(|message| {
        message.content.iter().any(|content| {
            matches!(
                content,
                ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. }
            )
        })
    })
}

#[cfg(test)]
mod fuzz_tests;
