use serde::{Deserialize, de::DeserializeOwned};

use crate::{core::GatewayError, upstream_failure};

use super::response::{self, CompletionTokensDetails, PromptTokensDetails};

/// A lenient chunk; unknown members are ignored.
#[derive(Deserialize)]
#[serde(bound = "U: Deserialize<'de>")]
pub(crate) struct StreamChunk<U> {
    pub(crate) choices: Vec<StreamChoice>,
    #[serde(default)]
    pub(crate) usage: Option<U>,
}

#[derive(Deserialize)]
pub(crate) struct StreamChoice {
    pub(crate) index: u64,
    pub(crate) delta: StreamDelta,
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct StreamDelta {
    #[serde(default)]
    pub(crate) role: Option<String>,
    #[serde(default)]
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) reasoning: Option<String>,
    #[serde(default)]
    pub(crate) reasoning_content: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Option<Vec<ToolDelta>>,
}

impl StreamDelta {
    pub(crate) fn has_reasoning(&self) -> bool {
        self.reasoning.is_some() || self.reasoning_content.is_some()
    }

    pub(crate) fn take_reasoning(&mut self) -> Option<String> {
        self.reasoning
            .take()
            .or(self.reasoning_content.take())
            .filter(|text| !text.is_empty())
    }
}

/// Tool-call delta wire structs; the variants differ only in serde attributes.
macro_rules! tool_delta_wire {
    ($tool:ident, $function:ident $(, #[$attr:meta])*) => {
        #[derive(Deserialize)]
        $(#[$attr])*
        pub(crate) struct $tool {
            pub(crate) index: u64,
            #[serde(default)]
            pub(crate) id: Option<String>,
            #[serde(rename = "type", default)]
            pub(crate) kind: Option<String>,
            #[serde(default)]
            pub(crate) function: Option<$function>,
        }

        #[derive(Deserialize)]
        $(#[$attr])*
        pub(crate) struct $function {
            #[serde(default)]
            pub(crate) name: Option<String>,
            #[serde(default)]
            pub(crate) arguments: Option<String>,
        }
    };
}

tool_delta_wire!(ToolDelta, FunctionDelta);
tool_delta_wire!(
    StrictToolDelta,
    StrictFunctionDelta,
    #[serde(deny_unknown_fields)]
);

impl From<StrictToolDelta> for ToolDelta {
    fn from(call: StrictToolDelta) -> Self {
        Self {
            index: call.index,
            id: call.id,
            kind: call.kind,
            function: call.function.map(|function| FunctionDelta {
                name: function.name,
                arguments: function.arguments,
            }),
        }
    }
}

/// Lenient usage, including cached prompt tokens.
#[derive(Deserialize)]
pub(crate) struct UsagePayload {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

impl From<UsagePayload> for crate::core::Usage {
    fn from(usage: UsagePayload) -> Self {
        response::usage(
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens,
            usage.prompt_tokens_details,
            usage.completion_tokens_details,
        )
    }
}

pub(crate) fn parse<T: DeserializeOwned>(data: &str) -> Result<T, GatewayError> {
    serde_json::from_str(data).map_err(|_| upstream_failure())
}
