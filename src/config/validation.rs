use super::*;

pub(super) fn validate(
    raw: RawConfig,
    config_dir: &Path,
    read_keys_file: bool,
) -> Result<ValidatedConfig, ConfigError> {
    let mut publication = validate_publication(&raw.publication)?;
    let listeners = validate_listeners(&raw.listeners, publication.tailnet_addresses())?;
    let (limits, timeouts) = validate_bounds(&raw.limits, &raw.timeouts)?;

    let mut adapter_ids = BTreeSet::new();
    let mut adapters = Vec::with_capacity(raw.adapters.len());
    for (index, adapter) in raw.adapters.into_iter().enumerate() {
        let path = format!("adapters[{index}]");
        if !valid_identifier(&adapter.id) || !adapter_ids.insert(adapter.id.clone()) {
            return Err(ConfigError::new(
                format!("{path}.id"),
                "invalid_or_duplicate_id",
            ));
        }
        let base_url = validate_url(&adapter, &path)?;
        let secret_ref = if adapter.kind.is_private_only() {
            if adapter.secret_ref.is_some() {
                return Err(ConfigError::new(
                    format!("{path}.secret_ref"),
                    if adapter.kind == ProviderKind::Codex {
                        "codex_secret_ref_unsupported"
                    } else {
                        "chatgpt_secret_ref_unsupported"
                    },
                ));
            }
            None
        } else if let Some(secret_ref) = &adapter.secret_ref {
            Some(validate_secret_ref(
                secret_ref,
                &format!("{path}.secret_ref"),
                false,
            )?)
        } else if adapter.kind == ProviderKind::Openrouter
            || (matches!(adapter.kind, ProviderKind::Vllm | ProviderKind::Speech)
                && adapter.trust_zone == TrustZone::External)
        {
            return Err(ConfigError::new(format!("{path}.secret_ref"), "required"));
        } else {
            None
        };
        if matches!(adapter.kind, ProviderKind::Vllm | ProviderKind::Speech)
            && secret_ref.is_some()
            && base_url.scheme() != "https"
        {
            return Err(ConfigError::new(
                format!("{path}.base_url"),
                "https_required",
            ));
        }
        validate_provider_zone(&adapter, &path)?;
        let extension_allowlist = validate_extension_allowlist(
            adapter.extension_allowlist,
            &format!("{path}.extension_allowlist"),
        )?;
        let mut declared_operations = BTreeSet::new();
        for (operation_index, operation) in adapter.capabilities.operations.iter().enumerate() {
            if !declared_operations.insert(*operation) {
                return Err(ConfigError::new(
                    format!("{path}.capabilities.operations[{operation_index}]"),
                    "duplicate",
                ));
            }
        }
        let operations: BTreeSet<_> = adapter.capabilities.operations.into_iter().collect();
        if operations.is_empty() {
            return Err(ConfigError::new(
                format!("{path}.capabilities.operations"),
                "empty",
            ));
        }
        if operations.contains(&Operation::Speech) != (adapter.kind == ProviderKind::Speech) {
            return Err(ConfigError::new(
                format!("{path}.capabilities.operations"),
                "speech_adapter_required",
            ));
        }
        if adapter.kind == ProviderKind::Speech
            && (operations.len() != 1
                || adapter.capabilities.streaming_chat
                || adapter.capabilities.function_tools)
        {
            return Err(ConfigError::new(
                format!("{path}.capabilities"),
                "speech_only",
            ));
        }
        if operations.contains(&Operation::Embeddings) && adapter.kind != ProviderKind::Ollama {
            return Err(ConfigError::new(
                format!("{path}.capabilities.operations"),
                "embeddings_unsupported_by_adapter_kind",
            ));
        }
        if adapter.kind == ProviderKind::Chatgpt {
            if operations != BTreeSet::from([Operation::Chat]) || adapter.capabilities.input_audio {
                return Err(ConfigError::new(
                    format!("{path}.capabilities"),
                    "chatgpt_text_chat_only",
                ));
            }
            if base_url.as_str() != "https://api.openai.com/v1" {
                return Err(ConfigError::new(
                    format!("{path}.base_url"),
                    "chatgpt_pinned_origin_required",
                ));
            }
            if !extension_allowlist.is_empty() {
                return Err(ConfigError::new(
                    format!("{path}.extension_allowlist"),
                    "unsupported_by_adapter_kind",
                ));
            }
        }
        if adapter.kind == ProviderKind::AppleFm {
            if operations.contains(&Operation::Transcription) {
                return Err(ConfigError::new(
                    format!("{path}.capabilities.operations"),
                    "apple_fm_chat_only",
                ));
            }
            // fm serve returns tool arguments as plain text and takes no audio.
            for (name, declared) in [
                ("function_tools", adapter.capabilities.function_tools),
                ("input_audio", adapter.capabilities.input_audio),
            ] {
                if declared {
                    return Err(ConfigError::new(
                        format!("{path}.capabilities.{name}"),
                        "unsupported_by_adapter_kind",
                    ));
                }
            }
        }
        if adapter.kind == ProviderKind::Codex && operations.contains(&Operation::Transcription) {
            return Err(ConfigError::new(
                format!("{path}.capabilities.operations"),
                "codex_chat_only",
            ));
        }
        let transcription_mode = match (adapter.kind, adapter.transcription_mode) {
            (ProviderKind::Vllm, Some(_)) if !operations.contains(&Operation::Transcription) => {
                return Err(ConfigError::new(
                    format!("{path}.transcription_mode"),
                    "without_transcription_operation",
                ));
            }
            (ProviderKind::Vllm, Some(VllmTranscriptionMode::AudioChat))
                if !adapter.capabilities.input_audio =>
            {
                return Err(ConfigError::new(
                    format!("{path}.transcription_mode"),
                    "requires_input_audio_capability",
                ));
            }
            (ProviderKind::Vllm, mode) if operations.contains(&Operation::Transcription) => {
                let Some(mode) = mode else {
                    return Err(ConfigError::new(
                        format!("{path}.transcription_mode"),
                        "required_for_transcription",
                    ));
                };
                Some(mode)
            }
            (ProviderKind::Vllm, mode) => mode,
            (_, Some(_)) => {
                return Err(ConfigError::new(
                    format!("{path}.transcription_mode"),
                    "vllm_only",
                ));
            }
            (_, None) => None,
        };
        if adapter.capabilities.input_images {
            if !matches!(
                adapter.kind,
                ProviderKind::Ollama | ProviderKind::Vllm | ProviderKind::Openrouter
            ) {
                return Err(ConfigError::new(
                    format!("{path}.capabilities.input_images"),
                    "unsupported_by_adapter_kind",
                ));
            }
            if !operations.contains(&Operation::Chat) {
                return Err(ConfigError::new(
                    format!("{path}.capabilities.input_images"),
                    "chat_operation_required",
                ));
            }
        }
        if adapter.capabilities.input_audio && !operations.contains(&Operation::Chat) {
            return Err(ConfigError::new(
                format!("{path}.capabilities.input_audio"),
                "chat_operation_required",
            ));
        }
        if adapter.capabilities.audio_streaming_chat
            && (!adapter.capabilities.input_audio || !adapter.capabilities.streaming_chat)
        {
            return Err(ConfigError::new(
                format!("{path}.capabilities.audio_streaming_chat"),
                "requires_audio_and_streaming_chat",
            ));
        }
        if adapter.capabilities.audio_function_tools
            && (!adapter.capabilities.input_audio || !adapter.capabilities.function_tools)
        {
            return Err(ConfigError::new(
                format!("{path}.capabilities.audio_function_tools"),
                "requires_audio_and_function_tools",
            ));
        }
        let (structured_output, sampling_controls, reasoning_control) =
            adapter.kind.supports_chat_options();
        for (name, declared, supported) in [
            (
                "structured_output",
                adapter.capabilities.structured_output,
                structured_output,
            ),
            (
                "sampling_controls",
                adapter.capabilities.sampling_controls,
                sampling_controls,
            ),
            (
                "reasoning_control",
                adapter.capabilities.reasoning_control,
                reasoning_control,
            ),
        ] {
            if !declared {
                continue;
            }
            if !supported {
                return Err(ConfigError::new(
                    format!("{path}.capabilities.{name}"),
                    "unsupported_by_adapter_kind",
                ));
            }
            if !operations.contains(&Operation::Chat) {
                return Err(ConfigError::new(
                    format!("{path}.capabilities.{name}"),
                    "chat_operation_required",
                ));
            }
        }
        let max_in_flight = validate_optional_limit(adapter.max_in_flight, &path)?;
        let circuit_breaker = validate_circuit_breaker(adapter.circuit_breaker, &path)?;
        adapters.push(ValidatedAdapter {
            id: adapter.id,
            kind: adapter.kind,
            base_url,
            trust_zone: adapter.trust_zone,
            secret_ref,
            transcription_mode,
            extension_allowlist,
            capabilities: Capabilities {
                operations,
                streaming_chat: adapter.capabilities.streaming_chat,
                function_tools: adapter.capabilities.function_tools,
                input_audio: adapter.capabilities.input_audio,
                input_images: adapter.capabilities.input_images,
                audio_streaming_chat: adapter.capabilities.audio_streaming_chat,
                audio_function_tools: adapter.capabilities.audio_function_tools,
                structured_output: adapter.capabilities.structured_output,
                sampling_controls: adapter.capabilities.sampling_controls,
                reasoning_control: adapter.capabilities.reasoning_control,
            },
            max_in_flight,
            circuit_breaker,
        });
    }
    if adapters.is_empty() {
        return Err(ConfigError::new("adapters", "empty"));
    }
    let has_codex_adapter = adapters
        .iter()
        .any(|adapter| adapter.kind == ProviderKind::Codex);
    let codex_auth = match (has_codex_adapter, raw.codex_auth) {
        (true, Some(auth)) => Some(validate_codex_auth(auth)?),
        (true, None) => return Err(ConfigError::new("codex_auth", "required")),
        (false, Some(_)) => return Err(ConfigError::new("codex_auth", "without_codex_adapter")),
        (false, None) => None,
    };

    let has_chatgpt_adapter = adapters
        .iter()
        .any(|adapter| adapter.kind == ProviderKind::Chatgpt);
    let chatgpt_auth = match (has_chatgpt_adapter, raw.chatgpt_auth) {
        (true, Some(auth)) => {
            let state_dir = PathBuf::from(&auth.state_dir);
            if auth.state_dir.chars().any(char::is_control)
                || !state_dir.is_absolute()
                || state_dir
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                || !state_dir
                    .components()
                    .any(|part| matches!(part, std::path::Component::Normal(_)))
            {
                return Err(ConfigError::new(
                    "chatgpt_auth.state_dir",
                    "invalid_absolute_path",
                ));
            }
            Some(ValidatedChatgptAuth { state_dir })
        }
        (true, None) => return Err(ConfigError::new("chatgpt_auth", "required")),
        (false, Some(_)) => {
            return Err(ConfigError::new("chatgpt_auth", "without_chatgpt_adapter"));
        }
        (false, None) => None,
    };

    let mut route_ids = BTreeSet::new();
    let mut selectors = BTreeSet::new();
    let mut routes = Vec::with_capacity(raw.routes.len());
    for (index, route) in raw.routes.into_iter().enumerate() {
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
        let codex_reasoning_effort = match adapter.kind {
            ProviderKind::Codex if route.operation == Operation::Chat => {
                match (alias_effort, route.codex_reasoning_effort) {
                    (Some(alias), Some(field))
                        if CodexReasoningEffort::from_request(alias) != Some(field) =>
                    {
                        return Err(ConfigError::new(
                            format!("{path}.codex_reasoning_effort"),
                            "does_not_match_alias",
                        ));
                    }
                    (Some(_), None) => {
                        return Err(ConfigError::new(
                            format!("{path}.codex_reasoning_effort"),
                            "required_for_effort_alias",
                        ));
                    }
                    (None, Some(_)) => {
                        return Err(ConfigError::new(
                            format!("{path}.codex_reasoning_effort"),
                            "effort_requires_alias",
                        ));
                    }
                    (Some(_), Some(field)) => Some(field),
                    (None, None) => Some(CodexReasoningEffort::Medium),
                }
            }
            ProviderKind::Codex => {
                if route.codex_reasoning_effort.is_some() {
                    return Err(ConfigError::new(
                        format!("{path}.codex_reasoning_effort"),
                        "codex_chat_only",
                    ));
                }
                if alias_effort.is_some() {
                    return Err(ConfigError::new(
                        format!("{path}.model_alias"),
                        "codex_chat_only",
                    ));
                }
                None
            }
            ProviderKind::Chatgpt => {
                if route.codex_reasoning_effort.is_some() {
                    return Err(ConfigError::new(
                        format!("{path}.codex_reasoning_effort"),
                        "codex_only",
                    ));
                }
                None
            }
            _ => {
                if route.codex_reasoning_effort.is_some() {
                    return Err(ConfigError::new(
                        format!("{path}.codex_reasoning_effort"),
                        "codex_only",
                    ));
                }
                if alias_effort.is_some() {
                    return Err(ConfigError::new(
                        format!("{path}.model_alias"),
                        "codex_effort_alias_only",
                    ));
                }
                None
            }
        };
        if adapter.kind == ProviderKind::Chatgpt {
            if let Some(effort) = route.reasoning_effort {
                if route.operation != Operation::Chat || !adapter.capabilities.reasoning_control {
                    return Err(ConfigError::new(
                        format!("{path}.reasoning_effort"),
                        "unsupported_by_adapter",
                    ));
                }
                if alias_effort.is_some_and(|alias| alias != effort) {
                    return Err(ConfigError::new(
                        format!("{path}.reasoning_effort"),
                        "does_not_match_alias",
                    ));
                }
            } else if alias_effort.is_some() || adapter.capabilities.reasoning_control {
                return Err(ConfigError::new(
                    format!("{path}.reasoning_effort"),
                    "required",
                ));
            }
        } else if route.reasoning_effort.is_some() {
            return Err(ConfigError::new(
                format!("{path}.reasoning_effort"),
                "chatgpt_only",
            ));
        }
        if route.reasoning_summary.is_some() {
            if !matches!(adapter.kind, ProviderKind::Chatgpt | ProviderKind::Codex)
                || route.operation != Operation::Chat
            {
                return Err(ConfigError::new(
                    format!("{path}.reasoning_summary"),
                    "unsupported_by_adapter",
                ));
            }
            if route.codex_reasoning_summary.is_some() {
                return Err(ConfigError::new(
                    format!("{path}.reasoning_summary"),
                    "conflicting_summary_options",
                ));
            }
        }
        if route.codex_reasoning_summary.is_some() {
            if adapter.kind != ProviderKind::Codex {
                return Err(ConfigError::new(
                    format!("{path}.codex_reasoning_summary"),
                    "codex_only",
                ));
            }
            if route.operation != Operation::Chat {
                return Err(ConfigError::new(
                    format!("{path}.codex_reasoning_summary"),
                    "codex_chat_only",
                ));
            }
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
            if !adapter.capabilities.sampling_controls || adapter.kind == ProviderKind::Codex {
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
        routes.push(ValidatedRoute {
            identity: RouteIdentity {
                route_id: route.id,
                upstream_id: route.upstream_id,
                selector,
            },
            adapter_id: route.adapter_id,
            codex_reasoning_effort,
            reasoning_effort: route.reasoning_effort,
            reasoning_summary: route.reasoning_summary,
            codex_reasoning_summary: route.codex_reasoning_summary,
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
        });
    }
    if routes.is_empty() {
        return Err(ConfigError::new("routes", "empty"));
    }

    for (index, route) in routes.iter().enumerate().filter(|(_, route)| {
        route.pinned_reasoning_effort().is_some()
            && adapters.iter().any(|adapter| {
                adapter.id == route.adapter_id && adapter.capabilities.reasoning_control
            })
    }) {
        for other in routes.iter().filter(|other| {
            other.identity.selector.operation == Operation::Chat
                && other.model_family_alias() == route.model_family_alias()
        }) {
            if other.pinned_reasoning_effort().is_none()
                || route.adapter_id != other.adapter_id
                || route.identity.upstream_id != other.identity.upstream_id
                || route.allows_input_images != other.allows_input_images
                || route.extension_allowlist != other.extension_allowlist
                || route.enable_thinking != other.enable_thinking
                || route.allows_input_audio != other.allows_input_audio
                || route.allows_audio_streaming_chat != other.allows_audio_streaming_chat
                || route.allows_audio_function_tools != other.allows_audio_function_tools
                || route.requires_streaming_chat != other.requires_streaming_chat
                || route.requires_function_tools != other.requires_function_tools
                || route.context_tokens != other.context_tokens
                || route.max_output_tokens != other.max_output_tokens
                || route.reasoning_summary() != other.reasoning_summary()
            {
                return Err(ConfigError::new(
                    format!("routes[{index}]"),
                    "inconsistent_reasoning_family",
                ));
            }
        }
    }

    publication.public_routes = validate_public_routes(
        &raw.publication.public_routes,
        listeners.public.is_some(),
        &routes,
        &adapters,
    )?;
    let (application_keys, key_source) = match raw.keys {
        Some(_) if !raw.application_keys.is_empty() => {
            return Err(ConfigError::new("keys", "conflicting_key_sources"));
        }
        Some(keys) => load_key_file(&keys, config_dir, &routes, read_keys_file)?,
        None => (
            validate_application_keys(raw.application_keys, &routes)?,
            KeySource::Inline,
        ),
    };
    Ok(ValidatedConfig {
        listeners,
        publication,
        codex_auth,
        chatgpt_auth,
        adapters,
        routes,
        application_keys,
        key_source,
        limits,
        timeouts,
        logging: ValidatedLogging {
            level: raw.logging.level,
            format: raw.logging.format,
        },
    })
}

pub(super) fn validate_codex_auth(auth: RawCodexAuth) -> Result<ValidatedCodexAuth, ConfigError> {
    let store = auth.store.unwrap_or(CodexAuthStore::Keyring);
    let state_dir = auth
        .state_dir
        .as_deref()
        .ok_or_else(|| ConfigError::new("codex_auth.state_dir", "required"))?;
    if state_dir.is_empty() {
        return Err(ConfigError::new("codex_auth.state_dir", "empty"));
    }
    if state_dir.chars().any(char::is_control) {
        return Err(ConfigError::new("codex_auth.state_dir", "invalid_path"));
    }
    let state_dir_path = Path::new(state_dir);
    if !state_dir_path.is_absolute() {
        return Err(ConfigError::new(
            "codex_auth.state_dir",
            "absolute_path_required",
        ));
    }
    if state_dir_path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ConfigError::new(
            "codex_auth.state_dir",
            "parent_traversal_forbidden",
        ));
    }
    if !state_dir_path
        .components()
        .any(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ConfigError::new("codex_auth.state_dir", "root_not_allowed"));
    }
    Ok(ValidatedCodexAuth {
        store,
        state_dir: state_dir_path.to_path_buf(),
    })
}

