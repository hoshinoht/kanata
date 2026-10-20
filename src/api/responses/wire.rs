use std::borrow::Cow;
use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::core::{ChatContent, ChatMessage, ChatRequest, ChatRole, MaxTokensParam, ReasoningItem};

use super::super::wire::{ChatWire, ChatWireError, serde_error};

const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";
const MAX_CLIENT_METADATA_ENTRIES: usize = 32;
/// Bound on the summed key and value bytes of `client_metadata`.
const MAX_CLIENT_METADATA_BYTES: usize = 16 * 1024;
const MAX_METADATA_ENTRIES: usize = 16;
const MAX_METADATA_KEY_BYTES: usize = 64;
const MAX_METADATA_VALUE_BYTES: usize = 512;
const MAX_USER_BYTES: usize = 256;

/// Responses member a converted chat message came from, for naming failures.
#[derive(Clone, Copy)]
enum Origin {
    Instructions,
    Input,
    Item(usize),
}

/// Where an encrypted reasoning input item lands in the converted history.
enum Placement {
    /// Into chat message `message` at IR content index `position`.
    Into { message: usize, position: usize },
    /// As a reasoning-only assistant turn inserted before chat message `message`.
    Before { message: usize },
}

pub(in crate::api) fn parse(
    bytes: &[u8],
    max_audio_bytes: usize,
) -> Result<(ChatRequest, bool), ChatWireError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ChatWireError::Malformed)?;
    let Value::Object(mut value) = value else {
        return Err(ChatWireError::Malformed);
    };
    for (name, expected) in [
        ("background", json!(false)),
        ("truncation", json!("disabled")),
    ] {
        if let Some(actual) = value.remove(name)
            && actual != expected
        {
            return Err(ChatWireError::param(name));
        }
    }
    for name in [
        "previous_response_id",
        "conversation",
        "service_tier",
        "prompt",
        "stream_options",
    ] {
        if value.contains_key(name) {
            return Err(ChatWireError::param(name));
        }
    }
    let include_encrypted_reasoning = match value.remove("include") {
        None => false,
        Some(Value::Array(items))
            if items
                .iter()
                .all(|item| item.as_str() == Some(ENCRYPTED_REASONING)) =>
        {
            !items.is_empty()
        }
        Some(_) => return Err(ChatWireError::param("include")),
    };
    // Client bookkeeping: validated for size, then dropped; never forwarded or logged.
    if value
        .remove("client_metadata")
        .is_some_and(|metadata| !valid_client_metadata(&metadata))
    {
        return Err(ChatWireError::param("client_metadata"));
    }
    if value
        .remove("metadata")
        .is_some_and(|metadata| !valid_metadata(&metadata))
    {
        return Err(ChatWireError::param("metadata"));
    }
    if value.remove("user").is_some_and(|user| {
        !user
            .as_str()
            .is_some_and(|user| (1..=MAX_USER_BYTES).contains(&user.len()))
    }) {
        return Err(ChatWireError::param("user"));
    }
    if let Some(text) = value.remove("text") {
        validate_text(text)?;
    }
    let mut chat = Map::new();
    match value.remove("reasoning") {
        None | Some(Value::Null) => {}
        Some(Value::Object(mut reasoning)) => {
            if let Some(effort) = reasoning.remove("effort") {
                chat.insert("reasoning_effort".into(), effort);
            }
            // The operator's route setting governs summaries; a client preference is ignored.
            if reasoning.remove("summary").is_some_and(|summary| {
                !summary.is_null()
                    && !matches!(summary.as_str(), Some("auto" | "concise" | "detailed"))
            }) {
                return Err(ChatWireError::param("reasoning.summary"));
            }
            if let Some(key) = reasoning.keys().next() {
                return Err(ChatWireError::member("reasoning", key));
            }
        }
        Some(_) => return Err(ChatWireError::param("reasoning")),
    }
    for name in [
        "model",
        "stream",
        "temperature",
        "top_p",
        "store",
        "parallel_tool_calls",
        "prompt_cache_key",
    ] {
        if let Some(value) = value.remove(name) {
            chat.insert(name.into(), value);
        }
    }
    if let Some(value) = value.remove("max_output_tokens") {
        chat.insert("max_completion_tokens".into(), value);
    }
    let mut messages = Vec::new();
    let mut origins = Vec::new();
    if let Some(instructions) = value.remove("instructions") {
        if !instructions.is_string() {
            return Err(ChatWireError::param("instructions"));
        }
        messages.push(json!({"role":"system","content":instructions}));
        origins.push(Origin::Instructions);
    }
    let mut placements = Vec::new();
    match value.remove("input") {
        Some(Value::String(text)) => {
            messages.push(json!({"role":"user","content":text}));
            origins.push(Origin::Input);
        }
        Some(Value::Array(items)) if !items.is_empty() => {
            let mut history = History {
                messages,
                origins,
                placements,
                pending_calls: BTreeSet::new(),
                seen_calls: BTreeSet::new(),
                pending_reasoning: Vec::new(),
            };
            for (index, item) in items.into_iter().enumerate() {
                history.push(index, item)?;
            }
            if !history.pending_calls.is_empty() {
                return Err(ChatWireError::param("input"));
            }
            history.flush_orphans();
            History {
                messages,
                origins,
                placements,
                ..
            } = history;
        }
        _ => return Err(ChatWireError::param("input")),
    }
    chat.insert("messages".into(), Value::Array(messages));
    if let Some(tools) = value.remove("tools") {
        let Value::Array(tools) = tools else {
            return Err(ChatWireError::param("tools"));
        };
        let mut converted = Vec::with_capacity(tools.len());
        for (index, tool) in tools.into_iter().enumerate() {
            let Value::Object(mut tool) = tool else {
                return Err(ChatWireError::param(format!("tools[{index}]")));
            };
            // Hosted and grouped tools (`namespace`, `web_search`, `custom`, ...) have no
            // chat equivalent.
            if tool.remove("type").as_ref().and_then(Value::as_str) != Some("function") {
                return Err(ChatWireError::param(format!("tools[{index}]")));
            }
            converted.push(json!({"type":"function","function":tool}));
        }
        chat.insert("tools".into(), Value::Array(converted));
    }
    if let Some(mut choice) = value.remove("tool_choice") {
        if let Value::Object(object) = &mut choice {
            if object.remove("type") != Some(json!("function"))
                || object.len() != 1
                || !object.get("name").is_some_and(Value::is_string)
            {
                return Err(ChatWireError::param("tool_choice"));
            }
            choice = json!({"type":"function","function":{"name":object.remove("name")}});
        }
        chat.insert("tool_choice".into(), choice);
    }
    if let Some(key) = value.keys().next() {
        return Err(ChatWireError::member("", key));
    }
    let rename = |error| responses_param(error, &origins);
    let wire: ChatWire = serde_path_to_error::deserialize(Value::Object(chat))
        .map_err(|error| rename(serde_error(error)))?;
    let (mut chat, _) = wire.into_core(max_audio_bytes).map_err(rename)?;
    chat.options.max_output_tokens_param = MaxTokensParam::MaxOutputTokens;
    chat.include_encrypted_reasoning = include_encrypted_reasoning;
    place_reasoning(&mut chat.messages, placements);
    Ok((chat, true))
}

