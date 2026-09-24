use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

use crate::core::{
    Capabilities, ExtensionKey, MAX_INPUT_AUDIO_BYTES, ModelAlias, Operation, ReasoningEffort,
    RouteIdentity, RouteSelector, TrustZone,
};

pub const MAX_TIMEOUT_MS: u64 = 604_800_000;
pub const MAX_CONFIG_AUDIO_BYTES: u64 = MAX_INPUT_AUDIO_BYTES as u64;

#[derive(Clone, Debug)]
pub struct ValidatedConfig {
    listeners: ValidatedListeners,
    publication: ValidatedPublication,
    codex_auth: Option<ValidatedCodexAuth>,
    adapters: Vec<ValidatedAdapter>,
    routes: Vec<ValidatedRoute>,
    application_keys: Vec<ValidatedApplicationKey>,
    limits: ValidatedLimits,
    timeouts: ValidatedTimeouts,
    logging: ValidatedLogging,
}

impl ValidatedConfig {
    pub fn listeners(&self) -> &ValidatedListeners {
        &self.listeners
    }
    pub fn publication(&self) -> &ValidatedPublication {
        &self.publication
    }
    pub fn codex_auth(&self) -> Option<&ValidatedCodexAuth> {
        self.codex_auth.as_ref()
    }
    pub fn adapters(&self) -> &[ValidatedAdapter] {
        &self.adapters
    }
    pub fn routes(&self) -> &[ValidatedRoute] {
        &self.routes
    }
    pub fn application_keys(&self) -> &[ValidatedApplicationKey] {
        &self.application_keys
    }
    pub fn limits(&self) -> &ValidatedLimits {
        &self.limits
    }
    pub fn timeouts(&self) -> &ValidatedTimeouts {
        &self.timeouts
    }
    pub fn logging(&self) -> &ValidatedLogging {
        &self.logging
    }

    /// Narrows the config to what one `kanata serve --plane` process needs.
    pub fn for_plane(&self, plane: Plane) -> Result<Self, ConfigError> {
        let mut config = self.clone();
        match plane {
            Plane::All => {}
            Plane::Private => {
                config.listeners.public = None;
                config.publication.public_routes.clear();
            }
            Plane::Public => {
                if config.listeners.public.is_none() {
                    return Err(ConfigError::new(
                        "listeners.public",
                        "required_for_public_plane",
                    ));
                }
                let public = config.publication.public_routes.clone();
                let codex_selectors: Vec<RouteSelector> = config
                    .routes
                    .iter()
                    .filter(|route| {
                        config.adapters.iter().any(|adapter| {
                            adapter.id == route.adapter_id && adapter.kind == ProviderKind::Codex
                        })
                    })
                    .map(|route| route.identity.selector.clone())
                    .collect();
                // Container loopback only: the public process serves no private clients.
                config.listeners.client.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
                config
                    .routes
                    .retain(|route| public.contains(&route.identity.selector));
                let adapter_ids: BTreeSet<String> = config
                    .routes
                    .iter()
                    .map(|route| route.adapter_id.clone())
                    .collect();
                config
                    .adapters
                    .retain(|adapter| adapter_ids.contains(&adapter.id));
                config.codex_auth = None;
                config.application_keys = config
                    .application_keys
                    .into_iter()
                    // Keys that can reach Codex never enter the public process.
                    .filter(|key| {
                        !key.owner
                            && key
                                .permissions
                                .iter()
                                .all(|selector| !codex_selectors.contains(selector))
                    })
                    .filter_map(|mut key| {
                        key.permissions.retain(|selector| public.contains(selector));
                        (!key.permissions.is_empty()).then_some(key)
                    })
                    .collect();
            }
        }
        Ok(config)
    }
}

/// Which listeners one `kanata serve` process runs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Plane {
    /// Private and public listeners in one process.
    #[default]
    All,
    /// Private listener only; no public listener or public routes.
    Private,
    /// Public listener only, with just the public routes, their adapters and non-owner keys.
    Public,
}

