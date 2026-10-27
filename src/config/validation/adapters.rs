use super::*;

pub(in crate::config) fn validate_adapter(
    index: usize,
    adapter: RawAdapter,
    adapter_ids: &mut BTreeSet<String>,
) -> Result<ValidatedAdapter, ConfigError> {
    let path = format!("adapters[{index}]");
    if !valid_identifier(&adapter.id) || !adapter_ids.insert(adapter.id.clone()) {
        return Err(ConfigError::new(
            format!("{path}.id"),
            ConfigErrorClass::InvalidOrDuplicateId,
        ));
    }
    let base_url = validate_url(&adapter, &path)?;
    let ca_bundle = validate_ca_file(&adapter, &base_url, &path, index)?;
    let secret_ref = validate_adapter_secret(&adapter, &base_url, &path)?;
    validate_provider_zone(&adapter, &path)?;
    let extension_allowlist = validate_extension_allowlist(
        adapter.extension_allowlist,
        &format!("{path}.extension_allowlist"),
    )?;
    let operations = validate_operations(&adapter.capabilities.operations, &path)?;
    validate_kind_operations(
        adapter.kind,
        &adapter.capabilities,
        &operations,
        &base_url,
        &extension_allowlist,
        &path,
    )?;
    let anthropic = validate_anthropic(
        adapter.kind,
        adapter.anthropic,
        adapter.capabilities.input_audio,
        &operations,
        &base_url,
        &path,
    )?;
    if adapter.kind == ProviderKind::Anthropic && !extension_allowlist.is_empty() {
        return Err(ConfigError::new(
            format!("{path}.extension_allowlist"),
            ConfigErrorClass::UnsupportedByAdapterKind,
        ));
    }
    let transcription_mode = validate_transcription_mode(
        adapter.kind,
        adapter.transcription_mode,
        adapter.capabilities.input_audio,
        &operations,
        &path,
    )?;
    validate_capability_dependencies(adapter.kind, &adapter.capabilities, &operations, &path)?;
    let max_in_flight = validate_optional_limit(adapter.max_in_flight, &path)?;
    let circuit_breaker = validate_circuit_breaker(adapter.circuit_breaker, &path)?;
    Ok(ValidatedAdapter {
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
        ca_bundle,
        anthropic,
    })
}

/// `secret_ref` rules per kind; credentials need a base URL that may carry them.
fn validate_adapter_secret(
    adapter: &RawAdapter,
    base_url: &Url,
    path: &str,
) -> Result<Option<SecretReference>, ConfigError> {
    let secret_ref = if adapter.kind == ProviderKind::Chatgpt {
        if adapter.secret_ref.is_some() {
            return Err(ConfigError::new(
                format!("{path}.secret_ref"),
                ConfigErrorClass::ChatgptSecretRefUnsupported,
            ));
        }
        None
    } else if let Some(secret_ref) = &adapter.secret_ref {
        Some(validate_secret_ref(
            secret_ref,
            &format!("{path}.secret_ref"),
        )?)
    } else if matches!(
        adapter.kind,
        ProviderKind::Openrouter | ProviderKind::Cliproxy | ProviderKind::Anthropic
    ) || (matches!(adapter.kind, ProviderKind::Vllm | ProviderKind::Speech)
        && adapter.trust_zone == TrustZone::External)
    {
        return Err(ConfigError::new(
            format!("{path}.secret_ref"),
            ConfigErrorClass::Required,
        ));
    } else {
        None
    };
    if matches!(
        adapter.kind,
        ProviderKind::Vllm | ProviderKind::Speech | ProviderKind::Cliproxy
    ) && secret_ref.is_some()
        && !url_permits_credentials(base_url)
    {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            ConfigErrorClass::HttpsRequired,
        ));
    }
    Ok(secret_ref)
}

