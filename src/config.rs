pub(crate) use raw::{RawPermission, RawRateLimit};
mod catalog;
pub(crate) mod commands;
mod error;
mod plane;
mod raw;
mod reload;
mod validation;
pub use error::{ConfigError, ConfigErrorClass};
use raw::*;
use std::collections::BTreeSet;
use std::fmt;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedConfig {
    listeners: ValidatedListeners,
    publication: ValidatedPublication,
    chatgpt_auth: Option<ValidatedChatgptAuth>,
    adapters: Vec<ValidatedAdapter>,
    routes: Vec<ValidatedRoute>,
    keys: Vec<ValidatedApplicationKey>,
    key_source: KeySource,
    limits: ValidatedLimits,
    timeouts: ValidatedTimeouts,
    logging: ValidatedLogging,
    route_sources: std::collections::BTreeMap<String, String>,
}

/// The `[keys] file`, resolved against the config file's directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeySource {
    pub path: PathBuf,
    pub usage_dir: Option<PathBuf>,
    /// The file did not exist; no keys are loaded.
    pub missing: bool,
    /// SHA-256 of the file bytes the current keys were parsed from.
    pub sha256: Option<[u8; 32]>,
}

impl ValidatedConfig {
    getters! {
        pub listeners: &ValidatedListeners,
        pub publication: &ValidatedPublication,
        pub chatgpt_auth: Option<&ValidatedChatgptAuth> = as_ref,
        pub adapters: &[ValidatedAdapter],
        pub routes: &[ValidatedRoute],
        /// Non-revoked keys, expired ones included.
        pub keys: &[ValidatedApplicationKey],
        pub key_source: &KeySource,
    }

    /// Replaces the keys with the active records of a reloaded keys file.
    pub fn with_keys(&self, file: &KeysFile) -> Result<Self, ConfigError> {
        let KeySource {
            path, usage_dir, ..
        } = &self.key_source;
        if usage_dir.is_none() && file.active().any(|key| key.daily_quota().is_some()) {
            return Err(ConfigError::new(
                "keys.usage_dir",
                ConfigErrorClass::RequiredForDailyQuota,
            ));
        }
        let keys = keys_from_file(file, &self.routes)?;
        Ok(Self {
            keys,
            key_source: KeySource {
                path: path.clone(),
                usage_dir: usage_dir.clone(),
                missing: false,
                sha256: Some(file.sha256()),
            },
            ..self.clone()
        })
    }

