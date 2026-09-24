mod request;
mod response;
mod stream;
mod stream_state;
mod stream_wire;
mod validation;

#[cfg(test)]
mod tests;

use std::{fmt, sync::Arc};

use futures_util::StreamExt;
use http::Method;

use crate::adapter::diagnostics::UpstreamLabel;

use crate::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    auth::{SecretResolver, canonical_bearer_token},
    config::{ProviderKind, ValidatedAdapter, ValidatedConfig, ValidatedTimeouts},
    core::{
        Capabilities, ChatContent, ErrorKind, GatewayError, Operation, Request, Response,
        RoutedRequest, TrustZone,
    },
};

use super::transport::{
    Accept, COMPLETE_RESPONSE_BYTES, CredentialHeader, Endpoint, ResponseBody, ResponseContentType,
    STREAM_RESPONSE_BYTES, Transport, TransportRequest,
};

pub struct OpenRouterAdapter {
    id: String,
    capabilities: Capabilities,
    bearer_token: String,
    max_body_bytes: usize,
    max_audio_bytes: usize,
    max_audio_chat_body_bytes: usize,
    route_bindings: Vec<validation::RouteBinding>,
    transport: Arc<Transport>,
}

impl fmt::Debug for OpenRouterAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterAdapter")
            .field("id", &self.id)
            .field("capabilities", &self.capabilities)
            .field("trust_zone", &TrustZone::External)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("route_count", &self.route_bindings.len())
            .finish_non_exhaustive()
    }
}

impl OpenRouterAdapter {
    pub fn from_config(
        config: &ValidatedConfig,
        adapter_id: &str,
        route_id: &str,
        resolver: &impl SecretResolver,
    ) -> Result<Self, GatewayError> {
        Self::build_from_config(config, adapter_id, route_id, resolver, Transport::new)
    }

    fn build_from_config(
        config: &ValidatedConfig,
        adapter_id: &str,
        route_id: &str,
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
        let selected_route = config
            .routes()
            .iter()
            .find(|route| route.identity().route_id == route_id)
            .filter(|route| route.adapter_id() == adapter_id)
            .ok_or_else(internal_error)?;
        let configured = adapter.capabilities();
        if adapter.kind() != ProviderKind::Openrouter
            || adapter.trust_zone() != TrustZone::External
            || adapter.secret_ref().is_none()
            || configured.operations.is_empty()
            || configured.function_tools
            || configured.audio_function_tools
            || (configured.input_audio && !configured.operations.contains(&Operation::Chat))
            || (configured.audio_streaming_chat
                && !(configured.input_audio && configured.streaming_chat))
        {
            return Err(internal_error());
        }

        let route_bindings = config
            .routes()
            .iter()
            .filter(|route| route.adapter_id() == adapter_id)
            .map(|route| validation::bind_route(adapter_id, route, configured))
            .collect::<Result<Vec<_>, _>>()?;
        if route_bindings.is_empty()
            || !route_bindings
                .iter()
                .any(|binding| binding.identity().route_id == selected_route.identity().route_id)
        {
            return Err(internal_error());
        }

        let secret_ref = adapter.secret_ref().ok_or_else(internal_error)?;
        let secret = resolver.resolve(secret_ref).map_err(|_| internal_error())?;
        let bearer_token =
            canonical_bearer_token(secret_ref, secret).map_err(|_| internal_error())?;
        let max_body_bytes =
            usize::try_from(config.limits().max_body_bytes()).map_err(|_| internal_error())?;
        let max_audio_bytes =
            usize::try_from(config.limits().max_audio_bytes()).map_err(|_| internal_error())?;
        let max_audio_chat_body_bytes =
            usize::try_from(config.limits().max_audio_chat_body_bytes())
                .map_err(|_| internal_error())?;
        if max_body_bytes == 0 || max_audio_bytes == 0 || max_audio_chat_body_bytes == 0 {
            return Err(internal_error());
        }

        let mut capabilities = Capabilities::new(configured.operations.iter().copied());
        capabilities.streaming_chat = configured.streaming_chat;
        capabilities.input_audio = configured.input_audio;
        capabilities.audio_streaming_chat = configured.audio_streaming_chat;
        capabilities.structured_output = configured.structured_output;
        capabilities.sampling_controls = configured.sampling_controls;
        capabilities.reasoning_control = configured.reasoning_control;

        Ok(Self {
            id: adapter.id().to_owned(),
            capabilities,
            bearer_token,
            max_body_bytes,
            max_audio_bytes,
            max_audio_chat_body_bytes,
            route_bindings,
            transport: Arc::new(make_transport(adapter, config.timeouts())?),
        })
    }