pub(super) fn validate_listeners(
    listeners: &RawListeners,
    tailnet_addresses: &[IpAddr],
) -> Result<ValidatedListeners, ConfigError> {
    let admin = listeners
        .admin
        .bind
        .parse::<IpAddr>()
        .map_err(|_| ConfigError::new("listeners.admin.bind", "invalid_ip"))?;
    if !admin.is_loopback() {
        return Err(ConfigError::new("listeners.admin.bind", "not_loopback"));
    }
    if listeners.admin.port == 0 {
        return Err(ConfigError::new("listeners.admin.port", "zero"));
    }
    if listeners.client.port == 0 {
        return Err(ConfigError::new("listeners.client.port", "zero"));
    }
    if listeners.client.port == listeners.admin.port {
        return Err(ConfigError::new("listeners", "client_admin_port_collision"));
    }
    let client = listeners
        .client
        .bind
        .parse::<IpAddr>()
        .map_err(|_| ConfigError::new("listeners.client.bind", "invalid_ip"))?;
    if !client.is_unspecified()
        && !client.is_loopback()
        && !is_private_public_bind(client)
        && !tailnet_addresses.contains(&client)
    {
        return Err(ConfigError::new(
            "listeners.client.bind",
            "not_internal_address",
        ));
    }
    let public = if let Some(public) = &listeners.public {
        if public.port == 0 {
            return Err(ConfigError::new("listeners.public.port", "zero"));
        }
        let bind = public
            .bind
            .parse::<IpAddr>()
            .map_err(|_| ConfigError::new("listeners.public.bind", "invalid_ip"))?;
        if bind.is_loopback() || bind.is_unspecified() {
            return Err(ConfigError::new("listeners.public.bind", "not_concrete"));
        }
        if !is_private_public_bind(bind) {
            return Err(ConfigError::new(
                "listeners.public.bind",
                "not_internal_address",
            ));
        }
        if shares_configured_tailnet_prefix(bind, tailnet_addresses) {
            return Err(ConfigError::new(
                "listeners.public.bind",
                "tailnet_address_forbidden",
            ));
        }
        if listener_bindings_conflict(client, listeners.client.port, bind, public.port) {
            return Err(ConfigError::new(
                "listeners.public",
                "client_port_collision",
            ));
        }
        if bind == client {
            return Err(ConfigError::new(
                "listeners.public",
                "client_bind_collision",
            ));
        }
        Some(ValidatedListener {
            bind,
            port: public.port,
        })
    } else {
        None
    };
    Ok(ValidatedListeners {
        client: ValidatedListener {
            bind: client,
            port: listeners.client.port,
        },
        admin: ValidatedListener {
            bind: admin,
            port: listeners.admin.port,
        },
        public,
    })
}