/// Input items converted to chat messages, plus encrypted reasoning held for placement.
struct History {
    messages: Vec<Value>,
    origins: Vec<Origin>,
    placements: Vec<(Placement, ReasoningItem)>,
    pending_calls: BTreeSet<String>,
    seen_calls: BTreeSet<String>,
    /// Encrypted reasoning not yet followed by assistant output.
    pending_reasoning: Vec<ReasoningItem>,
}

impl History {
    fn push(&mut self, index: usize, item: Value) -> Result<(), ChatWireError> {
        let path = format!("input[{index}]");
        let invalid = || ChatWireError::param(path.clone());
        let Value::Object(mut item) = item else {
            return Err(invalid());
        };
        let kind = match item.remove("type") {
            None => "message".to_owned(),
            Some(Value::String(kind)) => kind,
            Some(_) => return Err(ChatWireError::member(&path, "type")),
        };
        match kind.as_str() {
            "reasoning" => {
                let id = replay_metadata(&mut item, &path)?;
                let Some(Value::Array(parts)) = item.remove("summary") else {
                    return Err(ChatWireError::member(&path, "summary"));
                };
                for part in parts {
                    let Value::Object(mut part) = part else {
                        return Err(ChatWireError::member(&path, "summary"));
                    };
                    if part.remove("type") != Some(json!("summary_text"))
                        || !part.remove("text").is_some_and(|text| text.is_string())
                        || !part.is_empty()
                    {
                        return Err(ChatWireError::member(&path, "summary"));
                    }
                }
                // Summary-only items are discarded; encrypted ones are replayed.
                let encrypted = match item.remove("encrypted_content") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(encrypted)) => Some(encrypted),
                    Some(_) => return Err(ChatWireError::member(&path, "encrypted_content")),
                };
                reject_leftover(&item, &path)?;
                if let Some(encrypted) = encrypted {
                    let reasoning = ReasoningItem::new(id, encrypted)
                        .map_err(|_| ChatWireError::member(&path, "encrypted_content"))?;
                    self.pending_reasoning.push(reasoning);
                }
            }
            "message" => {
                if !self.pending_calls.is_empty() {
                    return Err(invalid());
                }
                let role = item
                    .remove("role")
                    .ok_or_else(|| ChatWireError::member(&path, "role"))?;
                let assistant = role == "assistant";
                if !matches!(
                    role.as_str(),
                    Some("system" | "developer" | "user" | "assistant")
                ) {
                    return Err(ChatWireError::member(&path, "role"));
                }
                replay_metadata(&mut item, &path)?;
                let content = text_content(item.remove("content"), assistant)
                    .ok_or_else(|| ChatWireError::member(&path, "content"))?;
                reject_leftover(&item, &path)?;
                // A replayed empty answer maps to no turn; held reasoning waits for the
                // next assistant output or the end of its turn.
                if assistant && is_empty_text(&content) {
                    return Ok(());
                }
                if assistant {
                    self.place_pending(self.messages.len(), 0);
                } else {
                    self.flush_orphans();
                }
                self.messages.push(json!({"role":role,"content":content}));
                self.origins.push(Origin::Item(index));
            }
            "function_call" => {
                let last_is_assistant = self
                    .messages
                    .last()
                    .is_some_and(|message| message["role"] == "assistant");
                if !self.pending_calls.is_empty() && !last_is_assistant {
                    return Err(invalid());
                }
                replay_metadata(&mut item, &path)?;
                let call_id = take_string(&mut item, "call_id", &path)?;
                let name = take_string(&mut item, "name", &path)?;
                let arguments = take_string(&mut item, "arguments", &path)?;
                reject_leftover(&item, &path)?;
                if !self.seen_calls.insert(call_id.clone()) {
                    return Err(ChatWireError::member(&path, "call_id"));
                }
                self.pending_calls.insert(call_id.clone());
                let call = json!({"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}});
                if last_is_assistant {
                    let message = self.messages.len() - 1;
                    self.place_pending(message, ir_len(&self.messages[message]));
                    let last = &mut self.messages[message];
                    if last.get("tool_calls").is_none() {
                        last["tool_calls"] = json!([]);
                    }
                    last["tool_calls"]
                        .as_array_mut()
                        .ok_or_else(invalid)?
                        .push(call);
                } else {
                    self.place_pending(self.messages.len(), 0);
                    self.messages
                        .push(json!({"role":"assistant","content":null,"tool_calls":[call]}));
                    self.origins.push(Origin::Item(index));
                }
            }
            "function_call_output" => {
                replay_metadata(&mut item, &path)?;
                let call_id = take_string(&mut item, "call_id", &path)?;
                let output = match item.remove("output") {
                    Some(Value::String(output)) => output,
                    Some(Value::Array(parts)) => {
                        input_text(parts).ok_or_else(|| ChatWireError::member(&path, "output"))?
                    }
                    _ => return Err(ChatWireError::member(&path, "output")),
                };
                reject_leftover(&item, &path)?;
                if !self.pending_calls.remove(&call_id) {
                    return Err(ChatWireError::member(&path, "call_id"));
                }
                self.flush_orphans();
                self.messages
                    .push(json!({"role":"tool","tool_call_id":call_id,"content":output}));
                self.origins.push(Origin::Item(index));
            }
            _ => return Err(ChatWireError::member(&path, "type")),
        }
        Ok(())
    }