/// Declared operations, rejecting duplicates and an empty list.
fn validate_operations(
    operations: &[Operation],
    path: &str,
) -> Result<BTreeSet<Operation>, ConfigError> {
    let mut declared = BTreeSet::new();
    for (operation_index, operation) in operations.iter().enumerate() {
        if !declared.insert(*operation) {
            return Err(ConfigError::new(
                format!("{path}.capabilities.operations[{operation_index}]"),
                ConfigErrorClass::Duplicate,
            ));
        }
    }
    if declared.is_empty() {
        return Err(ConfigError::new(
            format!("{path}.capabilities.operations"),
            ConfigErrorClass::Empty,
        ));
    }
    Ok(declared)
}

/// Operation and capability limits of individual kinds.
fn validate_kind_operations(
    kind: ProviderKind,
    capabilities: &RawCapabilities,
    operations: &BTreeSet<Operation>,
    base_url: &Url,
    extension_allowlist: &BTreeSet<ExtensionKey>,
    path: &str,
) -> Result<(), ConfigError> {
    if operations.contains(&Operation::Speech) != (kind == ProviderKind::Speech) {
        return Err(ConfigError::new(
            format!("{path}.capabilities.operations"),
            ConfigErrorClass::SpeechAdapterRequired,
        ));
    }
    if kind == ProviderKind::Speech
        && (operations.len() != 1 || capabilities.streaming_chat || capabilities.function_tools)
    {
        return Err(ConfigError::new(
            format!("{path}.capabilities"),
            ConfigErrorClass::SpeechOnly,
        ));
    }
    if operations.contains(&Operation::Embeddings)
        && !matches!(
            kind,
            ProviderKind::Ollama | ProviderKind::Vllm | ProviderKind::Openrouter
        )
    {
        return Err(ConfigError::new(
            format!("{path}.capabilities.operations"),
            ConfigErrorClass::EmbeddingsUnsupportedByAdapterKind,
        ));
    }
    if kind == ProviderKind::Chatgpt {
        if *operations != BTreeSet::from([Operation::Chat]) || capabilities.input_audio {
            return Err(ConfigError::new(
                format!("{path}.capabilities"),
                ConfigErrorClass::ChatgptTextChatOnly,
            ));
        }
        if base_url.as_str() != "https://api.openai.com/v1" {
            return Err(ConfigError::new(
                format!("{path}.base_url"),
                ConfigErrorClass::ChatgptPinnedOriginRequired,
            ));
        }
        if !extension_allowlist.is_empty() {
            return Err(ConfigError::new(
                format!("{path}.extension_allowlist"),
                ConfigErrorClass::UnsupportedByAdapterKind,
            ));
        }
    }
    if kind == ProviderKind::AppleFm {
        if operations.contains(&Operation::Transcription) {
            return Err(ConfigError::new(
                format!("{path}.capabilities.operations"),
                ConfigErrorClass::AppleFmChatOnly,
            ));
        }
        // fm serve returns tool arguments as plain text and takes no audio.
        for (name, declared) in [
            ("function_tools", capabilities.function_tools),
            ("input_audio", capabilities.input_audio),
        ] {
            if declared {
                return Err(ConfigError::new(
                    format!("{path}.capabilities.{name}"),
                    ConfigErrorClass::UnsupportedByAdapterKind,
                ));
            }
        }
    }
    if kind == ProviderKind::Cliproxy {
        if *operations != BTreeSet::from([Operation::Chat]) {
            return Err(ConfigError::new(
                format!("{path}.capabilities.operations"),
                ConfigErrorClass::CliproxyChatOnly,
            ));
        }
        // Inline audio chat is a vLLM model feature, unverified through CLIProxyAPI.
        if capabilities.input_audio {
            return Err(ConfigError::new(
                format!("{path}.capabilities.input_audio"),
                ConfigErrorClass::UnsupportedByAdapterKind,
            ));
        }
    }
    Ok(())
}

