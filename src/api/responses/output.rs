use axum::{
    Json,
    body::Bytes,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::super::{
    errors::{gateway_error_observed, sse_error_data},
    keepalive::COMMENT,
    serialization::finish_matches_tool_calls,
    sse::typed::{EventOutput, event_frame},
};
use crate::{
    core::{
        ChatContent, ChatRequest, ChatResponse, ChatRole, ErrorKind, FinishReason, GatewayError,
        MAX_REASONING_BYTES, ModelAlias, NormalizedEvent, ReasoningItem, Usage,
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
    reasoning_effort: Option<crate::core::ReasoningEffort>,
    expose_reasoning: bool,
    encrypted_reasoning: bool,
    reasoning_summary: Option<&'static str>,
}

impl Options {
    pub(in crate::api) fn new(
        chat: &ChatRequest,
        expose_reasoning: bool,
        reasoning_summary: Option<&'static str>,
    ) -> Self {
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
            reasoning_effort: chat.options.reasoning_effort,
            expose_reasoning,
            encrypted_reasoning: expose_reasoning && chat.include_encrypted_reasoning,
            reasoning_summary: reasoning_summary.filter(|_| expose_reasoning),
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
        if let Some(text) = response.reasoning {
            output.accept(NormalizedEvent::ChatReasoningDelta { text }, false)?;
        }
        for item in response.reasoning_items {
            output.accept(NormalizedEvent::ChatReasoningItem { item }, false)?;
        }
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
        Ok(output.into_response())
    })();
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => gateway_error_observed(error, observer),
    }
}

pub(in crate::api) struct Output {
    /// Response object; its `output` member is filled from `items` when emitted.
    response: Value,
    id: String,
    items: Vec<Value>,
    sequence: u64,
    bytes: usize,
    message_index: Option<usize>,
    reasoning_index: Option<usize>,
    expose_reasoning: bool,
    encrypted_reasoning: bool,
    reasoning_bytes: usize,
    tools: std::collections::BTreeMap<String, usize>,
}

impl Output {
    pub(super) fn new(options: Options) -> Self {
        let id = super::super::serialization::next_chat_id().replace("chatcmpl_", "resp_");
        Self {
            response: json!({
                "id":&id,"object":"response","created_at":crate::keys::time::now(),"completed_at":null,
                "status":"in_progress","model":options.model.0,"output":[],"error":null,"incomplete_details":null,
                "store":false,"background":false,"previous_response_id":null,"instructions":null,
                "max_output_tokens":options.max_output_tokens,"parallel_tool_calls":true,"tool_choice":options.tool_choice,
                "tools":options.tools,"temperature":options.temperature,"top_p":options.top_p,
                "text":{"format":{"type":"text"}},"reasoning":{"effort":options.reasoning_effort,"summary":options.reasoning_summary},
                "truncation":"disabled","usage":null,"metadata":{},"user":null
            }),
            id,
            items: Vec::new(),
            sequence: 0,
            bytes: 0,
            message_index: None,
            reasoning_index: None,
            expose_reasoning: options.expose_reasoning,
            encrypted_reasoning: options.encrypted_reasoning,
            reasoning_bytes: 0,
            tools: std::collections::BTreeMap::new(),
        }
    }

    pub(super) fn start(&self) -> Vec<Value> {
        vec![
            json!({"type":"response.created","response":self.snapshot()}),
            json!({"type":"response.in_progress","response":self.snapshot()}),
        ]
    }

    /// The response object with its current output items.
    fn snapshot(&self) -> Value {
        let mut response = self.response.clone();
        response["output"] = Value::Array(self.items.clone());
        response
    }

    fn into_response(mut self) -> Value {
        self.response["output"] = Value::Array(self.items);
        self.response
    }

    pub(super) fn accept(
        &mut self,
        event: NormalizedEvent,
        streamed: bool,
    ) -> Result<Vec<Value>, GatewayError> {
        let mut frames = Vec::new();
        match event {
            NormalizedEvent::ChatTextDelta { text } => self.accept_text(&text, &mut frames)?,
            NormalizedEvent::ChatToolCallDelta {
                call_id,
                name,
                arguments_delta,
            } => self.accept_tool_call(call_id, name, arguments_delta, streamed, &mut frames)?,
            NormalizedEvent::ChatReasoningDelta { text } => {
                self.accept_reasoning_delta(text, &mut frames)?;
            }
            NormalizedEvent::ChatReasoningItem { item } => {
                self.accept_reasoning_item(&item, &mut frames)?;
            }
            _ => return Err(failure()),
        }
        Ok(frames)
    }

