//! OpenAI-compatible `POST embeddings` shared by Ollama, vLLM and OpenRouter. Callers validate
//! the request first; providers pass extra body members and their response envelope.

use std::sync::Arc;

use http::Method;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    adapter::{
        AdapterOutput,
        diagnostics::UpstreamLabel,
        transport::{
            Accept, COMPLETE_RESPONSE_BYTES, CredentialHeader, Endpoint, Transport,
            TransportRequest,
        },
        upstream_failure,
    },
    core::{EmbeddingRequest, EmbeddingResponse, GatewayError, Response, Usage},
};

#[derive(Serialize)]
struct Payload<'a, E> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
    #[serde(flatten)]
    extra: E,
}

/// OpenAI embedding list; unknown members are ignored.
#[derive(Deserialize)]
pub(crate) struct List {
    pub(crate) object: String,
    pub(crate) data: Vec<Item>,
    #[serde(default)]
    pub(crate) usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
pub(crate) struct Item {
    pub(crate) object: String,
    pub(crate) index: usize,
    pub(crate) embedding: Vec<f32>,
}

#[derive(Deserialize)]
pub(crate) struct TokenUsage {
    pub(crate) prompt_tokens: u64,
    pub(crate) total_tokens: u64,
}

/// Posts `request` as `upstream` with `extra` merged into the body and decodes envelope `W`.
pub(crate) async fn execute<W, E>(
    transport: Arc<Transport>,
    request: EmbeddingRequest,
    upstream: String,
    extra: E,
    credential: Option<CredentialHeader>,
    max_body_bytes: usize,
    label: UpstreamLabel,
) -> Result<AdapterOutput, GatewayError>
where
    W: DeserializeOwned + Into<List>,
    E: Serialize,
{
    let outbound = TransportRequest::json(
        Method::POST,
        Endpoint::new(&["embeddings"])?,
        &Payload {
            model: &upstream,
            input: &request.input,
            encoding_format: "float",
            dimensions: request.dimensions,
            extra,
        },
        credential,
        Some(Accept::Json),
        max_body_bytes,
        COMPLETE_RESPONSE_BYTES,
    )?;
    let body = super::post_json(&transport, outbound, &label).await?;
    let wire: W = serde_json::from_slice(&body).map_err(|_| upstream_failure())?;
    let response = decode(wire.into(), &request)?;
    Ok(AdapterOutput::Complete(Response::Embeddings(response)))
}

fn decode(mut wire: List, request: &EmbeddingRequest) -> Result<EmbeddingResponse, GatewayError> {
    if wire.object != "list" || wire.data.len() != request.input.len() {
        return Err(upstream_failure());
    }
    wire.data.sort_unstable_by_key(|item| item.index);
    if wire
        .data
        .iter()
        .enumerate()
        .any(|(index, item)| item.index != index || item.object != "embedding")
    {
        return Err(upstream_failure());
    }
    let response = EmbeddingResponse {
        model: request.model.clone(),
        vectors: wire.data.into_iter().map(|item| item.embedding).collect(),
        usage: wire.usage.map(|usage| Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: 0,
            total_tokens: usage.total_tokens,
            reasoning_tokens: None,
            cached_input_tokens: None,
        }),
    };
    if !response.valid_for(request) {
        return Err(upstream_failure());
    }
    Ok(response)
}