pub(super) fn is_private_public_bind(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private(),
        IpAddr::V6(address) => address.is_unique_local(),
    }
}

pub(super) fn shares_configured_tailnet_prefix(
    address: IpAddr,
    tailnet_addresses: &[IpAddr],
) -> bool {
    match address {
        IpAddr::V4(_) => false,
        IpAddr::V6(address) => tailnet_addresses.iter().any(|tailnet| match tailnet {
            IpAddr::V4(_) => false,
            IpAddr::V6(tailnet) => address.octets()[..6] == tailnet.octets()[..6],
        }),
    }
}

pub(super) fn listener_bindings_conflict(
    first_bind: IpAddr,
    first_port: u16,
    second_bind: IpAddr,
    second_port: u16,
) -> bool {
    first_port == second_port
        && (first_bind == second_bind
            || first_bind.is_unspecified()
            || second_bind.is_unspecified())
}

pub(super) fn validate_publication(
    publication: &RawPublication,
) -> Result<ValidatedPublication, ConfigError> {
    if publication.tailnet_addresses.is_empty() {
        return Err(ConfigError::new("publication.tailnet_addresses", "empty"));
    }
    let mut addresses = BTreeSet::new();
    for (index, value) in publication.tailnet_addresses.iter().enumerate() {
        let address = value.parse::<IpAddr>().map_err(|_| {
            ConfigError::new(
                format!("publication.tailnet_addresses[{index}]"),
                "invalid_ip",
            )
        })?;
        let valid = match address {
            IpAddr::V4(ip) => ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]),
            IpAddr::V6(ip) => ip.octets()[..6] == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0],
        };
        if !valid {
            return Err(ConfigError::new(
                format!("publication.tailnet_addresses[{index}]"),
                "not_tailnet_address",
            ));
        }
        if !addresses.insert(address) {
            return Err(ConfigError::new(
                format!("publication.tailnet_addresses[{index}]"),
                "duplicate",
            ));
        }
    }
    Ok(ValidatedPublication {
        tailnet_addresses: addresses.into_iter().collect(),
        public_routes: Vec::new(),
    })
}

