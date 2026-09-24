mod request;
mod response;
mod validation;

use std::{fmt, sync::Arc};

use bytes::Bytes;
use futures_util::StreamExt;
use http::Method;

use crate::adapter::diagnostics::UpstreamLabel;

use crate::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    config::{
        ProviderKind, ValidatedAdapter, ValidatedConfig, ValidatedLimits, ValidatedRoute,
        ValidatedTimeouts, VllmTranscriptionMode,
    },
    core::{
        Capabilities, ErrorKind, GatewayError, Operation, Request, Response, RoutedRequest,
        TrustZone,
    },
};

use super::transport::{
    Accept, COMPLETE_RESPONSE_BYTES, Endpoint, MultipartFile, MultipartRequest,
    ResponseContentType, Transport, TransportRequest,
};

const NATIVE_ASR_MULTIPART_OVERHEAD_BYTES: usize = 8 * 1024;

pub struct VllmAdapter {
    id: String,
    capabilities: Capabilities,
    trust_zone: TrustZone,
    max_body_bytes: usize,
    max_audio_bytes: usize,
    max_audio_chat_body_bytes: usize,
    transcription_mode: Option<VllmTranscriptionMode>,
    route_bindings: Option<Vec<validation::RouteBinding>>,
    transport: Arc<Transport>,
}

impl fmt::Debug for VllmAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VllmAdapter")
            .field("id", &self.id)
            .field("capabilities", &self.capabilities)
            .field("trust_zone", &self.trust_zone)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_audio_chat_body_bytes", &self.max_audio_chat_body_bytes)
            .field("route_bound", &self.route_bindings.is_some())
            .finish_non_exhaustive()
    }
}

impl VllmAdapter {
    pub fn new(
        adapter: &ValidatedAdapter,
        timeouts: &ValidatedTimeouts,
        limits: &ValidatedLimits,
    ) -> Result<Self, GatewayError> {
        if adapter.capabilities().input_audio
            || adapter
                .capabilities()
                .operations
                .contains(&Operation::Transcription)
            || adapter.transcription_mode().is_some()
        {
            return Err(internal_error());
        }
        Self::build(adapter, timeouts, limits)
    }

    fn build(
        adapter: &ValidatedAdapter,
        timeouts: &ValidatedTimeouts,
        limits: &ValidatedLimits,
    ) -> Result<Self, GatewayError> {
        let configured = adapter.capabilities();
        let supports_chat = configured.operations.contains(&Operation::Chat);
        let supports_transcription = configured.operations.contains(&Operation::Transcription);
        let transcription_mode = adapter.transcription_mode();
        let supported_operations = match (configured.operations.len(), transcription_mode) {
            (1, None) => supports_chat && !supports_transcription,
            (2, Some(VllmTranscriptionMode::AudioChat)) => {
                supports_chat && supports_transcription && configured.input_audio
            }
            (1, Some(VllmTranscriptionMode::NativeAsr)) => {
                supports_transcription && !supports_chat && !configured.input_audio
            }
            _ => false,
        };
        if adapter.kind() != ProviderKind::Vllm
            || !matches!(
                adapter.trust_zone(),
                TrustZone::Local | TrustZone::PrivateNetwork
            )
            || adapter.secret_ref().is_some()
            || !supported_operations
            || configured.streaming_chat
            || configured.function_tools
            || configured.audio_streaming_chat
            || configured.audio_function_tools
            || configured.reasoning_control
        {
            return Err(internal_error());
        }

        let max_body_bytes =
            usize::try_from(limits.max_body_bytes()).map_err(|_| internal_error())?;
        let max_audio_bytes =
            usize::try_from(limits.max_audio_bytes()).map_err(|_| internal_error())?;
        let max_audio_chat_body_bytes =
            usize::try_from(limits.max_audio_chat_body_bytes()).map_err(|_| internal_error())?;
        if max_body_bytes == 0 || max_audio_bytes == 0 || max_audio_chat_body_bytes == 0 {
            return Err(internal_error());
        }

        Ok(Self {
            id: adapter.id().to_owned(),
            capabilities: configured.clone(),
            trust_zone: adapter.trust_zone(),
            max_body_bytes,
            max_audio_bytes,
            max_audio_chat_body_bytes,
            transcription_mode,
            route_bindings: None,
            transport: Arc::new(Transport::new(adapter, timeouts)?),
        })
    }

    pub fn new_for_route(
        adapter: &ValidatedAdapter,
        route: &ValidatedRoute,
        timeouts: &ValidatedTimeouts,
        limits: &ValidatedLimits,
    ) -> Result<Self, GatewayError> {
        let mut adapter = Self::build(adapter, timeouts, limits)?;
        let route_binding = validation::bind_route(&adapter.id, &adapter.capabilities, route)?;
        adapter.route_bindings = Some(vec![route_binding]);
        Ok(adapter)
    }

