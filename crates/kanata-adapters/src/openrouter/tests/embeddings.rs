use std::time::Duration;

use serde_json::{Value, json};

use crate::core::{EmbeddingRequest, GatewayError, Response};

use super::*;

const EMBEDDINGS: Features = Features {
    streaming_chat: false,
    input_images: false,
    allows_input_images: false,
    embeddings: true,
};
const EMBEDDINGS_RESPONSE: &str =
    include_str!("../../../../../tests/fixtures/openrouter/embeddings.json");

fn request(dimensions: Option<usize>) -> CoreRequest {
    CoreRequest::Embeddings(EmbeddingRequest {
        model: ModelAlias(PUBLIC_MODEL.into()),
        input: vec!["alpha".into(), "beta".into()],
        dimensions,
    })
}

async fn embed(
    status: u16,
    body: &str,
    dimensions: Option<usize>,
) -> (Result<AdapterOutput, GatewayError>, RequestRecord) {
    let (client, server) =
        Client::responding_with(EMBEDDINGS, status, "application/json", body.as_bytes()).await;
    let result = tokio::time::timeout(Duration::from_secs(3), client.execute(request(dimensions)))
        .await
        .unwrap_or_else(|_| panic!("OpenRouter embeddings timeout"));
    (result, recorded(server).await)
}

#[tokio::test]
async fn embeddings_pin_the_provider_and_normalize_the_fixture() {
    for dimensions in [None, Some(3)] {
        let (result, record) = embed(200, EMBEDDINGS_RESPONSE, dimensions).await;
        let Ok(AdapterOutput::Complete(Response::Embeddings(response))) = result else {
            panic!("embeddings response for {dimensions:?}")
        };
        assert_eq!(response.model, ModelAlias(PUBLIC_MODEL.into()));
        assert_eq!(
            response.vectors,
            vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]]
        );
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 4,
                output_tokens: 0,
                total_tokens: 4,
                reasoning_tokens: None,
                cached_input_tokens: None,
            })
        );

        assert_eq!(record.method, "POST");
        assert_eq!(record.path, "/api/v1/embeddings");
        assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(record.headers["accept"], "application/json");
        let mut expected = json!({
            "model": UPSTREAM_MODEL,
            "input": ["alpha", "beta"],
            "encoding_format": "float",
            "provider": {"allow_fallbacks": false},
        });
        if let Some(dimensions) = dimensions {
            expected["dimensions"] = json!(dimensions);
        }
        let body: Value =
            serde_json::from_slice(&record.body).unwrap_or_else(|_| panic!("request json"));
        assert_eq!(body, expected);
    }
}

#[tokio::test]
async fn embeddings_statuses_and_invalid_lists_map_to_gateway_errors() {
    let unknown_member = EMBEDDINGS_RESPONSE.replacen(
        "\"object\": \"list\",",
        "\"object\": \"list\",\n  \"private\": true,",
        1,
    );
    assert_ne!(unknown_member, EMBEDDINGS_RESPONSE);
    let item = |index: u8| format!(r#"{{"object":"embedding","index":{index},"embedding":[0.1]}}"#);
    let list = |data: &str| format!(r#"{{"object":"list","model":"m","data":[{data}]}}"#);
    let one_vector = list(&item(0));
    let duplicate_index = list(&format!("{},{}", item(0), item(0)));
    let missing_model = format!(r#"{{"object":"list","data":[{},{}]}}"#, item(0), item(1));
    let error = r#"{"error":{"message":"fixture"}}"#;
    for (status, body, dimensions, expected) in [
        (400, error, None, ErrorKind::InvalidRequest),
        (401, error, None, ErrorKind::UpstreamUnavailable),
        (404, error, None, ErrorKind::NotFound),
        (429, error, None, ErrorKind::RateLimited),
        (500, error, None, ErrorKind::UpstreamFailure),
        (200, "not json", None, ErrorKind::UpstreamFailure),
        (200, &unknown_member, None, ErrorKind::UpstreamFailure),
        (200, &missing_model, None, ErrorKind::UpstreamFailure),
        (200, &one_vector, None, ErrorKind::UpstreamFailure),
        (200, &duplicate_index, None, ErrorKind::UpstreamFailure),
        (
            200,
            EMBEDDINGS_RESPONSE,
            Some(2),
            ErrorKind::UpstreamFailure,
        ),
    ] {
        let (result, record) = embed(status, body, dimensions).await;
        assert_eq!(error_kind(result), expected, "{status} {body}");
        assert_eq!(record.path, "/api/v1/embeddings");
    }
}