    fn accept_text(&mut self, text: &str, frames: &mut Vec<Value>) -> Result<(), GatewayError> {
        if text.is_empty() {
            return Ok(());
        }
        self.reserve(text.len())?;
        let index = match self.message_index {
            Some(index) => index,
            None => self.open_message(frames),
        };
        let item = &mut self.items[index];
        frames.push(json!({"type":"response.output_text.delta","item_id":item["id"],"output_index":index,"content_index":0,"delta":text,"logprobs":[]}));
        item["content"][0]["text"].as_str().ok_or_else(failure)?;
        if let Value::String(current) = &mut item["content"][0]["text"] {
            current.push_str(text);
        }
        Ok(())
    }

    fn accept_tool_call(
        &mut self,
        call_id: String,
        name: Option<String>,
        arguments_delta: String,
        streamed: bool,
        frames: &mut Vec<Value>,
    ) -> Result<(), GatewayError> {
        if call_id.is_empty()
            || call_id.len() > 128
            || !call_id.bytes().all(|b| b.is_ascii_graphic())
            || (streamed && arguments_delta.len() > 16 * 1024)
        {
            return Err(failure());
        }
        self.reserve(arguments_delta.len())?;
        let index = match self.tools.get(&call_id) {
            Some(&index) => {
                if name
                    .as_deref()
                    .is_some_and(|name| Some(name) != self.items[index]["name"].as_str())
                {
                    return Err(failure());
                }
                index
            }
            None => self.open_tool_call(call_id, name, frames)?,
        };
        let item = &mut self.items[index];
        frames.push(json!({"type":"response.function_call_arguments.delta","item_id":item["id"],"output_index":index,"delta":arguments_delta}));
        if let Value::String(current) = &mut item["arguments"] {
            current.push_str(&arguments_delta);
        }
        Ok(())
    }

    fn open_tool_call(
        &mut self,
        call_id: String,
        name: Option<String>,
        frames: &mut Vec<Value>,
    ) -> Result<usize, GatewayError> {
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
        let index = self.items.len();
        let id = format!("{}_fc_{index}", self.id);
        let item = json!({"type":"function_call","id":id,"call_id":call_id,"name":name,"arguments":"","status":"in_progress"});
        frames.push(json!({"type":"response.output_item.added","output_index":index,"item":item}));
        self.items.push(item);
        self.tools.insert(call_id, index);
        Ok(index)
    }

    fn accept_reasoning_delta(
        &mut self,
        mut text: String,
        frames: &mut Vec<Value>,
    ) -> Result<(), GatewayError> {
        if !self.expose_reasoning {
            return Ok(());
        }
        super::super::serialization::truncate_at_char_boundary(
            &mut text,
            MAX_REASONING_BYTES.saturating_sub(self.reasoning_bytes),
        );
        if text.is_empty() {
            return Ok(());
        }
        self.reserve(text.len())?;
        self.reasoning_bytes += text.len();
        let index = match self.reasoning_index {
            Some(index) => index,
            None => self.open_reasoning_summary(frames),
        };
        let item = &mut self.items[index];
        frames.push(json!({"type":"response.reasoning_summary_text.delta","item_id":item["id"],"output_index":index,"summary_index":0,"delta":text}));
        if let Value::String(current) = &mut item["summary"][0]["text"] {
            current.push_str(&text);
        }
        Ok(())
    }

    fn open_reasoning_summary(&mut self, frames: &mut Vec<Value>) -> usize {
        let index = self.items.len();
        let id = format!("{}_rs_{index}", self.id);
        let item = json!({"id":id,"type":"reasoning","status":"in_progress","summary":[]});
        frames.push(json!({"type":"response.output_item.added","output_index":index,"item":item}));
        let part = json!({"type":"summary_text","text":""});
        frames.push(json!({"type":"response.reasoning_summary_part.added","item_id":id,"output_index":index,"summary_index":0,"part":part}));
        self.items.push(item);
        self.items[index]["summary"] = json!([part]);
        self.reasoning_index = Some(index);
        index
    }