/// `transcription_mode`: vLLM only, and required with its transcription operation.
fn validate_transcription_mode(
    kind: ProviderKind,
    mode: Option<VllmTranscriptionMode>,
    input_audio: bool,
    operations: &BTreeSet<Operation>,
    path: &str,
) -> Result<Option<VllmTranscriptionMode>, ConfigError> {
    Ok(match (kind, mode) {
        (ProviderKind::Vllm, Some(_)) if !operations.contains(&Operation::Transcription) => {
            return Err(ConfigError::new(
                format!("{path}.transcription_mode"),
                ConfigErrorClass::WithoutTranscriptionOperation,
            ));
        }
        (ProviderKind::Vllm, Some(VllmTranscriptionMode::AudioChat)) if !input_audio => {
            return Err(ConfigError::new(
                format!("{path}.transcription_mode"),
                ConfigErrorClass::RequiresInputAudioCapability,
            ));
        }
        (ProviderKind::Vllm, mode) if operations.contains(&Operation::Transcription) => {
            let Some(mode) = mode else {
                return Err(ConfigError::new(
                    format!("{path}.transcription_mode"),
                    ConfigErrorClass::RequiredForTranscription,
                ));
            };
            Some(mode)
        }
        (ProviderKind::Vllm, mode) => mode,
        (_, Some(_)) => {
            return Err(ConfigError::new(
                format!("{path}.transcription_mode"),
                ConfigErrorClass::VllmOnly,
            ));
        }
        (_, None) => None,
    })
}

/// Optional capabilities: supported by the kind and backed by the operations they need.
fn validate_capability_dependencies(
    kind: ProviderKind,
    capabilities: &RawCapabilities,
    operations: &BTreeSet<Operation>,
    path: &str,
) -> Result<(), ConfigError> {
    if capabilities.input_images {
        if !matches!(
            kind,
            ProviderKind::Ollama
                | ProviderKind::Vllm
                | ProviderKind::Openrouter
                | ProviderKind::Cliproxy
                | ProviderKind::Anthropic
        ) {
            return Err(ConfigError::new(
                format!("{path}.capabilities.input_images"),
                ConfigErrorClass::UnsupportedByAdapterKind,
            ));
        }
        if !operations.contains(&Operation::Chat) {
            return Err(ConfigError::new(
                format!("{path}.capabilities.input_images"),
                ConfigErrorClass::ChatOperationRequired,
            ));
        }
    }
    if capabilities.input_audio && !operations.contains(&Operation::Chat) {
        return Err(ConfigError::new(
            format!("{path}.capabilities.input_audio"),
            ConfigErrorClass::ChatOperationRequired,
        ));
    }
    if capabilities.audio_streaming_chat
        && (!capabilities.input_audio || !capabilities.streaming_chat)
    {
        return Err(ConfigError::new(
            format!("{path}.capabilities.audio_streaming_chat"),
            ConfigErrorClass::RequiresAudioAndStreamingChat,
        ));
    }
    if capabilities.audio_function_tools
        && (!capabilities.input_audio || !capabilities.function_tools)
    {
        return Err(ConfigError::new(
            format!("{path}.capabilities.audio_function_tools"),
            ConfigErrorClass::RequiresAudioAndFunctionTools,
        ));
    }
    let (structured_output, sampling_controls, reasoning_control) = kind.supports_chat_options();
    for (name, declared, supported) in [
        (
            "structured_output",
            capabilities.structured_output,
            structured_output,
        ),
        (
            "sampling_controls",
            capabilities.sampling_controls,
            sampling_controls,
        ),
        (
            "reasoning_control",
            capabilities.reasoning_control,
            reasoning_control,
        ),
    ] {
        if !declared {
            continue;
        }
        if !supported {
            return Err(ConfigError::new(
                format!("{path}.capabilities.{name}"),
                ConfigErrorClass::UnsupportedByAdapterKind,
            ));
        }
        if !operations.contains(&Operation::Chat) {
            return Err(ConfigError::new(
                format!("{path}.capabilities.{name}"),
                ConfigErrorClass::ChatOperationRequired,
            ));
        }
    }
    Ok(())
}

