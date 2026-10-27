use crate::upstream_failure;
use std::collections::BTreeSet;

use crate::{
    core::{
        FinishReason, GatewayError, MAX_ENCRYPTED_REASONING_BYTES, ModelAlias, NormalizedEvent,
    },
    diagnostics::{ProviderError, UpstreamLabel},
};

use super::{
    response::{
        self, ContentBlock, MAX_TOOL_CALLS, REDACTED_THINKING_PREFIX, THINKING_PREFIX, UsageFields,
        valid_id, valid_tool_name,
    },
    stream_wire::{Delta, StreamEvent},
};

/// Content blocks per reply.
const MAX_BLOCKS: usize = 1024;

/// The open content block; reasoning payloads are kept only when the client asked for them
/// and they fit the per-reply budget.
enum Block {
    Text,
    Thinking { signature: Option<String> },
    RedactedThinking { data: Option<String> },
    ToolUse { id: String },
}

pub(super) struct StreamState {
    public_model: ModelAlias,
    capture_reasoning: bool,
    label: UpstreamLabel,
    message_id: Option<String>,
    open: Option<(usize, Block)>,
    next_index: usize,
    tool_ids: BTreeSet<String>,
    reasoning_bytes: usize,
    usage: UsageFields,
    finish: Option<FinishReason>,
    done: bool,
}

impl StreamState {
    pub(super) fn new(
        public_model: ModelAlias,
        capture_reasoning: bool,
        label: UpstreamLabel,
    ) -> Self {
        Self {
            public_model,
            capture_reasoning,
            label,
            message_id: None,
            open: None,
            next_index: 0,
            tool_ids: BTreeSet::new(),
            reasoning_bytes: 0,
            usage: UsageFields::default(),
            finish: None,
            done: false,
        }
    }

    pub(super) fn is_done(&self) -> bool {
        self.done
    }

