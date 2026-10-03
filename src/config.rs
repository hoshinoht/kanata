pub(crate) use raw::{RawPermission, RawRateLimit};
mod plane;
mod raw;
mod reload;
mod validation;
use raw::*;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use validation::*;
pub(crate) use validation::{
    parse_model_alias, parse_sha256_hex, valid_identifier, validate_key_limits,
    validate_permissions,
};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::core::{
    Capabilities, ExtensionKey, MAX_INPUT_AUDIO_BYTES, ModelAlias, Operation, ReasoningEffort,
    RouteIdentity, RouteSelector, TrustZone,
};
use crate::keys::{self, file::KeysFile};

/// Keys within this many seconds of expiry are reported by [`ValidatedConfig::key_warnings`].
pub const KEY_EXPIRY_WARNING_SECS: u64 = 7 * 86_400;

pub const MAX_TIMEOUT_MS: u64 = 604_800_000;
/// Upper bound for configured concurrency and request counts.
pub const MAX_CONCURRENCY_LIMIT: u64 = 1_000_000;
pub const MAX_CONFIG_AUDIO_BYTES: u64 = MAX_INPUT_AUDIO_BYTES as u64;

#[derive(Clone, Debug)]
pub struct ValidatedConfig {
    listeners: ValidatedListeners,
    publication: ValidatedPublication,
    codex_auth: Option<ValidatedCodexAuth>,
    chatgpt_auth: Option<ValidatedChatgptAuth>,
    adapters: Vec<ValidatedAdapter>,
    routes: Vec<ValidatedRoute>,
    application_keys: Vec<ValidatedApplicationKey>,
    key_source: KeySource,
    limits: ValidatedLimits,
    timeouts: ValidatedTimeouts,
    logging: ValidatedLogging,
}

/// Where application keys come from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeySource {
    /// Deprecated `[[application_keys]]` in the config file.
    Inline,
    /// `[keys] file`, resolved against the config file's directory.
    File {
        path: PathBuf,
        usage_dir: Option<PathBuf>,
        /// The file did not exist; no keys are loaded.
        missing: bool,
        /// SHA-256 of the file bytes the current keys were parsed from.
        sha256: Option<[u8; 32]>,
    },
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
    pub fn chatgpt_auth(&self) -> Option<&ValidatedChatgptAuth> {
        self.chatgpt_auth.as_ref()
    }
    pub fn adapters(&self) -> &[ValidatedAdapter] {
        &self.adapters
    }
    pub fn routes(&self) -> &[ValidatedRoute] {
        &self.routes
    }
    /// Non-revoked keys, expired ones included.
    pub fn application_keys(&self) -> &[ValidatedApplicationKey] {
        &self.application_keys
    }
    pub fn key_source(&self) -> &KeySource {
        &self.key_source
    }

    /// Replaces the keys with the active records of a reloaded keys file.
    pub fn with_application_keys(&self, file: &KeysFile) -> Result<Self, ConfigError> {
        let KeySource::File {
            path, usage_dir, ..
        } = &self.key_source
        else {
            return Err(ConfigError::new("keys", "required"));
        };
        if usage_dir.is_none() && file.active().any(|key| key.daily_quota().is_some()) {
            return Err(ConfigError::new(
                "keys.usage_dir",
                "required_for_daily_quota",
            ));
        }
        let application_keys = application_keys_from_file(file, &self.routes)?;
        Ok(Self {
            application_keys,
            key_source: KeySource::File {
                path: path.clone(),
                usage_dir: usage_dir.clone(),
                missing: false,
                sha256: Some(file.sha256()),
            },
            ..self.clone()
        })
    }

    /// Operator warnings: deprecated inline keys, a missing keys file, and keys
    /// expired or expiring within [`KEY_EXPIRY_WARNING_SECS`]. Ids and dates only.
    pub fn key_warnings(&self, now: u64) -> Vec<String> {
        let mut warnings = Vec::new();
        match &self.key_source {
            KeySource::Inline => warnings.push(
                "inline [[application_keys]] are deprecated; move them to a keys file with `kanata key migrate`".into(),
            ),
            KeySource::File {
                path,
                missing: true,
                ..
            } => warnings.push(format!(
                "keys file {} not found; no application keys are loaded (create one with `kanata key new`)",
                path.display()
            )),
            KeySource::File { .. } => {}
        }
        for key in &self.application_keys {
            let Some(expires_at) = key.expires_at else {
                continue;
            };
            let at = keys::time::format(expires_at);
            if key.is_expired(now) {
                warnings.push(format!("key {} expired at {at}", key.id));
            } else if expires_at - now <= KEY_EXPIRY_WARNING_SECS {
                warnings.push(format!("key {} expires at {at}", key.id));
            }
        }
        warnings
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

/// Reasoning summary detail requested from Codex.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CodexReasoningSummary {
    Auto,
    Concise,
    Detailed,
}