fn validate_url(adapter: &RawAdapter, path: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(&adapter.base_url)
        .map_err(|_| ConfigError::new(format!("{path}.base_url"), ConfigErrorClass::InvalidUrl))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            ConfigErrorClass::InvalidHttpUrl,
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            ConfigErrorClass::UserinfoForbidden,
        ));
    }
    if url.fragment().is_some() {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            ConfigErrorClass::FragmentForbidden,
        ));
    }
    if url.query().is_some() {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            ConfigErrorClass::QueryForbidden,
        ));
    }
    if adapter.trust_zone == TrustZone::External && !url_permits_credentials(&url) {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            ConfigErrorClass::HttpsRequired,
        ));
    }
    Ok(url)
}

/// Largest accepted `ca_file`; a private CA bundle holds a few certificates.
const MAX_CA_FILE_BYTES: usize = 64 * 1024;

/// `adapters[].ca_file`: PEM certificates that replace the platform roots for one adapter.
/// Only the path is checked here; [`ValidatedConfig::load_ca_bundles`] reads the file, so a
/// plane process never needs the bundle of an adapter it drops.
fn validate_ca_file(
    adapter: &RawAdapter,
    base_url: &Url,
    path: &str,
    index: usize,
) -> Result<Option<CaBundle>, ConfigError> {
    let Some(file) = &adapter.ca_file else {
        return Ok(None);
    };
    let field = format!("{path}.ca_file");
    // Only kinds that dial their configured base_url: ChatGPT pins a public origin,
    // and `fm serve` speaks plain HTTP.
    if !matches!(
        adapter.kind,
        ProviderKind::Ollama
            | ProviderKind::Vllm
            | ProviderKind::Openrouter
            | ProviderKind::Speech
            | ProviderKind::Cliproxy
    ) {
        return Err(ConfigError::new(
            field,
            ConfigErrorClass::UnsupportedByAdapterKind,
        ));
    }
    if base_url.scheme() != "https" {
        return Err(ConfigError::new(
            field,
            ConfigErrorClass::HttpsBaseUrlRequired,
        ));
    }
    let file = validate_key_path(file, &field)?;
    if !file.is_absolute() {
        return Err(ConfigError::new(
            field,
            ConfigErrorClass::AbsolutePathRequired,
        ));
    }
    Ok(Some(CaBundle {
        path: file.to_owned(),
        index,
        certificates: std::sync::Arc::from([]),
    }))
}

impl ValidatedConfig {
    /// Reads and parses every remaining adapter's `ca_file`. Errors name the adapter's index in
    /// the original config, which [`ValidatedConfig::for_plane`] may since have narrowed.
    pub(in crate::config) fn load_ca_bundles(mut self) -> Result<Self, ConfigError> {
        for bundle in self
            .adapters
            .iter_mut()
            .filter_map(|adapter| adapter.ca_bundle.as_mut())
        {
            bundle.certificates = read_ca_file(&bundle.path).map_err(|class| {
                ConfigError::new(format!("adapters[{}].ca_file", bundle.index), class)
            })?;
        }
        Ok(self)
    }
}

fn read_ca_file(
    file: &Path,
) -> Result<std::sync::Arc<[rustls::pki_types::CertificateDer<'static>]>, ConfigErrorClass> {
    use rustls::pki_types::{
        CertificateDer,
        pem::{PemObject as _, SectionKind},
    };
    use std::io::Read as _;

    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .and_then(|opened| {
            opened
                .take(MAX_CA_FILE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|_| ConfigErrorClass::ReadError)?;
    if bytes.len() > MAX_CA_FILE_BYTES {
        return Err(ConfigErrorClass::TooLarge);
    }
    let mut roots = rustls::RootCertStore::empty();
    let mut certificates = Vec::new();
    for section in <(SectionKind, Vec<u8>)>::pem_slice_iter(&bytes) {
        let (kind, der) = section.map_err(|_| ConfigErrorClass::InvalidPem)?;
        // A key or CSR in a trust bundle is a mistake worth failing on, not skipping.
        if !matches!(kind, SectionKind::Certificate) {
            return Err(ConfigErrorClass::CertificatesOnly);
        }
        let certificate = CertificateDer::from(der);
        // Parse as a trust anchor now so a bad bundle fails here, not on the first request.
        roots
            .add(certificate.clone())
            .map_err(|_| ConfigErrorClass::InvalidCertificate)?;
        certificates.push(certificate);
    }
    if certificates.is_empty() {
        return Err(ConfigErrorClass::NoCertificates);
    }
    Ok(certificates.into())
}

fn validate_provider_zone(adapter: &RawAdapter, path: &str) -> Result<(), ConfigError> {
    let valid = match adapter.kind {
        ProviderKind::Openrouter
        | ProviderKind::Chatgpt
        | ProviderKind::Cliproxy
        | ProviderKind::Anthropic => adapter.trust_zone == TrustZone::External,
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
            ConfigErrorClass::ProviderZoneMismatch,
        ))
    }
}