pub(super) fn validate_public_routes(
    selectors: &[RawPermission],
    public_listener_configured: bool,
    routes: &[ValidatedRoute],
    adapters: &[ValidatedAdapter],
) -> Result<Vec<RouteSelector>, ConfigError> {
    let mut seen = BTreeSet::new();
    let mut validated = Vec::with_capacity(selectors.len());
    for (index, selector) in selectors.iter().enumerate() {
        let path = format!("publication.public_routes[{index}]");
        if !public_listener_configured {
            return Err(ConfigError::new(path, "public_listener_required"));
        }
        if !seen.insert((selector.model_alias.clone(), selector.operation)) {
            return Err(ConfigError::new(path, "duplicate"));
        }
        let Some(route) = routes.iter().find(|route| {
            route.identity.selector.model_alias.0 == selector.model_alias
                && route.identity.selector.operation == selector.operation
        }) else {
            return Err(ConfigError::new(path, "unknown_route_selector"));
        };
        let Some(adapter) = adapters
            .iter()
            .find(|adapter| adapter.id == route.adapter_id)
        else {
            return Err(ConfigError::new(path, "missing_adapter"));
        };
        if !adapter
            .capabilities
            .operations
            .contains(&selector.operation)
        {
            return Err(ConfigError::new(path, "unsupported_by_adapter"));
        }
        if adapter.kind.is_private_only() {
            return Err(ConfigError::new(
                path,
                if adapter.kind == ProviderKind::Codex {
                    "codex_not_public"
                } else {
                    "chatgpt_not_public"
                },
            ));
        }
        validated.push(RouteSelector {
            model_alias: ModelAlias(selector.model_alias.clone()),
            operation: selector.operation,
        });
    }
    Ok(validated)
}