impl CodexReasoningSummary {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Concise => "concise",
            Self::Detailed => "detailed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedChatgptAuth {
    state_dir: PathBuf,
}
impl ValidatedChatgptAuth {
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
    max_in_flight: Option<u64>,
    circuit_breaker: CircuitBreakerPolicy,
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
    /// Concurrency cap shared by every route on this adapter; `None` means uncapped.
    pub fn max_in_flight(&self) -> Option<u64> {
        self.max_in_flight
    }
    pub fn circuit_breaker(&self) -> CircuitBreakerPolicy {
        self.circuit_breaker
    }
}

/// Fail-fast policy for an adapter whose backend keeps failing at the transport level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CircuitBreakerPolicy {
    pub enabled: bool,
    pub failures: u32,
    pub cooldown_ms: u64,
}

impl Default for CircuitBreakerPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            failures: 5,
            cooldown_ms: 30_000,
        }
    }
}

/// Token bucket: at most `requests` per `per_ms`, refilled continuously.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyRateLimit {
    pub requests: u64,
    pub per_ms: u64,
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
    codex_reasoning_summary: Option<CodexReasoningSummary>,
    extension_allowlist: BTreeSet<ExtensionKey>,
    requires_streaming_chat: bool,
    requires_function_tools: bool,
    allows_input_audio: bool,
    allows_input_images: bool,
    speech: Option<crate::core::SpeechPolicy>,
    allows_audio_streaming_chat: bool,
    allows_audio_function_tools: bool,
    context_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
    enable_thinking: Option<bool>,
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
    pub fn codex_reasoning_summary(&self) -> Option<CodexReasoningSummary> {
        self.codex_reasoning_summary
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
    pub fn speech(&self) -> Option<&crate::core::SpeechPolicy> {
        self.speech.as_ref()
    }

    pub fn allows_input_images(&self) -> bool {
        self.allows_input_images
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
    /// Declared output cap; larger client requests are rejected.
    pub fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }
    /// Reasoning effort fixed by the route rather than the client.
    pub fn pinned_reasoning_effort(&self) -> Option<&'static str> {
        self.codex_reasoning_effort
            .map(CodexReasoningEffort::as_str)
    }
    /// Published model name for a pinned reasoning route.
    pub fn model_family_alias(&self) -> &str {
        let alias = self.identity.selector.model_alias.0.as_str();
        alias
            .rsplit_once(':')
            .filter(|(_, effort)| Some(*effort) == self.pinned_reasoning_effort())
            .map(|(model, _)| model)
            .unwrap_or(alias)
    }
    /// vLLM chat-template thinking switch; `None` leaves the template default.
    pub fn enable_thinking(&self) -> Option<bool> {
        self.enable_thinking
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedApplicationKey {
    id: String,
    owner: bool,
    secret_ref: SecretReference,
    permissions: Vec<RouteSelector>,
    max_in_flight: Option<u64>,
    rate_limit: Option<KeyRateLimit>,
    daily_quota: Option<crate::keys::quota::DailyQuota>,
    expires_at: Option<u64>,
}
impl ValidatedApplicationKey {
    pub fn daily_quota(&self) -> Option<crate::keys::quota::DailyQuota> {
        self.daily_quota
    }
    /// Unix seconds; `None` never expires (always for inline keys).
    pub fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }
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
    pub fn max_in_flight(&self) -> Option<u64> {
        self.max_in_flight
    }
    pub fn rate_limit(&self) -> Option<KeyRateLimit> {
        self.rate_limit
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedLimits {
    max_uploads: u64,
    max_buffered_bytes: u64,
    max_queue: u64,
    max_in_flight: u64,
    max_body_bytes: u64,
    max_audio_bytes: u64,
    max_audio_chat_body_bytes: u64,
    max_extension_bytes: u64,
}
impl ValidatedLimits {
    pub fn max_uploads(&self) -> u64 {
        self.max_uploads
    }
    pub fn max_buffered_bytes(&self) -> u64 {
        self.max_buffered_bytes
    }
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedTimeouts {
    upload_ms: u64,
    queue_ms: u64,
    connect_ms: u64,
    headers_ms: u64,
    first_byte_ms: u64,
    idle_ms: u64,
    overall_ms: u64,
}
impl ValidatedTimeouts {
    pub fn upload_ms(&self) -> u64 {
        self.upload_ms
    }
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
    Chatgpt,
    /// Apple Foundation Models through macOS `fm serve`.
    AppleFm,
    Speech,
}

impl ProviderKind {
    pub fn is_private_only(self) -> bool {
        matches!(self, Self::Codex | Self::Chatgpt)
    }

    pub fn accepts_reasoning_effort(self, effort: ReasoningEffort) -> bool {
        match self {
            Self::Codex => CodexReasoningEffort::from_request(effort).is_some(),
            Self::Ollama | Self::Vllm | Self::Openrouter => true,
            Self::AppleFm | Self::Speech | Self::Chatgpt => false,
        }
    }

    /// Whether a request may set `chat_template_kwargs.enable_thinking`.
    pub fn accepts_enable_thinking(self) -> bool {
        matches!(self, Self::Vllm)
    }

    /// Config name, used as the provider label in logs and metrics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Vllm => "vllm",
            Self::Openrouter => "openrouter",
            Self::Codex => "codex",
            Self::Chatgpt => "chatgpt",
            Self::AppleFm => "apple_fm",
            Self::Speech => "speech",
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
            Self::Speech | Self::Chatgpt => (false, false, false),
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
    pub(crate) fn new(path: impl Into<String>, class: &'static str) -> Self {
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

/// Parses TOML into `T`; errors carry only the field path and a class.
/// `root` is `"config"` for the config file (paths stay unprefixed) or a
/// prefix for other files, whose unknown key names are never echoed.
pub(crate) fn parse_toml<T: serde::de::DeserializeOwned>(
    contents: &str,
    root: &str,
) -> Result<T, ConfigError> {
    let deserializer = toml::de::Deserializer::parse(contents)
        .map_err(|_| ConfigError::new(root, "parse_error"))?;
    serde_path_to_error::deserialize(deserializer)
        .map_err(|error| sanitized_deserialize_error(error, root))
}

fn sanitized_deserialize_error(
    error: serde_path_to_error::Error<toml::de::Error>,
    root: &str,
) -> ConfigError {
    let is_config = root == "config";
    let message = error.inner().to_string();
    let mut path = error.path().to_string();
    let class = if message.contains("unknown field") || message.contains("unknown key") {
        if is_config
            && path.is_empty()
            && let Some(field) = message.split('`').nth(1).filter(valid_path_token)
        {
            path = field.into();
        }
        if !is_config {
            // The last segment is the unknown key itself, i.e. file content.
            path = path
                .rsplit_once('.')
                .map_or_else(String::new, |(parent, _)| parent.to_owned());
        }
        "unknown_field"
    } else if message.contains("invalid type") {
        "type_error"
    } else {
        "schema_error"
    };
    let valid = valid_diagnostic_path(&path);
    let path = match (is_config, valid) {
        (true, true) => path,
        (false, true) => format!("{root}.{path}"),
        (_, false) => root.to_owned(),
    };
    ConfigError::new(path, class)
}

pub fn load(path: impl AsRef<Path>) -> Result<ValidatedConfig, ConfigError> {
    let path = path.as_ref();
    let contents =
        fs::read_to_string(path).map_err(|_| ConfigError::new("config", "read_error"))?;
    let raw = parse_toml(&contents, "config")?;
    validate(raw, path.parent().unwrap_or(Path::new("")), true)
}

/// Like [`load`] but leaves a `[keys] file` unread (no application keys, `sha256: None`),
/// so the host key CLI can validate it itself and repair route drift.
pub fn load_deferring_keys(path: impl AsRef<Path>) -> Result<ValidatedConfig, ConfigError> {
    let path = path.as_ref();
    let contents =
        fs::read_to_string(path).map_err(|_| ConfigError::new("config", "read_error"))?;
    let raw = parse_toml(&contents, "config")?;
    validate(raw, path.parent().unwrap_or(Path::new("")), false)
}