    /// Held reasoning precedes the assistant output about to land at `message`/`position`.
    fn place_pending(&mut self, message: usize, position: usize) {
        self.placements.extend(
            self.pending_reasoning
                .drain(..)
                .map(|item| (Placement::Into { message, position }, item)),
        );
    }

    /// Reasoning with no assistant output after it in its turn: appended to the turn's
    /// assistant message when one is open, otherwise kept as a reasoning-only turn.
    fn flush_orphans(&mut self) {
        if self.pending_reasoning.is_empty() {
            return;
        }
        let last_is_assistant = self
            .messages
            .last()
            .is_some_and(|message| message["role"] == "assistant");
        let owner = (last_is_assistant || !self.pending_calls.is_empty())
            .then(|| {
                self.messages
                    .iter()
                    .rposition(|message| message["role"] == "assistant")
            })
            .flatten();
        match owner {
            Some(message) => self.place_pending(message, ir_len(&self.messages[message])),
            None => {
                let message = self.messages.len();
                self.placements.extend(
                    self.pending_reasoning
                        .drain(..)
                        .map(|item| (Placement::Before { message }, item)),
                );
            }
        }
    }
}

/// IR content items the chat message converts to: its text parts, then its tool calls.
fn ir_len(message: &Value) -> usize {
    let text = match &message["content"] {
        Value::String(_) => 1,
        Value::Array(parts) => parts.len(),
        _ => 0,
    };
    text + message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// Placements are recorded in input order, so applying them in reverse keeps every
/// recorded index valid.
fn place_reasoning(messages: &mut Vec<ChatMessage>, placements: Vec<(Placement, ReasoningItem)>) {
    let mut inserted_before = None;
    for (placement, item) in placements.into_iter().rev() {
        let item = ChatContent::Reasoning { item };
        match placement {
            Placement::Into { message, position } => {
                messages[message].content.insert(position, item);
            }
            Placement::Before { message } if inserted_before == Some(message) => {
                messages[message].content.insert(0, item);
            }
            Placement::Before { message } => {
                messages.insert(
                    message,
                    ChatMessage {
                        role: ChatRole::Assistant,
                        content: vec![item],
                    },
                );
                inserted_before = Some(message);
            }
        }
    }
}

/// Renames a chat-shaped failure to the Responses member it came from.
fn responses_param(error: ChatWireError, origins: &[Origin]) -> ChatWireError {
    let ChatWireError::InvalidParam(param) = error else {
        return error;
    };
    let renamed: Cow<'static, str> = match param.as_ref() {
        "max_completion_tokens" => "max_output_tokens".into(),
        "reasoning_effort" => "reasoning.effort".into(),
        "messages" => "input".into(),
        param if param.starts_with("messages[") => {
            let origin = param["messages[".len()..]
                .split(']')
                .next()
                .and_then(|index| index.parse::<usize>().ok())
                .and_then(|index| origins.get(index).copied());
            match origin {
                Some(Origin::Instructions) => "instructions".into(),
                Some(Origin::Item(index)) => format!("input[{index}]").into(),
                Some(Origin::Input) | None => "input".into(),
            }
        }
        // Responses tools are flat: `tools[i].function.name` is `tools[i].name`.
        param if param.starts_with("tools[") => param.replacen("].function", "]", 1).into(),
        _ => param,
    };
    ChatWireError::InvalidParam(renamed)
}