    // One output item per upstream reasoning item, apart from the summary item:
    // its id was streamed before the upstream id was known, and a replayed item
    // must carry the upstream id.
    fn accept_reasoning_item(
        &mut self,
        item: &ReasoningItem,
        frames: &mut Vec<Value>,
    ) -> Result<(), GatewayError> {
        if !self.encrypted_reasoning {
            return Ok(());
        }
        self.reserve(item.encrypted_content().len())?;
        let index = self.items.len();
        let id = match item.id() {
            Some(id) => id.to_owned(),
            None => format!("{}_rs_{index}", self.id),
        };
        let added = json!({"id":id,"type":"reasoning","status":"in_progress","summary":[]});
        frames.push(json!({"type":"response.output_item.added","output_index":index,"item":added}));
        let mut stored = added;
        stored["encrypted_content"] = json!(item.encrypted_content());
        self.items.push(stored);
        Ok(())
    }

    pub(super) fn finish(
        &mut self,
        reason: FinishReason,
        usage: Option<Usage>,
    ) -> Result<Vec<Value>, GatewayError> {
        if !finish_matches_tool_calls(reason, !self.tools.is_empty()) {
            return Err(failure());
        }
        let mut frames = Vec::new();
        let incomplete = match reason {
            FinishReason::Cancelled => {
                return Err(GatewayError::from(ErrorKind::Cancelled));
            }
            FinishReason::Length => Some("max_output_tokens"),
            FinishReason::ContentFilter => Some("content_filter"),
            _ => None,
        };
        // An empty answer is still one assistant message whose output_text is "".
        if reason == FinishReason::Stop && self.message_index.is_none() && self.tools.is_empty() {
            self.open_message(&mut frames);
        }
        for (index, item) in self.items.iter_mut().enumerate() {
            item["status"] = json!(if incomplete.is_some() {
                "incomplete"
            } else {
                "completed"
            });
            if item["type"] == "message" {
                frames.push(json!({"type":"response.output_text.done","item_id":item["id"],"output_index":index,"content_index":0,"text":item["content"][0]["text"],"logprobs":[]}));
                frames.push(json!({"type":"response.content_part.done","item_id":item["id"],"output_index":index,"content_index":0,"part":item["content"][0]}));
            } else if item["type"] == "reasoning" {
                // Encrypted-only items have no summary part to close.
                if item["summary"]
                    .as_array()
                    .is_some_and(|parts| !parts.is_empty())
                {
                    frames.push(json!({"type":"response.reasoning_summary_text.done","item_id":item["id"],"output_index":index,"summary_index":0,"text":item["summary"][0]["text"]}));
                    let mut part_done = json!({"type":"response.reasoning_summary_part.done","item_id":item["id"],"output_index":index,"summary_index":0,"part":item["summary"][0]});
                    if incomplete.is_some() {
                        part_done["status"] = json!("incomplete");
                    }
                    frames.push(part_done);
                }
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
            if let Some(tokens) = usage.cached_input_tokens { value["input_tokens_details"] = json!({"cached_tokens":tokens}); }
            value
        }).unwrap_or(Value::Null);
        frames.push(json!({"type":if incomplete.is_some() { "response.incomplete" } else { "response.completed" },"response":self.snapshot()}));
        Ok(frames)
    }

