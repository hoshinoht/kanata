use crate::adapter::{chat_completions, invalid_request, upstream_failure};
use http::Method;
use serde::{Deserialize, Serialize};

use crate::core::{EmbeddingRequest, EmbeddingResponse, Usage};

use super::*;
use crate::adapter::transport::{Accept, COMPLETE_RESPONSE_BYTES, Endpoint, TransportRequest};

#[derive(Serialize)]
struct Payload<'a> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Deserialize)]
struct WireResponse {
    object: String,
    data: Vec<WireEmbedding>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireEmbedding {
    object: String,
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    total_tokens: u64,
}

pub(super) async fn execute(
    transport: Arc<Transport>,
    request: EmbeddingRequest,
    upstream: String,
    max_body_bytes: usize,
    label: UpstreamLabel,
) -> Result<AdapterOutput, GatewayError> {
    request.validate().map_err(|_| invalid_request())?;
    let outbound = TransportRequest::json(
        Method::POST,
        Endpoint::new(&["embeddings"])?,
        &Payload {
            model: &upstream,
            input: &request.input,
            encoding_format: "float",
            dimensions: request.dimensions,
        },
        None,
        Some(Accept::Json),
        max_body_bytes,
        COMPLETE_RESPONSE_BYTES,
    )?;
    let body = chat_completions::post_json(&transport, outbound, &label).await?;
    let response = decode(&body, &request)?;
    Ok(AdapterOutput::Complete(Response::Embeddings(response)))
}

fn decode(bytes: &[u8], request: &EmbeddingRequest) -> Result<EmbeddingResponse, GatewayError> {
    let mut wire: WireResponse = serde_json::from_slice(bytes).map_err(|_| upstream_failure())?;
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