    pub fn from_config(
        config: &ValidatedConfig,
        adapter_id: &str,
        route_id: &str,
    ) -> Result<Self, GatewayError> {
        let configured_adapter = config
            .adapters()
            .iter()
            .find(|adapter| adapter.id() == adapter_id)
            .ok_or_else(internal_error)?;
        let selected_route = config
            .routes()
            .iter()
            .find(|route| route.identity().route_id == route_id)
            .ok_or_else(internal_error)?;
        if selected_route.adapter_id() != configured_adapter.id() {
            return Err(internal_error());
        }

        let mut adapter = Self::build(configured_adapter, config.timeouts(), config.limits())?;
        let route_bindings = config
            .routes()
            .iter()
            .filter(|route| route.adapter_id() == adapter_id)
            .map(|route| validation::bind_route(adapter_id, &adapter.capabilities, route))
            .collect::<Result<Vec<_>, _>>()?;
        if route_bindings.is_empty() {
            return Err(internal_error());
        }
        adapter.route_bindings = Some(route_bindings);
        Ok(adapter)
    }

    pub fn configured_id(&self) -> &str {
        &self.id
    }

    async fn post_completion(
        transport: Arc<Transport>,
        payload: request::ChatPayload,
        request_budget: usize,
        label: UpstreamLabel,
    ) -> Result<Vec<u8>, GatewayError> {
        let endpoint = Endpoint::new(&["chat", "completions"])?;
        let request = TransportRequest::json(
            Method::POST,
            endpoint,
            &payload,
            None,
            Some(Accept::Json),
            request_budget,
            COMPLETE_RESPONSE_BYTES,
        )?;
        Self::post_json(transport, request, label).await
    }

    fn native_transcription_request(
        transcription: &crate::core::TranscriptionRequest,
        upstream_model: &str,
        max_audio_bytes: usize,
    ) -> Result<TransportRequest, GatewayError> {
        let mut fields = vec![("model".to_owned(), upstream_model.to_owned())];
        if let Some(language) = &transcription.language {
            fields.push(("language".to_owned(), language.clone()));
        }
        if let Some(prompt) = &transcription.prompt {
            fields.push(("prompt".to_owned(), prompt.clone()));
        }
        fields.push(("response_format".to_owned(), "json".to_owned()));
        let file = MultipartFile::new(
            "file".to_owned(),
            transcription.file.file_name().to_owned(),
            transcription.file.media_type().to_owned(),
            Bytes::copy_from_slice(transcription.file.bytes()),
        )?;
        let multipart = MultipartRequest::new(fields, file)?;
        let request_budget = max_audio_bytes
            .checked_add(NATIVE_ASR_MULTIPART_OVERHEAD_BYTES)
            .ok_or_else(internal_error)?;
        TransportRequest::multipart(
            Method::POST,
            Endpoint::new(&["audio", "transcriptions"])?,
            multipart,
            None,
            Some(Accept::Json),
            request_budget,
            COMPLETE_RESPONSE_BYTES,
        )
    }

    async fn post_json(
        transport: Arc<Transport>,
        request: TransportRequest,
        label: UpstreamLabel,
    ) -> Result<Vec<u8>, GatewayError> {
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
}

impl Adapter for VllmAdapter {
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
            self.trust_zone,
            self.transcription_mode,
            self.max_audio_bytes,
            self.route_bindings.as_deref(),
        ) {
            return Box::pin(async move { Err(error) });
        }

        let (context, request) = routed.into_parts();
        let transport = self.transport.clone();
        let label = UpstreamLabel::new(&self.id, "vllm");
        match request {
            Request::Chat(chat) => {
                let public_model = context.route.selector.model_alias.clone();
                let has_audio = chat.messages.iter().any(|message| {
                    message.content.iter().any(|content| {
                        matches!(content, crate::core::ChatContent::InputAudio { .. })
                    })
                });
                let request_budget = if has_audio {
                    self.max_audio_chat_body_bytes
                } else {
                    self.max_body_bytes
                };
                let payload = match request::encode(&chat, &context.route.upstream_id) {
                    Ok(payload) => payload,
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
                Box::pin(async move {
                    let body =
                        Self::post_completion(transport, payload, request_budget, label).await?;
                    let response = response::decode(&body, public_model)?;
                    Ok(AdapterOutput::Complete(Response::Chat(response)))
                })
            }
            Request::Transcription(transcription) => match self.transcription_mode {
                Some(VllmTranscriptionMode::AudioChat) => {
                    let payload = match request::encode_transcription(
                        &transcription,
                        &context.route.upstream_id,
                        self.max_audio_bytes,
                    ) {
                        Ok(payload) => payload,
                        Err(error) => return Box::pin(async move { Err(error) }),
                    };
                    let request_budget = self.max_audio_chat_body_bytes;
                    Box::pin(async move {
                        let body = Self::post_completion(transport, payload, request_budget, label)
                            .await?;
                        let response = response::decode_transcription(&body)?;
                        Ok(AdapterOutput::Complete(Response::Transcription(response)))
                    })
                }
                Some(VllmTranscriptionMode::NativeAsr) => {
                    let request = match Self::native_transcription_request(
                        &transcription,
                        &context.route.upstream_id,
                        self.max_audio_bytes,
                    ) {
                        Ok(request) => request,
                        Err(error) => return Box::pin(async move { Err(error) }),
                    };
                    Box::pin(async move {
                        let body = Self::post_json(transport, request, label).await?;
                        let response = response::decode_native_transcription(&body)?;
                        Ok(AdapterOutput::Complete(Response::Transcription(response)))
                    })
                }
                None => Box::pin(async { Err(internal_error()) }),
            },
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
