use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    adapter::diagnostics::ProviderError,
    adapter::transport::sse::{SseFramer, SseRecord},
    core::{
        ChatContent, ChatMessage, ChatResponse, ChatRole, ErrorKind, FinishReason, GatewayError,
        ModelAlias, NormalizedEvent, ToolCall, Usage,
    },
};

/// Codex repeats instructions, tools and output in single events.
const MAX_CODEX_EVENT_BYTES: usize = 4 * 1024 * 1024;
/// Assembled text and tool-argument bytes per response.
const MAX_CODEX_OUTPUT_BYTES: usize = 1024 * 1024;

const MAX_OUTPUT_ITEMS: usize = 64;
const MAX_TOOL_CALL_ID_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResponsesStreamError {
    BeforeOutput,
    AfterOutput,
}

impl ResponsesStreamError {
    pub(crate) fn gateway_error(self) -> GatewayError {
        GatewayError {
            kind: ErrorKind::UpstreamFailure,
        }
    }
}

/// Why the stream was rejected, for operator diagnostics only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StreamFailure {
    pub(crate) event: String,
    pub(crate) error: ProviderError,
}

impl StreamFailure {
    fn new(event: &str) -> Self {
        Self {
            event: event.to_owned(),
            error: ProviderError::default(),
        }
    }

    fn from_record(record: &SseRecord) -> Self {
        let value: Option<Value> = serde_json::from_str(&record.data).ok();
        let kind = record
            .event
            .as_deref()
            .or_else(|| value.as_ref()?.get("type")?.as_str());
        let mut failure = Self::new(
            &kind
                .and_then(crate::telemetry::sanitize::token)
                .unwrap_or_else(|| "unparseable".to_owned()),
        );
        if let Some(value) = value.as_ref() {
            match failure.event.as_str() {
                "response.failed" | "response.incomplete" => {
                    let response = value.get("response").unwrap_or(value);
                    failure.error = ProviderError::from_json(response);
                    if failure.error.code.is_none() {
                        failure.error.code = response
                            .pointer("/incomplete_details/reason")
                            .and_then(Value::as_str)
                            .and_then(crate::telemetry::sanitize::token);
                    }
                }
                "error" => failure.error = ProviderError::from_json(value),
                _ => {}
            }
        }
        failure
    }
}

pub(crate) struct ResponsesStreamParser {
    framer: SseFramer,
    public_model: ModelAlias,
    response_id: Option<String>,
    parts: Vec<OutputPart>,
    tools: BTreeMap<usize, ToolState>,
    messages: BTreeMap<usize, MessageState>,
    tool_call_ids: BTreeSet<String>,
    untagged_text: bool,
    last_sequence_number: Option<u64>,
    output_bytes: usize,
    collected_response: Option<ChatResponse>,
    started: bool,
    output_started: bool,
    done: bool,
    done_marker_seen: bool,
    input_finished: bool,
    failed: bool,
    failure: Option<StreamFailure>,
}

impl ResponsesStreamParser {
    pub(crate) fn new(public_model: ModelAlias) -> Self {
        Self {
            framer: SseFramer::new_with_named_events()
                .with_limits(MAX_CODEX_EVENT_BYTES, MAX_CODEX_EVENT_BYTES),
            public_model,
            response_id: None,
            parts: Vec::new(),
            tools: BTreeMap::new(),
            messages: BTreeMap::new(),
            tool_call_ids: BTreeSet::new(),
            untagged_text: false,
            last_sequence_number: None,
            output_bytes: 0,
            collected_response: None,
            started: false,
            output_started: false,
            done: false,
            done_marker_seen: false,
            input_finished: false,
            failed: false,
            failure: None,
        }
    }

