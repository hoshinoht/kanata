pub mod auth;
#[cfg(test)]
mod provider_tests;
mod request;
#[cfg(test)]
mod tests;

use super::{
    Adapter, AdapterFuture, AdapterOutput,
    codex::stream,
    diagnostics::UpstreamLabel,
    transport::{
        Accept, COMPLETE_RESPONSE_BYTES, CredentialHeader, Endpoint, ResponseContentType,
        STREAM_RESPONSE_BYTES, Transport, TransportRequest,
    },
};
use crate::{
    config::{ProviderKind, ValidatedConfig},
    core::{
        Capabilities, ErrorKind, GatewayError, ModelAlias, Request, Response, RoutedRequest,
        TrustZone,
    },
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

const HOST: &str = "api.openai.com";
const NAMESPACE: &str = "kanata";

pub struct ChatgptAdapter {
    id: String,
    capabilities: Capabilities,
    routes: Vec<request::RouteBinding>,
    transport: Arc<Transport>,
    auth: Arc<auth::AuthManager>,
    request_budget: usize,
    timeout: Duration,
}

impl ChatgptAdapter {
    pub fn from_config(config: &ValidatedConfig, id: &str) -> Result<Self, GatewayError> {
        let adapter = config
            .adapters()
            .iter()
            .find(|adapter| adapter.id() == id)
            .ok_or_else(internal)?;
        if adapter.kind() != ProviderKind::Chatgpt
            || adapter.trust_zone() != TrustZone::External
            || adapter.base_url().as_str() != "https://api.openai.com/v1"
            || adapter.secret_ref().is_some()
        {
            return Err(internal());
        }
        let auth = config.chatgpt_auth().ok_or_else(internal)?;
        Ok(Self {
            id: id.into(),
            capabilities: adapter.capabilities().clone(),
            routes: config
                .routes()
                .iter()
                .filter(|route| route.adapter_id() == id)
                .map(request::RouteBinding::from_route)
                .collect(),
            transport: Arc::new(Transport::new_pinned_https(HOST, config.timeouts())?),
            auth: Arc::new(
                auth::AuthManager::new(auth, config.timeouts()).map_err(|_| internal())?,
            ),
            request_budget: config.limits().max_body_bytes() as usize,
            timeout: Duration::from_millis(config.timeouts().overall_ms()),
        })
    }
}

impl Adapter for ChatgptAdapter {
    fn id(&self) -> &str {
        &self.id
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
        let payload = request::encode(&routed, &self.capabilities, &self.routes);
        let transport = self.transport.clone();
        let auth = self.auth.clone();
        let budget = self.request_budget;
        let timeout = self.timeout;
        let label = UpstreamLabel::new(&self.id, "chatgpt");
        Box::pin(async move {
            let payload = payload?;
            let Request::Chat(chat) = routed.request() else {
                return Err(invalid());
            };
            let model = chat.model.clone();
            let streaming = chat.stream;
            let deadline = tokio::time::Instant::now() + timeout;
            tokio::time::timeout(timeout, async {
                let token = auth.access_token().await.map_err(|_| unavailable())?;
                execute_with_token(
                    &transport,
                    token.bearer(),
                    Inference {
                        payload,
                        model,
                        streaming,
                        deadline,
                        label,
                        request_budget: budget,
                    },
                )
                .await
            })
            .await
            .map_err(|_| GatewayError {
                kind: ErrorKind::Timeout {
                    phase: crate::core::TimeoutPhase::Overall,
                },
            })?
        })
    }
}

struct Inference {
    payload: serde_json::Value,
    model: ModelAlias,
    streaming: bool,
    deadline: tokio::time::Instant,
    label: UpstreamLabel,
    request_budget: usize,
}

async fn execute_with_token(
    transport: &Transport,
    bearer: &str,
    inference: Inference,
) -> Result<AdapterOutput, GatewayError> {
    let Inference {
        payload,
        model,
        streaming,
        deadline,
        label,
        request_budget,
    } = inference;
    let request = TransportRequest::json(
        http::Method::POST,
        Endpoint::new(&["v1", "responses"])?,
        &payload,
        Some(CredentialHeader::authorization(format!("Bearer {bearer}"))),
        Some(Accept::EventStream),
        request_budget,
        STREAM_RESPONSE_BYTES,
    )?;
    let mut response = transport.execute(request).await?;
    if response.status != 200 {
        label.status(response.status, &mut response.body).await;
        return Err(status_error(response.status));
    }
    if response.content_type != Some(ResponseContentType::EventStream)
        && response.content_type_present
    {
        label.content_type(response.status, response.media_type.as_deref());
        return Err(failure());
    }
    if streaming {
        Ok(AdapterOutput::Events(stream::event_stream_with_parser(
            response.body,
            model,
            deadline,
            label,
            Some(NAMESPACE),
        )))
    } else {
        let response = stream::collect_response_with_parser(
            response.body,
            model,
            deadline,
            &label,
            Some(NAMESPACE),
        )
        .await?;
        Ok(AdapterOutput::Complete(Response::Chat(response)))
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_reasoning_efforts: Option<Vec<crate::core::ReasoningEffort>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<crate::core::ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsupported_reasoning_efforts: Vec<String>,
}

pub async fn models(config: &ValidatedConfig) -> Result<Vec<Model>, String> {
    let settings = config
        .chatgpt_auth()
        .ok_or("ChatGPT authentication is not configured")?;
    let auth =
        auth::AuthManager::new(settings, config.timeouts()).map_err(|error| error.to_string())?;
    let transport = Transport::new_pinned_https(HOST, config.timeouts())
        .map_err(|_| "ChatGPT transport unavailable")?;
    tokio::time::timeout(Duration::from_secs(30), async {
        let token = auth
            .access_token()
            .await
            .map_err(|error| error.to_string())?;
        let request = TransportRequest::get(
            Endpoint::new(&["v1", "models"]).map_err(|_| "invalid endpoint")?,
            Some(CredentialHeader::authorization(format!(
                "Bearer {}",
                token.bearer()
            ))),
            COMPLETE_RESPONSE_BYTES,
        )
        .map_err(|_| "ChatGPT model request failed")?;
        let mut response = transport
            .execute(request)
            .await
            .map_err(|_| "ChatGPT model request failed")?;
        if response.status != 200 || response.content_type != Some(ResponseContentType::Json) {
            return Err(
                "ChatGPT model catalog unavailable; check sign-in status and account permissions"
                    .into(),
            );
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.body.next().await {
            bytes.extend_from_slice(&chunk.map_err(|_| "ChatGPT model response interrupted")?);
        }
        decode_models(&bytes).map_err(|_| "ChatGPT model catalog is invalid".into())
    })
    .await
    .map_err(|_| "ChatGPT model catalog timed out")?
}

fn decode_models(bytes: &[u8]) -> Result<Vec<Model>, ()> {
    #[derive(Deserialize)]
    struct Catalog {
        models: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        slug: String,
        display_name: String,
        visibility: String,
        #[serde(default)]
        supported_reasoning_levels: Option<Vec<Level>>,
        #[serde(default)]
        default_reasoning_level: Option<String>,
    }
    #[derive(Deserialize)]
    struct Level {
        effort: String,
    }
    let catalog: Catalog = serde_json::from_slice(bytes).map_err(|_| ())?;
    if catalog.models.len() > 16_384 {
        return Err(());
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut models = Vec::new();
    for entry in catalog.models {
        if entry.slug.is_empty()
            || entry.slug.len() > 512
            || entry.display_name.is_empty()
            || entry.display_name.len() > 512
            || entry.slug.chars().any(char::is_control)
            || entry.display_name.chars().any(char::is_control)
            || !seen.insert(entry.slug.clone())
        {
            return Err(());
        }
        if entry.visibility == "list" {
            let mut unsupported_reasoning_efforts = Vec::new();
            let supported_reasoning_efforts = match entry.supported_reasoning_levels {
                Some(levels) => {
                    if levels.len() > 16 {
                        return Err(());
                    }
                    let mut efforts = Vec::new();
                    let mut seen = std::collections::BTreeSet::new();
                    for level in levels {
                        if !crate::config::valid_identifier(&level.effort)
                            || level.effort.len() > 32
                            || !seen.insert(level.effort.clone())
                        {
                            return Err(());
                        }
                        match crate::core::ReasoningEffort::parse(&level.effort) {
                            Some(effort) => efforts.push(effort),
                            None => unsupported_reasoning_efforts.push(level.effort),
                        }
                    }
                    Some(efforts)
                }
                None => None,
            };
            let default_reasoning_effort = entry
                .default_reasoning_level
                .as_deref()
                .and_then(crate::core::ReasoningEffort::parse);
            if let Some(default) = default_reasoning_effort
                && supported_reasoning_efforts
                    .as_ref()
                    .is_some_and(|efforts| !efforts.contains(&default))
            {
                return Err(());
            }
            models.push(Model {
                slug: entry.slug,
                display_name: entry.display_name,
                supported_reasoning_efforts,
                default_reasoning_effort,
                unsupported_reasoning_efforts,
            });
        }
    }
    Ok(models)
}

fn status_error(status: u16) -> GatewayError {
    GatewayError {
        kind: match status {
            400 | 422 => ErrorKind::InvalidRequest,
            404 => ErrorKind::NotFound,
            401 | 403 | 408 | 503 | 504 => ErrorKind::UpstreamUnavailable,
            429 => ErrorKind::RateLimited,
            _ => ErrorKind::UpstreamFailure,
        },
    }
}
fn internal() -> GatewayError {
    GatewayError {
        kind: ErrorKind::Internal,
    }
}
fn invalid() -> GatewayError {
    GatewayError {
        kind: ErrorKind::InvalidRequest,
    }
}
fn unavailable() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamUnavailable,
    }
}
fn failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
