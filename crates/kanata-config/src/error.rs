use std::fmt;

str_enum! {
    /// Every class a [`ConfigError`] can carry; `config/README.md` documents each one.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ConfigErrorClass {
        // Parsing and reading.
        ParseError => "parse_error",
        SchemaError => "schema_error",
        TypeError => "type_error",
        UnknownField => "unknown_field",
        InlineKeysRemoved => "inline_keys_removed",
        ReadError => "read_error",
        SerializationError => "serialization_error",
        // Shared value rules.
        Required => "required",
        Empty => "empty",
        Zero => "zero",
        Duplicate => "duplicate",
        OutOfRange => "out_of_range",
        TooLarge => "too_large",
        TooMany => "too_many",
        LimitTooLarge => "limit_too_large",
        TimeoutTooLarge => "timeout_too_large",
        InvalidOrDuplicateId => "invalid_or_duplicate_id",
        InvalidOrEmpty => "invalid_or_empty",
        InvalidExactAlias => "invalid_exact_alias",
        InvalidPath => "invalid_path",
        InvalidAbsolutePath => "invalid_absolute_path",
        AbsolutePathRequired => "absolute_path_required",
        InvalidExtensionKey => "invalid_extension_key",
        InvalidAllowlist => "invalid_allowlist",
        InvalidIp => "invalid_ip",
        // Listeners and publication.
        NotLoopback => "not_loopback",
        NotInternalAddress => "not_internal_address",
        NotConcrete => "not_concrete",
        TailnetAddressForbidden => "tailnet_address_forbidden",
        ClientAdminPortCollision => "client_admin_port_collision",
        ClientPortCollision => "client_port_collision",
        ClientBindCollision => "client_bind_collision",
        NotTailnetAddress => "not_tailnet_address",
        PublicListenerRequired => "public_listener_required",
        UnknownRouteSelector => "unknown_route_selector",
        ChatgptNotPublic => "chatgpt_not_public",
        CliproxyNotPublic => "cliproxy_not_public",
        AnthropicNotPublic => "anthropic_not_public",
        RequiredForPublicPlane => "required_for_public_plane",
        // Limits and timeouts.
        AudioLimitTooLarge => "audio_limit_too_large",
        AudioEnvelopeOverflow => "audio_envelope_overflow",
        ExceedsOverall => "exceeds_overall",
        BelowRequestReservation => "below_request_reservation",
        // Adapters.
        InvalidUrl => "invalid_url",
        InvalidHttpUrl => "invalid_http_url",
        UserinfoForbidden => "userinfo_forbidden",
        FragmentForbidden => "fragment_forbidden",
        QueryForbidden => "query_forbidden",
        HttpsRequired => "https_required",
        HttpsBaseUrlRequired => "https_base_url_required",
        InvalidSecretReference => "invalid_secret_reference",
        DigestReferenceUnsupported => "digest_reference_unsupported",
        ProviderZoneMismatch => "provider_zone_mismatch",
        UnsupportedByAdapterKind => "unsupported_by_adapter_kind",
        ChatOperationRequired => "chat_operation_required",
        SpeechAdapterRequired => "speech_adapter_required",
        SpeechOnly => "speech_only",
        EmbeddingsUnsupportedByAdapterKind => "embeddings_unsupported_by_adapter_kind",
        ChatgptTextChatOnly => "chatgpt_text_chat_only",
        ChatgptPinnedOriginRequired => "chatgpt_pinned_origin_required",
        ChatgptSecretRefUnsupported => "chatgpt_secret_ref_unsupported",
        WithoutChatgptAdapter => "without_chatgpt_adapter",
        AppleFmChatOnly => "apple_fm_chat_only",
        CliproxyChatOnly => "cliproxy_chat_only",
        AnthropicChatOnly => "anthropic_chat_only",
        AnthropicOnly => "anthropic_only",
        AnthropicPinnedOriginRequired => "anthropic_pinned_origin_required",
        InvalidWorkspaceId => "invalid_workspace_id",
        WithoutTranscriptionOperation => "without_transcription_operation",
        RequiresInputAudioCapability => "requires_input_audio_capability",
        RequiredForTranscription => "required_for_transcription",
        VllmOnly => "vllm_only",
        RequiresAudioAndStreamingChat => "requires_audio_and_streaming_chat",
        RequiresAudioAndFunctionTools => "requires_audio_and_function_tools",
        InvalidPem => "invalid_pem",
        CertificatesOnly => "certificates_only",
        InvalidCertificate => "invalid_certificate",
        NoCertificates => "no_certificates",
        // Routes.
        MissingAdapter => "missing_adapter",
        DuplicateSelector => "duplicate_selector",
        EffortAliasRequiresReasoningEffort => "effort_alias_requires_reasoning_effort",
        UnsupportedByAdapter => "unsupported_by_adapter",
        UnsupportedEffort => "unsupported_effort",
        DoesNotMatchAlias => "does_not_match_alias",
        ChatOnly => "chat_only",
        UnsupportedOutputCap => "unsupported_output_cap",
        ExceedsContextTokens => "exceeds_context_tokens",
        RequiresChatTemplate => "requires_chat_template",
        ConflictsWithReasoningControl => "conflicts_with_reasoning_control",
        InconsistentReasoningFamily => "inconsistent_reasoning_family",
        // Catalogs and fragments.
        TooManyFragments => "too_many_fragments",
        RelativeFragmentRequired => "relative_fragment_required",
        OutsideConfigDirectory => "outside_config_directory",
        DuplicateFragment => "duplicate_fragment",
        DocumentTooLarge => "document_too_large",
        ChangedDuringRead => "changed_during_read",
        InvalidProfile => "invalid_profile",
        ModelOnlyField => "model_only_field",
        MissingProfile => "missing_profile",
        InvalidClearField => "invalid_clear_field",
        ConflictingOverride => "conflicting_override",
        ReasoningChatFamilyRequired => "reasoning_chat_family_required",
        ConflictingEffortOptions => "conflicting_effort_options",
        UnknownVariant => "unknown_variant",
        // Keys.
        RequiredForDailyQuota => "required_for_daily_quota",
        RequiredForMonthlyQuota => "required_for_monthly_quota",
        NotFound => "not_found",
        InsecurePermissions => "insecure_permissions",
        UnsupportedVersion => "unsupported_version",
        InvalidTimestamp => "invalid_timestamp",
        InvalidDigest => "invalid_digest",
        DuplicateSecret => "duplicate_secret",
        MultipleOwners => "multiple_owners",
        InvalidTokenReservation => "invalid_token_reservation",
        BuildError => "build_error",
        // Reload and config commands.
        RestartRequired => "restart_required",
        AlreadyExists => "already_exists",
        WriteError => "write_error",
        RandomError => "random_error",
        NotEquivalent => "not_equivalent",
    }
    pub const fn as_str;
    const ALL;
}

impl fmt::Display for ConfigErrorClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug)]
pub struct ConfigError {
    pub(super) path: String,
    class: ConfigErrorClass,
}

impl ConfigError {
    pub fn new(path: impl Into<String>, class: ConfigErrorClass) -> Self {
        Self {
            path: path.into(),
            class,
        }
    }

    pub fn class(&self) -> &'static str {
        self.class.as_str()
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "config error at {}: {}", self.path, self.class)
    }
}

impl std::error::Error for ConfigError {}