pub(super) fn validate_url(adapter: &RawAdapter, path: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(&adapter.base_url)
        .map_err(|_| ConfigError::new(format!("{path}.base_url"), "invalid_url"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            "invalid_http_url",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            "userinfo_forbidden",
        ));
    }
    if url.fragment().is_some() {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            "fragment_forbidden",
        ));
    }
    if url.query().is_some() {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            "query_forbidden",
        ));
    }
    if adapter.trust_zone == TrustZone::External && url.scheme() != "https" {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            "https_required",
        ));
    }
    Ok(url)
}

pub(super) fn validate_provider_zone(adapter: &RawAdapter, path: &str) -> Result<(), ConfigError> {
    let valid = match adapter.kind {
        ProviderKind::Openrouter | ProviderKind::Codex | ProviderKind::Chatgpt => {
            adapter.trust_zone == TrustZone::External
        }
        ProviderKind::Vllm | ProviderKind::Speech => true,
        ProviderKind::Ollama | ProviderKind::AppleFm => matches!(
            adapter.trust_zone,
            TrustZone::Local | TrustZone::PrivateNetwork
        ),
    };
    if valid {
        Ok(())
    } else {
        Err(ConfigError::new(
            format!("{path}.trust_zone"),
            "provider_zone_mismatch",
        ))
    }
}