    pub(crate) fn feed(
        &mut self,
        bytes: &[u8],
    ) -> Result<Vec<NormalizedEvent>, ResponsesStreamError> {
        if self.failed || self.input_finished || (self.done && self.done_marker_seen) {
            return Err(self.current_error());
        }
        let (records, _) = match self.framer.feed_until(bytes, is_terminal_record) {
            Ok(result) => result,
            Err(_) => return Err(self.fail_with(StreamFailure::new("sse_framing"))),
        };

        let mut events = Vec::new();
        for record in records {
            match self.record(&record) {
                Ok(mut translated) => events.append(&mut translated),
                Err(()) => return Err(self.fail_with(StreamFailure::from_record(&record))),
            }
        }
        if !events.is_empty() {
            self.output_started = true;
        }
        Ok(events)
    }

    pub(crate) fn finish_input(&mut self) -> Result<(), ResponsesStreamError> {
        if self.failed {
            return Err(self.current_error());
        }
        if self.framer.finish().is_err() {
            return Err(self.fail_with(StreamFailure::new("sse_framing")));
        }
        if !self.done {
            return Err(self.fail_with(StreamFailure::new("eof_before_completed")));
        }
        self.input_finished = true;
        Ok(())
    }

    pub(crate) fn take_chat_response(&mut self) -> Option<ChatResponse> {
        if self.done && !self.failed {
            self.collected_response.take()
        } else {
            None
        }
    }

    fn record(&mut self, record: &SseRecord) -> Result<Vec<NormalizedEvent>, ()> {
        if self.done {
            if record.data == "[DONE]" && record.event.is_none() {
                self.done_marker_seen = true;
            }
            // Records after response.completed are not read by the reference client either.
            return Ok(Vec::new());
        }
        if record.data == "[DONE]" {
            return Err(());
        }

        let value: Value = serde_json::from_str(&record.data).map_err(|_| ())?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(())?
            .to_owned();
        if record.event.as_deref().is_some_and(|event| event != kind) {
            return Err(());
        }
        if let Some(sequence_number) = value.get("sequence_number").and_then(Value::as_u64) {
            self.observe_sequence(sequence_number)?;
        }
        match kind.as_str() {
            "response.failed" | "response.incomplete" | "error" => Err(()),
            "response.created" => self.created(value),
            "response.output_item.added" => self.output_item_added(value),
            "response.output_item.done" => self.output_item_done(value),
            "response.output_text.delta" => self.text_delta(value),
            "response.function_call_arguments.delta" => self.tool_arguments_delta(value),
            "response.function_call_arguments.done" => self.tool_arguments_done(value),
            "response.completed" => self.completed(value),
            // Lifecycle, metadata, reasoning, and unknown events carry nothing Kanata exposes.
            _ => Ok(Vec::new()),
        }
    }

    fn created(&mut self, value: Value) -> Result<Vec<NormalizedEvent>, ()> {
        let event: ResponseEvent<ResponseIdentity> = parse_event(value)?;
        if self.response_id.is_some()
            || !self.parts.is_empty()
            || event.response.id.trim().is_empty()
        {
            return Err(());
        }
        self.response_id = Some(event.response.id);
        Ok(Vec::new())
    }

    fn text_delta(&mut self, value: Value) -> Result<Vec<NormalizedEvent>, ()> {
        let event: TextDeltaEvent = parse_event(value)?;
        if event.delta.is_empty() {
            return Ok(Vec::new());
        }
        self.add_output_bytes(event.delta.len())?;
        let known = event.item_id.as_deref().and_then(|item_id| {
            self.messages
                .iter_mut()
                .find(|(_, message)| message.item_id == item_id)
        });
        match known {
            Some((index, message)) => {
                if message.done || event.output_index.is_some_and(|output| output != *index) {
                    return Err(());
                }
                message.text.push_str(&event.delta);
            }
            None => {
                if event.item_id.is_some()
                    && event
                        .output_index
                        .is_some_and(|output| self.messages.contains_key(&output))
                {
                    return Err(());
                }
                self.untagged_text = true;
            }
        }
        Ok(self.emit_text(event.delta))
    }