    /// Operator warnings: a missing keys file, and keys expired or expiring within
    /// [`KEY_EXPIRY_WARNING_SECS`]. Ids and dates only.
    pub fn key_warnings(&self, now: u64) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.key_source.missing {
            warnings.push(format!(
                "keys file {} not found; no application keys are loaded (create one with `kanata key new`)",
                self.key_source.path.display()
            ));
        }
        for key in &self.keys {
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

    getters! {
        pub limits: &ValidatedLimits,
        pub timeouts: &ValidatedTimeouts,
        pub logging: &ValidatedLogging,
    }
}

str_enum! {
    /// Which listeners one `kanata serve` process runs.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub enum Plane {
        /// Private and public listeners in one process.
        #[default]
        All => "all",
        /// Private listener only; no public listener or public routes.
        Private => "private",
        /// Public listener only, with just the public routes, their adapters and non-owner keys.
        Public => "public",
    }
    pub const fn as_str;
    pub fn parse;
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
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
    getters! {
        pub level: LogLevel,
        pub format: LogFormat,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedListeners {
    client: ValidatedListener,
    admin: ValidatedListener,
    public: Option<ValidatedListener>,
}
impl ValidatedListeners {
    getters! {
        pub client: &ValidatedListener,
        pub admin: &ValidatedListener,
        pub public: Option<&ValidatedListener> = as_ref,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedListener {
    bind: IpAddr,
    port: u16,
}
impl ValidatedListener {
    getters! {
        pub bind: IpAddr,
        pub port: u16,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedPublication {
    tailnet_addresses: Vec<IpAddr>,
    public_routes: Vec<RouteSelector>,
}
impl ValidatedPublication {
    getters! {
        pub tailnet_addresses: &[IpAddr],
        pub public_routes: &[RouteSelector],
    }
}

str_enum! {
    /// Requested reasoning summary detail.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "lowercase")]
    pub enum ReasoningSummary {
        Auto => "auto",
        Concise => "concise",
        Detailed => "detailed",
    }
    pub const fn as_str;
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

#[derive(Clone, Debug, Eq, PartialEq)]
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
    ca_bundle: Option<CaBundle>,
    anthropic: Option<ValidatedAnthropicSettings>,
}
impl ValidatedAdapter {
    getters! {
        pub id: &str,
        pub kind: ProviderKind,
        pub base_url: &Url,
        pub trust_zone: TrustZone,
        pub transcription_mode: Option<VllmTranscriptionMode>,
        pub secret_ref: Option<&SecretReference> = as_ref,
        pub extension_allowlist: &BTreeSet<ExtensionKey>,
        pub capabilities: &Capabilities,
        /// Concurrency cap shared by every route on this adapter; `None` means uncapped.
        pub max_in_flight: Option<u64>,
        pub circuit_breaker: CircuitBreakerPolicy,
        /// Private CA that replaces the platform roots for this adapter's TLS (`ca_file`).
        pub ca_bundle: Option<&CaBundle> = as_ref,
        /// `[adapters.anthropic]` settings, defaults applied; `Some` exactly for Anthropic adapters.
        pub anthropic: Option<&ValidatedAnthropicSettings> = as_ref,
    }
}

/// Default `max_tokens` sent to the Claude Messages API, which requires one.
pub const DEFAULT_ANTHROPIC_MAX_OUTPUT_TOKENS: u32 = 16_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedAnthropicSettings {
    workspace_id: Option<String>,
    default_max_output_tokens: u32,
}
impl ValidatedAnthropicSettings {
    getters! {
        /// Sent as `anthropic-workspace-id` when set.
        pub workspace_id: Option<&str> = as_deref,
        /// `max_tokens` on every request.
        pub default_max_output_tokens: u32,
    }
}

/// Certificates from `adapters[].ca_file`, parsed for the adapters a loaded plane keeps.
#[derive(Clone, Eq, PartialEq)]
pub struct CaBundle {
    path: PathBuf,
    /// Position in the configured `adapters`, kept for errors after plane narrowing.
    index: usize,
    /// Empty when unread: in the full config from [`load_plane`] and in
    /// [`load_deferring_keys`]; such a bundle cannot build a TLS client.
    certificates: std::sync::Arc<[rustls::pki_types::CertificateDer<'static>]>,
}
impl CaBundle {
    getters! {
        pub path: &Path,
        pub(crate) certificates: &[rustls::pki_types::CertificateDer<'static>],
    }
}
impl fmt::Debug for CaBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaBundle")
            .field("path", &self.path)
            .field("certificates", &self.certificates.len())
            .finish()
    }
}

/// Whether credentials may be sent to an adapter `base_url`: HTTPS, or plain HTTP to a literal
/// loopback IP. A loopback literal never leaves the host and is connected to without DNS, so a
/// hostname such as `localhost` does not qualify.
pub(crate) fn url_permits_credentials(url: &Url) -> bool {
    match (url.scheme(), url.host()) {
        ("https", _) => true,
        ("http", Some(url::Host::Ipv4(ip))) => ip.is_loopback(),
        ("http", Some(url::Host::Ipv6(ip))) => ip.is_loopback(),
        _ => false,
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
}
impl SecretReference {
    pub fn env_name(&self) -> Option<&str> {
        match self {
            Self::Env(name) => Some(name),
            Self::File(_) => None,
        }
    }
}

const MIN_CONTEXT_TOKENS: u32 = 256;
const MAX_CONTEXT_TOKENS: u32 = 16_777_216;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedRoute {
    identity: RouteIdentity,
    adapter_id: String,
    reasoning_effort: Option<ReasoningEffort>,
    reasoning_summary: Option<ReasoningSummary>,
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
    getters! {
        pub identity: &RouteIdentity,
        pub adapter_id: &str,
        pub reasoning_effort: Option<ReasoningEffort>,
        pub extension_allowlist: &BTreeSet<ExtensionKey>,
        pub requires_streaming_chat: bool,
        pub requires_function_tools: bool,
        pub speech: Option<&crate::core::SpeechPolicy> = as_ref,
        pub allows_input_images: bool,
        pub allows_input_audio: bool,
        pub allows_audio_streaming_chat: bool,
        pub allows_audio_function_tools: bool,
        /// Declared upstream context window; `None` means the provider's own.
        pub context_tokens: Option<u32>,
        /// Declared output cap; larger client requests are rejected.
        pub max_output_tokens: Option<u32>,
        /// vLLM chat-template thinking switch; `None` leaves the template default.
        pub enable_thinking: Option<bool>,
        pub reasoning_summary: Option<ReasoningSummary>,
    }
    /// Reasoning effort fixed by the route rather than the client.
    pub fn pinned_reasoning_effort(&self) -> Option<&'static str> {
        self.reasoning_effort.map(ReasoningEffort::as_str)
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedApplicationKey {
    id: String,
    owner: bool,
    digest: [u8; 32],
    permissions: Vec<RouteSelector>,
    max_in_flight: Option<u64>,
    rate_limit: Option<KeyRateLimit>,
    daily_quota: Option<crate::keys::quota::DailyQuota>,
    expires_at: Option<u64>,
}
impl ValidatedApplicationKey {
    getters! {
        pub daily_quota: Option<crate::keys::quota::DailyQuota>,
        /// Unix seconds; `None` never expires.
        pub expires_at: Option<u64>,
        pub id: &str,
        pub is_owner(owner): bool,
        /// SHA-256 of the key secret.
        pub digest: &[u8; 32],
        pub permissions: &[RouteSelector],
        pub max_in_flight: Option<u64>,
        pub rate_limit: Option<KeyRateLimit>,
    }
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
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
    getters! {
        pub max_uploads: u64,
        pub max_buffered_bytes: u64,
        pub max_queue: u64,
        pub max_in_flight: u64,
        pub max_body_bytes: u64,
        pub max_audio_bytes: u64,
        pub max_audio_chat_body_bytes: u64,
        pub max_extension_bytes: u64,
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
    sse_keepalive_ms: u64,
}
impl ValidatedTimeouts {
    getters! {
        pub upload_ms: u64,
        pub queue_ms: u64,
        pub connect_ms: u64,
        pub headers_ms: u64,
        pub first_byte_ms: u64,
        pub idle_ms: u64,
        pub overall_ms: u64,
        pub sse_keepalive_ms: u64,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Ollama,
    Vllm,
    Openrouter,
    Chatgpt,
    /// Apple Foundation Models through macOS `fm serve`.
    AppleFm,
    Speech,
    /// CLIProxyAPI's OpenAI-compatible chat endpoint (account sign-ins behind a local proxy);
    /// reuses the vLLM chat path and, like ChatGPT, is never public.
    Cliproxy,
    /// Claude Messages API with a Console API key; never public.
    Anthropic,
}

impl ProviderKind {
    /// Never published, never on the public plane, and keys reaching it stay private.
    pub fn is_private_only(self) -> bool {
        matches!(self, Self::Chatgpt | Self::Cliproxy | Self::Anthropic)
    }

    /// Whether the adapter applies a route's pinned effort itself (ChatGPT pinned inference).
    /// Other reasoning-control adapters receive the pin as the request's `reasoning_effort`,
    /// so families on them need `reasoning_control`.
    pub fn applies_route_effort_pin(self) -> bool {
        matches!(self, Self::Chatgpt)
    }

    pub fn accepts_reasoning_effort(self, effort: ReasoningEffort) -> bool {
        match self {
            Self::Ollama | Self::Vllm | Self::Openrouter | Self::Chatgpt | Self::Cliproxy => true,
            // `output_config.effort` has no off or minimal level.
            Self::Anthropic => !matches!(effort, ReasoningEffort::None | ReasoningEffort::Minimal),
            Self::AppleFm | Self::Speech => false,
        }
    }

    /// Whether a request may set `chat_template_kwargs.enable_thinking`.
    pub fn accepts_enable_thinking(self) -> bool {
        matches!(self, Self::Vllm)
    }

    /// Whether stop sequences and presence/frequency penalties reach a backend that honours
    /// them on routes with sampling controls; `fm serve` support is unproven.
    pub fn accepts_stop_and_penalties(self) -> bool {
        !matches!(self, Self::AppleFm)
    }

    /// Whether the upstream receives the request's `prompt_cache_key`.
    pub fn forwards_prompt_cache_key(self) -> bool {
        matches!(self, Self::Chatgpt)
    }

    /// Whether assistant `ChatContent::Reasoning` items are replayed upstream; others skip them.
    /// Anthropic replays only the thinking items it produced.
    pub fn replays_encrypted_reasoning(self) -> bool {
        matches!(self, Self::Chatgpt | Self::Anthropic)
    }

    /// Whether assistant `ChatContent::ReasoningText` is replayed upstream as the assistant
    /// message's `reasoning` string; others skip it. vLLM (canonical input field since it
    /// renamed `reasoning_content`), Ollama's OpenAI layer (maps it to `thinking`) and
    /// OpenRouter all read `reasoning`. CLIProxyAPI turns replayed reasoning into unsigned
    /// Claude thinking only on opt-in compat endpoints, so it is dropped there; `fm serve`
    /// support is unproven.
    pub fn replays_reasoning_text(self) -> bool {
        matches!(self, Self::Vllm | Self::Ollama | Self::Openrouter)
    }

    /// Config name, used as the provider label in logs and metrics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Vllm => "vllm",
            Self::Openrouter => "openrouter",
            Self::Chatgpt => "chatgpt",
            Self::AppleFm => "apple_fm",
            Self::Speech => "speech",
            Self::Cliproxy => "cliproxy",
            Self::Anthropic => "anthropic",
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
            Self::Ollama | Self::Vllm | Self::Openrouter | Self::Cliproxy => (true, true, true),
            Self::Chatgpt => (false, false, true),
            // Claude 4.7+ rejects non-default sampling; the output cap is an adapter setting.
            Self::Anthropic => (true, false, true),
            Self::AppleFm => (true, true, false),
            Self::Speech => (false, false, false),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VllmTranscriptionMode {
    NativeAsr,
    AudioChat,
}

/// Parses TOML into `T`; errors carry only the field path and a class.
/// `root` is `"config"` for the config file (paths stay unprefixed) or a
/// prefix for other files, whose unknown key names are never echoed.
pub(crate) fn parse_toml<T: serde::de::DeserializeOwned>(
    contents: &str,
    root: &str,
) -> Result<T, ConfigError> {
    let deserializer = toml::de::Deserializer::parse(contents)
        .map_err(|_| ConfigError::new(root, ConfigErrorClass::ParseError))?;
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
    let name = message.split('`').nth(1).unwrap_or_default();
    let class = if message.contains("unknown field") || message.contains("unknown key") {
        if is_config && path.is_empty() && valid_path_token(&name) {
            path = name.into();
        }
        if !is_config {
            // The last segment is the unknown key itself, i.e. file content.
            path = path
                .rsplit_once('.')
                .map_or_else(String::new, |(parent, _)| parent.to_owned());
        }
        removed_setting_class(name, is_config && path == name)
            .unwrap_or(ConfigErrorClass::UnknownField)
    } else if message.contains("invalid type") {
        ConfigErrorClass::TypeError
    } else {
        ConfigErrorClass::SchemaError
    };
    let valid = valid_diagnostic_path(&path);
    let path = match (is_config, valid) {
        (true, true) => path,
        (false, true) => format!("{root}.{path}"),
        (_, false) => root.to_owned(),
    };
    ConfigError::new(path, class)
}

/// Explicit classes for removed settings; `top_level` means a key of the config root.
fn removed_setting_class(name: &str, top_level: bool) -> Option<ConfigErrorClass> {
    match name {
        "application_keys" if top_level => Some(ConfigErrorClass::InlineKeysRemoved),
        _ => None,
    }
}

pub fn load(path: impl AsRef<Path>) -> Result<ValidatedConfig, ConfigError> {
    let path = path.as_ref();
    catalog::Document::load(path)?.validate(path, true)
}

/// [`load`] for one `--plane`: returns the full config and its [`ValidatedConfig::for_plane`]
/// narrowing. Adapter `ca_file`s are read only for adapters the plane keeps, so a plane
/// container need not mount the CA of an adapter it never dials; the full config's bundles
/// stay unread.
pub fn load_plane(
    path: impl AsRef<Path>,
    plane: Plane,
) -> Result<(ValidatedConfig, ValidatedConfig), ConfigError> {
    let path = path.as_ref();
    let full = catalog::Document::load(path)?.validate_deferring_ca(path, true)?;
    let narrowed = full.for_plane(plane)?.load_ca_bundles()?;
    Ok((full, narrowed))
}

/// Like [`load`] but leaves a `[keys] file` unread (no application keys, `sha256: None`),
/// so the host key CLI can validate it itself and repair route drift. Adapter `ca_file`s are
/// left unread too: they name paths inside the serving container, not on the host.
pub fn load_deferring_keys(path: impl AsRef<Path>) -> Result<ValidatedConfig, ConfigError> {
    let path = path.as_ref();
    catalog::Document::load(path)?.validate(path, false)
}