fn valid_client_metadata(metadata: &Value) -> bool {
    let Value::Object(metadata) = metadata else {
        return false;
    };
    metadata.len() <= MAX_CLIENT_METADATA_ENTRIES
        && metadata
            .iter()
            .try_fold(0_usize, |total, (key, value)| {
                total
                    .checked_add(key.len())?
                    .checked_add(value.as_str()?.len())
            })
            .is_some_and(|total| total <= MAX_CLIENT_METADATA_BYTES)
}

fn valid_metadata(metadata: &Value) -> bool {
    let Value::Object(metadata) = metadata else {
        return false;
    };
    metadata.len() <= MAX_METADATA_ENTRIES
        && metadata.iter().all(|(key, value)| {
            (1..=MAX_METADATA_KEY_BYTES).contains(&key.len())
                && value
                    .as_str()
                    .is_some_and(|value| value.len() <= MAX_METADATA_VALUE_BYTES)
        })
}

/// `verbosity` is a hint and is ignored; only plain-text output is supported.
fn validate_text(text: Value) -> Result<(), ChatWireError> {
    let Value::Object(mut text) = text else {
        return Err(ChatWireError::param("text"));
    };
    if text
        .remove("verbosity")
        .is_some_and(|verbosity| !matches!(verbosity.as_str(), Some("low" | "medium" | "high")))
    {
        return Err(ChatWireError::param("text.verbosity"));
    }
    if text
        .remove("format")
        .is_some_and(|format| format != json!({"type":"text"}))
    {
        return Err(ChatWireError::param("text.format"));
    }
    match text.keys().next() {
        Some(key) => Err(ChatWireError::member("text", key)),
        None => Ok(()),
    }
}