pub(super) fn validate_application_keys(
    keys: Vec<RawApplicationKey>,
    routes: &[ValidatedRoute],
) -> Result<Vec<ValidatedApplicationKey>, ConfigError> {
    let mut ids = BTreeSet::new();
    let mut digests = BTreeSet::new();
    let mut owner_seen = false;
    let mut validated = Vec::with_capacity(keys.len());
    for (index, key) in keys.into_iter().enumerate() {
        let path = format!("application_keys[{index}]");
        if key.owner {
            if owner_seen {
                return Err(ConfigError::new(format!("{path}.owner"), "multiple_owners"));
            }
            owner_seen = true;
        }
        if !valid_identifier(&key.id) || !ids.insert(key.id.clone()) {
            return Err(ConfigError::new(
                format!("{path}.id"),
                "invalid_or_duplicate_id",
            ));
        }
        let secret_ref = validate_secret_ref(&key.secret_ref, &format!("{path}.secret_ref"), true)?;
        if let SecretReference::Sha256(digest) = &secret_ref
            && !digests.insert(*digest)
        {
            return Err(ConfigError::new(
                format!("{path}.secret_ref"),
                "duplicate_secret",
            ));
        }
        let permissions = validate_permissions(&key.permissions, &path, Some(routes))?;
        let (max_in_flight, rate_limit) =
            validate_key_limits(key.max_in_flight, key.rate_limit, &path)?;
        validated.push(ValidatedApplicationKey {
            id: key.id,
            owner: key.owner,
            max_in_flight,
            rate_limit,
            secret_ref,
            permissions,
            expires_at: None,
            daily_quota: None,
        });
    }
    if ids.is_empty() {
        return Err(ConfigError::new("application_keys", "empty"));
    }
    Ok(validated)
}

/// Key permission rules shared by inline keys and the keys file. `routes = None`
/// (revoked records) checks syntax only.
pub(crate) fn validate_permissions(
    raw: &[RawPermission],
    path: &str,
    routes: Option<&[ValidatedRoute]>,
) -> Result<Vec<RouteSelector>, ConfigError> {
    if raw.is_empty() {
        return Err(ConfigError::new(format!("{path}.permissions"), "empty"));
    }
    let mut seen = BTreeSet::new();
    for (index, permission) in raw.iter().enumerate() {
        let permission_path = || format!("{path}.permissions[{index}]");
        if !seen.insert((permission.model_alias.as_str(), permission.operation)) {
            return Err(ConfigError::new(permission_path(), "duplicate"));
        }
        match routes {
            Some(routes)
                if !routes.iter().any(|route| {
                    route.identity.selector.model_alias.0 == permission.model_alias
                        && route.identity.selector.operation == permission.operation
                }) =>
            {
                return Err(ConfigError::new(
                    permission_path(),
                    "unknown_route_selector",
                ));
            }
            None if parse_model_alias(&permission.model_alias).is_none() => {
                return Err(ConfigError::new(permission_path(), "invalid_exact_alias"));
            }
            _ => {}
        }
    }
    Ok(raw
        .iter()
        .map(|permission| RouteSelector {
            model_alias: ModelAlias(permission.model_alias.clone()),
            operation: permission.operation,
        })
        .collect())
}

pub(crate) fn validate_key_limits(
    max_in_flight: Option<u64>,
    rate_limit: Option<RawRateLimit>,
    path: &str,
) -> Result<(Option<u64>, Option<KeyRateLimit>), ConfigError> {
    let max_in_flight = validate_optional_limit(max_in_flight, path)?;
    let rate_limit = rate_limit
        .map(|limit| validate_rate_limit(limit, path))
        .transpose()?;
    Ok((max_in_flight, rate_limit))
}

pub(super) fn load_key_file(
    raw: &RawKeys,
    config_dir: &Path,
    routes: &[ValidatedRoute],
    read_file: bool,
) -> Result<(Vec<ValidatedApplicationKey>, KeySource), ConfigError> {
    let path = config_dir.join(validate_key_path(&raw.file, "keys.file")?);
    let usage_dir = raw
        .usage_dir
        .as_deref()
        .map(|dir| validate_key_path(dir, "keys.usage_dir").map(|dir| config_dir.join(dir)))
        .transpose()?;
    if !read_file {
        return Ok((
            Vec::new(),
            KeySource::File {
                path,
                usage_dir,
                missing: false,
                sha256: None,
            },
        ));
    }
    let Some(bytes) = keys::file::read(&path)? else {
        return Ok((
            Vec::new(),
            KeySource::File {
                path,
                usage_dir,
                missing: true,
                sha256: None,
            },
        ));
    };
    let file = keys::file::parse(&bytes, routes)?;
    if usage_dir.is_none() && file.active().any(|key| key.daily_quota().is_some()) {
        return Err(ConfigError::new(
            "keys.usage_dir",
            "required_for_daily_quota",
        ));
    }
    Ok((
        application_keys_from_file(&file, routes)?,
        KeySource::File {
            path,
            usage_dir,
            missing: false,
            sha256: Some(file.sha256()),
        },
    ))
}