    fn output_item_added(&mut self, value: Value) -> Result<Vec<NormalizedEvent>, ()> {
        let event: OutputItemEvent = parse_event(value)?;
        let index = event.output_index;
        if self.tools.contains_key(&index) || self.messages.contains_key(&index) {
            return Err(());
        }
        match item_type(&event.item) {
            "function_call" => {
                let item: FunctionCallItem = parse_event(event.item)?;
                self.add_tool(index, item)
            }
            "message" => {
                let item: MessageItem = parse_event(event.item)?;
                let item_id = item.id.ok_or(())?;
                if !valid_id(&item_id)
                    || self
                        .messages
                        .values()
                        .any(|message| message.item_id == item_id)
                {
                    return Err(());
                }
                self.reserve_output_item()?;
                self.messages.insert(
                    index,
                    MessageState {
                        item_id,
                        text: String::new(),
                        done: false,
                    },
                );
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    fn output_item_done(&mut self, value: Value) -> Result<Vec<NormalizedEvent>, ()> {
        let event: OutputItemEvent = parse_event(value)?;
        let index = event.output_index;
        match item_type(&event.item) {
            "function_call" => {
                let item: FunctionCallItem = parse_event(event.item)?;
                let Some(tool) = self.tools.get(&index) else {
                    if self.messages.contains_key(&index) {
                        return Err(());
                    }
                    let events = self.add_tool(index, item)?;
                    let tool = self.tools.get_mut(&index).ok_or(())?;
                    tool.arguments_done = true;
                    tool.item_done = true;
                    return Ok(events);
                };
                if tool.item_done
                    || tool.call_id != item.call_id
                    || tool.name != item.name
                    || item
                        .id
                        .as_ref()
                        .zip(tool.item_id.as_ref())
                        .is_some_and(|(done_id, added_id)| done_id != added_id)
                    || (tool.arguments_done && tool.arguments != item.arguments)
                    || !item.arguments.starts_with(&tool.arguments)
                {
                    return Err(());
                }
                let events = self.finish_tool_arguments(index, item.arguments)?;
                self.tools.get_mut(&index).ok_or(())?.item_done = true;
                Ok(events)
            }
            "message" => {
                let item: MessageItem = parse_event(event.item)?;
                let text: String = item
                    .content
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect();
                let streamed = match self.messages.get_mut(&index) {
                    Some(message) => {
                        if message.done
                            || item.id.as_deref().is_some_and(|id| id != message.item_id)
                            || (!message.text.is_empty() && message.text != text)
                        {
                            return Err(());
                        }
                        message.done = true;
                        !message.text.is_empty()
                    }
                    None => {
                        if self.tools.contains_key(&index) {
                            return Err(());
                        }
                        self.reserve_output_item()?;
                        self.messages.insert(
                            index,
                            MessageState {
                                item_id: item.id.unwrap_or_default(),
                                text: String::new(),
                                done: true,
                            },
                        );
                        false
                    }
                };
                // Items whose text never arrived as deltas are emitted whole.
                if streamed || self.untagged_text || text.is_empty() {
                    return Ok(Vec::new());
                }
                self.add_output_bytes(text.len())?;
                Ok(self.emit_text(text))
            }
            _ => Ok(Vec::new()),
        }
    }

    fn tool_arguments_delta(&mut self, value: Value) -> Result<Vec<NormalizedEvent>, ()> {
        let event: ToolArgumentsDeltaEvent = parse_event(value)?;
        let tool = self.tools.get(&event.output_index).ok_or(())?;
        if tool.arguments_done || mismatched_item(event.item_id.as_deref(), tool) {
            return Err(());
        }
        if event.delta.is_empty() {
            return Ok(Vec::new());
        }
        self.add_output_bytes(event.delta.len())?;
        let tool = self.tools.get_mut(&event.output_index).ok_or(())?;
        tool.arguments.push_str(&event.delta);
        Ok(vec![NormalizedEvent::ChatToolCallDelta {
            call_id: tool.call_id.clone(),
            name: None,
            arguments_delta: event.delta,
        }])
    }

    fn tool_arguments_done(&mut self, value: Value) -> Result<Vec<NormalizedEvent>, ()> {
        let event: ToolArgumentsDoneEvent = parse_event(value)?;
        let tool = self.tools.get(&event.output_index).ok_or(())?;
        if tool.arguments_done
            || mismatched_item(event.item_id.as_deref(), tool)
            || !event.arguments.starts_with(&tool.arguments)
        {
            return Err(());
        }
        self.finish_tool_arguments(event.output_index, event.arguments)
    }

    fn completed(&mut self, value: Value) -> Result<Vec<NormalizedEvent>, ()> {
        let event: ResponseEvent<CompletedResponse> = parse_event(value)?;
        if event.response.id.trim().is_empty()
            || self.response_id.as_deref() != Some(event.response.id.as_str())
            || self
                .tools
                .values()
                .any(|tool| !tool.arguments_done && !tool.item_done)
            || self.messages.values().any(|message| !message.done)
        {
            return Err(());
        }
        let usage = event.response.usage.map(normalize_usage).transpose()?;
        let content = self.collected_content();
        if content.is_empty() {
            return Err(());
        }
        let finish_reason = if self.tools.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        };
        self.collected_response = Some(ChatResponse {
            model: self.public_model.clone(),
            message: ChatMessage {
                role: ChatRole::Assistant,
                content,
            },
            finish_reason,
            usage: usage.clone(),
        });
        self.done = true;
        Ok(vec![NormalizedEvent::ChatCompleted {
            finish_reason,
            usage,
        }])
    }

    fn add_tool(
        &mut self,
        index: usize,
        item: FunctionCallItem,
    ) -> Result<Vec<NormalizedEvent>, ()> {
        if !valid_id(&item.call_id)
            || !valid_tool_name(&item.name)
            || item.id.as_deref().is_some_and(|id| !valid_id(id))
        {
            return Err(());
        }
        self.reserve_output_item()?;
        if !self.tool_call_ids.insert(item.call_id.clone()) {
            return Err(());
        }
        self.add_output_bytes(item.arguments.len())?;
        self.parts.push(OutputPart::Tool(index));
        self.tools.insert(
            index,
            ToolState {
                item_id: item.id,
                call_id: item.call_id.clone(),
                name: item.name.clone(),
                arguments: item.arguments.clone(),
                arguments_done: false,
                item_done: false,
            },
        );
        let mut events = self.start_event();
        events.push(NormalizedEvent::ChatToolCallDelta {
            call_id: item.call_id,
            name: Some(item.name),
            arguments_delta: item.arguments,
        });
        Ok(events)
    }

    /// Callers verify `arguments` extends the streamed prefix.
    fn finish_tool_arguments(
        &mut self,
        index: usize,
        arguments: String,
    ) -> Result<Vec<NormalizedEvent>, ()> {
        let tool = self.tools.get(&index).ok_or(())?;
        let suffix = arguments.get(tool.arguments.len()..).ok_or(())?.to_owned();
        self.add_output_bytes(suffix.len())?;
        let tool = self.tools.get_mut(&index).ok_or(())?;
        tool.arguments = arguments;
        tool.arguments_done = true;
        if suffix.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![NormalizedEvent::ChatToolCallDelta {
            call_id: tool.call_id.clone(),
            name: None,
            arguments_delta: suffix,
        }])
    }

    fn collected_content(&self) -> Vec<ChatContent> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                OutputPart::Text(text) => Some(ChatContent::Text { text: text.clone() }),
                OutputPart::Tool(index) => {
                    self.tools.get(index).map(|tool| ChatContent::ToolCall {
                        call: ToolCall {
                            id: tool.call_id.clone(),
                            name: tool.name.clone(),
                            arguments: tool.arguments.clone(),
                        },
                    })
                }
            })
            .collect()
    }

    fn emit_text(&mut self, text: String) -> Vec<NormalizedEvent> {
        if let Some(OutputPart::Text(current)) = self.parts.last_mut() {
            current.push_str(&text);
        } else {
            self.parts.push(OutputPart::Text(text.clone()));
        }
        let mut events = self.start_event();
        events.push(NormalizedEvent::ChatTextDelta { text });
        events
    }

    fn start_event(&mut self) -> Vec<NormalizedEvent> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![NormalizedEvent::ChatStarted {
            model: self.public_model.clone(),
        }]
    }

    fn reserve_output_item(&self) -> Result<(), ()> {
        if self.tools.len() + self.messages.len() >= MAX_OUTPUT_ITEMS {
            return Err(());
        }
        Ok(())
    }

    fn add_output_bytes(&mut self, bytes: usize) -> Result<(), ()> {
        self.output_bytes = self.output_bytes.checked_add(bytes).ok_or(())?;
        if self.output_bytes > MAX_CODEX_OUTPUT_BYTES {
            return Err(());
        }
        Ok(())
    }

    fn observe_sequence(&mut self, sequence_number: u64) -> Result<(), ()> {
        if self
            .last_sequence_number
            .is_some_and(|previous| sequence_number <= previous)
        {
            return Err(());
        }
        self.last_sequence_number = Some(sequence_number);
        Ok(())
    }

    pub(crate) fn failure(&self) -> Option<&StreamFailure> {
        self.failure.as_ref()
    }

    fn fail_with(&mut self, failure: StreamFailure) -> ResponsesStreamError {
        self.failure = Some(failure);
        self.failed = true;
        self.current_error()
    }

    fn current_error(&self) -> ResponsesStreamError {
        if self.output_started {
            ResponsesStreamError::AfterOutput
        } else {
            ResponsesStreamError::BeforeOutput
        }
    }
}

