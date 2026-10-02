use axum::{
    Json,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::super::{
    errors::{gateway_error_observed, sse_error_data},
    serialization::finish_matches_tool_calls,
};
use crate::{
    core::{
        ChatContent, ChatRequest, ChatResponse, ChatRole, ErrorKind, FinishReason, GatewayError,
        ModelAlias, NormalizedEvent, Usage,
    },
    telemetry::Observer,
};

const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

pub(in crate::api) struct Options {
    model: ModelAlias,
    tools: Value,
    tool_choice: Value,
    max_output_tokens: Option<u32>,
    temperature: Option<f64>,
    top_p: Option<f64>,
}

impl Options {
    pub(in crate::api) fn new(chat: &ChatRequest) -> Self {
        Self {
            model: chat.model.clone(),
            tools: Value::Array(chat.tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false})).collect()),
            tool_choice: match &chat.tool_choice {
                crate::core::ToolChoice::None => json!("none"),
                crate::core::ToolChoice::Auto => json!("auto"),
                crate::core::ToolChoice::Required => json!("required"),
                crate::core::ToolChoice::Function { name } => json!({"type":"function","name":name}),
            },
            max_output_tokens: chat.options.max_output_tokens,
            temperature: chat.options.sampling.temperature.map(|value| value.get()),
            top_p: chat.options.sampling.top_p.map(|value| value.get()),
        }
    }
}

pub(in crate::api) fn complete(
    response: ChatResponse,
    options: Options,
    observer: Option<&Observer>,
) -> Response {
    let result = (|| {
        if response.model != options.model || response.message.role != ChatRole::Assistant {
            return Err(failure());
        }
        let mut output = Output::new(options);
        let mut calls = std::collections::BTreeSet::new();
        for content in response.message.content {
            match content {
                ChatContent::Text { text } => {
                    output.accept(NormalizedEvent::ChatTextDelta { text }, false)?;
                }
                ChatContent::ToolCall { call } => {
                    if !calls.insert(call.id.clone()) {
                        return Err(failure());
                    }
                    output.accept(
                        NormalizedEvent::ChatToolCallDelta {
                            call_id: call.id,
                            name: Some(call.name),
                            arguments_delta: call.arguments,
                        },
                        false,
                    )?;
                }
                _ => return Err(failure()),
            }
        }
        output.finish(response.finish_reason, response.usage.clone())?;
        if let Some(observer) = observer {
            observer.first_content();
            observer.record_usage(response.usage);
        }
        Ok(output.response)
    })();
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => gateway_error_observed(error, observer),
    }
}

pub(super) struct Output {
    pub(super) response: Value,
    sequence: u64,
    bytes: usize,
    message_index: Option<usize>,
    tools: std::collections::BTreeMap<String, usize>,
}

impl Output {
    pub(super) fn new(options: Options) -> Self {
        let id = super::super::serialization::next_chat_id().replace("chatcmpl_", "resp_");
        Self {
            response: json!({
                "id":id,"object":"response","created_at":crate::keys::time::now(),"completed_at":null,
                "status":"in_progress","model":options.model.0,"output":[],"error":null,"incomplete_details":null,
                "store":false,"background":false,"previous_response_id":null,"instructions":null,
                "max_output_tokens":options.max_output_tokens,"parallel_tool_calls":true,"tool_choice":options.tool_choice,
                "tools":options.tools,"temperature":options.temperature,"top_p":options.top_p,
                "text":{"format":{"type":"text"}},"reasoning":{"effort":null,"summary":null},
                "truncation":"disabled","usage":null,"metadata":{},"user":null
            }),
            sequence: 0,
            bytes: 0,
            message_index: None,
            tools: std::collections::BTreeMap::new(),
        }
    }

    pub(super) fn start(&self) -> Vec<Value> {
        vec![
            json!({"type":"response.created","response":self.response}),
            json!({"type":"response.in_progress","response":self.response}),
        ]
    }

    pub(super) fn frame(&mut self, mut value: Value) -> axum::body::Bytes {
        value["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        format!(
            "event: {}\ndata: {}\n\n",
            value["type"].as_str().expect("event type"),
            value
        )
        .into()
    }