pub(super) fn validate_key_path<'a>(value: &'a str, path: &str) -> Result<&'a Path, ConfigError> {
    if value.is_empty() {
        return Err(ConfigError::new(path, "empty"));
    }
    if value.chars().any(char::is_control) {
        return Err(ConfigError::new(path, "invalid_path"));
    }
    Ok(Path::new(value))
}

/// Active records only; re-checks scopes so a file parsed elsewhere cannot
/// grant a selector this config does not route.
pub(super) fn application_keys_from_file(
    file: &KeysFile,
    routes: &[ValidatedRoute],
) -> Result<Vec<ValidatedApplicationKey>, ConfigError> {
    let mut keys = Vec::new();
    for (index, record) in file.records().iter().enumerate() {
        if record.is_revoked() {
            continue;
        }
        for (permission_index, selector) in record.permissions().iter().enumerate() {
            if !routes
                .iter()
                .any(|route| route.identity.selector == *selector)
            {
                return Err(ConfigError::new(
                    format!("keys_file.keys[{index}].permissions[{permission_index}]"),
                    "unknown_route_selector",
                ));
            }
        }
        keys.push(ValidatedApplicationKey {
            id: record.id().to_owned(),
            owner: record.is_owner(),
            secret_ref: SecretReference::Sha256(*record.digest()),
            permissions: record.permissions().to_vec(),
            max_in_flight: record.max_in_flight(),
            rate_limit: record.rate_limit(),
            expires_at: record.expires_at(),
            daily_quota: record.daily_quota(),
        });
    }
    Ok(keys)
}

pub(super) fn validate_secret_ref(
    value: &str,
    path: &str,
    allow_digest: bool,
) -> Result<SecretReference, ConfigError> {
    if let Some(hex) = value.strip_prefix("sha256:") {
        if !allow_digest {
            return Err(ConfigError::new(path, "digest_reference_unsupported"));
        }
        return parse_sha256_hex(hex)
            .map(SecretReference::Sha256)
            .ok_or_else(|| ConfigError::new(path, "invalid_secret_reference"));
    }
    if let Some(name) = value.strip_prefix("env:").filter(valid_env_name) {
        return Ok(SecretReference::Env(name.into()));
    }
    if let Some(name) = value
        .strip_prefix("file:")
        .filter(|name| Path::new(name).is_absolute())
    {
        return Ok(SecretReference::File(name.into()));
    }
    Err(ConfigError::new(path, "invalid_secret_reference"))
}

pub(super) fn validate_extension_allowlist(
    values: Vec<String>,
    path: &str,
) -> Result<BTreeSet<ExtensionKey>, ConfigError> {
    let mut validated = BTreeSet::new();
    for (index, value) in values.into_iter().enumerate() {
        let entry_path = format!("{path}[{index}]");
        let key = ExtensionKey::parse(value)
            .map_err(|_| ConfigError::new(entry_path.clone(), "invalid_extension_key"))?;
        if !validated.insert(key) {
            return Err(ConfigError::new(entry_path, "duplicate"));
        }
    }
    Ok(validated)
}

/// Optional concurrency cap: unset means uncapped, zero is rejected.
pub(super) fn validate_optional_limit(
    value: Option<u64>,
    path: &str,
) -> Result<Option<u64>, ConfigError> {
    match value {
        Some(0) => Err(ConfigError::new(format!("{path}.max_in_flight"), "zero")),
        Some(value) if value > MAX_CONCURRENCY_LIMIT => Err(ConfigError::new(
            format!("{path}.max_in_flight"),
            "limit_too_large",
        )),
        value => Ok(value),
    }
}

pub(super) fn validate_circuit_breaker(
    raw: Option<RawCircuitBreaker>,
    path: &str,
) -> Result<CircuitBreakerPolicy, ConfigError> {
    let defaults = CircuitBreakerPolicy::default();
    let Some(raw) = raw else {
        return Ok(defaults);
    };
    let policy = CircuitBreakerPolicy {
        enabled: raw.enabled.unwrap_or(defaults.enabled),
        failures: raw.failures.unwrap_or(defaults.failures),
        cooldown_ms: raw.cooldown_ms.unwrap_or(defaults.cooldown_ms),
    };
    if policy.failures == 0 {
        return Err(ConfigError::new(
            format!("{path}.circuit_breaker.failures"),
            "zero",
        ));
    }
    if policy.cooldown_ms == 0 {
        return Err(ConfigError::new(
            format!("{path}.circuit_breaker.cooldown_ms"),
            "zero",
        ));
    }
    if policy.cooldown_ms > MAX_TIMEOUT_MS {
        return Err(ConfigError::new(
            format!("{path}.circuit_breaker.cooldown_ms"),
            "timeout_too_large",
        ));
    }
    Ok(policy)
}

pub(super) fn validate_rate_limit(
    raw: RawRateLimit,
    path: &str,
) -> Result<KeyRateLimit, ConfigError> {
    if raw.requests == 0 {
        return Err(ConfigError::new(
            format!("{path}.rate_limit.requests"),
            "zero",
        ));
    }
    if raw.requests > MAX_CONCURRENCY_LIMIT {
        return Err(ConfigError::new(
            format!("{path}.rate_limit.requests"),
            "limit_too_large",
        ));
    }
    if raw.per_ms == 0 {
        return Err(ConfigError::new(
            format!("{path}.rate_limit.per_ms"),
            "zero",
        ));
    }
    if raw.per_ms > MAX_TIMEOUT_MS {
        return Err(ConfigError::new(
            format!("{path}.rate_limit.per_ms"),
            "timeout_too_large",
        ));
    }
    Ok(KeyRateLimit {
        requests: raw.requests,
        per_ms: raw.per_ms,
    })
}

