use super::*;

pub(in crate::config) fn validate_route(
    index: usize,
    route: RawRoute,
    adapters: &[ValidatedAdapter],
    route_ids: &mut BTreeSet<String>,
    selectors: &mut BTreeSet<(String, Operation)>,
) -> Result<ValidatedRoute, ConfigError> {
    let path = format!("routes[{index}]");
    if !valid_identifier(&route.id) || !route_ids.insert(route.id.clone()) {
        return Err(ConfigError::new(
            format!("{path}.id"),
            ConfigErrorClass::InvalidOrDuplicateId,
        ));
    }
    let Some(alias_effort) = parse_model_alias(&route.model_alias) else {
        return Err(ConfigError::new(
            format!("{path}.model_alias"),
            ConfigErrorClass::InvalidExactAlias,
        ));
    };
    let selector = RouteSelector {
        model_alias: ModelAlias(route.model_alias.clone()),
        operation: route.operation,
    };
    if !selectors.insert((route.model_alias.clone(), route.operation)) {
        return Err(ConfigError::new(path, ConfigErrorClass::DuplicateSelector));
    }
    if route.upstream_id.trim().is_empty() {
        return Err(ConfigError::new(
            format!("{path}.upstream_id"),
            ConfigErrorClass::InvalidOrEmpty,
        ));
    }
    let adapter = adapters
        .iter()
        .find(|adapter| adapter.id == route.adapter_id)
        .ok_or_else(|| {
            ConfigError::new(
                format!("{path}.adapter_id"),
                ConfigErrorClass::MissingAdapter,
            )
        })?;
    validate_route_reasoning(&route, adapter, alias_effort, &path)?;
    validate_route_capabilities(&route, adapter, &path)?;
    validate_route_token_caps(&route, adapter, &path)?;
    validate_enable_thinking(&route, adapter, &path)?;
    let speech = validate_speech_policy(&route, &path)?;
    let extension_allowlist = validate_extension_allowlist(
        route.extension_allowlist,
        &format!("{path}.extension_allowlist"),
    )?;
    Ok(ValidatedRoute {
        identity: RouteIdentity {
            route_id: route.id,
            upstream_id: route.upstream_id,
            selector,
        },
        adapter_id: route.adapter_id,
        reasoning_effort: route.reasoning_effort,
        reasoning_summary: route.reasoning_summary,
        extension_allowlist,
        requires_streaming_chat: route.requires_streaming_chat,
        requires_function_tools: route.requires_function_tools,
        allows_input_audio: route.allows_input_audio,
        allows_input_images: route.allows_input_images,
        speech,
        allows_audio_streaming_chat: route.allows_audio_streaming_chat,
        allows_audio_function_tools: route.allows_audio_function_tools,
        context_tokens: route.context_tokens,
        max_output_tokens: route.max_output_tokens,
        enable_thinking: route.enable_thinking,
    })
}

/// Effort pins, effort-suffixed aliases and reasoning summaries.
fn validate_route_reasoning(
    route: &RawRoute,
    adapter: &ValidatedAdapter,
    alias_effort: Option<ReasoningEffort>,
    path: &str,
) -> Result<(), ConfigError> {
    if adapter.kind != ProviderKind::Chatgpt
        && alias_effort.is_some()
        && route.reasoning_effort.is_none()
    {
        return Err(ConfigError::new(
            format!("{path}.model_alias"),
            ConfigErrorClass::EffortAliasRequiresReasoningEffort,
        ));
    }
    // ChatGPT always runs at a pinned effort; other reasoning-control adapters pin
    // optionally (effort families) and otherwise forward the client's effort.
    if let Some(effort) = route.reasoning_effort {
        if route.operation != Operation::Chat || !adapter.capabilities.reasoning_control {
            return Err(ConfigError::new(
                format!("{path}.reasoning_effort"),
                ConfigErrorClass::UnsupportedByAdapter,
            ));
        }
        if !adapter.kind.accepts_reasoning_effort(effort) {
            return Err(ConfigError::new(
                format!("{path}.reasoning_effort"),
                ConfigErrorClass::UnsupportedEffort,
            ));
        }
        if alias_effort.is_some_and(|alias| alias != effort) {
            return Err(ConfigError::new(
                format!("{path}.reasoning_effort"),
                ConfigErrorClass::DoesNotMatchAlias,
            ));
        }
    } else if adapter.kind == ProviderKind::Chatgpt
        && (alias_effort.is_some() || adapter.capabilities.reasoning_control)
    {
        return Err(ConfigError::new(
            format!("{path}.reasoning_effort"),
            ConfigErrorClass::Required,
        ));
    }
    if route.reasoning_summary.is_some()
        && (adapter.kind != ProviderKind::Chatgpt || route.operation != Operation::Chat)
    {
        return Err(ConfigError::new(
            format!("{path}.reasoning_summary"),
            ConfigErrorClass::UnsupportedByAdapter,
        ));
    }
    Ok(())
}