/// Chat-only kind pinned to the Claude API origin; `[adapters.anthropic]` belongs to it alone.
fn validate_anthropic(
    kind: ProviderKind,
    settings: Option<RawAnthropicSettings>,
    input_audio: bool,
    operations: &BTreeSet<Operation>,
    base_url: &Url,
    path: &str,
) -> Result<Option<ValidatedAnthropicSettings>, ConfigError> {
    if kind != ProviderKind::Anthropic {
        return match settings {
            Some(_) => Err(ConfigError::new(
                format!("{path}.anthropic"),
                ConfigErrorClass::AnthropicOnly,
            )),
            None => Ok(None),
        };
    }
    if *operations != BTreeSet::from([Operation::Chat]) {
        return Err(ConfigError::new(
            format!("{path}.capabilities.operations"),
            ConfigErrorClass::AnthropicChatOnly,
        ));
    }
    if input_audio {
        return Err(ConfigError::new(
            format!("{path}.capabilities.input_audio"),
            ConfigErrorClass::UnsupportedByAdapterKind,
        ));
    }
    if base_url.as_str() != "https://api.anthropic.com/v1" {
        return Err(ConfigError::new(
            format!("{path}.base_url"),
            ConfigErrorClass::AnthropicPinnedOriginRequired,
        ));
    }
    let settings = settings.unwrap_or(RawAnthropicSettings {
        workspace_id: None,
        default_max_output_tokens: None,
    });
    if let Some(workspace_id) = &settings.workspace_id {
        let valid = workspace_id.len() <= 128
            && workspace_id.strip_prefix("wrkspc_").is_some_and(|rest| {
                !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_alphanumeric())
            });
        if !valid {
            return Err(ConfigError::new(
                format!("{path}.anthropic.workspace_id"),
                ConfigErrorClass::InvalidWorkspaceId,
            ));
        }
    }
    let default_max_output_tokens = settings
        .default_max_output_tokens
        .unwrap_or(DEFAULT_ANTHROPIC_MAX_OUTPUT_TOKENS);
    if !(1..=crate::core::MAX_OUTPUT_TOKENS).contains(&default_max_output_tokens) {
        return Err(ConfigError::new(
            format!("{path}.anthropic.default_max_output_tokens"),
            ConfigErrorClass::OutOfRange,
        ));
    }
    Ok(Some(ValidatedAnthropicSettings {
        workspace_id: settings.workspace_id,
        default_max_output_tokens,
    }))
}

fn validate_secret_ref(value: &str, path: &str) -> Result<SecretReference, ConfigError> {
    if value.starts_with("sha256:") {
        return Err(ConfigError::new(
            path,
            ConfigErrorClass::DigestReferenceUnsupported,
        ));
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
    Err(ConfigError::new(
        path,
        ConfigErrorClass::InvalidSecretReference,
    ))
}

fn validate_circuit_breaker(
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
            ConfigErrorClass::Zero,
        ));
    }
    if policy.cooldown_ms == 0 {
        return Err(ConfigError::new(
            format!("{path}.circuit_breaker.cooldown_ms"),
            ConfigErrorClass::Zero,
        ));
    }
    if policy.cooldown_ms > MAX_TIMEOUT_MS {
        return Err(ConfigError::new(
            format!("{path}.circuit_breaker.cooldown_ms"),
            ConfigErrorClass::TimeoutTooLarge,
        ));
    }
    Ok(policy)
}
