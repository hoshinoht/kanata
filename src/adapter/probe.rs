use std::{collections::BTreeSet, time::Duration};

use futures_util::StreamExt;
use serde::Deserialize;

use crate::{
    auth::{SecretResolver, canonical_bearer_token},
    config::{ValidatedAdapter, ValidatedTimeouts},
};

use super::transport::{
    COMPLETE_RESPONSE_BYTES, CredentialHeader, Endpoint, ResponseContentType, Transport,
    TransportRequest,
};

pub(crate) async fn models(
    adapter: &ValidatedAdapter,
    timeouts: &ValidatedTimeouts,
    resolver: &impl SecretResolver,
) -> Result<BTreeSet<String>, &'static str> {
    if adapter.kind().is_private_only() {
        return Err("not_supported");
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        let credential = adapter
            .secret_ref()
            .map(|reference| -> Result<_, &'static str> {
                let bytes = resolver
                    .resolve(reference)
                    .map_err(|_| "credential_unavailable")?;
                let token = canonical_bearer_token(reference, bytes)
                    .map_err(|_| "credential_unavailable")?;
                Ok(CredentialHeader::authorization(format!("Bearer {token}")))
            })
            .transpose()?;
        let transport = Transport::new(adapter, timeouts).map_err(|_| "transport_unavailable")?;
        let request = TransportRequest::get(
            Endpoint::new(&["models"]).map_err(|_| "transport_unavailable")?,
            credential,
            COMPLETE_RESPONSE_BYTES,
        )
        .map_err(|_| "transport_unavailable")?;
        let mut response = transport
            .execute(request)
            .await
            .map_err(|_| "request_failed")?;
        match response.status {
            200 => {}
            401 | 403 => return Err("authentication_rejected"),
            404 | 405 => return Err("not_supported"),
            429 => return Err("rate_limited"),
            _ => return Err("upstream_error"),
        }
        if response.content_type != Some(ResponseContentType::Json) {
            return Err("invalid_response");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.body.next().await {
            bytes.extend_from_slice(&chunk.map_err(|_| "invalid_response")?);
        }
        decode_models(&bytes)
    })
    .await
    .unwrap_or(Err("timeout"))
}

fn decode_models(bytes: &[u8]) -> Result<BTreeSet<String>, &'static str> {
    #[derive(Deserialize)]
    struct Catalog {
        data: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        id: String,
    }
    let catalog: Catalog = serde_json::from_slice(bytes).map_err(|_| "invalid_response")?;
    if catalog.data.len() > 16_384 {
        return Err("invalid_response");
    }
    let mut ids = BTreeSet::new();
    for model in catalog.data {
        if model.id.is_empty()
            || model.id.len() > 512
            || model.id.chars().any(char::is_control)
            || !ids.insert(model.id)
        {
            return Err("invalid_response");
        }
    }
    Ok(ids)
}