/// Route policies the adapter's operations and capabilities must back.
fn validate_route_capabilities(
    route: &RawRoute,
    adapter: &ValidatedAdapter,
    path: &str,
) -> Result<(), ConfigError> {
    if !adapter.capabilities.operations.contains(&route.operation) {
        return Err(ConfigError::new(
            format!("{path}.operation"),
            ConfigErrorClass::UnsupportedByAdapter,
        ));
    }
    if route.requires_streaming_chat
        && (route.operation != Operation::Chat || !adapter.capabilities.streaming_chat)
    {
        return Err(ConfigError::new(
            format!("{path}.requires_streaming_chat"),
            ConfigErrorClass::UnsupportedByAdapter,
        ));
    }
    if route.requires_function_tools
        && (route.operation != Operation::Chat || !adapter.capabilities.function_tools)
    {
        return Err(ConfigError::new(
            format!("{path}.requires_function_tools"),
            ConfigErrorClass::UnsupportedByAdapter,
        ));
    }
    if route.allows_input_images
        && (route.operation != Operation::Chat || !adapter.capabilities.input_images)
    {
        return Err(ConfigError::new(
            format!("{path}.allows_input_images"),
            ConfigErrorClass::UnsupportedByAdapter,
        ));
    }
    if route.allows_input_audio
        && (route.operation != Operation::Chat || !adapter.capabilities.input_audio)
    {
        return Err(ConfigError::new(
            format!("{path}.allows_input_audio"),
            ConfigErrorClass::UnsupportedByAdapter,
        ));
    }
    if route.allows_audio_streaming_chat
        && (!route.allows_input_audio
            || !adapter.capabilities.audio_streaming_chat
            || route.operation != Operation::Chat)
    {
        return Err(ConfigError::new(
            format!("{path}.allows_audio_streaming_chat"),
            ConfigErrorClass::UnsupportedByAdapter,
        ));
    }
    if route.allows_audio_function_tools
        && (!route.allows_input_audio
            || !adapter.capabilities.audio_function_tools
            || route.operation != Operation::Chat)
    {
        return Err(ConfigError::new(
            format!("{path}.allows_audio_function_tools"),
            ConfigErrorClass::UnsupportedByAdapter,
        ));
    }
    Ok(())
}

/// `context_tokens` and `max_output_tokens` bounds.
fn validate_route_token_caps(
    route: &RawRoute,
    adapter: &ValidatedAdapter,
    path: &str,
) -> Result<(), ConfigError> {
    if let Some(tokens) = route.context_tokens {
        if route.operation != Operation::Chat {
            return Err(ConfigError::new(
                format!("{path}.context_tokens"),
                ConfigErrorClass::ChatOnly,
            ));
        }
        if !(MIN_CONTEXT_TOKENS..=MAX_CONTEXT_TOKENS).contains(&tokens) {
            return Err(ConfigError::new(
                format!("{path}.context_tokens"),
                ConfigErrorClass::OutOfRange,
            ));
        }
    }
    if let Some(tokens) = route.max_output_tokens {
        let path = format!("{path}.max_output_tokens");
        if route.operation != Operation::Chat {
            return Err(ConfigError::new(path, ConfigErrorClass::ChatOnly));
        }
        if !adapter.capabilities.sampling_controls {
            return Err(ConfigError::new(
                path,
                ConfigErrorClass::UnsupportedOutputCap,
            ));
        }
        if !(1..=crate::core::MAX_OUTPUT_TOKENS).contains(&tokens) {
            return Err(ConfigError::new(path, ConfigErrorClass::OutOfRange));
        }
        if route.context_tokens.is_some_and(|context| tokens > context) {
            return Err(ConfigError::new(
                path,
                ConfigErrorClass::ExceedsContextTokens,
            ));
        }
    }
    Ok(())
}

/// `enable_thinking`: vLLM chat-template routes without reasoning control.
fn validate_enable_thinking(
    route: &RawRoute,
    adapter: &ValidatedAdapter,
    path: &str,
) -> Result<(), ConfigError> {
    if route.enable_thinking.is_some() {
        let uses_chat_template = route.operation == Operation::Chat
            || adapter.transcription_mode == Some(VllmTranscriptionMode::AudioChat);
        if adapter.kind != ProviderKind::Vllm {
            return Err(ConfigError::new(
                format!("{path}.enable_thinking"),
                ConfigErrorClass::VllmOnly,
            ));
        }
        if !uses_chat_template {
            return Err(ConfigError::new(
                format!("{path}.enable_thinking"),
                ConfigErrorClass::RequiresChatTemplate,
            ));
        }
        // A chat route on a reasoning-control adapter takes the client's effort instead;
        // sending both switches would leave the backend to pick one.
        if route.operation == Operation::Chat && adapter.capabilities.reasoning_control {
            return Err(ConfigError::new(
                format!("{path}.enable_thinking"),
                ConfigErrorClass::ConflictsWithReasoningControl,
            ));
        }
    }
    Ok(())
}

/// Speech voice and format allowlists, required on speech routes only.
fn validate_speech_policy(
    route: &RawRoute,
    path: &str,
) -> Result<Option<crate::core::SpeechPolicy>, ConfigError> {
    Ok(if route.operation == Operation::Speech {
        let voices: BTreeSet<_> = route.speech_voices.iter().cloned().collect();
        let formats: BTreeSet<_> = route.speech_formats.iter().copied().collect();
        if voices.is_empty()
            || voices.len() > 128
            || voices.len() != route.speech_voices.len()
            || voices
                .iter()
                .any(|voice| !crate::core::valid_speech_voice(voice))
        {
            return Err(ConfigError::new(
                format!("{path}.speech_voices"),
                ConfigErrorClass::InvalidAllowlist,
            ));
        }
        if formats.is_empty() || formats.len() != route.speech_formats.len() {
            return Err(ConfigError::new(
                format!("{path}.speech_formats"),
                ConfigErrorClass::InvalidAllowlist,
            ));
        }
        Some(crate::core::SpeechPolicy { voices, formats })
    } else {
        if !route.speech_voices.is_empty() || !route.speech_formats.is_empty() {
            return Err(ConfigError::new(
                format!("{path}.speech_voices"),
                ConfigErrorClass::SpeechOnly,
            ));
        }
        None
    })
}