impl Plane {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "private" => Some(Self::Private),
            "public" => Some(Self::Public),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ValidatedLogging {
    level: LogLevel,
    format: LogFormat,
}

impl ValidatedLogging {
    pub fn level(&self) -> LogLevel {
        self.level
    }
    pub fn format(&self) -> LogFormat {
        self.format
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedListeners {
    client: ValidatedListener,
    admin: ValidatedListener,
    public: Option<ValidatedListener>,
}
impl ValidatedListeners {
    pub fn client(&self) -> &ValidatedListener {
        &self.client
    }
    pub fn admin(&self) -> &ValidatedListener {
        &self.admin
    }
    pub fn public(&self) -> Option<&ValidatedListener> {
        self.public.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedListener {
    bind: IpAddr,
    port: u16,
}
impl ValidatedListener {
    pub fn bind(&self) -> IpAddr {
        self.bind
    }
    pub fn port(&self) -> u16 {
        self.port
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedPublication {
    tailnet_addresses: Vec<IpAddr>,
    public_routes: Vec<RouteSelector>,
}
impl ValidatedPublication {
    pub fn tailnet_addresses(&self) -> &[IpAddr] {
        &self.tailnet_addresses
    }
    pub fn public_routes(&self) -> &[RouteSelector] {
        &self.public_routes
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CodexAuthStore {
    Keyring,
    File,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CodexReasoningEffort {
    Low,
    Medium,
    High,
}

impl CodexReasoningEffort {
    /// Maps a client effort onto the subset the Responses backend accepts.
    pub fn from_request(effort: ReasoningEffort) -> Option<Self> {
        match effort {
            ReasoningEffort::Low => Some(Self::Low),
            ReasoningEffort::Medium => Some(Self::Medium),
            ReasoningEffort::High => Some(Self::High),
            ReasoningEffort::None
            | ReasoningEffort::Minimal
            | ReasoningEffort::Xhigh
            | ReasoningEffort::Max => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedCodexAuth {
    store: CodexAuthStore,
    state_dir: PathBuf,
}
impl ValidatedCodexAuth {
    pub fn store(&self) -> CodexAuthStore {
        self.store
    }
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedAdapter {
    id: String,
    kind: ProviderKind,
    base_url: Url,
    trust_zone: TrustZone,
    secret_ref: Option<SecretReference>,
    transcription_mode: Option<VllmTranscriptionMode>,
    extension_allowlist: BTreeSet<ExtensionKey>,
    capabilities: Capabilities,
}
impl ValidatedAdapter {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn kind(&self) -> ProviderKind {
        self.kind
    }
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }
    pub fn trust_zone(&self) -> TrustZone {
        self.trust_zone
    }
    pub fn transcription_mode(&self) -> Option<VllmTranscriptionMode> {
        self.transcription_mode
    }
    pub fn secret_ref(&self) -> Option<&SecretReference> {
        self.secret_ref.as_ref()
    }
    pub fn extension_allowlist(&self) -> &BTreeSet<ExtensionKey> {
        &self.extension_allowlist
    }
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretReference {
    Env(String),
    File(PathBuf),
    /// SHA-256 digest of an application key; never resolved to plaintext.
    Sha256([u8; 32]),
}
impl SecretReference {
    pub fn env_name(&self) -> Option<&str> {
        match self {
            Self::Env(name) => Some(name),
            Self::File(_) | Self::Sha256(_) => None,
        }
    }
    pub fn file_path(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Env(_) | Self::Sha256(_) => None,
        }
    }
    pub fn sha256_digest(&self) -> Option<&[u8; 32]> {
        match self {
            Self::Sha256(digest) => Some(digest),
            Self::Env(_) | Self::File(_) => None,
        }
    }
}

const MIN_CONTEXT_TOKENS: u32 = 256;
const MAX_CONTEXT_TOKENS: u32 = 16_777_216;

#[derive(Clone, Debug)]
pub struct ValidatedRoute {
    identity: RouteIdentity,
    adapter_id: String,
    codex_reasoning_effort: Option<CodexReasoningEffort>,
    extension_allowlist: BTreeSet<ExtensionKey>,
    requires_streaming_chat: bool,
    requires_function_tools: bool,
    allows_input_audio: bool,
    allows_audio_streaming_chat: bool,
    allows_audio_function_tools: bool,
    context_tokens: Option<u32>,
}
impl ValidatedRoute {
    pub fn identity(&self) -> &RouteIdentity {
        &self.identity
    }
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }
    pub fn codex_reasoning_effort(&self) -> Option<CodexReasoningEffort> {
        self.codex_reasoning_effort
    }
    pub fn extension_allowlist(&self) -> &BTreeSet<ExtensionKey> {
        &self.extension_allowlist
    }
    pub fn requires_streaming_chat(&self) -> bool {
        self.requires_streaming_chat
    }
    pub fn requires_function_tools(&self) -> bool {
        self.requires_function_tools
    }
    pub fn allows_input_audio(&self) -> bool {
        self.allows_input_audio
    }
    pub fn allows_audio_streaming_chat(&self) -> bool {
        self.allows_audio_streaming_chat
    }
    pub fn allows_audio_function_tools(&self) -> bool {
        self.allows_audio_function_tools
    }
    /// Declared upstream context window; `None` means the provider's own.
    pub fn context_tokens(&self) -> Option<u32> {
        self.context_tokens
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedApplicationKey {
    id: String,
    owner: bool,
    secret_ref: SecretReference,
    permissions: Vec<RouteSelector>,
}
impl ValidatedApplicationKey {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn is_owner(&self) -> bool {
        self.owner
    }
    pub fn secret_ref(&self) -> &SecretReference {
        &self.secret_ref
    }
    pub fn permissions(&self) -> &[RouteSelector] {
        &self.permissions
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedLimits {
    max_queue: u64,
    max_in_flight: u64,
    max_body_bytes: u64,
    max_audio_bytes: u64,
    max_audio_chat_body_bytes: u64,
    max_extension_bytes: u64,
}
impl ValidatedLimits {
    pub fn max_queue(&self) -> u64 {
        self.max_queue
    }
    pub fn max_in_flight(&self) -> u64 {
        self.max_in_flight
    }
    pub fn max_body_bytes(&self) -> u64 {
        self.max_body_bytes
    }
    pub fn max_audio_bytes(&self) -> u64 {
        self.max_audio_bytes
    }
    pub fn max_audio_chat_body_bytes(&self) -> u64 {
        self.max_audio_chat_body_bytes
    }
    pub fn max_extension_bytes(&self) -> u64 {
        self.max_extension_bytes
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedTimeouts {
    queue_ms: u64,
    connect_ms: u64,
    headers_ms: u64,
    first_byte_ms: u64,
    idle_ms: u64,
    overall_ms: u64,
}
impl ValidatedTimeouts {
    pub fn queue_ms(&self) -> u64 {
        self.queue_ms
    }
    pub fn connect_ms(&self) -> u64 {
        self.connect_ms
    }
    pub fn headers_ms(&self) -> u64 {
        self.headers_ms
    }
    pub fn first_byte_ms(&self) -> u64 {
        self.first_byte_ms
    }
    pub fn idle_ms(&self) -> u64 {
        self.idle_ms
    }
    pub fn overall_ms(&self) -> u64 {
        self.overall_ms
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Ollama,
    Vllm,
    Openrouter,
    Codex,
    /// Apple Foundation Models through macOS `fm serve`.
    AppleFm,
}

impl ProviderKind {
    pub fn accepts_reasoning_effort(self, effort: ReasoningEffort) -> bool {
        match self {
            Self::Codex => CodexReasoningEffort::from_request(effort).is_some(),
            Self::Ollama | Self::Vllm | Self::Openrouter => true,
            Self::AppleFm => false,
        }
    }

    /// Config name, used as the provider label in logs and metrics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Vllm => "vllm",
            Self::Openrouter => "openrouter",
            Self::Codex => "codex",
            Self::AppleFm => "apple_fm",
        }
    }

    /// Accepted efforts in level order; `None` when the provider passes efforts through.
    pub fn restricted_reasoning_efforts(self) -> Option<Vec<ReasoningEffort>> {
        const ALL: [ReasoningEffort; 7] = [
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
        ];
        let accepted: Vec<_> = ALL
            .into_iter()
            .filter(|effort| self.accepts_reasoning_effort(*effort))
            .collect();
        (accepted.len() < ALL.len()).then_some(accepted)
    }

    // Chat option capabilities each adapter kind actually encodes:
    // structured_output / sampling_controls / reasoning_control.
    fn supports_chat_options(self) -> (bool, bool, bool) {
        match self {
            Self::Ollama | Self::Openrouter => (true, true, true),
            Self::Vllm => (true, true, false),
            Self::Codex => (false, false, true),
            Self::AppleFm => (true, true, false),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VllmTranscriptionMode {
    NativeAsr,
    AudioChat,
}

#[derive(Debug)]
pub struct ConfigError {
    path: String,
    class: &'static str,
}

impl ConfigError {
    fn new(path: impl Into<String>, class: &'static str) -> Self {
        Self {
            path: path.into(),
            class,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "config error at {}: {}", self.path, self.class)
    }
}

impl std::error::Error for ConfigError {}

fn sanitized_deserialize_error(error: serde_path_to_error::Error<toml::de::Error>) -> ConfigError {
    let message = error.inner().to_string();
    let mut path = error.path().to_string();
    let class = if message.contains("unknown field") || message.contains("unknown key") {
        if path.is_empty()
            && let Some(field) = message.split('`').nth(1).filter(valid_path_token)
        {
            path = field.into();
        }
        "unknown_field"
    } else if message.contains("invalid type") {
        "type_error"
    } else {
        "schema_error"
    };
    if !valid_diagnostic_path(&path) {
        path = "config".into();
    }
    ConfigError::new(path, class)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listeners: RawListeners,
    publication: RawPublication,
    #[serde(default)]
    codex_auth: Option<RawCodexAuth>,
    adapters: Vec<RawAdapter>,
    routes: Vec<RawRoute>,
    application_keys: Vec<RawApplicationKey>,
    limits: RawLimits,
    timeouts: RawTimeouts,
    #[serde(default)]
    logging: RawLogging,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogging {
    #[serde(default)]
    level: LogLevel,
    #[serde(default)]
    format: LogFormat,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawListeners {
    client: RawListener,
    admin: RawListener,
    #[serde(default)]
    public: Option<RawListener>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawListener {
    bind: String,
    port: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublication {
    tailnet_addresses: Vec<String>,
    #[serde(default)]
    public_routes: Vec<RawPermission>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCodexAuth {
    #[serde(default)]
    store: Option<CodexAuthStore>,
    #[serde(default)]
    state_dir: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdapter {
    id: String,
    kind: ProviderKind,
    base_url: String,
    trust_zone: TrustZone,
    secret_ref: Option<String>,
    #[serde(default)]
    transcription_mode: Option<VllmTranscriptionMode>,
    #[serde(default)]
    extension_allowlist: Vec<String>,
    capabilities: RawCapabilities,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapabilities {
    operations: Vec<Operation>,
    streaming_chat: bool,
    function_tools: bool,
    #[serde(default)]
    input_audio: bool,
    #[serde(default)]
    audio_streaming_chat: bool,
    #[serde(default)]
    audio_function_tools: bool,
    #[serde(default)]
    structured_output: bool,
    #[serde(default)]
    sampling_controls: bool,
    #[serde(default)]
    reasoning_control: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    id: String,
    model_alias: String,
    operation: Operation,
    adapter_id: String,
    upstream_id: String,
    #[serde(default)]
    codex_reasoning_effort: Option<CodexReasoningEffort>,
    #[serde(default)]
    extension_allowlist: Vec<String>,
    requires_streaming_chat: bool,
    requires_function_tools: bool,
    #[serde(default)]
    allows_input_audio: bool,
    #[serde(default)]
    allows_audio_streaming_chat: bool,
    #[serde(default)]
    allows_audio_function_tools: bool,
    #[serde(default)]
    context_tokens: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApplicationKey {
    id: String,
    secret_ref: String,
    #[serde(default)]
    owner: bool,
    permissions: Vec<RawPermission>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermission {
    model_alias: String,
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    max_queue: u64,
    max_in_flight: u64,
    max_body_bytes: u64,
    max_audio_bytes: u64,
    max_extension_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTimeouts {
    queue_ms: u64,
    connect_ms: u64,
    headers_ms: u64,
    first_byte_ms: u64,
    idle_ms: u64,
    overall_ms: u64,
}

pub fn load(path: impl AsRef<Path>) -> Result<ValidatedConfig, ConfigError> {
    let contents =
        fs::read_to_string(path).map_err(|_| ConfigError::new("config", "read_error"))?;
    let deserializer = toml::de::Deserializer::parse(&contents)
        .map_err(|_| ConfigError::new("config", "parse_error"))?;
    let raw =
        serde_path_to_error::deserialize(deserializer).map_err(sanitized_deserialize_error)?;
    validate(raw)
}

fn validate(raw: RawConfig) -> Result<ValidatedConfig, ConfigError> {
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
        let secret_ref = if adapter.kind == ProviderKind::Codex {
            if adapter.secret_ref.is_some() {
                return Err(ConfigError::new(
                    format!("{path}.secret_ref"),
                    "codex_secret_ref_unsupported",
                ));
            }
            None
        } else if let Some(secret_ref) = &adapter.secret_ref {
            Some(validate_secret_ref(
                secret_ref,
                &format!("{path}.secret_ref"),
                false,
            )?)
        } else if adapter.kind == ProviderKind::Openrouter {
            return Err(ConfigError::new(format!("{path}.secret_ref"), "required"));
        } else {
            None
        };
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
                audio_streaming_chat: adapter.capabilities.audio_streaming_chat,
                audio_function_tools: adapter.capabilities.audio_function_tools,
                structured_output: adapter.capabilities.structured_output,
                sampling_controls: adapter.capabilities.sampling_controls,
                reasoning_control: adapter.capabilities.reasoning_control,
            },
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
                    (Some(alias), Some(field)) if alias != field => {
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
            extension_allowlist,
            requires_streaming_chat: route.requires_streaming_chat,
            requires_function_tools: route.requires_function_tools,
            allows_input_audio: route.allows_input_audio,
            allows_audio_streaming_chat: route.allows_audio_streaming_chat,
            allows_audio_function_tools: route.allows_audio_function_tools,
            context_tokens: route.context_tokens,
        });
    }
    if routes.is_empty() {
        return Err(ConfigError::new("routes", "empty"));
    }

    publication.public_routes = validate_public_routes(
        &raw.publication.public_routes,
        listeners.public.is_some(),
        &routes,
        &adapters,
    )?;
    let application_keys = validate_application_keys(raw.application_keys, &routes)?;
    Ok(ValidatedConfig {
        listeners,
        publication,
        codex_auth,
        adapters,
        routes,
        application_keys,
        limits,
        timeouts,
        logging: ValidatedLogging {
            level: raw.logging.level,
            format: raw.logging.format,
        },
    })
}

fn validate_codex_auth(auth: RawCodexAuth) -> Result<ValidatedCodexAuth, ConfigError> {
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

fn validate_listeners(
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

fn is_private_public_bind(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private(),
        IpAddr::V6(address) => address.is_unique_local(),
    }
}

fn shares_configured_tailnet_prefix(address: IpAddr, tailnet_addresses: &[IpAddr]) -> bool {
    match address {
        IpAddr::V4(_) => false,
        IpAddr::V6(address) => tailnet_addresses.iter().any(|tailnet| match tailnet {
            IpAddr::V4(_) => false,
            IpAddr::V6(tailnet) => address.octets()[..6] == tailnet.octets()[..6],
        }),
    }
}

fn listener_bindings_conflict(
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

fn validate_publication(publication: &RawPublication) -> Result<ValidatedPublication, ConfigError> {
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

fn validate_public_routes(
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
        if adapter.kind == ProviderKind::Codex {
            return Err(ConfigError::new(path, "codex_not_public"));
        }
        validated.push(RouteSelector {
            model_alias: ModelAlias(selector.model_alias.clone()),
            operation: selector.operation,
        });
    }
    Ok(validated)
}

fn validate_url(adapter: &RawAdapter, path: &str) -> Result<Url, ConfigError> {
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

fn validate_provider_zone(adapter: &RawAdapter, path: &str) -> Result<(), ConfigError> {
    let valid = match adapter.kind {
        ProviderKind::Openrouter | ProviderKind::Codex => adapter.trust_zone == TrustZone::External,
        ProviderKind::Ollama | ProviderKind::Vllm | ProviderKind::AppleFm => matches!(
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

fn validate_application_keys(
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
        if key.permissions.is_empty() {
            return Err(ConfigError::new(format!("{path}.permissions"), "empty"));
        }
        let mut permissions = BTreeSet::new();
        for (permission_index, permission) in key.permissions.iter().enumerate() {
            if !permissions.insert((permission.model_alias.clone(), permission.operation)) {
                return Err(ConfigError::new(
                    format!("{path}.permissions[{permission_index}]"),
                    "duplicate",
                ));
            }
            if !routes.iter().any(|route| {
                route.identity.selector.model_alias.0 == permission.model_alias
                    && route.identity.selector.operation == permission.operation
            }) {
                return Err(ConfigError::new(
                    format!("{path}.permissions[{permission_index}]"),
                    "unknown_route_selector",
                ));
            }
        }
        validated.push(ValidatedApplicationKey {
            id: key.id,
            owner: key.owner,
            secret_ref,
            permissions: key
                .permissions
                .into_iter()
                .map(|permission| RouteSelector {
                    model_alias: ModelAlias(permission.model_alias),
                    operation: permission.operation,
                })
                .collect(),
        });
    }
    if ids.is_empty() {
        return Err(ConfigError::new("application_keys", "empty"));
    }
    Ok(validated)
}

fn validate_secret_ref(
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

fn validate_extension_allowlist(
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

fn validate_bounds(
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
    Ok((
        ValidatedLimits {
            max_queue: limits.max_queue,
            max_in_flight: limits.max_in_flight,
            max_body_bytes: limits.max_body_bytes,
            max_audio_bytes: limits.max_audio_bytes,
            max_audio_chat_body_bytes,
            max_extension_bytes: limits.max_extension_bytes,
        },
        ValidatedTimeouts {
            queue_ms: timeouts.queue_ms,
            connect_ms: timeouts.connect_ms,
            headers_ms: timeouts.headers_ms,
            first_byte_ms: timeouts.first_byte_ms,
            idle_ms: timeouts.idle_ms,
            overall_ms: timeouts.overall_ms,
        },
    ))
}

fn parse_sha256_hex(value: &str) -> Option<[u8; 32]> {
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
pub(crate) fn parse_model_alias(value: &str) -> Option<Option<CodexReasoningEffort>> {
    let Some((model, effort)) = value.split_once(':') else {
        return valid_identifier(value).then_some(None);
    };
    if !valid_identifier(model) || effort.contains(':') {
        return None;
    }
    let effort = match effort {
        "low" => CodexReasoningEffort::Low,
        "medium" => CodexReasoningEffort::Medium,
        "high" => CodexReasoningEffort::High,
        _ => return None,
    };
    Some(Some(effort))
}
fn valid_env_name(value: &&str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_uppercase() || (index > 0 && byte.is_ascii_digit())
        })
}

fn valid_path_token(value: &&str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
fn valid_diagnostic_path(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'[' | b']'))
}
