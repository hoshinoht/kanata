use std::{fmt, future::Future, pin::Pin, sync::Arc, time::Duration};

use http::Method;
use serde_json::Value;

use crate::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    config::{ProviderKind, ValidatedConfig},
    core::{
        Capabilities, ErrorKind, GatewayError, Operation, Request, Response, RoutedRequest,
        TrustZone,
    },
};

use super::{
    auth::{
        AccessToken, CodexAuthClient, Credential, CredentialStore, MAX_REFRESH_LOCK_WAIT,
        RefreshCoordinator, RefreshError, RefreshExchangeError, RefreshResponse,
    },
    protocol, stream,
    validation::{self, RouteBinding},
};
use crate::adapter::diagnostics::UpstreamLabel;
use crate::adapter::transport::{
    Accept, CredentialHeader, Endpoint, ResponseBody, ResponseContentType, STREAM_RESPONSE_BYTES,
    Transport, TransportRequest,
};

const CODEX_HOST: &str = "chatgpt.com";
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const RESPONSE_ENDPOINT: &[&str] = &["backend-api", "codex", "responses"];
const REFRESH_SKEW: Duration = Duration::from_secs(60);

type ExchangeFuture =
    Pin<Box<dyn Future<Output = Result<RefreshResponse, RefreshExchangeError>> + Send + 'static>>;
pub(super) type RefreshExchange = Arc<dyn Fn(Credential) -> ExchangeFuture + Send + Sync>;

pub struct CodexAdapter {
    id: String,
    capabilities: Capabilities,
    max_body_bytes: usize,
    route_bindings: Vec<RouteBinding>,
    transport: Arc<Transport>,
    coordinator: Arc<RefreshCoordinator>,
    refresh_exchange: RefreshExchange,
    overall_timeout: Duration,
}

impl fmt::Debug for CodexAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexAdapter")
            .field("id", &self.id)
            .field("capabilities", &self.capabilities)
            .field("trust_zone", &TrustZone::External)
            .field("route_count", &self.route_bindings.len())
            .finish_non_exhaustive()
    }
}

impl CodexAdapter {
    pub fn from_config(config: &ValidatedConfig, adapter_id: &str) -> Result<Self, GatewayError> {
        let parts = bind_config(config, adapter_id)?;
        let auth = config.codex_auth().ok_or_else(internal_error)?;
        let coordinator = Arc::new(
            RefreshCoordinator::new(
                CredentialStore::from_config(auth),
                REFRESH_SKEW,
                MAX_REFRESH_LOCK_WAIT,
            )
            .map_err(|_| internal_error())?,
        );
        let auth_client = Arc::new(CodexAuthClient::new(config.timeouts())?);
        let refresh_exchange: RefreshExchange = Arc::new(move |credential| {
            let auth_client = auth_client.clone();
            Box::pin(async move { auth_client.refresh(credential).await })
        });
        let transport = Transport::new_pinned_https(CODEX_HOST, config.timeouts())?;
        Ok(Self::new(
            parts,
            coordinator,
            refresh_exchange,
            transport,
            config,
        ))
    }

    #[cfg(test)]
    fn with_test_root(
        config: &ValidatedConfig,
        adapter_id: &str,
        coordinator: Arc<RefreshCoordinator>,
        refresh_exchange: RefreshExchange,
        certificate: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
        address: std::net::SocketAddr,
    ) -> Result<Self, GatewayError> {
        let parts = bind_config(config, adapter_id)?;
        let transport = Transport::new_pinned_https_with_test_root(
            CODEX_HOST,
            config.timeouts(),
            certificate,
            address,
        )?;
        Ok(Self::new(
            parts,
            coordinator,
            refresh_exchange,
            transport,
            config,
        ))
    }

    fn new(
        parts: ConfigParts,
        coordinator: Arc<RefreshCoordinator>,
        refresh_exchange: RefreshExchange,
        transport: Transport,
        config: &ValidatedConfig,
    ) -> Self {
        Self {
            id: parts.id,
            capabilities: parts.capabilities,
            max_body_bytes: parts.max_body_bytes,
            route_bindings: parts.route_bindings,
            transport: Arc::new(transport),
            coordinator,
            refresh_exchange,
            overall_timeout: Duration::from_millis(config.timeouts().overall_ms()),
        }
    }

    pub fn configured_id(&self) -> &str {
        &self.id
    }
}

impl Adapter for CodexAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
        let binding = match validation::validate(&routed, &self.capabilities, &self.route_bindings)
        {
            Ok(binding) => binding,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let reasoning_effort = match validation::reasoning_effort(&routed, binding) {
            Ok(effort) => effort,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let public_model = routed.context().route.selector.model_alias.clone();
        let streaming = match routed.request() {
            Request::Chat(chat) => chat.stream,
            Request::Transcription(_) => {
                return Box::pin(async { Err(unsupported_operation()) });
            }
        };
        let payload = match protocol::to_responses_request(&routed, reasoning_effort) {
            Ok(payload) => payload,
            Err(error) => return Box::pin(async move { Err(error) }),
        };

        let transport = self.transport.clone();
        let coordinator = self.coordinator.clone();
        let refresh_exchange = self.refresh_exchange.clone();
        let request_budget = self.max_body_bytes;
        let overall_timeout = self.overall_timeout;
        let label = UpstreamLabel::new(&self.id, "codex");

        Box::pin(async move {
            let deadline = tokio::time::Instant::now()
                .checked_add(overall_timeout)
                .ok_or_else(overall_timeout_error)?;
            let request = async {
                let body = open_response(
                    transport,
                    coordinator,
                    refresh_exchange,
                    &payload,
                    request_budget,
                    &label,
                )
                .await?;
                if streaming {
                    Ok(DispatchResult::Stream(body))
                } else {
                    let response =
                        stream::collect_response(body, public_model.clone(), deadline, &label)
                            .await?;
                    Ok(DispatchResult::Complete(Response::Chat(response)))
                }
            };
            let result = tokio::time::timeout(overall_timeout, request)
                .await
                .map_err(|_| overall_timeout_error())??;
            match result {
                DispatchResult::Stream(body) => Ok(AdapterOutput::Events(stream::event_stream(
                    body,
                    public_model,
                    deadline,
                    label,
                ))),
                DispatchResult::Complete(response) => Ok(AdapterOutput::Complete(response)),
            }
        })
    }
}