fn parse_event<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, ()> {
    serde_json::from_value(value).map_err(|_| ())
}

fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn mismatched_item(item_id: Option<&str>, tool: &ToolState) -> bool {
    item_id
        .zip(tool.item_id.as_deref())
        .is_some_and(|(event_id, added_id)| event_id != added_id)
}

fn is_terminal_record(record: &SseRecord) -> bool {
    record.data == "[DONE]"
}

fn normalize_usage(usage: ResponsesUsage) -> Result<Usage, ()> {
    if usage.input_tokens.checked_add(usage.output_tokens) != Some(usage.total_tokens) {
        return Err(());
    }
    Ok(Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
    })
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_CALL_ID_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn valid_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

// Upstream payloads carry many fields Kanata does not use; unknown fields are ignored.
#[derive(Deserialize)]
struct ResponseEvent<T> {
    response: T,
}

#[derive(Deserialize)]
struct ResponseIdentity {
    id: String,
}

#[derive(Deserialize)]
struct CompletedResponse {
    id: String,
    usage: Option<ResponsesUsage>,
}

#[derive(Deserialize)]
struct ResponsesUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

#[derive(Deserialize)]
struct TextDeltaEvent {
    delta: String,
    item_id: Option<String>,
    output_index: Option<usize>,
}

#[derive(Deserialize)]
struct OutputItemEvent {
    output_index: usize,
    item: Value,
}

#[derive(Deserialize)]
struct FunctionCallItem {
    id: Option<String>,
    call_id: String,
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize)]
struct MessageItem {
    id: Option<String>,
    #[serde(default)]
    content: Vec<Value>,
}

#[derive(Deserialize)]
struct ToolArgumentsDeltaEvent {
    output_index: usize,
    item_id: Option<String>,
    delta: String,
}

#[derive(Deserialize)]
struct ToolArgumentsDoneEvent {
    output_index: usize,
    item_id: Option<String>,
    arguments: String,
}

enum OutputPart {
    Text(String),
    Tool(usize),
}

struct ToolState {
    item_id: Option<String>,
    call_id: String,
    name: String,
    arguments: String,
    arguments_done: bool,
    item_done: bool,
}

struct MessageState {
    item_id: String,
    text: String,
    done: bool,
}
