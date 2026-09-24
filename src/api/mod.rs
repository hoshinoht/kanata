mod chat;
mod deadline;
mod errors;
mod extensions;
mod lifecycle;
mod multipart;
mod serialization;
mod sse;
mod stream_timeout;
mod transcription;
mod wire;

use axum::{Router, routing::post};

use crate::core::{Capabilities, ChatContent, Request as CoreRequest};
use crate::server::ClientState;

pub(super) use extensions::validate_extensions;

pub(crate) fn routes() -> Router<ClientState> {
    Router::new()
        .route("/chat/completions", post(chat::chat_completions))
        .route("/audio/transcriptions", post(transcription::transcriptions))
}

pub(super) fn supported(
    configured: &Capabilities,
    actual: &Capabilities,
    request: &CoreRequest,
) -> bool {
    check_supported(configured, actual, request).is_ok()
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
