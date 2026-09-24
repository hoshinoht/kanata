use axum::body::Bytes;
use serde_json::{Value, json};

use crate::core::{GatewayError, ModelAlias, Usage};

use super::super::{errors, serialization};

pub(in crate::api) struct Metadata {
    id: String,
    model: ModelAlias,
}

impl Metadata {
    pub(in crate::api) fn new(model: ModelAlias) -> Self {
        Self {
            id: serialization::next_chat_id(),
            model,
        }
    }
}

pub(super) fn start(metadata: &Metadata) -> Bytes {
    chunk(
        metadata,
        json!([{
            "index": 0,
            "delta": {"role": "assistant"},
            "finish_reason": null
        }]),
        None,
    )
}

pub(super) fn text(metadata: &Metadata, text: String) -> Bytes {
    chunk(
        metadata,
        json!([{
            "index": 0,
            "delta": {"content": text},
            "finish_reason": null
        }]),
        None,
    )
}

pub(super) fn tool_initial(
    metadata: &Metadata,
    index: usize,
    call_id: String,
    name: String,
    arguments: String,
) -> Bytes {
    chunk(
        metadata,
        json!([{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": index,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }]
            },
            "finish_reason": null
        }]),
        None,
    )
}

pub(super) fn tool_continuation(metadata: &Metadata, index: usize, arguments: String) -> Bytes {
    chunk(
        metadata,
        json!([{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": index,
                    "function": {"arguments": arguments}
                }]
            },
            "finish_reason": null
        }]),
        None,
    )
}

pub(super) fn finish(metadata: &Metadata, reason: &'static str) -> Bytes {
    chunk(
        metadata,
        json!([{
            "index": 0,
            "delta": {},
            "finish_reason": reason
        }]),
        None,
    )
}

pub(super) fn usage(metadata: &Metadata, value: Usage) -> Bytes {
    chunk(metadata, json!([]), Some(serialization::usage(value)))
}

pub(super) fn error(error: GatewayError) -> Bytes {
    frame(errors::sse_error_data(error))
}

pub(super) fn done() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

fn chunk(metadata: &Metadata, choices: Value, usage: Option<Value>) -> Bytes {
    frame(json!({
        "id": metadata.id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": metadata.model.0,
        "system_fingerprint": null,
        "choices": choices,
        "usage": usage.unwrap_or(Value::Null)
    }))
}

fn frame(value: Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}
