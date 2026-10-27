pub const MAX_TRANSCRIPTION_OVERHEAD_BYTES: usize = 104 * 1024;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::{EmbeddingRequest, EmbeddingResponse};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

mod capabilities;
mod chat;
mod content;
mod errors;
mod extensions;
mod options;
mod response;

pub use capabilities::*;
pub use chat::*;
pub use content::*;
pub use errors::*;
pub use extensions::*;
pub use options::*;
pub use response::*;

pub use kanata_contract::{Operation, TrustZone};

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ModelAlias(pub String);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteSelector {
    pub model_alias: ModelAlias,
    pub operation: Operation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteIdentity {
    pub route_id: String,
    pub upstream_id: String,
    #[serde(flatten)]
    pub selector: RouteSelector,
}

impl RouteIdentity {
    pub fn new(
        route_id: impl Into<String>,
        upstream_id: impl Into<String>,
        model_alias: ModelAlias,
        operation: Operation,
    ) -> Self {
        Self {
            route_id: route_id.into(),
            upstream_id: upstream_id.into(),
            selector: RouteSelector {
                model_alias,
                operation,
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContext {
    pub request_id: String,
    pub route: RouteIdentity,
    pub trust_zone: TrustZone,
    #[serde(default)]
    pub extensions: Extensions,
}

impl RequestContext {
    pub fn check_request(&self, request: &Request) -> Result<(), RouteError> {
        let request_operation = request.operation();
        if self.route.selector.operation != request_operation {
            return Err(RouteError::OperationMismatch {
                route_operation: self.route.selector.operation,
                request_operation,
            });
        }
        if self.route.selector.model_alias != *request.model_alias() {
            return Err(RouteError::ModelAliasMismatch {
                route_model_alias: self.route.selector.model_alias.clone(),
                request_model_alias: request.model_alias().clone(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteError {
    OperationMismatch {
        route_operation: Operation,
        request_operation: Operation,
    },
    ModelAliasMismatch {
        route_model_alias: ModelAlias,
        request_model_alias: ModelAlias,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestValidationError {
    InvalidEmbedding { param: &'static str },
    InvalidSpeech { param: &'static str },
    RequiredToolChoiceWithoutTools,
    UndeclaredToolChoice { name: String },
    InputImageRoleMismatch,
    InputImagesTooLarge,
    InputAudioRoleMismatch,
    InputAudioTooLarge,
    ReasoningRoleMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoutedRequestError {
    Route(RouteError),
    Request(RequestValidationError),
}

pub struct RoutedRequest {
    context: RequestContext,
    request: Request,
}

impl RoutedRequest {
    pub fn new(context: RequestContext, request: Request) -> Result<Self, RoutedRequestError> {
        context
            .check_request(&request)
            .map_err(RoutedRequestError::Route)?;
        request.validate().map_err(RoutedRequestError::Request)?;
        Ok(Self { context, request })
    }

    getters! {
        pub context: &RequestContext,
        pub request: &Request,
    }

    pub fn into_parts(self) -> (RequestContext, Request) {
        (self.context, self.request)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionRequest {
    pub model: ModelAlias,
    pub file: ValidatedFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub extensions: Extensions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Chat(ChatRequest),
    Transcription(TranscriptionRequest),
    Embeddings(EmbeddingRequest),
    Speech(super::SpeechRequest),
}

impl Request {
    pub fn operation(&self) -> Operation {
        match self {
            Self::Chat(_) => Operation::Chat,
            Self::Transcription(_) => Operation::Transcription,
            Self::Embeddings(_) => Operation::Embeddings,
            Self::Speech(_) => Operation::Speech,
        }
    }

    pub fn model_alias(&self) -> &ModelAlias {
        match self {
            Self::Chat(request) => &request.model,
            Self::Transcription(request) => &request.model,
            Self::Embeddings(request) => &request.model,
            Self::Speech(request) => &request.model,
        }
    }

    fn validate(&self) -> Result<(), RequestValidationError> {
        if let Self::Speech(request) = self {
            return request
                .validate()
                .map_err(|param| RequestValidationError::InvalidSpeech { param });
        }
        if let Self::Embeddings(request) = self {
            return request
                .validate()
                .map_err(|param| RequestValidationError::InvalidEmbedding { param });
        }
        let Self::Chat(chat) = self else {
            return Ok(());
        };
        chat.validate_audio()?;
        chat.validate_images()?;
        chat.validate_reasoning()?;
        match &chat.tool_choice {
            ToolChoice::Required if chat.tools.is_empty() => {
                Err(RequestValidationError::RequiredToolChoiceWithoutTools)
            }
            ToolChoice::Function { name } if !chat.tools.iter().any(|tool| tool.name == *name) => {
                Err(RequestValidationError::UndeclaredToolChoice { name: name.clone() })
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_cache_key_is_one_to_sixty_four_printable_ascii_bytes() {
        for valid in [
            "k",
            "0f8b1c2e-6a4d-4f1e-9a3b-2c7d5e8f1a0b",
            " ~",
            &"a".repeat(64),
        ] {
            assert_eq!(
                PromptCacheKey::new(valid.to_owned()).map(|key| key.as_str().to_owned()),
                Ok(valid.to_owned())
            );
        }
        for invalid in ["", &"a".repeat(65), "tab\there", "line\n", "é"] {
            assert_eq!(
                PromptCacheKey::new(invalid.to_owned()),
                Err(InvalidChatOption)
            );
        }
    }

    #[test]
    fn cached_input_tokens_never_exceed_input_tokens() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 1,
            total_tokens: 11,
            reasoning_tokens: None,
            cached_input_tokens: None,
        };
        let cached = usage.clone().with_cached_input_tokens(10);
        assert_eq!(cached.cached_input_tokens, Some(10));
        assert_eq!(cached.without_cached_input_tokens(), usage);
        assert_eq!(usage.with_cached_input_tokens(11).cached_input_tokens, None);
    }

    #[test]
    fn penalties_and_stop_sequences_are_bounded() {
        for valid in [-2.0, 0.0, 2.0] {
            assert!(Penalty::new(valid).is_ok());
        }
        for invalid in [-2.01, 2.01, f64::NAN, f64::INFINITY] {
            assert_eq!(Penalty::new(invalid), Err(InvalidChatOption));
        }
        let stop = |items: &[&str]| items.iter().map(|item| (*item).to_owned()).collect();
        assert!(SamplingOptions::validate_stop(stop(&["a", "b", "c", &"d".repeat(256)])).is_ok());
        for invalid in [
            stop(&["a", "b", "c", "d", "e"]),
            stop(&[""]),
            stop(&[&"d".repeat(257)]),
        ] {
            assert_eq!(
                SamplingOptions::validate_stop(invalid),
                Err(InvalidChatOption)
            );
        }
    }
}
