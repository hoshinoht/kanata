use crate::{
    chat_completions::validation::{has_input_audio, uses_tools, validate_messages},
    internal_error, invalid_request, unsupported_operation,
};
use crate::{
    config::ValidatedRoute,
    config::VllmTranscriptionMode,
    core::{Capabilities, GatewayError, Request, RouteIdentity, RoutedRequest, TrustZone},
};

pub(super) struct RouteBinding {
    identity: RouteIdentity,
    allows_input_audio: bool,
    allows_input_images: bool,
    allows_audio_streaming_chat: bool,
    allows_audio_function_tools: bool,
    enable_thinking: Option<bool>,
}

impl RouteBinding {
    pub(super) fn route_id(&self) -> &str {
        &self.identity.route_id
    }

    pub(super) fn enable_thinking(&self) -> Option<bool> {
        self.enable_thinking
    }
}

pub(super) fn bind_route(
    adapter_id: &str,
    capabilities: &Capabilities,
    route: &ValidatedRoute,
) -> Result<RouteBinding, GatewayError> {
    if route.adapter_id() != adapter_id
        || !capabilities
            .operations
            .contains(&route.identity().selector.operation)
        || route.identity().route_id.trim().is_empty()
        || route.identity().upstream_id.trim().is_empty()
        || (route.requires_streaming_chat() && !capabilities.streaming_chat)
        || (route.requires_function_tools() && !capabilities.function_tools)
        || (route.allows_audio_streaming_chat()
            && !(route.allows_input_audio() && capabilities.audio_streaming_chat))
        || (route.allows_audio_function_tools()
            && !(route.allows_input_audio() && capabilities.audio_function_tools))
        || (route.allows_input_images()
            && (route.identity().selector.operation != crate::core::Operation::Chat
                || !capabilities.input_images))
        || (route.allows_input_audio()
            && (route.identity().selector.operation != crate::core::Operation::Chat
                || !capabilities.input_audio))
    {
        return Err(internal_error());
    }
    Ok(RouteBinding {
        identity: route.identity().clone(),
        allows_input_audio: route.allows_input_audio(),
        allows_input_images: route.allows_input_images(),
        allows_audio_streaming_chat: route.allows_audio_streaming_chat(),
        allows_audio_function_tools: route.allows_audio_function_tools(),
        enable_thinking: route.enable_thinking(),
    })
}

pub(super) fn validate(
    routed: &RoutedRequest,
    capabilities: &Capabilities,
    trust_zone: TrustZone,
    transcription_mode: Option<VllmTranscriptionMode>,
    max_audio_bytes: usize,
    route_bindings: Option<&[RouteBinding]>,
) -> Result<(), GatewayError> {
    let request = routed.request();
    capabilities
        .check_request(request)
        .map_err(|_| unsupported_operation())?;

    let context = routed.context();
    if context.check_request(request).is_err()
        || context.trust_zone != trust_zone
        || context.route.route_id.trim().is_empty()
        || context.route.upstream_id.trim().is_empty()
        || !context.extensions.iter().next().is_none()
    {
        return Err(invalid_request());
    }
    let route_binding = if let Some(bindings) = route_bindings {
        let Some(binding) = bindings
            .iter()
            .find(|binding| binding.identity.route_id == context.route.route_id)
        else {
            return Err(invalid_request());
        };
        if binding.identity != context.route {
            return Err(invalid_request());
        }
        Some(binding)
    } else {
        None
    };

    match request {
        Request::Speech(_) => Err(unsupported_operation()),
        Request::Embeddings(embedding) => embedding.validate().map_err(|_| invalid_request()),
        Request::Chat(chat) => validate_chat(
            chat,
            max_audio_bytes,
            capabilities.function_tools,
            route_binding,
        ),
        Request::Transcription(transcription) => {
            if !matches!(
                transcription_mode,
                Some(VllmTranscriptionMode::AudioChat | VllmTranscriptionMode::NativeAsr)
            ) {
                return Err(unsupported_operation());
            }
            if transcription_mode == Some(VllmTranscriptionMode::NativeAsr)
                && route_binding.is_none()
            {
                return Err(invalid_request());
            }
            validate_transcription(transcription, max_audio_bytes, transcription_mode)
        }
    }
}

fn validate_chat(
    chat: &crate::core::ChatRequest,
    max_audio_bytes: usize,
    function_tools: bool,
    route_binding: Option<&RouteBinding>,
) -> Result<(), GatewayError> {
    if chat.model.0.is_empty()
        || chat.messages.is_empty()
        || chat.extensions.iter().next().is_some()
    {
        return Err(invalid_request());
    }

    let has_audio = has_input_audio(chat);
    let uses_tools = uses_tools(chat);
    // Tool history needs tool support even when the request declares no tools.
    if uses_tools && !function_tools {
        return Err(unsupported_operation());
    }
    // An effort and the chat-template thinking switch are alternatives; never send both.
    if chat.options.reasoning_effort.is_some()
        && (chat.options.enable_thinking.is_some()
            || route_binding.is_some_and(|binding| binding.enable_thinking.is_some()))
    {
        return Err(invalid_request());
    }
    if let Some(binding) = route_binding
        && has_audio
        && (!binding.allows_input_audio
            || (chat.stream && !binding.allows_audio_streaming_chat)
            || (uses_tools && !binding.allows_audio_function_tools))
    {
        return Err(unsupported_operation());
    }

    validate_messages(
        chat,
        max_audio_bytes,
        true,
        route_binding.is_none_or(|binding| binding.allows_input_images),
    )
}

fn validate_transcription(
    transcription: &crate::core::TranscriptionRequest,
    max_audio_bytes: usize,
    mode: Option<VllmTranscriptionMode>,
) -> Result<(), GatewayError> {
    let valid_media_type = match mode {
        Some(VllmTranscriptionMode::AudioChat) => {
            super::request::transcription_format(transcription.file.media_type()).is_some()
        }
        Some(VllmTranscriptionMode::NativeAsr) => {
            super::request::native_asr_accepts_media_type(transcription.file.media_type())
                && safe_filename(transcription.file.file_name())
        }
        None => false,
    };
    if transcription.model.0.is_empty()
        || transcription.file.bytes().is_empty()
        || transcription.file.bytes().len() > max_audio_bytes
        || transcription.extensions.iter().next().is_some()
        || !valid_media_type
        || !valid_hint(transcription.language.as_deref(), 80)
        || !valid_hint(transcription.prompt.as_deref(), 1000)
    {
        return Err(invalid_request());
    }
    Ok(())
}

fn safe_filename(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| !byte.is_ascii_control() && !matches!(byte, b'/' | b'\\' | b':' | b'"'))
}

fn valid_hint(value: Option<&str>, max_bytes: usize) -> bool {
    value.is_none_or(|value| value.len() <= max_bytes && !value.chars().any(char::is_control))
}