fn reject_leftover(item: &Map<String, Value>, path: &str) -> Result<(), ChatWireError> {
    match item.keys().next() {
        Some(key) => Err(ChatWireError::member(path, key)),
        None => Ok(()),
    }
}

fn take_string(
    item: &mut Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<String, ChatWireError> {
    match item.remove(key) {
        Some(Value::String(text)) if !text.is_empty() => Ok(text),
        _ => Err(ChatWireError::member(path, key)),
    }
}

/// Validates replayed `id`/`status` and returns the id.
fn replay_metadata(
    item: &mut Map<String, Value>,
    path: &str,
) -> Result<Option<String>, ChatWireError> {
    let id = match item.remove("id") {
        None => None,
        Some(Value::String(id)) if !id.is_empty() && id.len() <= 256 => Some(id),
        Some(_) => return Err(ChatWireError::member(path, "id")),
    };
    if let Some(status) = item.remove("status")
        && !matches!(status.as_str(), Some("completed" | "incomplete"))
    {
        return Err(ChatWireError::member(path, "status"));
    }
    Ok(id)
}

/// `function_call_output` parts: `input_text` only, joined in order.
fn input_text(parts: Vec<Value>) -> Option<String> {
    let mut output = String::new();
    for part in parts {
        let Value::Object(mut part) = part else {
            return None;
        };
        if part.remove("type")? != "input_text" {
            return None;
        }
        let Value::String(text) = part.remove("text")? else {
            return None;
        };
        if !part.is_empty() {
            return None;
        }
        output.push_str(&text);
    }
    Some(output)
}

fn is_empty_text(content: &Value) -> bool {
    match content {
        Value::String(text) => text.is_empty(),
        Value::Array(parts) => parts.is_empty(),
        _ => false,
    }
}

/// Assistant `output_text` parts with empty text are skipped, so a replayed empty answer
/// converts to no parts; other empty parts are rejected.
fn text_content(content: Option<Value>, assistant: bool) -> Option<Value> {
    match content {
        Some(Value::String(text)) => Some(Value::String(text)),
        Some(Value::Array(parts)) if !parts.is_empty() => {
            let mut converted = Vec::with_capacity(parts.len());
            for part in parts {
                let Value::Object(mut part) = part else {
                    return None;
                };
                let kind = part.remove("type");
                if kind != Some(json!("input_text"))
                    && !(assistant && kind == Some(json!("output_text")))
                {
                    return None;
                }
                for name in ["annotations", "logprobs"] {
                    if let Some(value) = part.remove(name)
                        && (!assistant || value != json!([]))
                    {
                        return None;
                    }
                }
                let Some(Value::String(text)) = part.remove("text") else {
                    return None;
                };
                if !part.is_empty() {
                    return None;
                }
                if text.is_empty() {
                    if assistant && kind == Some(json!("output_text")) {
                        continue;
                    }
                    return None;
                }
                converted.push(json!({"type":"text","text":text}));
            }
            Some(Value::Array(converted))
        }
        _ => None,
    }
}
