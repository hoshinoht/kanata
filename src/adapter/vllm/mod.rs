mod request;
mod response;
mod stream;
mod validation;

use crate::adapter::internal_error;
use std::{fmt, sync::Arc};

use bytes::Bytes;
use http::Method;

use crate::adapter::diagnostics::UpstreamLabel;

use crate::{
    adapter::{
        Adapter, AdapterFuture, AdapterOutput,
        chat_completions::{self, stream::ChatStream, validation::has_input_audio},
    },
    auth::{SecretResolver, canonical_bearer_token},
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
    Accept, COMPLETE_RESPONSE_BYTES, CredentialHeader, Endpoint, MultipartFile, MultipartRequest,
    Transport, TransportRequest,
};

const NATIVE_ASR_MULTIPART_OVERHEAD_BYTES: usize = 8 * 1024;

pub struct VllmAdapter {
    id: String,
    /// Provider label for diagnostics: `vllm`, or `cliproxy` for CLIProxyAPI on this path.
    provider: &'static str,
    /// Whether assistant reasoning text reaches the upstream (vLLM yes, CLIProxyAPI no).
    replays_reasoning_text: bool,
    capabilities: Capabilities,
    trust_zone: TrustZone,
    max_body_bytes: usize,
    max_audio_bytes: usize,
    max_audio_chat_body_bytes: usize,
    transcription_mode: Option<VllmTranscriptionMode>,
    route_bindings: Option<Vec<validation::RouteBinding>>,
    bearer_token: Option<String>,
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
            .field("authenticated", &self.bearer_token.is_some())
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
        Self::build(adapter, timeouts, limits, None)
    }

    fn build(
        adapter: &ValidatedAdapter,
        timeouts: &ValidatedTimeouts,
        limits: &ValidatedLimits,
        bearer_token: Option<String>,
    ) -> Result<Self, GatewayError> {
        let configured = adapter.capabilities();
        let supports_chat = configured.operations.contains(&Operation::Chat);
        let supports_transcription = configured.operations.contains(&Operation::Transcription);
        let supports_embeddings = configured.operations.contains(&Operation::Embeddings);
        let transcription_mode = adapter.transcription_mode();
        // Embeddings may join any otherwise valid chat/transcription set, or stand alone.
        let other_operations = configured.operations.len() - usize::from(supports_embeddings);
        let supported_operations = match (other_operations, transcription_mode) {
            (0, None) => supports_embeddings,
            (1, None) => supports_chat && !supports_transcription,
            (2, Some(VllmTranscriptionMode::AudioChat)) => {
                supports_chat && supports_transcription && configured.input_audio
            }
            (1, Some(VllmTranscriptionMode::NativeAsr)) => {
                supports_transcription && !supports_chat && !configured.input_audio
            }
            _ => false,
        };
        let zone_allowed = match adapter.trust_zone() {
            TrustZone::Local | TrustZone::PrivateNetwork => true,
            TrustZone::External => bearer_token.is_some(),
        };
        // Streaming and tools are chat features; audio variants also need input audio.
        let chat_features_valid = (supports_chat
            || !(configured.streaming_chat || configured.function_tools))
            && (!configured.audio_streaming_chat
                || (configured.input_audio && configured.streaming_chat))
            && (!configured.audio_function_tools
                || (configured.input_audio && configured.function_tools));
        // CLIProxyAPI reuses this chat path only: chat, always with its client key.
        let kind_allowed = match adapter.kind() {
            ProviderKind::Vllm => true,
            ProviderKind::Cliproxy => {
                bearer_token.is_some() && !supports_transcription && !supports_embeddings
            }
            _ => false,
        };
        if !kind_allowed
            || !zone_allowed
            || adapter.secret_ref().is_some() != bearer_token.is_some()
            || !supported_operations
            || !chat_features_valid
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
            provider: adapter.kind().label(),
            replays_reasoning_text: adapter.kind().replays_reasoning_text(),
            capabilities: configured.clone(),
            trust_zone: adapter.trust_zone(),
            max_body_bytes,
            max_audio_bytes,
            max_audio_chat_body_bytes,
            transcription_mode,
            route_bindings: None,
            bearer_token,
            transport: Arc::new(Transport::new(adapter, timeouts)?),
        })
    }

    pub fn new_for_route(
        adapter: &ValidatedAdapter,
        route: &ValidatedRoute,
        timeouts: &ValidatedTimeouts,
        limits: &ValidatedLimits,
    ) -> Result<Self, GatewayError> {
        let mut adapter = Self::build(adapter, timeouts, limits, None)?;
        let route_binding = validation::bind_route(&adapter.id, &adapter.capabilities, route)?;
        adapter.route_bindings = Some(vec![route_binding]);
        Ok(adapter)
    }

    pub fn from_config(
        config: &ValidatedConfig,
        adapter_id: &str,
        route_id: &str,
    ) -> Result<Self, GatewayError> {
        Self::build_from_config(config, adapter_id, route_id, None)
    }

    /// Builds from config, resolving the adapter's `secret_ref` into a bearer token.
    pub fn from_config_with_secrets(
        config: &ValidatedConfig,
        adapter_id: &str,
        route_id: &str,
        resolver: &impl SecretResolver,
    ) -> Result<Self, GatewayError> {
        let configured_adapter = config
            .adapters()
            .iter()
            .find(|adapter| adapter.id() == adapter_id)
            .ok_or_else(internal_error)?;
        let bearer_token = match configured_adapter.secret_ref() {
            Some(secret_ref) => {
                let secret = resolver.resolve(secret_ref).map_err(|_| internal_error())?;
                Some(canonical_bearer_token(secret_ref, secret).map_err(|_| internal_error())?)
            }
            None => None,
        };
        Self::build_from_config(config, adapter_id, route_id, bearer_token)
    }

    fn build_from_config(
        config: &ValidatedConfig,
        adapter_id: &str,
        route_id: &str,
        bearer_token: Option<String>,
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

        let mut adapter = Self::build(
            configured_adapter,
            config.timeouts(),
            config.limits(),
            bearer_token,
        )?;
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

    fn credential(&self) -> Option<CredentialHeader> {
        self.bearer_token
            .as_ref()
            .map(|token| CredentialHeader::authorization(format!("Bearer {token}")))
    }

    fn enable_thinking(&self, route_id: &str) -> Option<bool> {
        self.route_bindings
            .as_deref()?
            .iter()
            .find(|binding| binding.route_id() == route_id)?
            .enable_thinking()
    }

    fn native_transcription_request(
        transcription: &crate::core::TranscriptionRequest,
        upstream_model: &str,
        max_audio_bytes: usize,
        credential: Option<CredentialHeader>,
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
            credential,
            Some(Accept::Json),
            request_budget,
            COMPLETE_RESPONSE_BYTES,
        )
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
        let label = UpstreamLabel::new(&self.id, self.provider);
        let credential = self.credential();
        let enable_thinking = self.enable_thinking(&context.route.route_id);
        match request {
            Request::Speech(_) => {
                Box::pin(async { Err(GatewayError::from(ErrorKind::UnsupportedOperation)) })
            }
            Request::Embeddings(embedding) => Box::pin(chat_completions::embeddings::execute::<
                chat_completions::embeddings::List,
                _,
            >(
                transport,
                embedding,
                context.route.upstream_id,
                (),
                credential,
                self.max_body_bytes,
                label,
            )),
            Request::Chat(chat) => {
                let public_model = context.route.selector.model_alias.clone();
                let request_budget = if has_input_audio(&chat) {
                    self.max_audio_chat_body_bytes
                } else {
                    self.max_body_bytes
                };
                let payload = match request::encode(
                    &chat,
                    &context.route.upstream_id,
                    enable_thinking,
                    self.replays_reasoning_text,
                ) {
                    Ok(payload) => payload,
                    Err(error) => return Box::pin(async move { Err(error) }),
                };
                let streaming = chat.stream;
                Box::pin(async move {
                    let request = chat_completions::chat_request(
                        &payload,
                        credential,
                        streaming,
                        request_budget,
                    )?;
                    if streaming {
                        let body =
                            chat_completions::post_events(&transport, request, &label).await?;
                        return Ok(AdapterOutput::Events(Box::pin(
                            ChatStream::<stream::Vllm>::new(body, public_model),
                        )));
                    }
                    let body = chat_completions::post_json(&transport, request, &label).await?;
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
                        enable_thinking,
                    ) {
                        Ok(payload) => payload,
                        Err(error) => return Box::pin(async move { Err(error) }),
                    };
                    let request_budget = self.max_audio_chat_body_bytes;
                    Box::pin(async move {
                        let request = chat_completions::chat_request(
                            &payload,
                            credential,
                            false,
                            request_budget,
                        )?;
                        let body = chat_completions::post_json(&transport, request, &label).await?;
                        let response = response::decode_transcription(&body)?;
                        Ok(AdapterOutput::Complete(Response::Transcription(response)))
                    })
                }
                Some(VllmTranscriptionMode::NativeAsr) => {
                    let request = match Self::native_transcription_request(
                        &transcription,
                        &context.route.upstream_id,
                        self.max_audio_bytes,
                        credential,
                    ) {
                        Ok(request) => request,
                        Err(error) => return Box::pin(async move { Err(error) }),
                    };
                    Box::pin(async move {
                        let body = chat_completions::post_json(&transport, request, &label).await?;
                        let response = response::decode_native_transcription(&body)?;
                        Ok(AdapterOutput::Complete(Response::Transcription(response)))
                    })
                }
                None => Box::pin(async { Err(internal_error()) }),
            },
        }
    }
}
