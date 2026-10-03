use serde::{Deserialize, Serialize};

use super::{ModelAlias, Usage};

pub const MAX_EMBEDDING_INPUTS: usize = 128;
pub const MAX_EMBEDDING_DIMENSIONS: usize = 16_384;
pub const MAX_EMBEDDING_VALUES: usize = 262_144;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingEncoding {
    #[default]
    Float,
    Base64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingRequest {
    pub model: ModelAlias,
    pub input: Vec<String>,
    #[serde(default)]
    pub dimensions: Option<usize>,
}

impl EmbeddingRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.model.0.is_empty() {
            return Err("model");
        }
        if self.input.is_empty()
            || self.input.len() > MAX_EMBEDDING_INPUTS
            || self.input.iter().any(String::is_empty)
        {
            return Err("input");
        }
        if let Some(dimensions) = self.dimensions
            && (dimensions == 0
                || dimensions > MAX_EMBEDDING_DIMENSIONS
                || dimensions.saturating_mul(self.input.len()) > MAX_EMBEDDING_VALUES)
        {
            return Err("dimensions");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingResponse {
    pub model: ModelAlias,
    pub vectors: Vec<Vec<f32>>,
    pub usage: Option<Usage>,
}

impl EmbeddingResponse {
    pub fn valid_for(&self, request: &EmbeddingRequest) -> bool {
        let dimensions = self.vectors.first().map_or(0, Vec::len);
        self.model == request.model
            && self.vectors.len() == request.input.len()
            && self.vectors.len() <= MAX_EMBEDDING_INPUTS
            && (1..=MAX_EMBEDDING_DIMENSIONS).contains(&dimensions)
            && dimensions.saturating_mul(self.vectors.len()) <= MAX_EMBEDDING_VALUES
            && request
                .dimensions
                .is_none_or(|expected| expected == dimensions)
            && self.vectors.iter().all(|vector| {
                vector.len() == dimensions && vector.iter().all(|value| value.is_finite())
            })
            && self.usage.as_ref().is_none_or(|usage| {
                usage.output_tokens == 0
                    && usage.reasoning_tokens.is_none()
                    && usage.total_tokens == usage.input_tokens
            })
    }
}
