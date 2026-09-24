mod request;
mod response;
mod sse;
mod stream;
mod stream_state;
mod stream_wire;
mod validation;

use std::{fmt, sync::Arc};

use futures_util::StreamExt;
use http::Method;

use crate::adapter::diagnostics::UpstreamLabel;

use crate::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    config::{
        ProviderKind, ValidatedAdapter, ValidatedConfig, ValidatedLimits, ValidatedRoute,
        ValidatedTimeouts,
    },
    core::{
        Capabilities, ErrorKind, GatewayError, Operation, Request, Response, RoutedRequest,
        TrustZone,
    },
};

use super::transport::{
    Accept, COMPLETE_RESPONSE_BYTES, Endpoint, ResponseContentType, STREAM_RESPONSE_BYTES,
    Transport, TransportRequest,
};

pub struct OllamaAdapter {
    id: String,
    capabilities: Capabilities,
    trust_zone: TrustZone,
    max_body_bytes: usize,
    transport: Arc<Transport>,
}

impl fmt::Debug for OllamaAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OllamaAdapter")
            .field("id", &self.id)
            .field("capabilities", &self.capabilities)
            .field("trust_zone", &self.trust_zone)
            .field("max_body_bytes", &self.max_body_bytes)
            .finish_non_exhaustive()
    }
}

impl OllamaAdapter {
    pub fn new(
        adapter: &ValidatedAdapter,
        timeouts: &ValidatedTimeouts,
        limits: &ValidatedLimits,
    ) -> Result<Self, GatewayError> {
        if adapter.kind() != ProviderKind::Ollama
            || !matches!(
                adapter.trust_zone(),
                TrustZone::Local | TrustZone::PrivateNetwork
            )
            || adapter.secret_ref().is_some()
        {
            return Err(internal_error());
        }

        let configured = adapter.capabilities();
        if !configured.operations.contains(&Operation::Chat)
            || configured
                .operations
                .iter()
                .any(|operation| *operation != Operation::Chat)
            || configured.input_audio
            || configured.audio_streaming_chat
            || configured.audio_function_tools
        {
            return Err(internal_error());
        }

        let max_body_bytes =
            usize::try_from(limits.max_body_bytes()).map_err(|_| internal_error())?;
        if max_body_bytes == 0 {
            return Err(internal_error());
        }
        let transport = Transport::new(adapter, timeouts)?;

        Ok(Self {
            id: adapter.id().to_owned(),
            capabilities: Capabilities {
                operations: [Operation::Chat].into_iter().collect(),
                streaming_chat: configured.streaming_chat,
                function_tools: configured.function_tools,
                input_audio: false,
                audio_streaming_chat: false,
                audio_function_tools: false,
                structured_output: configured.structured_output,
                sampling_controls: configured.sampling_controls,
                reasoning_control: configured.reasoning_control,
            },
            trust_zone: adapter.trust_zone(),
            max_body_bytes,
            transport: Arc::new(transport),
        })
    }

    pub fn new_for_route(
        adapter: &ValidatedAdapter,
        route: &ValidatedRoute,
        timeouts: &ValidatedTimeouts,
        limits: &ValidatedLimits,
    ) -> Result<Self, GatewayError> {
        if route.adapter_id() != adapter.id()
            || route.identity().selector.operation != Operation::Chat
            || (route.requires_streaming_chat() && !adapter.capabilities().streaming_chat)
            || (route.requires_function_tools() && !adapter.capabilities().function_tools)
            || route.allows_input_audio()
            || route.allows_audio_streaming_chat()
            || route.allows_audio_function_tools()
        {
            return Err(internal_error());
        }
        Self::new(adapter, timeouts, limits)
    }

    pub fn from_config(
        config: &ValidatedConfig,
        adapter_id: &str,
        route_id: &str,
    ) -> Result<Self, GatewayError> {
        let adapter = config
            .adapters()
            .iter()
            .find(|adapter| adapter.id() == adapter_id)
            .ok_or_else(internal_error)?;
        let route = config
            .routes()
            .iter()
            .find(|route| route.identity().route_id == route_id)
            .ok_or_else(internal_error)?;
        Self::new_for_route(adapter, route, config.timeouts(), config.limits())
    }

    pub fn configured_id(&self) -> &str {
        &self.id
    }

    async fn execute_nonstream(
        transport: Arc<Transport>,
        payload: request::ChatPayload,
        public_model: crate::core::ModelAlias,
        max_body_bytes: usize,
        label: UpstreamLabel,
    ) -> Result<AdapterOutput, GatewayError> {
        let endpoint = Endpoint::new(&["chat", "completions"])?;
        let request = TransportRequest::json(
            Method::POST,
            endpoint,
            &payload,
            None,
            Some(Accept::Json),
            max_body_bytes,
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
        let response = response::decode(&body, public_model)?;
        Ok(AdapterOutput::Complete(Response::Chat(response)))
    }

    async fn execute_stream(
        transport: Arc<Transport>,
        payload: request::ChatPayload,
        public_model: crate::core::ModelAlias,
        max_body_bytes: usize,
        label: UpstreamLabel,
    ) -> Result<AdapterOutput, GatewayError> {
        let endpoint = Endpoint::new(&["chat", "completions"])?;
        let request = TransportRequest::json(
            Method::POST,
            endpoint,
            &payload,
            None,
            Some(Accept::EventStream),
            max_body_bytes,
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
        Ok(AdapterOutput::Events(Box::pin(stream::OllamaStream::new(
            response.body,
            public_model,
        ))))
    }
}

impl Adapter for OllamaAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
        if let Err(error) = validation::validate(&routed, &self.capabilities, self.trust_zone) {
            return Box::pin(async move { Err(error) });
        }

        let (context, request) = routed.into_parts();
        let Request::Chat(chat) = request else {
            return Box::pin(async { Err(unsupported_operation()) });
        };
        let public_model = context.route.selector.model_alias.clone();
        let payload = match request::encode(&chat, &context.route.upstream_id, chat.stream) {
            Ok(payload) => payload,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let transport = self.transport.clone();
        let max_body_bytes = self.max_body_bytes;
        let label = UpstreamLabel::new(&self.id, "ollama");
        if chat.stream {
            Box::pin(async move {
                Self::execute_stream(transport, payload, public_model, max_body_bytes, label).await
            })
        } else {
            Box::pin(async move {
                Self::execute_nonstream(transport, payload, public_model, max_body_bytes, label)
                    .await
            })
        }
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

fn unsupported_operation() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UnsupportedOperation,
    }
}
