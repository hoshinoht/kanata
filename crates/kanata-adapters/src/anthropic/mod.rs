mod request;
mod response;
mod stream;
mod stream_state;
mod stream_wire;
mod validation;

#[cfg(test)]
mod tests;

use crate::{internal_error, unsupported_operation, upstream_failure};
use std::{fmt, sync::Arc};

use futures_util::StreamExt;
use http::Method;

use crate::{
    Adapter, AdapterFuture, AdapterOutput,
    config::secret::{SecretResolver, canonical_bearer_token},
    config::{ProviderKind, ValidatedAdapter, ValidatedConfig, ValidatedTimeouts},
    core::{
        Capabilities, ErrorKind, GatewayError, Operation, Request, Response, RoutedRequest,
        TrustZone,
    },
    diagnostics::UpstreamLabel,
};

use super::transport::{
    Accept, COMPLETE_RESPONSE_BYTES, CredentialHeader, Endpoint, ResponseContentType,
    STREAM_RESPONSE_BYTES, Transport, TransportRequest,
};

const API_VERSION: &str = "2023-06-01";

/// Claude Messages API (`POST {base_url}/messages`) with a Console API key.
pub struct AnthropicAdapter {
    id: String,
    capabilities: Capabilities,
    bearer_token: String,
    workspace_id: Option<String>,
    max_tokens: u32,
    max_body_bytes: usize,
    route_bindings: Vec<validation::RouteBinding>,
    transport: Arc<Transport>,
}

impl fmt::Debug for AnthropicAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicAdapter")
            .field("id", &self.id)
            .field("capabilities", &self.capabilities)
            .field("max_tokens", &self.max_tokens)
            .field("route_count", &self.route_bindings.len())
            .finish_non_exhaustive()
    }
}

impl AnthropicAdapter {
    pub fn from_config(
        config: &ValidatedConfig,
        adapter_id: &str,
        resolver: &impl SecretResolver,
    ) -> Result<Self, GatewayError> {
        Self::build_from_config(config, adapter_id, resolver, Transport::new)
    }

    fn build_from_config(
        config: &ValidatedConfig,
        adapter_id: &str,
        resolver: &impl SecretResolver,
        make_transport: impl FnOnce(
            &ValidatedAdapter,
            &ValidatedTimeouts,
        ) -> Result<Transport, GatewayError>,
    ) -> Result<Self, GatewayError> {
        let adapter = config
            .adapters()
            .iter()
            .find(|adapter| adapter.id() == adapter_id)
            .ok_or_else(internal_error)?;
        let configured = adapter.capabilities();
        let (Some(settings), Some(secret_ref)) = (adapter.anthropic(), adapter.secret_ref()) else {
            return Err(internal_error());
        };
        if adapter.kind() != ProviderKind::Anthropic
            || adapter.trust_zone() != TrustZone::External
            || configured.operations.iter().ne([Operation::Chat].iter())
            || configured.input_audio
            || configured.sampling_controls
        {
            return Err(internal_error());
        }
        let route_bindings = config
            .routes()
            .iter()
            .filter(|route| route.adapter_id() == adapter_id)
            .map(|route| validation::bind_route(adapter_id, route, configured))
            .collect::<Result<Vec<_>, _>>()?;
        if route_bindings.is_empty() {
            return Err(internal_error());
        }
        let secret = resolver.resolve(secret_ref).map_err(|_| internal_error())?;
        let bearer_token =
            canonical_bearer_token(secret_ref, secret).map_err(|_| internal_error())?;
        let max_body_bytes =
            usize::try_from(config.limits().max_body_bytes()).map_err(|_| internal_error())?;
        if max_body_bytes == 0 {
            return Err(internal_error());
        }
        Ok(Self {
            id: adapter.id().to_owned(),
            capabilities: configured.clone(),
            bearer_token,
            workspace_id: settings.workspace_id().map(str::to_owned),
            max_tokens: settings.default_max_output_tokens(),
            max_body_bytes,
            route_bindings,
            transport: Arc::new(make_transport(adapter, config.timeouts())?),
        })
    }

    #[cfg(test)]
    fn from_config_with_tls_fixture(
        config: &ValidatedConfig,
        adapter_id: &str,
        resolver: &impl SecretResolver,
        certificate: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
        address: std::net::SocketAddr,
    ) -> Result<Self, GatewayError> {
        Self::build_from_config(config, adapter_id, resolver, move |adapter, timeouts| {
            Transport::new_with_test_root(adapter, timeouts, certificate, address)
        })
    }

    fn credential(&self) -> Result<CredentialHeader, GatewayError> {
        let bearer = format!("Bearer {}", self.bearer_token);
        match self.workspace_id.as_deref() {
            Some(workspace) => CredentialHeader::authorization_with_headers(
                bearer,
                &[
                    ("anthropic-version", API_VERSION),
                    ("anthropic-workspace-id", workspace),
                ],
            ),
            None => CredentialHeader::authorization_with_headers(
                bearer,
                &[("anthropic-version", API_VERSION)],
            ),
        }
    }
}

impl Adapter for AnthropicAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
        if let Err(error) = validation::validate(&routed, &self.capabilities, &self.route_bindings)
        {
            return Box::pin(async move { Err(error) });
        }
        let (context, request) = routed.into_parts();
        let Request::Chat(chat) = request else {
            return Box::pin(async { Err(unsupported_operation()) });
        };
        let capture_reasoning =
            super::responses::requests_encrypted_reasoning(&chat, ProviderKind::Anthropic);
        let streaming = chat.stream;
        // Encoded here so request errors surface before any network call.
        let prepared = request::encode(&chat, &context.route.upstream_id, self.max_tokens)
            .and_then(|payload| {
                TransportRequest::json(
                    Method::POST,
                    Endpoint::new(&["messages"])?,
                    &payload,
                    Some(self.credential()?),
                    Some(if streaming {
                        Accept::EventStream
                    } else {
                        Accept::Json
                    }),
                    self.max_body_bytes,
                    if streaming {
                        STREAM_RESPONSE_BYTES
                    } else {
                        COMPLETE_RESPONSE_BYTES
                    },
                )
            });
        let transport_request = match prepared {
            Ok(request) => request,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let transport = self.transport.clone();
        let label = UpstreamLabel::new(&self.id, ProviderKind::Anthropic.label());
        let public_model = context.route.selector.model_alias;
        Box::pin(async move {
            let mut response = transport.execute(transport_request).await?;
            if response.status != 200 {
                label.status(response.status, &mut response.body).await;
                return Err(status_error(response.status));
            }
            let expected = if streaming {
                ResponseContentType::EventStream
            } else {
                ResponseContentType::Json
            };
            if response.content_type != Some(expected) {
                label.content_type(response.status, response.media_type.as_deref());
                return Err(upstream_failure());
            }
            if streaming {
                return Ok(AdapterOutput::Events(stream::event_stream(
                    response.body,
                    public_model,
                    capture_reasoning,
                    label,
                )));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.body.next().await {
                let chunk = chunk?;
                if body.len().saturating_add(chunk.len()) > COMPLETE_RESPONSE_BYTES {
                    return Err(upstream_failure());
                }
                body.extend_from_slice(&chunk);
            }
            let reply = response::decode(&body, public_model, capture_reasoning)?;
            Ok(AdapterOutput::Complete(Response::Chat(reply)))
        })
    }
}

/// The shared mapping plus 529 (`overloaded_error`) as unavailable.
fn status_error(status: u16) -> GatewayError {
    match status {
        529 => ErrorKind::UpstreamUnavailable.into(),
        _ => super::status_error(status),
    }
}