    #[cfg(test)]
    fn from_config_with_tls_fixture(
        config: &ValidatedConfig,
        adapter_id: &str,
        route_id: &str,
        resolver: &impl SecretResolver,
        certificate: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
        address: std::net::SocketAddr,
    ) -> Result<Self, GatewayError> {
        Self::build_from_config(
            config,
            adapter_id,
            route_id,
            resolver,
            move |adapter, timeouts| {
                Transport::new_with_test_root(adapter, timeouts, certificate, address)
            },
        )
    }

    pub fn configured_id(&self) -> &str {
        &self.id
    }

    async fn post_json(
        transport: Arc<Transport>,
        path: &'static [&'static str],
        payload: &impl serde::Serialize,
        bearer_token: String,
        request_budget: usize,
        label: UpstreamLabel,
    ) -> Result<Vec<u8>, GatewayError> {
        let request = TransportRequest::json(
            Method::POST,
            Endpoint::new(path)?,
            payload,
            Some(CredentialHeader::authorization(bearer_token)),
            Some(Accept::Json),
            request_budget,
            COMPLETE_RESPONSE_BYTES,
        )?;
        let mut response = transport.execute(request).await?;
        if response.status != 200 {
            label.status(response.status, &mut response.body).await;
            return Err(status_error(response.status));
        }
        if !matches!(response.content_type, Some(ResponseContentType::Json)) {
            label.content_type(response.status, response.media_type.as_deref());
            return Err(upstream_failure());
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.body.next().await {
            let chunk = chunk?;
            let Some(next) = body.len().checked_add(chunk.len()) else {
                return Err(upstream_failure());
            };
            if next > COMPLETE_RESPONSE_BYTES {
                return Err(upstream_failure());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn post_stream(
        transport: Arc<Transport>,
        payload: request::ChatPayload,
        bearer_token: String,
        request_budget: usize,
        label: UpstreamLabel,
    ) -> Result<ResponseBody, GatewayError> {
        let request = TransportRequest::json(
            Method::POST,
            Endpoint::new(&["chat", "completions"])?,
            &payload,
            Some(CredentialHeader::authorization(bearer_token)),
            Some(Accept::EventStream),
            request_budget,
            STREAM_RESPONSE_BYTES,
        )?;
        let mut response = transport.execute(request).await?;
        if response.status != 200 {
            label.status(response.status, &mut response.body).await;
            return Err(status_error(response.status));
        }
        if !matches!(
            response.content_type,
            Some(ResponseContentType::EventStream)
        ) {
            label.content_type(response.status, response.media_type.as_deref());
            return Err(upstream_failure());
        }
        Ok(response.body)
    }
}

impl Adapter for OpenRouterAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
        if let Err(error) = validation::validate(
            &routed,
            &self.capabilities,
            &self.route_bindings,
            self.max_audio_bytes,
        ) {
            return Box::pin(async move { Err(error) });
        }

        let (context, request) = routed.into_parts();
        let transport = self.transport.clone();
        let bearer_token = format!("Bearer {}", self.bearer_token);
        let label = UpstreamLabel::new(&self.id, "openrouter");
        let chat = match request {
            Request::Chat(chat) => chat,
            Request::Transcription(transcription) => {
                let payload =
                    match request::encode_transcription(&transcription, &context.route.upstream_id)
                    {
                        Ok(payload) => payload,
                        Err(error) => return Box::pin(async move { Err(error) }),
                    };
                let request_budget = self.max_audio_chat_body_bytes;
                return Box::pin(async move {
                    let body = Self::post_json(
                        transport,
                        &["audio", "transcriptions"],
                        &payload,
                        bearer_token,
                        request_budget,
                        label,
                    )
                    .await?;
                    let response = response::decode_transcription(&body)?;
                    Ok(AdapterOutput::Complete(Response::Transcription(response)))
                });
            }
        };
        let public_model = context.route.selector.model_alias;
        let streaming = chat.stream;
        let has_audio = chat.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, ChatContent::InputAudio { .. }))
        });
        let request_budget = if has_audio {
            self.max_audio_chat_body_bytes
        } else {
            self.max_body_bytes
        };
        let payload = match request::encode(&chat, &context.route.upstream_id, streaming) {
            Ok(payload) => payload,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        Box::pin(async move {
            if streaming {
                let body =
                    Self::post_stream(transport, payload, bearer_token, request_budget, label)
                        .await?;
                Ok(AdapterOutput::Events(stream::event_stream(
                    body,
                    public_model,
                )))
            } else {
                let body = Self::post_json(
                    transport,
                    &["chat", "completions"],
                    &payload,
                    bearer_token,
                    request_budget,
                    label,
                )
                .await?;
                let response = response::decode(&body, public_model)?;
                Ok(AdapterOutput::Complete(Response::Chat(response)))
            }
        })
    }
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

fn internal_error() -> GatewayError {
    GatewayError {
        kind: ErrorKind::Internal,
    }
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
