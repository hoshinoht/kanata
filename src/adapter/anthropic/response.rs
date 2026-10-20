use crate::adapter::upstream_failure;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::Value;

use crate::core::{
    ChatContent, ChatMessage, ChatResponse, ChatRole, FinishReason, GatewayError,
    MAX_ENCRYPTED_REASONING_BYTES, MAX_REASONING_BYTES, ModelAlias, ReasoningItem, ToolCall, Usage,
};

pub(super) const MAX_TOOL_CALLS: usize = 64;
const MAX_ID_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 128;

/// Reasoning item ids this adapter mints; replay accepts only these prefixes.
pub(super) const THINKING_PREFIX: &str = "anthropic_thinking_";
pub(super) const REDACTED_THINKING_PREFIX: &str = "anthropic_redacted_thinking_";

#[derive(Deserialize)]
struct MessagePayload {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    role: String,
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
    usage: UsageFields,
}

/// Content blocks Kanata understands; any other type fails the reply.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

/// Usage members; `message_delta` repeats any subset, so all are optional.
#[derive(Default, Deserialize)]
pub(super) struct UsageFields {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Clone, Copy, Deserialize)]
struct OutputTokensDetails {
    #[serde(default)]
    thinking_tokens: Option<u64>,
}

impl UsageFields {
    /// Later counts replace earlier ones.
    pub(super) fn merge(&mut self, update: UsageFields) {
        self.input_tokens = update.input_tokens.or(self.input_tokens);
        self.output_tokens = update.output_tokens.or(self.output_tokens);
        self.cache_creation_input_tokens = update
            .cache_creation_input_tokens
            .or(self.cache_creation_input_tokens);
        self.cache_read_input_tokens = update
            .cache_read_input_tokens
            .or(self.cache_read_input_tokens);
        self.output_tokens_details = update.output_tokens_details.or(self.output_tokens_details);
    }

    /// Input counts cache reads and writes; the cached part is the cache read.
    pub(super) fn normalize(&self) -> Result<Usage, GatewayError> {
        let (Some(uncached), Some(output_tokens)) = (self.input_tokens, self.output_tokens) else {
            return Err(upstream_failure());
        };
        let input_tokens = uncached
            .checked_add(self.cache_read_input_tokens.unwrap_or(0))
            .and_then(|total| total.checked_add(self.cache_creation_input_tokens.unwrap_or(0)))
            .ok_or_else(upstream_failure)?;
        let total_tokens = input_tokens
            .checked_add(output_tokens)
            .ok_or_else(upstream_failure)?;
        let usage = Usage {
            input_tokens,
            output_tokens,
            total_tokens,
            reasoning_tokens: self
                .output_tokens_details
                .and_then(|details| details.thinking_tokens)
                .filter(|thinking| *thinking <= output_tokens),
            cached_input_tokens: None,
        };
        Ok(match self.cache_read_input_tokens {
            Some(cached) => usage.with_cached_input_tokens(cached),
            None => usage,
        })
    }
}

pub(super) fn decode(
    bytes: &[u8],
    public_model: ModelAlias,
    capture_reasoning: bool,
) -> Result<ChatResponse, GatewayError> {
    let payload: MessagePayload = serde_json::from_slice(bytes).map_err(|_| upstream_failure())?;
    if !valid_id(&payload.id) || payload.kind != "message" || payload.role != "assistant" {
        return Err(upstream_failure());
    }
    let finish_reason = parse_stop_reason(payload.stop_reason.as_deref())?;
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut call_ids = BTreeSet::new();
    let mut reasoning = String::new();
    let mut reasoning_items = Vec::new();
    let mut reasoning_bytes = 0usize;
    for (index, block) in payload.content.into_iter().enumerate() {
        match block {
            ContentBlock::Text { text: part } => text.push_str(&part),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                reasoning.push_str(&thinking);
                if capture_reasoning {
                    reasoning_items.extend(reasoning_item(
                        THINKING_PREFIX,
                        &payload.id,
                        index,
                        signature,
                        &mut reasoning_bytes,
                    ));
                }
            }
            ContentBlock::RedactedThinking { data } => {
                if capture_reasoning {
                    reasoning_items.extend(reasoning_item(
                        REDACTED_THINKING_PREFIX,
                        &payload.id,
                        index,
                        data,
                        &mut reasoning_bytes,
                    ));
                }
            }
            ContentBlock::ToolUse { id, name, input } => {
                if calls.len() == MAX_TOOL_CALLS
                    || !valid_id(&id)
                    || !valid_tool_name(&name)
                    || !input.is_object()
                    || !call_ids.insert(id.clone())
                {
                    return Err(upstream_failure());
                }
                calls.push(ChatContent::ToolCall {
                    call: ToolCall {
                        id,
                        name,
                        arguments: input.to_string(),
                    },
                });
            }
        }
    }
    check_finish(finish_reason, !calls.is_empty())?;
    let mut content = Vec::with_capacity(calls.len() + 1);
    if !text.is_empty() {
        content.push(ChatContent::Text { text });
    }
    content.extend(calls);
    truncate_reasoning(&mut reasoning);
    Ok(ChatResponse {
        model: public_model,
        message: ChatMessage {
            role: ChatRole::Assistant,
            content,
        },
        finish_reason,
        usage: Some(payload.usage.normalize()?),
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        reasoning_items,
    })
}

/// A namespaced item for replay; empty payloads and those past the per-reply budget are
/// dropped.
pub(super) fn reasoning_item(
    prefix: &str,
    message_id: &str,
    index: usize,
    payload: String,
    total_bytes: &mut usize,
) -> Option<ReasoningItem> {
    let total = total_bytes.saturating_add(payload.len());
    if payload.is_empty() || total > MAX_ENCRYPTED_REASONING_BYTES {
        return None;
    }
    let item = ReasoningItem::new(Some(format!("{prefix}{message_id}_{index}")), payload).ok()?;
    *total_bytes = total;
    Some(item)
}

pub(super) fn parse_stop_reason(value: Option<&str>) -> Result<FinishReason, GatewayError> {
    match value {
        Some("end_turn" | "stop_sequence") => Ok(FinishReason::Stop),
        Some("max_tokens" | "model_context_window_exceeded") => Ok(FinishReason::Length),
        Some("tool_use") => Ok(FinishReason::ToolCalls),
        Some("refusal") => Ok(FinishReason::ContentFilter),
        _ => Err(upstream_failure()),
    }
}

/// A plain stop carries no tool calls and a tool stop at least one.
pub(super) fn check_finish(reason: FinishReason, has_calls: bool) -> Result<(), GatewayError> {
    match reason {
        FinishReason::Stop if has_calls => Err(upstream_failure()),
        FinishReason::ToolCalls if !has_calls => Err(upstream_failure()),
        _ => Ok(()),
    }
}

pub(super) fn truncate_reasoning(text: &mut String) {
    text.truncate(text.floor_char_boundary(MAX_REASONING_BYTES));
}

/// Message and tool-use ids (`msg_…`, `toolu_…`).
pub(super) fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub(super) fn valid_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
