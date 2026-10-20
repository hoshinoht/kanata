mod apple_fm;
mod embeddings;
mod request;
mod response;
mod stream;
mod validation;

use crate::adapter::{internal_error, invalid_request, status_error, unsupported_operation};
use std::{fmt, sync::Arc};

use crate::adapter::diagnostics::UpstreamLabel;

use crate::{
    adapter::{Adapter, AdapterFuture, AdapterOutput, chat_completions},
    config::{
        ProviderKind, ValidatedAdapter, ValidatedConfig, ValidatedLimits, ValidatedRoute,
        ValidatedTimeouts,
    },
    core::{
        Capabilities, GatewayError, ModelAlias, Operation, Request, Response, RoutedRequest,
        TrustZone,
    },
};

use super::transport::Transport;

pub struct OllamaAdapter {
    id: String,
    kind: ProviderKind,
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
        if !matches!(adapter.kind(), ProviderKind::Ollama | ProviderKind::AppleFm)
            || !matches!(
                adapter.trust_zone(),
                TrustZone::Local | TrustZone::PrivateNetwork
            )
            || adapter.secret_ref().is_some()
        {
            return Err(internal_error());
        }

        let configured = adapter.capabilities();
        if configured.operations.is_empty()
            || configured
                .operations
                .iter()
                .any(|operation| !matches!(operation, Operation::Chat | Operation::Embeddings))
            || (adapter.kind() == ProviderKind::AppleFm
                && configured.operations.contains(&Operation::Embeddings))
            || (adapter.kind() == ProviderKind::AppleFm && configured.input_images)
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
            kind: adapter.kind(),
            capabilities: Capabilities {
                operations: configured.operations.clone(),
                streaming_chat: configured.streaming_chat,
                function_tools: configured.function_tools,
                input_audio: false,
                input_images: configured.input_images,
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
            || !adapter
                .capabilities()
                .operations
                .contains(&route.identity().selector.operation)
            || (route.requires_streaming_chat() && !adapter.capabilities().streaming_chat)
            || (route.requires_function_tools() && !adapter.capabilities().function_tools)
            || (route.allows_input_images() && !adapter.capabilities().input_images)
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

    async fn execute_chat(
        transport: Arc<Transport>,
        payload: request::ChatPayload,
        public_model: ModelAlias,
        max_body_bytes: usize,
        label: UpstreamLabel,
        apple_fm: bool,
        streaming: bool,
    ) -> Result<AdapterOutput, GatewayError> {
        let request = chat_completions::chat_request(&payload, None, streaming, max_body_bytes)?;
        let mut upstream = transport.execute(request).await?;
        if upstream.status != 200 && apple_fm {
            let error = label
                .status_error(upstream.status, &mut upstream.body)
                .await;
            return match apple_fm::classify(upstream.status, &error) {
                Some(apple_fm::Failure::Guardrail) if streaming => {
                    Ok(AdapterOutput::Events(Box::pin(futures_util::stream::iter(
                        apple_fm::filtered_events(public_model),
                    ))))
                }
                Some(apple_fm::Failure::Guardrail) => Ok(AdapterOutput::Complete(Response::Chat(
                    apple_fm::filtered_reply(public_model),
                ))),
                Some(apple_fm::Failure::ContextOverflow) => Err(invalid_request()),
                None => Err(status_error(upstream.status)),
            };
        }
        if streaming {
            let body = chat_completions::event_body(upstream, &label).await?;
            return Ok(AdapterOutput::Events(stream::event_stream(
                body,
                public_model,
                apple_fm,
            )));
        }
        let body = chat_completions::json_body(upstream, &label).await?;
        let response = response::decode(&body, public_model)?;
        Ok(AdapterOutput::Complete(Response::Chat(response)))
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
        if let Request::Embeddings(request) = request {
            let transport = self.transport.clone();
            let max_body_bytes = self.max_body_bytes;
            let label = UpstreamLabel::new(&self.id, self.kind.label());
            return Box::pin(embeddings::execute(
                transport,
                request,
                context.route.upstream_id,
                max_body_bytes,
                label,
            ));
        }
        let Request::Chat(chat) = request else {
            return Box::pin(async { Err(unsupported_operation()) });
        };
        let public_model = context.route.selector.model_alias.clone();
        let apple_fm = self.kind == ProviderKind::AppleFm;
        let payload = match request::encode(
            &chat,
            &context.route.upstream_id,
            chat.stream,
            apple_fm,
            self.kind.replays_reasoning_text(),
        ) {
            Ok(payload) => payload,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let transport = self.transport.clone();
        let max_body_bytes = self.max_body_bytes;
        let label = UpstreamLabel::new(&self.id, self.kind.label());
        let streaming = chat.stream;
        Box::pin(async move {
            Self::execute_chat(
                transport,
                payload,
                public_model,
                max_body_bytes,
                label,
                apple_fm,
                streaming,
            )
            .await
        })
    }
}