pub(super) fn validate_bounds(
    limits: &RawLimits,
    timeouts: &RawTimeouts,
) -> Result<(ValidatedLimits, ValidatedTimeouts), ConfigError> {
    for (path, value) in [
        ("limits.max_queue", limits.max_queue),
        ("limits.max_in_flight", limits.max_in_flight),
        ("limits.max_body_bytes", limits.max_body_bytes),
        ("limits.max_audio_bytes", limits.max_audio_bytes),
        ("limits.max_extension_bytes", limits.max_extension_bytes),
    ] {
        if value == 0 {
            return Err(ConfigError::new(path, "zero"));
        }
    }
    if limits.max_audio_bytes > MAX_CONFIG_AUDIO_BYTES {
        return Err(ConfigError::new(
            "limits.max_audio_bytes",
            "audio_limit_too_large",
        ));
    }
    let encoded_audio_bytes = limits
        .max_audio_bytes
        .checked_add(2)
        .and_then(|value| value.checked_div(3))
        .and_then(|chunks| chunks.checked_mul(4))
        .ok_or_else(|| ConfigError::new("limits", "audio_envelope_overflow"))?;
    let max_audio_chat_body_bytes = limits
        .max_body_bytes
        .checked_add(encoded_audio_bytes)
        .ok_or_else(|| ConfigError::new("limits", "audio_envelope_overflow"))?;
    if timeouts.overall_ms == 0 {
        return Err(ConfigError::new("timeouts.overall_ms", "zero"));
    }
    if timeouts.overall_ms > MAX_TIMEOUT_MS {
        return Err(ConfigError::new("timeouts.overall_ms", "timeout_too_large"));
    }
    for (path, value) in [
        ("timeouts.queue_ms", timeouts.queue_ms),
        ("timeouts.connect_ms", timeouts.connect_ms),
        ("timeouts.headers_ms", timeouts.headers_ms),
        ("timeouts.first_byte_ms", timeouts.first_byte_ms),
        ("timeouts.idle_ms", timeouts.idle_ms),
    ] {
        if value == 0 {
            return Err(ConfigError::new(path, "zero"));
        }
        if value > MAX_TIMEOUT_MS {
            return Err(ConfigError::new(path, "timeout_too_large"));
        }
        if value > timeouts.overall_ms {
            return Err(ConfigError::new(path, "exceeds_overall"));
        }
    }
    let max_uploads = limits.max_uploads.unwrap_or(8);
    let max_buffered_bytes = limits.max_buffered_bytes.unwrap_or(256 * 1024 * 1024);
    if max_uploads == 0 || max_uploads > MAX_CONCURRENCY_LIMIT {
        return Err(ConfigError::new("limits.max_uploads", "out_of_range"));
    }
    if !(1..=4 * 1024 * 1024 * 1024).contains(&max_buffered_bytes) {
        return Err(ConfigError::new(
            "limits.max_buffered_bytes",
            "out_of_range",
        ));
    }
    let largest = max_audio_chat_body_bytes.max(
        limits
            .max_audio_bytes
            .saturating_add(crate::core::MAX_TRANSCRIPTION_OVERHEAD_BYTES as u64),
    );
    if largest
        .checked_mul(3)
        .is_none_or(|bytes| bytes > max_buffered_bytes)
    {
        return Err(ConfigError::new(
            "limits.max_buffered_bytes",
            "below_request_reservation",
        ));
    }
    let upload_ms = timeouts
        .upload_ms
        .unwrap_or(30_000.min(timeouts.overall_ms));
    if upload_ms == 0 || upload_ms > timeouts.overall_ms {
        return Err(ConfigError::new("timeouts.upload_ms", "out_of_range"));
    }
    Ok((
        ValidatedLimits {
            max_uploads,
            max_buffered_bytes,
            max_queue: limits.max_queue,
            max_in_flight: limits.max_in_flight,
            max_body_bytes: limits.max_body_bytes,
            max_audio_bytes: limits.max_audio_bytes,
            max_audio_chat_body_bytes,
            max_extension_bytes: limits.max_extension_bytes,
        },
        ValidatedTimeouts {
            upload_ms,
            queue_ms: timeouts.queue_ms,
            connect_ms: timeouts.connect_ms,
            headers_ms: timeouts.headers_ms,
            first_byte_ms: timeouts.first_byte_ms,
            idle_ms: timeouts.idle_ms,
            overall_ms: timeouts.overall_ms,
        },
    ))
}

pub(crate) fn parse_sha256_hex(value: &str) -> Option<[u8; 32]> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut digest = [0; 32];
    for (slot, [high, low]) in digest.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *slot = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Some(digest)
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
pub(crate) fn parse_model_alias(value: &str) -> Option<Option<ReasoningEffort>> {
    let Some((model, effort)) = value.split_once(':') else {
        return valid_identifier(value).then_some(None);
    };
    if !valid_identifier(model) || effort.contains(':') {
        return None;
    }
    let effort = ReasoningEffort::parse(effort)?;
    Some(Some(effort))
}
pub(super) fn valid_env_name(value: &&str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_uppercase() || (index > 0 && byte.is_ascii_digit())
        })
}

pub(super) fn valid_path_token(value: &&str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
pub(super) fn valid_diagnostic_path(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'[' | b']'))
}
