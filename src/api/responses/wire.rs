use serde_json::{Map, Value, json};

use crate::core::{ChatRequest, MaxTokensParam};

use super::super::wire::{ChatWire, ChatWireError};

pub(in crate::api) fn parse(
    bytes: &[u8],
    max_audio_bytes: usize,
) -> Result<(ChatRequest, bool), ChatWireError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ChatWireError::Invalid)?;
    let Value::Object(mut value) = value else {
        return Err(ChatWireError::Invalid);
    };
    for (name, expected) in [
        ("store", json!(false)),
        ("background", json!(false)),
        ("truncation", json!("disabled")),
    ] {
        if let Some(actual) = value.remove(name)
            && actual != expected
        {
            return Err(ChatWireError::InvalidParam(name));
        }
    }
    for name in [
        "previous_response_id",
        "conversation",
        "include",
        "metadata",
        "parallel_tool_calls",
        "text",
        "service_tier",
        "user",
        "prompt",
        "stream_options",
    ] {
        if value.contains_key(name) {
            return Err(ChatWireError::InvalidParam(name));
        }
    }
    let mut chat = Map::new();
    if let Some(reasoning) = value.remove("reasoning") {
        let Value::Object(mut reasoning) = reasoning else {
            return Err(ChatWireError::InvalidParam("reasoning"));
        };
        if let Some(effort) = reasoning.remove("effort") {
            chat.insert("reasoning_effort".into(), effort);
        }
        if !reasoning.is_empty() {
            return Err(ChatWireError::InvalidParam("reasoning"));
        }
    }
    for name in ["model", "stream", "temperature", "top_p"] {
        if let Some(value) = value.remove(name) {
            chat.insert(name.into(), value);
        }
    }
    if let Some(value) = value.remove("max_output_tokens") {
        chat.insert("max_completion_tokens".into(), value);
    }
    let mut messages = Vec::new();
    if let Some(instructions) = value.remove("instructions") {
        if !instructions.is_string() {
            return Err(ChatWireError::InvalidParam("instructions"));
        }
        messages.push(json!({"role":"system","content":instructions}));
    }
    match value.remove("input") {
        Some(Value::String(text)) => messages.push(json!({"role":"user","content":text})),
        Some(Value::Array(items)) if !items.is_empty() => {
            let mut pending = std::collections::BTreeSet::new();
            let mut seen = std::collections::BTreeSet::new();
            for item in items {
                let Value::Object(mut item) = item else {
                    return input_error();
                };
                match item.remove("type").unwrap_or(json!("message")).as_str() {
                    Some("reasoning") => {
                        replay_metadata(&mut item)?;
                        let Some(Value::Array(parts)) = item.remove("summary") else {
                            return input_error();
                        };
                        for part in parts {
                            let Value::Object(mut part) = part else {
                                return input_error();
                            };
                            if part.remove("type") != Some(json!("summary_text"))
                                || !part.remove("text").is_some_and(|text| text.is_string())
                                || !part.is_empty()
                            {
                                return input_error();
                            }
                        }
                        if !item.is_empty() {
                            return input_error();
                        }
                    }
                    Some("message") => {
                        if !pending.is_empty() {
                            return input_error();
                        }
                        let role = item
                            .remove("role")
                            .ok_or(ChatWireError::InvalidParam("input"))?;
                        if !matches!(
                            role.as_str(),
                            Some("system" | "developer" | "user" | "assistant")
                        ) {
                            return input_error();
                        }
                        replay_metadata(&mut item)?;
                        let content = text_content(item.remove("content"), role == "assistant")?;
                        if !item.is_empty() {
                            return input_error();
                        }
                        messages.push(json!({"role":role,"content":content}));
                    }
                    Some("function_call") => {
                        if !pending.is_empty()
                            && !messages
                                .last()
                                .is_some_and(|message| message["role"] == "assistant")
                        {
                            return input_error();
                        }
                        replay_metadata(&mut item)?;
                        let call_id = take_string(&mut item, "call_id")?;
                        let name = take_string(&mut item, "name")?;
                        let arguments = take_string(&mut item, "arguments")?;
                        if !item.is_empty() || !seen.insert(call_id.clone()) {
                            return input_error();
                        }
                        pending.insert(call_id.clone());
                        let call = json!({"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}});
                        if let Some(last) = messages
                            .last_mut()
                            .filter(|last| last["role"] == "assistant")
                        {
                            if last.get("tool_calls").is_none() {
                                last["tool_calls"] = json!([]);
                            }
                            last["tool_calls"]
                                .as_array_mut()
                                .ok_or(ChatWireError::Invalid)?
                                .push(call);
                        } else {
                            messages.push(
                                json!({"role":"assistant","content":null,"tool_calls":[call]}),
                            );
                        }
                    }
                    Some("function_call_output") => {
                        let call_id = take_string(&mut item, "call_id")?;
                        let content = take_string(&mut item, "output")?;
                        if !item.is_empty() || !pending.remove(&call_id) {
                            return input_error();
                        }
                        messages
                            .push(json!({"role":"tool","tool_call_id":call_id,"content":content}));
                    }
                    _ => return input_error(),
                }
            }
            if !pending.is_empty() {
                return input_error();
            }
        }
        _ => return input_error(),
    }
    chat.insert("messages".into(), Value::Array(messages));
    if let Some(tools) = value.remove("tools") {
        let Value::Array(tools) = tools else {
            return Err(ChatWireError::InvalidParam("tools"));
        };
        let mut converted = Vec::new();
        for tool in tools {
            let Value::Object(mut tool) = tool else {
                return Err(ChatWireError::InvalidParam("tools"));
            };
            if tool.remove("type") != Some(json!("function")) {
                return Err(ChatWireError::InvalidParam("tools"));
            }
            if let Some(strict) = tool.remove("strict")
                && strict != false
            {
                return Err(ChatWireError::InvalidParam("tools"));
            }
            converted.push(json!({"type":"function","function":tool}));
        }
        chat.insert("tools".into(), Value::Array(converted));
    }
    if let Some(mut choice) = value.remove("tool_choice") {
        if let Value::Object(ref mut object) = choice {
            if object.remove("type") != Some(json!("function"))
                || object.len() != 1
                || !object.get("name").is_some_and(Value::is_string)
            {
                return Err(ChatWireError::InvalidParam("tool_choice"));
            }
            choice = json!({"type":"function","function":{"name":object.remove("name")}});
        }
        chat.insert("tool_choice".into(), choice);
    }
    if !value.is_empty() {
        return Err(ChatWireError::Invalid);
    }
    let wire: ChatWire =
        serde_json::from_value(Value::Object(chat)).map_err(|_| ChatWireError::Invalid)?;
    let (mut chat, _) = wire
        .into_core(max_audio_bytes)
        .map_err(|error| match error {
            ChatWireError::InvalidParam("max_completion_tokens") => {
                ChatWireError::InvalidParam("max_output_tokens")
            }
            ChatWireError::InvalidParam("reasoning_effort") => {
                ChatWireError::InvalidParam("reasoning.effort")
            }
            other => other,
        })?;
    chat.options.max_output_tokens_param = MaxTokensParam::MaxOutputTokens;
    Ok((chat, true))
}

fn input_error<T>() -> Result<T, ChatWireError> {
    Err(ChatWireError::InvalidParam("input"))
}

fn take_string(item: &mut Map<String, Value>, key: &str) -> Result<String, ChatWireError> {
    match item.remove(key) {
        Some(Value::String(text)) if !text.is_empty() => Ok(text),
        _ => input_error(),
    }
}

fn replay_metadata(item: &mut Map<String, Value>) -> Result<(), ChatWireError> {
    if let Some(id) = item.remove("id")
        && !id
            .as_str()
            .is_some_and(|id| !id.is_empty() && id.len() <= 256)
    {
        return input_error();
    }
    if let Some(status) = item.remove("status")
        && !matches!(status.as_str(), Some("completed" | "incomplete"))
    {
        return input_error();
    }
    Ok(())
}

fn text_content(content: Option<Value>, assistant: bool) -> Result<Value, ChatWireError> {
    match content {
        Some(Value::String(text)) => Ok(Value::String(text)),
        Some(Value::Array(parts)) if !parts.is_empty() => {
            let mut converted = Vec::new();
            for part in parts {
                let Value::Object(mut part) = part else {
                    return input_error();
                };
                let kind = part.remove("type");
                if kind != Some(json!("input_text"))
                    && !(assistant && kind == Some(json!("output_text")))
                {
                    return input_error();
                }
                for name in ["annotations", "logprobs"] {
                    if let Some(value) = part.remove(name)
                        && (!assistant || value != json!([]))
                    {
                        return input_error();
                    }
                }
                let text = take_string(&mut part, "text")?;
                if !part.is_empty() {
                    return input_error();
                }
                converted.push(json!({"type":"text","text":text}));
            }
            Ok(Value::Array(converted))
        }
        _ => input_error(),
    }
}
