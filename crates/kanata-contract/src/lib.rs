//! Client-facing kanata wire contract: error envelope, `/v1/models` metadata, wire enums,
//! request limits and typed-stream event names. Serde only; no transport.

#![forbid(unsafe_code)]

pub mod errors;
pub mod events;
pub mod limits;
pub mod models;
mod wire;

pub use errors::{ErrorBody, ErrorEnvelope};
pub use models::{
    AdmissionSnapshot, EmbeddingsMetadata, ImageInputMetadata, MessagesEndpoint, ModelList,
    ModelMetadata, ModelObject, ResponsesEndpoint, SpeechMetadata, SpeedRange,
};
pub use wire::{Operation, ReasoningEffort, SpeechFormat, TrustZone};
