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
            "invalid_or_duplicate_id",
        ));
    }
    let Some(alias_effort) = parse_model_alias(&route.model_alias) else {
        return Err(ConfigError::new(
            format!("{path}.model_alias"),
            "invalid_exact_alias",
        ));
    };
    let selector = RouteSelector {
        model_alias: ModelAlias(route.model_alias.clone()),
        operation: route.operation,
    };
    if !selectors.insert((route.model_alias.clone(), route.operation)) {
        return Err(ConfigError::new(path, "duplicate_selector"));
    }
    if route.upstream_id.trim().is_empty() {
        return Err(ConfigError::new(
            format!("{path}.upstream_id"),
            "invalid_or_empty",
        ));
    }
    let adapter = adapters
        .iter()
        .find(|adapter| adapter.id == route.adapter_id)
        .ok_or_else(|| ConfigError::new(format!("{path}.adapter_id"), "missing_adapter"))?;
    if adapter.kind != ProviderKind::Chatgpt
        && alias_effort.is_some()
        && route.reasoning_effort.is_none()
    {
        return Err(ConfigError::new(
            format!("{path}.model_alias"),
            "effort_alias_requires_reasoning_effort",
        ));
    }
    // ChatGPT always runs at a pinned effort; other reasoning-control adapters pin
    // optionally (effort families) and otherwise forward the client's effort.
    if let Some(effort) = route.reasoning_effort {
        if route.operation != Operation::Chat || !adapter.capabilities.reasoning_control {
            return Err(ConfigError::new(
                format!("{path}.reasoning_effort"),
                "unsupported_by_adapter",
            ));
        }
        if !adapter.kind.accepts_reasoning_effort(effort) {
            return Err(ConfigError::new(
                format!("{path}.reasoning_effort"),
                "unsupported_effort",
            ));
        }
        if alias_effort.is_some_and(|alias| alias != effort) {
            return Err(ConfigError::new(
                format!("{path}.reasoning_effort"),
                "does_not_match_alias",
            ));
        }
    } else if adapter.kind == ProviderKind::Chatgpt
        && (alias_effort.is_some() || adapter.capabilities.reasoning_control)
    {
        return Err(ConfigError::new(
            format!("{path}.reasoning_effort"),
            "required",
        ));
    }
    if route.reasoning_summary.is_some()
        && (adapter.kind != ProviderKind::Chatgpt || route.operation != Operation::Chat)
    {
        return Err(ConfigError::new(
            format!("{path}.reasoning_summary"),
            "unsupported_by_adapter",
        ));
    }
    if !adapter.capabilities.operations.contains(&route.operation) {
        return Err(ConfigError::new(
            format!("{path}.operation"),
            "unsupported_by_adapter",
        ));
    }
    if route.requires_streaming_chat
        && (route.operation != Operation::Chat || !adapter.capabilities.streaming_chat)
    {
        return Err(ConfigError::new(
            format!("{path}.requires_streaming_chat"),
            "unsupported_by_adapter",
        ));
    }
    if route.requires_function_tools
        && (route.operation != Operation::Chat || !adapter.capabilities.function_tools)
    {
        return Err(ConfigError::new(
            format!("{path}.requires_function_tools"),
            "unsupported_by_adapter",
        ));
    }
    if route.allows_input_images
        && (route.operation != Operation::Chat || !adapter.capabilities.input_images)
    {
        return Err(ConfigError::new(
            format!("{path}.allows_input_images"),
            "unsupported_by_adapter",
        ));
    }
    if route.allows_input_audio
        && (route.operation != Operation::Chat || !adapter.capabilities.input_audio)
    {
        return Err(ConfigError::new(
            format!("{path}.allows_input_audio"),
            "unsupported_by_adapter",
        ));
    }
    if route.allows_audio_streaming_chat
        && (!route.allows_input_audio
            || !adapter.capabilities.audio_streaming_chat
            || route.operation != Operation::Chat)
    {
        return Err(ConfigError::new(
            format!("{path}.allows_audio_streaming_chat"),
            "unsupported_by_adapter",
        ));
    }
    if route.allows_audio_function_tools
        && (!route.allows_input_audio
            || !adapter.capabilities.audio_function_tools
            || route.operation != Operation::Chat)
    {
        return Err(ConfigError::new(
            format!("{path}.allows_audio_function_tools"),
            "unsupported_by_adapter",
        ));
    }
    if let Some(tokens) = route.context_tokens {
        if route.operation != Operation::Chat {
            return Err(ConfigError::new(
                format!("{path}.context_tokens"),
                "chat_only",
            ));
        }
        if !(MIN_CONTEXT_TOKENS..=MAX_CONTEXT_TOKENS).contains(&tokens) {
            return Err(ConfigError::new(
                format!("{path}.context_tokens"),
                "out_of_range",
            ));
        }
    }
    if let Some(tokens) = route.max_output_tokens {
        let path = format!("{path}.max_output_tokens");
        if route.operation != Operation::Chat {
            return Err(ConfigError::new(path, "chat_only"));
        }
        if !adapter.capabilities.sampling_controls {
            return Err(ConfigError::new(path, "unsupported_output_cap"));
        }
        if !(1..=crate::core::MAX_OUTPUT_TOKENS).contains(&tokens) {
            return Err(ConfigError::new(path, "out_of_range"));
        }
        if route.context_tokens.is_some_and(|context| tokens > context) {
            return Err(ConfigError::new(path, "exceeds_context_tokens"));
        }
    }
    if route.enable_thinking.is_some() {
        let uses_chat_template = route.operation == Operation::Chat
            || adapter.transcription_mode == Some(VllmTranscriptionMode::AudioChat);
        if adapter.kind != ProviderKind::Vllm {
            return Err(ConfigError::new(
                format!("{path}.enable_thinking"),
                "vllm_only",
            ));
        }
        if !uses_chat_template {
            return Err(ConfigError::new(
                format!("{path}.enable_thinking"),
                "requires_chat_template",
            ));
        }
        // A chat route on a reasoning-control adapter takes the client's effort instead;
        // sending both switches would leave the backend to pick one.
        if route.operation == Operation::Chat && adapter.capabilities.reasoning_control {
            return Err(ConfigError::new(
                format!("{path}.enable_thinking"),
                "conflicts_with_reasoning_control",
            ));
        }
    }
    let speech = if route.operation == Operation::Speech {
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
                "invalid_allowlist",
            ));
        }
        if formats.is_empty() || formats.len() != route.speech_formats.len() {
            return Err(ConfigError::new(
                format!("{path}.speech_formats"),
                "invalid_allowlist",
            ));
        }
        Some(crate::core::SpeechPolicy { voices, formats })
    } else {
        if !route.speech_voices.is_empty() || !route.speech_formats.is_empty() {
            return Err(ConfigError::new(
                format!("{path}.speech_voices"),
                "speech_only",
            ));
        }
        None
    };
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
