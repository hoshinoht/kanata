use serde::Deserialize;
use serde_json::Value;

use crate::core::{ErrorKind, GatewayError};

use super::response::{ContentBlock, UsageFields};

/// One Messages stream event, tagged by its `type`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum StreamEvent {
    MessageStart {
        message: StartMessage,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        #[serde(default)]
        usage: Option<UsageFields>,
    },
    MessageStop,
    Ping,
    Error {
        error: Value,
    },
    /// The API may add event types; unknown ones are skipped.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub(super) struct StartMessage {
    pub(super) id: String,
    pub(super) role: String,
    #[serde(default)]
    pub(super) usage: UsageFields,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(super) enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
}

#[derive(Deserialize)]
pub(super) struct MessageDeltaBody {
    #[serde(default)]
    pub(super) stop_reason: Option<String>,
}

pub(super) fn parse(data: &str) -> Result<StreamEvent, GatewayError> {
    serde_json::from_str(data).map_err(|_| GatewayError {
        kind: ErrorKind::UpstreamFailure,
    })
}
