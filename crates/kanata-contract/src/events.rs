//! `event:` names of the typed SSE streams; each frame's JSON `type` repeats its event name.

/// `POST /v1/responses` with `"stream": true`.
pub mod responses {
    pub const CREATED: &str = "response.created";
    pub const IN_PROGRESS: &str = "response.in_progress";
    pub const OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
    pub const OUTPUT_ITEM_DONE: &str = "response.output_item.done";
    pub const CONTENT_PART_ADDED: &str = "response.content_part.added";
    pub const CONTENT_PART_DONE: &str = "response.content_part.done";
    pub const OUTPUT_TEXT_DELTA: &str = "response.output_text.delta";
    pub const OUTPUT_TEXT_DONE: &str = "response.output_text.done";
    pub const FUNCTION_CALL_ARGUMENTS_DELTA: &str = "response.function_call_arguments.delta";
    pub const FUNCTION_CALL_ARGUMENTS_DONE: &str = "response.function_call_arguments.done";
    pub const REASONING_SUMMARY_PART_ADDED: &str = "response.reasoning_summary_part.added";
    pub const REASONING_SUMMARY_PART_DONE: &str = "response.reasoning_summary_part.done";
    pub const REASONING_SUMMARY_TEXT_DELTA: &str = "response.reasoning_summary_text.delta";
    pub const REASONING_SUMMARY_TEXT_DONE: &str = "response.reasoning_summary_text.done";
    pub const COMPLETED: &str = "response.completed";
    pub const INCOMPLETE: &str = "response.incomplete";
    pub const FAILED: &str = "response.failed";
}

/// `POST /v1/messages` with `"stream": true`.
pub mod messages {
    pub const MESSAGE_START: &str = "message_start";
    pub const CONTENT_BLOCK_START: &str = "content_block_start";
    pub const CONTENT_BLOCK_DELTA: &str = "content_block_delta";
    pub const CONTENT_BLOCK_STOP: &str = "content_block_stop";
    pub const MESSAGE_DELTA: &str = "message_delta";
    pub const MESSAGE_STOP: &str = "message_stop";
    pub const PING: &str = "ping";
    pub const ERROR: &str = "error";
}