    pub(super) fn accept(
        &mut self,
        event: NormalizedEvent,
        streamed: bool,
    ) -> Result<Vec<Value>, GatewayError> {
        let mut frames = Vec::new();
        match event {
            NormalizedEvent::ChatTextDelta { text } => {
                if text.is_empty() {
                    return Ok(frames);
                }
                self.reserve(text.len())?;
                let index = match self.message_index {
                    Some(index) => index,
                    None => {
                        let index = self.items().len();
                        let id = format!("{}_msg", self.response["id"].as_str().expect("id"));
                        let item = json!({"id":id,"type":"message","role":"assistant","status":"in_progress","content":[]});
                        frames.push(json!({"type":"response.output_item.added","output_index":index,"item":item}));
                        let part =
                            json!({"type":"output_text","text":"","annotations":[],"logprobs":[]});
                        frames.push(json!({"type":"response.content_part.added","item_id":id,"output_index":index,"content_index":0,"part":part}));
                        self.items().push(item);
                        self.response["output"][index]["content"] = json!([part]);
                        self.message_index = Some(index);
                        index
                    }
                };
                let item = &mut self.response["output"][index];
                frames.push(json!({"type":"response.output_text.delta","item_id":item["id"],"output_index":index,"content_index":0,"delta":text,"logprobs":[]}));
                item["content"][0]["text"].as_str().ok_or_else(failure)?;
                if let Value::String(current) = &mut item["content"][0]["text"] {
                    current.push_str(&text);
                }
            }
            NormalizedEvent::ChatToolCallDelta {
                call_id,
                name,
                arguments_delta,
            } => {
                if call_id.is_empty()
                    || call_id.len() > 128
                    || !call_id.bytes().all(|b| b.is_ascii_graphic())
                    || (streamed && arguments_delta.len() > 16 * 1024)
                {
                    return Err(failure());
                }
                self.reserve(arguments_delta.len())?;
                let index = if let Some(index) = self.tools.get(&call_id) {
                    if name.as_deref().is_some_and(|name| {
                        Some(name) != self.response["output"][*index]["name"].as_str()
                    }) {
                        return Err(failure());
                    }
                    *index
                } else {
                    let name = name
                        .filter(|name| {
                            !name.is_empty()
                                && name.len() <= 64
                                && name
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                        })
                        .ok_or_else(failure)?;
                    if self.tools.len() >= 64 {
                        return Err(failure());
                    }
                    let index = self.items().len();
                    let id = format!("{}_fc_{index}", self.response["id"].as_str().expect("id"));
                    let item = json!({"type":"function_call","id":id,"call_id":call_id,"name":name,"arguments":"","status":"in_progress"});
                    frames.push(json!({"type":"response.output_item.added","output_index":index,"item":item}));
                    self.items().push(item);
                    self.tools.insert(call_id, index);
                    index
                };
                let item = &mut self.response["output"][index];
                frames.push(json!({"type":"response.function_call_arguments.delta","item_id":item["id"],"output_index":index,"delta":arguments_delta}));
                if let Value::String(current) = &mut item["arguments"] {
                    current.push_str(&arguments_delta);
                }
            }
            NormalizedEvent::ChatReasoningDelta { .. } => {}
            _ => return Err(failure()),
        }
        Ok(frames)
    }

    pub(super) fn finish(
        &mut self,
        reason: FinishReason,
        usage: Option<Usage>,
    ) -> Result<Vec<Value>, GatewayError> {
        if !finish_matches_tool_calls(reason, !self.tools.is_empty()) {
            return Err(failure());
        }
        if self.items().is_empty()
            && !matches!(
                reason,
                FinishReason::Length | FinishReason::ContentFilter | FinishReason::Cancelled
            )
        {
            return Err(failure());
        }
        let incomplete = match reason {
            FinishReason::Cancelled => {
                return Err(GatewayError {
                    kind: ErrorKind::Cancelled,
                });
            }
            FinishReason::Length => Some("max_output_tokens"),
            FinishReason::ContentFilter => Some("content_filter"),
            _ => None,
        };
        let mut frames = Vec::new();
        for (index, item) in self.items().iter_mut().enumerate() {
            item["status"] = json!(if incomplete.is_some() {
                "incomplete"
            } else {
                "completed"
            });
            if item["type"] == "message" {
                frames.push(json!({"type":"response.output_text.done","item_id":item["id"],"output_index":index,"content_index":0,"text":item["content"][0]["text"],"logprobs":[]}));
                frames.push(json!({"type":"response.content_part.done","item_id":item["id"],"output_index":index,"content_index":0,"part":item["content"][0]}));
            } else {
                frames.push(json!({"type":"response.function_call_arguments.done","item_id":item["id"],"output_index":index,"arguments":item["arguments"]}));
            }
            frames
                .push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
        }
        self.response["status"] = json!(if incomplete.is_some() {
            "incomplete"
        } else {
            "completed"
        });
        self.response["completed_at"] = json!(crate::keys::time::now());
        self.response["incomplete_details"] = incomplete
            .map(|reason| json!({"reason":reason}))
            .unwrap_or(Value::Null);
        self.response["usage"] = usage.map(|usage| {
            let mut value = json!({"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"total_tokens":usage.total_tokens});
            if let Some(tokens) = usage.reasoning_tokens { value["output_tokens_details"] = json!({"reasoning_tokens":tokens}); }
            value
        }).unwrap_or(Value::Null);
        frames.push(json!({"type":if incomplete.is_some() { "response.incomplete" } else { "response.completed" },"response":self.response}));
        Ok(frames)
    }

    pub(super) fn failed(&mut self, error: GatewayError) -> Value {
        self.response["status"] = json!("failed");
        self.response["error"] = sse_error_data(error)["error"].clone();
        json!({"type":"response.failed","response":self.response})
    }

    fn items(&mut self) -> &mut Vec<Value> {
        self.response["output"]
            .as_array_mut()
            .expect("output items")
    }
    fn reserve(&mut self, bytes: usize) -> Result<(), GatewayError> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_OUTPUT_BYTES)
            .ok_or_else(failure)?;
        Ok(())
    }
}

pub(super) fn failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