    fn open_message(&mut self, frames: &mut Vec<Value>) -> usize {
        let index = self.items.len();
        let id = format!("{}_msg", self.id);
        let item = json!({"id":id,"type":"message","role":"assistant","status":"in_progress","content":[]});
        frames.push(json!({"type":"response.output_item.added","output_index":index,"item":item}));
        let part = json!({"type":"output_text","text":"","annotations":[],"logprobs":[]});
        frames.push(json!({"type":"response.content_part.added","item_id":id,"output_index":index,"content_index":0,"part":part}));
        self.items.push(item);
        self.items[index]["content"] = json!([part]);
        self.message_index = Some(index);
        index
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

/// Output state and opening frames of a Responses stream.
pub(in crate::api) fn stream_start(
    options: Options,
) -> impl FnOnce(&ModelAlias) -> (Output, Vec<Value>) {
    move |_| {
        let output = Output::new(options);
        let frames = output.start();
        (output, frames)
    }
}

impl EventOutput for Output {
    const KEEPALIVE: &'static [u8] = COMMENT;

    fn accept(&mut self, event: NormalizedEvent) -> Result<Vec<Value>, GatewayError> {
        Output::accept(self, event, true)
    }

    fn finish(
        &mut self,
        reason: FinishReason,
        usage: Option<&Usage>,
    ) -> Result<Vec<Value>, GatewayError> {
        Output::finish(self, reason, usage.cloned())
    }

    fn failed(&mut self, error: GatewayError) -> Value {
        self.response["status"] = json!("failed");
        self.response["error"] = sse_error_data(error)["error"].clone();
        json!({"type":"response.failed","response":self.snapshot()})
    }

    fn frame(&mut self, mut value: Value) -> Bytes {
        value["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        event_frame(&value)
    }
}

fn failure() -> GatewayError {
    GatewayError::from(ErrorKind::UpstreamFailure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChatMessage, Extensions, ReasoningItem, ToolChoice};

    fn chat(include_encrypted_reasoning: bool) -> ChatRequest {
        ChatRequest {
            model: ModelAlias("fixture-model".into()),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            stream: false,
            options: Default::default(),
            prompt_cache_key: None,
            include_encrypted_reasoning,
            extensions: Extensions::default(),
        }
    }

    fn item() -> ReasoningItem {
        ReasoningItem::new(Some("rs_fixture".into()), "ENCRYPTED".into()).unwrap()
    }

    fn usage(cached: Option<u64>) -> Usage {
        Usage {
            input_tokens: 2048,
            output_tokens: 3,
            total_tokens: 2051,
            reasoning_tokens: None,
            cached_input_tokens: cached,
        }
    }

    async fn complete_json(include: bool, expose: bool, cached: Option<u64>) -> Value {
        let response = ChatResponse {
            model: ModelAlias("fixture-model".into()),
            message: ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatContent::Text {
                    text: "answer".into(),
                }],
            },
            finish_reason: FinishReason::Stop,
            usage: Some(usage(cached)),
            reasoning: Some("summary".into()),
            reasoning_items: vec![item()],
        };
        let body = complete(response, Options::new(&chat(include), expose, None), None);
        let bytes = axum::body::to_bytes(body.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn json_output_carries_encrypted_reasoning_only_when_requested_and_exposed() {
        let value = complete_json(true, true, Some(1024)).await;
        let output = value["output"].as_array().unwrap();
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["summary"][0]["text"], "summary");
        assert!(output[0].get("encrypted_content").is_none());
        assert_eq!(
            output[1],
            json!({"id":"rs_fixture","type":"reasoning","status":"completed","summary":[],
                "encrypted_content":"ENCRYPTED"})
        );
        assert_eq!(output[2]["type"], "message");
        assert_eq!(
            value["usage"]["input_tokens_details"],
            json!({"cached_tokens":1024})
        );

        let unrequested = complete_json(false, true, None).await;
        assert_eq!(unrequested["output"].as_array().unwrap().len(), 2);
        assert!(unrequested["usage"].get("input_tokens_details").is_none());

        let public = complete_json(true, false, None).await;
        let output = public["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
    }

    #[test]
    fn streamed_encrypted_item_is_announced_then_completed_with_its_payload() {
        let mut output = Output::new(Options::new(&chat(true), true, None));
        let added = output
            .accept(NormalizedEvent::ChatReasoningItem { item: item() }, true)
            .unwrap();
        assert_eq!(
            added,
            vec![json!({"type":"response.output_item.added","output_index":0,
                "item":{"id":"rs_fixture","type":"reasoning","status":"in_progress","summary":[]}})]
        );
        output
            .accept(
                NormalizedEvent::ChatTextDelta {
                    text: "answer".into(),
                },
                true,
            )
            .unwrap();
        let frames = output
            .finish(FinishReason::Stop, Some(usage(Some(1024))))
            .unwrap();
        assert!(frames.iter().all(|frame| {
            !frame["type"]
                .as_str()
                .unwrap()
                .starts_with("response.reasoning_summary")
        }));
        assert_eq!(
            frames[0],
            json!({"type":"response.output_item.done","output_index":0,
                "item":{"id":"rs_fixture","type":"reasoning","status":"completed","summary":[],
                    "encrypted_content":"ENCRYPTED"}})
        );
        let completed = frames.last().unwrap();
        assert_eq!(
            completed["response"]["usage"]["input_tokens_details"]["cached_tokens"],
            1024
        );
    }
}
