use crate::adapter::{
    chat_completions::validation::{has_input_audio, uses_tools, validate_messages},
    internal_error, invalid_request, unsupported_operation,
};
use crate::{
    config::ValidatedRoute,
    core::{
        Capabilities, GatewayError, Operation, Request, RouteIdentity, RoutedRequest,
        TranscriptionRequest, TrustZone,
    },
};

pub(super) struct RouteBinding {
    identity: RouteIdentity,
    allows_input_audio: bool,
    allows_input_images: bool,
    allows_audio_function_tools: bool,
}

impl RouteBinding {
    pub(super) fn identity(&self) -> &RouteIdentity {
        &self.identity
    }
}

pub(super) fn bind_route(
    adapter_id: &str,
    route: &ValidatedRoute,
    capabilities: &Capabilities,
) -> Result<RouteBinding, GatewayError> {
    let operation = route.identity().selector.operation;
    if route.adapter_id() != adapter_id
        || !capabilities.operations.contains(&operation)
        || route.identity().route_id.trim().is_empty()
        || route.identity().selector.model_alias.0.trim().is_empty()
        || route.identity().upstream_id.trim().is_empty()
        || (route.requires_streaming_chat() && !capabilities.streaming_chat)
        || (route.requires_function_tools() && !capabilities.function_tools)
        || (route.allows_input_images()
            && (operation != Operation::Chat || !capabilities.input_images))
        || (route.allows_input_audio()
            && (operation != Operation::Chat || !capabilities.input_audio))
        || (route.allows_audio_streaming_chat() && !capabilities.audio_streaming_chat)
        || (route.allows_audio_function_tools()
            && !(route.allows_input_audio() && capabilities.audio_function_tools))
    {
        return Err(internal_error());
    }
    Ok(RouteBinding {
        identity: route.identity().clone(),
        allows_input_audio: route.allows_input_audio(),
        allows_input_images: route.allows_input_images(),
        allows_audio_function_tools: route.allows_audio_function_tools(),
    })
}

pub(super) fn validate(
    routed: &RoutedRequest,
    capabilities: &Capabilities,
    route_bindings: &[RouteBinding],
    max_audio_bytes: usize,
) -> Result<(), GatewayError> {
    let request = routed.request();
    capabilities
        .check_request(request)
        .map_err(|_| unsupported_operation())?;

    let context = routed.context();
    let binding = route_bindings
        .iter()
        .find(|binding| binding.identity == context.route);
    if context.trust_zone != TrustZone::External
        || context.extensions.iter().next().is_some()
        || context.check_request(request).is_err()
    {
        return Err(invalid_request());
    }
    let Some(binding) = binding else {
        return Err(invalid_request());
    };

    match request {
        Request::Speech(_) => Err(unsupported_operation()),
        Request::Embeddings(_) => Err(unsupported_operation()),
        Request::Chat(chat) => {
            validate_chat(chat, binding, capabilities.function_tools, max_audio_bytes)
        }
        Request::Transcription(transcription) => {
            validate_transcription(transcription, max_audio_bytes)
        }
    }
}

fn validate_transcription(
    transcription: &TranscriptionRequest,
    max_audio_bytes: usize,
) -> Result<(), GatewayError> {
    if transcription.model.0.trim().is_empty()
        || transcription.file.bytes().is_empty()
        || transcription.file.bytes().len() > max_audio_bytes
        || transcription.extensions.iter().next().is_some()
        || super::request::transcription_format(transcription.file.media_type()).is_none()
        || transcription.language.as_deref().is_some_and(|language| {
            language.is_empty() || language.len() > 80 || language.chars().any(char::is_control)
        })
    {
        return Err(invalid_request());
    }
    // The STT endpoint ignores prompts.
    if transcription.prompt.is_some() {
        return Err(unsupported_operation());
    }
    Ok(())
}

fn validate_chat(
    chat: &crate::core::ChatRequest,
    binding: &RouteBinding,
    function_tools: bool,
    max_audio_bytes: usize,
) -> Result<(), GatewayError> {
    if chat.model.0.trim().is_empty() || chat.messages.is_empty() {
        return Err(invalid_request());
    }
    if chat.extensions.iter().next().is_some() {
        return Err(invalid_request());
    }
    // Tool history needs tool support even when the request declares no tools.
    if uses_tools(chat)
        && (!function_tools || (has_input_audio(chat) && !binding.allows_audio_function_tools))
    {
        return Err(unsupported_operation());
    }
    validate_messages(
        chat,
        max_audio_bytes,
        binding.allows_input_audio,
        binding.allows_input_images,
    )
}
