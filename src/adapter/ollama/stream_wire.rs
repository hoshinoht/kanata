use serde::Deserialize;

use crate::core::{ErrorKind, GatewayError, Usage};

#[derive(Deserialize)]
pub(super) struct StreamChunk {
    pub(super) choices: Vec<StreamChoice>,
    #[serde(default)]
    pub(super) usage: Option<UsagePayload>,
}

#[derive(Deserialize)]
pub(super) struct StreamChoice {
    pub(super) index: u64,
    pub(super) delta: StreamDelta,
    #[serde(default)]
    pub(super) finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct StreamDelta {
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) content: Option<String>,
    /// Ollama's reasoning field.
    #[serde(default)]
    pub(super) reasoning: Option<String>,
    #[serde(default)]
    pub(super) reasoning_content: Option<String>,
    #[serde(default)]
    pub(super) tool_calls: Option<Vec<ToolDelta>>,
}

impl StreamDelta {
    pub(super) fn has_reasoning(&self) -> bool {
        self.reasoning.is_some() || self.reasoning_content.is_some()
    }
}

#[derive(Deserialize)]
pub(super) struct ToolDelta {
    pub(super) index: u64,
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(rename = "type", default)]
    pub(super) kind: Option<String>,
    #[serde(default)]
    pub(super) function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
pub(super) struct FunctionDelta {
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) arguments: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct UsagePayload {
    pub(super) prompt_tokens: u64,
    pub(super) completion_tokens: u64,
    pub(super) total_tokens: u64,
}

impl UsagePayload {
    pub(super) fn into_usage(self) -> Usage {
        Usage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
        }
    }
}

pub(super) fn parse(data: &str) -> Result<StreamChunk, GatewayError> {
    serde_json::from_str(data).map_err(|_| GatewayError {
        kind: ErrorKind::UpstreamFailure,
    })
}