    pub(super) fn data(&mut self, data: &str) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if self.done {
            return Err(upstream_failure());
        }
        let event = super::stream_wire::parse(data)?;
        // Content and the stop reason follow `message_start`; only `message_stop` follows
        // the stop reason.
        let out_of_order = self.message_id.is_none()
            || (self.finish.is_some() && !matches!(event, StreamEvent::MessageStop));
        match event {
            StreamEvent::Ping | StreamEvent::Unknown => Ok(Vec::new()),
            StreamEvent::Error { error } => {
                self.label
                    .stream("error", &ProviderError::from_json(&error));
                Err(upstream_failure())
            }
            StreamEvent::MessageStart { message } => {
                if self.message_id.is_some()
                    || message.role != "assistant"
                    || !valid_id(&message.id)
                {
                    return Err(upstream_failure());
                }
                self.message_id = Some(message.id);
                self.usage = message.usage;
                Ok(vec![NormalizedEvent::ChatStarted {
                    model: self.public_model.clone(),
                }])
            }
            _ if out_of_order => Err(upstream_failure()),
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => self.start_block(index, content_block),
            StreamEvent::ContentBlockDelta { index, delta } => self.delta(index, delta),
            StreamEvent::ContentBlockStop { index } => self.stop_block(index),
            StreamEvent::MessageDelta { delta, usage } => {
                if self.open.is_some() {
                    return Err(upstream_failure());
                }
                let reason = response::parse_stop_reason(delta.stop_reason.as_deref())?;
                response::check_finish(reason, !self.tool_ids.is_empty())?;
                self.finish = Some(reason);
                if let Some(usage) = usage {
                    self.usage.merge(&usage);
                }
                Ok(Vec::new())
            }
            StreamEvent::MessageStop => {
                let (Some(finish_reason), None) = (self.finish, self.open.as_ref()) else {
                    return Err(upstream_failure());
                };
                let usage = self.usage.normalize()?;
                self.done = true;
                Ok(vec![NormalizedEvent::ChatCompleted {
                    finish_reason,
                    usage: Some(usage),
                }])
            }
        }
    }

    fn start_block(
        &mut self,
        index: usize,
        block: ContentBlock,
    ) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if self.open.is_some() || index != self.next_index || index >= MAX_BLOCKS {
            return Err(upstream_failure());
        }
        let capture = self.capture_reasoning;
        let (block, event) = match block {
            ContentBlock::Text { text } => (
                Block::Text,
                (!text.is_empty()).then_some(NormalizedEvent::ChatTextDelta { text }),
            ),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => (
                Block::Thinking {
                    signature: capture.then_some(signature),
                },
                (!thinking.is_empty())
                    .then_some(NormalizedEvent::ChatReasoningDelta { text: thinking }),
            ),
            ContentBlock::RedactedThinking { data } => (
                Block::RedactedThinking {
                    data: capture.then_some(data),
                },
                None,
            ),
            ContentBlock::ToolUse { id, name, input } => {
                // Arguments arrive as `input_json_delta`; the start carries an empty object.
                if self.tool_ids.len() == MAX_TOOL_CALLS
                    || !valid_id(&id)
                    || !valid_tool_name(&name)
                    || !input.as_object().is_some_and(|input| input.is_empty())
                    || !self.tool_ids.insert(id.clone())
                {
                    return Err(upstream_failure());
                }
                (
                    Block::ToolUse { id: id.clone() },
                    Some(NormalizedEvent::ChatToolCallDelta {
                        call_id: id,
                        name: Some(name),
                        arguments_delta: String::new(),
                    }),
                )
            }
        };
        self.open = Some((index, block));
        Ok(event.into_iter().collect())
    }

    fn delta(&mut self, index: usize, delta: Delta) -> Result<Vec<NormalizedEvent>, GatewayError> {
        let Some((open_index, block)) = self.open.as_mut() else {
            return Err(upstream_failure());
        };
        if *open_index != index {
            return Err(upstream_failure());
        }
        let event = match (block, delta) {
            (Block::Text, Delta::Text { text }) => {
                (!text.is_empty()).then_some(NormalizedEvent::ChatTextDelta { text })
            }
            (Block::ToolUse { id }, Delta::InputJson { partial_json }) => {
                (!partial_json.is_empty()).then(|| NormalizedEvent::ChatToolCallDelta {
                    call_id: id.clone(),
                    name: None,
                    arguments_delta: partial_json,
                })
            }
            // Omitted display streams one empty thinking delta.
            (Block::Thinking { .. }, Delta::Thinking { thinking }) => (!thinking.is_empty())
                .then_some(NormalizedEvent::ChatReasoningDelta { text: thinking }),
            (Block::Thinking { signature }, Delta::Signature { signature: part }) => {
                if let Some(collected) = signature {
                    if collected.len().saturating_add(part.len()) > MAX_ENCRYPTED_REASONING_BYTES {
                        *signature = None;
                    } else {
                        collected.push_str(&part);
                    }
                }
                None
            }
            _ => return Err(upstream_failure()),
        };
        Ok(event.into_iter().collect())
    }

    fn stop_block(&mut self, index: usize) -> Result<Vec<NormalizedEvent>, GatewayError> {
        let Some((open_index, block)) = self.open.take() else {
            return Err(upstream_failure());
        };
        if open_index != index {
            return Err(upstream_failure());
        }
        self.next_index += 1;
        let (prefix, payload) = match block {
            Block::Thinking {
                signature: Some(signature),
            } => (THINKING_PREFIX, signature),
            Block::RedactedThinking { data: Some(data) } => (REDACTED_THINKING_PREFIX, data),
            _ => return Ok(Vec::new()),
        };
        let message_id = self.message_id.as_deref().ok_or_else(upstream_failure)?;
        Ok(response::reasoning_item(
            prefix,
            message_id,
            index,
            payload,
            &mut self.reasoning_bytes,
        )
        .map(|item| NormalizedEvent::ChatReasoningItem { item })
        .into_iter()
        .collect())
    }
}
