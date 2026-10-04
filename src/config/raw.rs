use super::*;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawConfig {
    pub(super) listeners: RawListeners,
    pub(super) publication: RawPublication,
    #[serde(default)]
    pub(super) codex_auth: Option<RawCodexAuth>,
    #[serde(default)]
    pub(super) chatgpt_auth: Option<RawChatgptAuth>,
    #[serde(default)]
    pub(super) adapters: Vec<RawAdapter>,
    #[serde(default)]
    pub(super) routes: Vec<RawRoute>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) include: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) route_profiles: std::collections::BTreeMap<String, RawModel>,
    #[serde(default, skip_serializing_if = "RawModels::is_empty")]
    pub(super) models: RawModels,
    #[serde(default)]
    pub(super) application_keys: Vec<RawApplicationKey>,
    #[serde(default)]
    pub(super) keys: Option<RawKeys>,
    pub(super) limits: RawLimits,
    pub(super) timeouts: RawTimeouts,
    #[serde(default)]
    pub(super) logging: RawLogging,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawLogging {
    #[serde(default)]
    pub(super) level: LogLevel,
    #[serde(default)]
    pub(super) format: LogFormat,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawListeners {
    pub(super) client: RawListener,
    pub(super) admin: RawListener,
    #[serde(default)]
    pub(super) public: Option<RawListener>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawListener {
    pub(super) bind: String,
    pub(super) port: u16,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPublication {
    pub(super) tailnet_addresses: Vec<String>,
    #[serde(default)]
    pub(super) public_routes: Vec<RawPermission>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawCodexAuth {
    #[serde(default)]
    pub(super) store: Option<CodexAuthStore>,
    #[serde(default)]
    pub(super) state_dir: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawChatgptAuth {
    pub(super) state_dir: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawAdapter {
    pub(super) id: String,
    pub(super) kind: ProviderKind,
    pub(super) base_url: String,
    pub(super) trust_zone: TrustZone,
    pub(super) secret_ref: Option<String>,
    #[serde(default)]
    pub(super) transcription_mode: Option<VllmTranscriptionMode>,
    #[serde(default)]
    pub(super) extension_allowlist: Vec<String>,
    pub(super) capabilities: RawCapabilities,
    #[serde(default)]
    pub(super) max_in_flight: Option<u64>,
    #[serde(default)]
    pub(super) circuit_breaker: Option<RawCircuitBreaker>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawCircuitBreaker {
    #[serde(default)]
    pub(super) enabled: Option<bool>,
    #[serde(default)]
    pub(super) failures: Option<u32>,
    #[serde(default)]
    pub(super) cooldown_ms: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawCapabilities {
    pub(super) operations: Vec<Operation>,
    pub(super) streaming_chat: bool,
    pub(super) function_tools: bool,
    #[serde(default)]
    pub(super) input_audio: bool,
    #[serde(default)]
    pub(super) input_images: bool,
    #[serde(default)]
    pub(super) audio_streaming_chat: bool,
    #[serde(default)]
    pub(super) audio_function_tools: bool,
    #[serde(default)]
    pub(super) structured_output: bool,
    #[serde(default)]
    pub(super) sampling_controls: bool,
    #[serde(default)]
    pub(super) reasoning_control: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRoute {
    pub(super) id: String,
    pub(super) model_alias: String,
    pub(super) operation: Operation,
    pub(super) adapter_id: String,
    pub(super) upstream_id: String,
    #[serde(default)]
    pub(super) reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub(super) reasoning_summary: Option<ReasoningSummary>,
    #[serde(default)]
    pub(super) codex_reasoning_effort: Option<CodexReasoningEffort>,
    #[serde(default)]
    pub(super) codex_reasoning_summary: Option<CodexReasoningSummary>,
    #[serde(default)]
    pub(super) extension_allowlist: Vec<String>,
    pub(super) requires_streaming_chat: bool,
    pub(super) requires_function_tools: bool,
    #[serde(default)]
    pub(super) allows_input_audio: bool,
    #[serde(default)]
    pub(super) allows_input_images: bool,
    #[serde(default)]
    pub(super) speech_voices: Vec<String>,
    #[serde(default)]
    pub(super) speech_formats: Vec<crate::core::SpeechFormat>,
    #[serde(default)]
    pub(super) allows_audio_streaming_chat: bool,
    #[serde(default)]
    pub(super) allows_audio_function_tools: bool,
    #[serde(default)]
    pub(super) context_tokens: Option<u32>,
    #[serde(default)]
    pub(super) max_output_tokens: Option<u32>,
    #[serde(default)]
    pub(super) enable_thinking: Option<bool>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawApplicationKey {
    pub(super) id: String,
    pub(super) secret_ref: String,
    #[serde(default)]
    pub(super) owner: bool,
    pub(super) permissions: Vec<RawPermission>,
    #[serde(default)]
    pub(super) max_in_flight: Option<u64>,
    #[serde(default)]
    pub(super) rate_limit: Option<RawRateLimit>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawKeys {
    pub(super) file: String,
    #[serde(default)]
    pub(super) usage_dir: Option<String>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRateLimit {
    pub(crate) requests: u64,
    pub(crate) per_ms: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPermission {
    pub(crate) model_alias: String,
    pub(crate) operation: Operation,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawLimits {
    #[serde(default)]
    pub(super) max_uploads: Option<u64>,
    #[serde(default)]
    pub(super) max_buffered_bytes: Option<u64>,
    pub(super) max_queue: u64,
    pub(super) max_in_flight: u64,
    pub(super) max_body_bytes: u64,
    pub(super) max_audio_bytes: u64,
    pub(super) max_extension_bytes: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTimeouts {
    #[serde(default)]
    pub(super) upload_ms: Option<u64>,
    pub(super) queue_ms: u64,
    pub(super) connect_ms: u64,
    pub(super) headers_ms: u64,
    pub(super) first_byte_ms: u64,
    pub(super) idle_ms: u64,
    pub(super) overall_ms: u64,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawModels {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) chat: std::collections::BTreeMap<String, RawModel>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) transcription: std::collections::BTreeMap<String, RawModel>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) embeddings: std::collections::BTreeMap<String, RawModel>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) speech: std::collections::BTreeMap<String, RawModel>,
}

impl RawModels {
    pub(super) fn is_empty(&self) -> bool {
        self.chat.is_empty()
            && self.transcription.is_empty()
            && self.embeddings.is_empty()
            && self.speech.is_empty()
    }

    pub(super) fn entries(
        &self,
    ) -> [(Operation, &std::collections::BTreeMap<String, RawModel>); 4] {
        [
            (Operation::Chat, &self.chat),
            (Operation::Transcription, &self.transcription),
            (Operation::Embeddings, &self.embeddings),
            (Operation::Speech, &self.speech),
        ]
    }

    pub(super) fn operation_mut(
        &mut self,
        operation: Operation,
    ) -> &mut std::collections::BTreeMap<String, RawModel> {
        match operation {
            Operation::Chat => &mut self.chat,
            Operation::Transcription => &mut self.transcription,
            Operation::Embeddings => &mut self.embeddings,
            Operation::Speech => &mut self.speech,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawFragment {
    #[serde(default)]
    pub(super) adapters: Vec<RawAdapter>,
    #[serde(default)]
    pub(super) routes: Vec<RawRoute>,
    #[serde(default)]
    pub(super) route_profiles: std::collections::BTreeMap<String, RawModel>,
    #[serde(default)]
    pub(super) models: RawModels,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawModel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) id: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) route_ids: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) upstream_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) default_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) efforts: Option<Vec<ReasoningEffort>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) unset: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) adapter_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) reasoning_summary: Option<ReasoningSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) codex_reasoning_effort: Option<CodexReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) codex_reasoning_summary: Option<CodexReasoningSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) extension_allowlist: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) requires_streaming_chat: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) requires_function_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allows_input_audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allows_input_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) speech_voices: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) speech_formats: Option<Vec<crate::core::SpeechFormat>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allows_audio_streaming_chat: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allows_audio_function_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) context_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) enable_thinking: Option<bool>,
}
