//! `GET /v1/models` and `GET /v1/models/{id}` bodies, including the `kanata` extension.

use serde::{Deserialize, Serialize};

use crate::{Operation, ReasoningEffort, SpeechFormat, TrustZone};

/// `{"object": "list", "data": [...]}`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelList {
    pub object: String,
    pub data: Vec<ModelObject>,
}

/// One listed model; `object` is `"model"`, `owned_by` is `"kanata"`, `created` is 0.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub owned_by: String,
    pub kanata: ModelMetadata,
}

/// The `kanata` member: what the model id accepts on this listener.
///
/// Optional sections are omitted, not `null`, when they do not apply; `images`,
/// `reasoning_efforts`, `trust_zone`, `context_tokens` and `max_output_tokens` are `null`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelMetadata {
    pub operations: Vec<Operation>,
    pub structured_output: bool,
    pub sampling_controls: bool,
    pub reasoning_control: bool,
    pub function_tools: bool,
    pub streaming: bool,
    pub input_audio: bool,
    pub input_images: bool,
    pub images: Option<ImageInputMetadata>,
    pub trust_zone: Option<TrustZone>,
    pub reasoning_efforts: Option<Vec<ReasoningEffort>>,
    pub context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses: Option<ResponsesEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<MessagesEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings: Option<EmbeddingsMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech: Option<SpeechMetadata>,
    /// Private listener only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionSnapshot>,
}

/// Image input over `data:` URLs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImageInputMetadata {
    pub transport: String,
    pub media_types: Vec<String>,
    pub max_count: usize,
    pub max_bytes_per_image: usize,
    pub max_total_bytes: usize,
    pub max_base64_bytes_per_image: usize,
    pub max_dimension: u32,
    pub max_total_pixels: u64,
    pub max_json_body_bytes_without_audio: usize,
}

/// `POST /v1/responses` for a chat model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResponsesEndpoint {
    pub endpoint: String,
    pub operation: Operation,
    pub stateless: bool,
    pub input: Vec<String>,
    pub function_tools: bool,
    pub streaming: bool,
    pub max_output_bytes: usize,
}

/// `POST /v1/messages` for a chat model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessagesEndpoint {
    pub endpoint: String,
    pub operation: Operation,
    pub stateless: bool,
    pub function_tools: bool,
    pub streaming: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmbeddingsMetadata {
    pub input: Vec<String>,
    pub encoding_formats: Vec<String>,
    pub max_inputs: usize,
    pub max_dimensions: usize,
    pub max_total_values: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SpeechMetadata {
    pub voices: Vec<String>,
    pub formats: Vec<SpeechFormat>,
    pub max_input_characters: usize,
    pub max_input_bytes: usize,
    pub max_response_bytes: usize,
    pub speed: SpeedRange,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct SpeedRange {
    pub min: f64,
    pub max: f64,
}

/// Capacity settings and live load of the model's chat route (else its first route).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdmissionSnapshot {
    pub max_in_flight: u64,
    pub max_queue: u64,
    pub queue_ms: u64,
    pub adapter_max_in_flight: Option<u64>,
    pub in_flight: Option<u64>,
    pub queued: Option<u64>,
    pub adapter_in_flight: Option<u64>,
    /// `closed`, `open` or `half_open`.
    pub breaker: Option<String>,
}