enum DispatchResult {
    Stream(ResponseBody),
    Complete(Response),
}

struct ConfigParts {
    id: String,
    capabilities: Capabilities,
    max_body_bytes: usize,
    route_bindings: Vec<RouteBinding>,
}

fn bind_config(config: &ValidatedConfig, adapter_id: &str) -> Result<ConfigParts, GatewayError> {
    let adapter = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == adapter_id)
        .ok_or_else(internal_error)?;
    let configured = adapter.capabilities();
    if adapter.kind() != ProviderKind::Codex
        || adapter.trust_zone() != TrustZone::External
        || adapter.secret_ref().is_some()
        || adapter.base_url().as_str() != CODEX_BASE_URL
        || !adapter.extension_allowlist().is_empty()
        || configured.operations.len() != 1
        || !configured.operations.contains(&Operation::Chat)
        || configured.input_audio
        || configured.audio_streaming_chat
        || configured.audio_function_tools
        || configured.structured_output
        || configured.sampling_controls
    {
        return Err(internal_error());
    }

    let route_bindings = config
        .routes()
        .iter()
        .filter(|route| route.adapter_id() == adapter_id)
        .map(|route| validation::bind_route(adapter_id, route))
        .collect::<Result<Vec<_>, _>>()?;
    if route_bindings.is_empty() {
        return Err(internal_error());
    }

    let max_body_bytes =
        usize::try_from(config.limits().max_body_bytes()).map_err(|_| internal_error())?;
    if max_body_bytes == 0 {
        return Err(internal_error());
    }
    let mut capabilities = Capabilities::new([Operation::Chat]);
    capabilities.streaming_chat = configured.streaming_chat;
    capabilities.function_tools = configured.function_tools;
    capabilities.reasoning_control = configured.reasoning_control;

    Ok(ConfigParts {
        id: adapter.id().to_owned(),
        capabilities,
        max_body_bytes,
        route_bindings,
    })
}

async fn open_response(
    transport: Arc<Transport>,
    coordinator: Arc<RefreshCoordinator>,
    refresh_exchange: RefreshExchange,
    payload: &Value,
    request_budget: usize,
    label: &UpstreamLabel,
) -> Result<ResponseBody, GatewayError> {
    let exchange = refresh_exchange.clone();
    let rejected_token = coordinator
        .access_token(move |credential| async move { exchange(credential).await })
        .await
        .map_err(|error| credential_unavailable(label, error))?;

    let mut response = post_responses(&transport, &rejected_token, payload, request_budget).await?;
    if response.status == 401 {
        let exchange = refresh_exchange.clone();
        let replacement = coordinator
            .refresh_after_unauthorized(&rejected_token, move |credential| async move {
                exchange(credential).await
            })
            .await
            .map_err(|error| credential_unavailable(label, error))?;
        response = post_responses(&transport, &replacement, payload, request_budget).await?;
    }

    if response.status != 200 {
        label.status(response.status, &mut response.body).await;
        return Err(status_error(response.status));
    }
    // The backend may omit Content-Type on its SSE stream; the stream parser still
    // rejects non-SSE bodies. An explicit non-SSE type is refused here.
    if response.content_type != Some(ResponseContentType::EventStream)
        && response.content_type_present
    {
        label.content_type(response.status, response.media_type.as_deref());
        return Err(upstream_failure());
    }
    Ok(response.body)
}

async fn post_responses(
    transport: &Transport,
    token: &AccessToken,
    payload: &Value,
    request_budget: usize,
) -> Result<crate::adapter::transport::TransportResponse, GatewayError> {
    let credential = CredentialHeader::authorization_with_chatgpt_account_id(
        format!("Bearer {}", token.as_str()),
        token.account_id(),
    )?;
    let request = TransportRequest::json(
        Method::POST,
        Endpoint::new(RESPONSE_ENDPOINT)?,
        payload,
        Some(credential),
        Some(Accept::EventStream),
        request_budget,
        STREAM_RESPONSE_BYTES,
    )?;
    transport.execute(request).await
}

fn status_error(status: u16) -> GatewayError {
    let kind = match status {
        400 | 422 => ErrorKind::InvalidRequest,
        404 => ErrorKind::NotFound,
        401 | 403 | 408 | 503 | 504 => ErrorKind::UpstreamUnavailable,
        429 => ErrorKind::RateLimited,
        500..=599 => ErrorKind::UpstreamFailure,
        _ => ErrorKind::UpstreamFailure,
    };
    GatewayError { kind }
}

fn credential_unavailable(label: &UpstreamLabel, error: RefreshError) -> GatewayError {
    label.credential(error);
    upstream_unavailable()
}

fn internal_error() -> GatewayError {
    GatewayError {
        kind: ErrorKind::Internal,
    }
}

fn upstream_unavailable() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamUnavailable,
    }
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}

fn unsupported_operation() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UnsupportedOperation,
    }
}

fn overall_timeout_error() -> GatewayError {
    GatewayError {
        kind: ErrorKind::Timeout {
            phase: crate::core::TimeoutPhase::Overall,
        },
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;
