#[path = "../adapter_vllm/support.rs"]
pub mod common;

use std::{fs, sync::atomic::AtomicUsize};

use kanata::{
    auth::{SecretResolutionError, SecretResolver},
    config::{self, ValidatedConfig, ValidatedRoute},
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, Extensions, InputAudioFormat, ModelAlias,
        Request, RequestContext, RoutedRequest, ToolChoice, TranscriptionRequest, TrustZone,
        ValidatedAudio, ValidatedFile,
    },
};

pub const TRANSCRIPTION_RESPONSE: &str =
    include_str!("../fixtures/vllm/audio-chat-transcription-response.json");
pub const NATIVE_ASR_RESPONSE: &str =
    include_str!("../fixtures/vllm/native-asr-transcription-response.json");

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

pub struct Resolver;

impl SecretResolver for Resolver {
    fn resolve(&self, _: &config::SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"test-key".to_vec())
    }
}

#[derive(Clone, Copy)]
pub struct ConfigOptions {
    pub transcription_mode: Option<&'static str>,
    pub input_audio: bool,
    pub streaming_chat: bool,
    pub function_tools: bool,
    pub audio_streaming_chat: bool,
    pub audio_function_tools: bool,
    pub route_input_audio: bool,
    pub route_audio_streaming_chat: bool,
    pub route_audio_function_tools: bool,
    pub secret_ref: bool,
    pub max_audio_bytes: usize,
}

impl Default for ConfigOptions {
    fn default() -> Self {
        Self {
            transcription_mode: None,
            input_audio: false,
            streaming_chat: false,
            function_tools: false,
            audio_streaming_chat: false,
            audio_function_tools: false,
            route_input_audio: false,
            route_audio_streaming_chat: false,
            route_audio_function_tools: false,
            secret_ref: false,
            max_audio_bytes: 2 * 1024 * 1024,
        }
    }
}

pub fn config_for(address: &str, options: ConfigOptions) -> ValidatedConfig {
    let transcription_mode_line = options
        .transcription_mode
        .map(|mode| format!("transcription_mode = \"{mode}\"\n"))
        .unwrap_or_default();
    // Credentials are only sent over HTTPS.
    let scheme = if options.secret_ref { "https" } else { "http" };
    let secret_ref_line = if options.secret_ref {
        "secret_ref = \"env:VLLM_KEY\"\n"
    } else {
        ""
    };
    let native_asr = options.transcription_mode == Some("native_asr");
    let audio_chat = options.transcription_mode == Some("audio_chat");
    let operations = if native_asr {
        "[\"transcription\"]"
    } else if audio_chat {
        "[\"chat\", \"transcription\"]"
    } else {
        "[\"chat\"]"
    };
    let routes = if native_asr {
        r#"
[[routes]]
id = "vllm-transcription"
model_alias = "vllm-public"
operation = "transcription"
adapter_id = "vllm-fixture"
upstream_id = "served-transcriber"
requires_streaming_chat = false
requires_function_tools = false
"#
        .to_owned()
    } else if audio_chat {
        r#"
[[routes]]
id = "vllm-chat"
model_alias = "vllm-public"
operation = "chat"
adapter_id = "vllm-fixture"
upstream_id = "served-checkpoint-alias"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = {route_input_audio}
allows_audio_streaming_chat = {route_audio_streaming_chat}
allows_audio_function_tools = {route_audio_function_tools}

[[routes]]
id = "vllm-transcription"
model_alias = "vllm-public"
operation = "transcription"
adapter_id = "vllm-fixture"
upstream_id = "served-transcriber"
requires_streaming_chat = false
requires_function_tools = false
"#
        .to_owned()
    } else {
        r#"
[[routes]]
id = "vllm-chat"
model_alias = "vllm-public"
operation = "chat"
adapter_id = "vllm-fixture"
upstream_id = "served-checkpoint-alias"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = {route_input_audio}
allows_audio_streaming_chat = {route_audio_streaming_chat}
allows_audio_function_tools = {route_audio_function_tools}
"#
        .to_owned()
    };
    let permissions = if native_asr {
        "{ model_alias = \"vllm-public\", operation = \"transcription\" }"
    } else if audio_chat {
        "{ model_alias = \"vllm-public\", operation = \"chat\" }, { model_alias = \"vllm-public\", operation = \"transcription\" }"
    } else {
        "{ model_alias = \"vllm-public\", operation = \"chat\" }"
    };
    let contents = format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080

[listeners.admin]
bind = "127.0.0.1"
port = 18081

[publication]
tailnet_addresses = ["100.64.0.1"]

[[adapters]]
id = "vllm-fixture"
kind = "vllm"
base_url = "{scheme}://{address}/v1"
trust_zone = "local"
{transcription_mode_line}{secret_ref_line}[adapters.capabilities]
operations = {operations}
streaming_chat = {streaming_chat}
function_tools = {function_tools}
input_audio = {input_audio}
audio_streaming_chat = {audio_streaming_chat}
audio_function_tools = {audio_function_tools}

{routes}
[[application_keys]]
id = "fixture-client"
secret_ref = "env:KANATA_TEST_KEY"
permissions = [{permissions}]

[limits]
max_queue = 2
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = {max_audio_bytes}
max_extension_bytes = 8192

[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#,
        streaming_chat = options.streaming_chat,
        function_tools = options.function_tools,
        input_audio = options.input_audio,
        audio_streaming_chat = options.audio_streaming_chat,
        audio_function_tools = options.audio_function_tools,
        routes = routes
            .replace(
                "{route_input_audio}",
                &options.route_input_audio.to_string()
            )
            .replace(
                "{route_audio_streaming_chat}",
                &options.route_audio_streaming_chat.to_string(),
            )
            .replace(
                "{route_audio_function_tools}",
                &options.route_audio_function_tools.to_string(),
            ),
        permissions = permissions,
        max_audio_bytes = options.max_audio_bytes,
    );
    load_config(&contents)
}

pub fn audio_chat_native_asr_config(
    audio_chat_address: &str,
    native_asr_address: &str,
) -> ValidatedConfig {
    let contents = format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080

[listeners.admin]
bind = "127.0.0.1"
port = 18081

[publication]
tailnet_addresses = ["100.64.0.1"]

[[adapters]]
id = "vllm-audio-chat"
kind = "vllm"
base_url = "http://{audio_chat_address}/v1"
trust_zone = "local"
transcription_mode = "audio_chat"
[adapters.capabilities]
operations = ["chat", "transcription"]
streaming_chat = false
function_tools = false
input_audio = true
audio_streaming_chat = false
audio_function_tools = false

[[adapters]]
id = "vllm-native-asr"
kind = "vllm"
base_url = "http://{native_asr_address}/v1"
trust_zone = "local"
transcription_mode = "native_asr"
[adapters.capabilities]
operations = ["transcription"]
streaming_chat = false
function_tools = false
input_audio = false
audio_streaming_chat = false
audio_function_tools = false

[[routes]]
id = "audio-chat-chat"
model_alias = "audio-public"
operation = "chat"
adapter_id = "vllm-audio-chat"
upstream_id = "served-audio-chat"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = true

[[routes]]
id = "audio-chat-transcription"
model_alias = "audio-public"
operation = "transcription"
adapter_id = "vllm-audio-chat"
upstream_id = "served-audio-transcriber"
requires_streaming_chat = false
requires_function_tools = false

[[routes]]
id = "native-asr-transcription"
model_alias = "native-public"
operation = "transcription"
adapter_id = "vllm-native-asr"
upstream_id = "served-native-asr"
requires_streaming_chat = false
requires_function_tools = false

[[application_keys]]
id = "fixture-client"
secret_ref = "env:KANATA_TEST_KEY"
permissions = [
  {{ model_alias = "audio-public", operation = "chat" }},
  {{ model_alias = "audio-public", operation = "transcription" }},
  {{ model_alias = "native-public", operation = "transcription" }}
]

[limits]
max_queue = 2
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = 2097152
max_extension_bytes = 8192

[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#
    );
    load_config(&contents)
}

pub fn two_origin_config(first: &str, second: &str) -> ValidatedConfig {
    let contents = format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080

[listeners.admin]
bind = "127.0.0.1"
port = 18081

[publication]
tailnet_addresses = ["100.64.0.1"]

[[adapters]]
id = "vllm-first"
kind = "vllm"
base_url = "http://{first}/v1"
trust_zone = "local"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = false
function_tools = false
input_audio = true

[[adapters]]
id = "vllm-second"
kind = "vllm"
base_url = "http://{second}/v1"
trust_zone = "local"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = false
function_tools = false
input_audio = true

[[routes]]
id = "first-route"
model_alias = "first-public"
operation = "chat"
adapter_id = "vllm-first"
upstream_id = "served-first"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = true

[[routes]]
id = "second-route"
model_alias = "second-public"
operation = "chat"
adapter_id = "vllm-second"
upstream_id = "served-second"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = true

[[application_keys]]
id = "fixture-client"
secret_ref = "env:KANATA_TEST_KEY"
permissions = [
  {{ model_alias = "first-public", operation = "chat" }},
  {{ model_alias = "second-public", operation = "chat" }}
]

[limits]
max_queue = 2
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = 2097152
max_extension_bytes = 8192

[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#
    );
    load_config(&contents)
}

pub fn multi_route_audio_chat_config(address: &str) -> ValidatedConfig {
    let contents = format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080

[listeners.admin]
bind = "127.0.0.1"
port = 18081

[publication]
tailnet_addresses = ["100.64.0.1"]

[[adapters]]
id = "vllm-fixture"
kind = "vllm"
base_url = "http://{address}/v1"
trust_zone = "local"
transcription_mode = "audio_chat"
[adapters.capabilities]
operations = ["chat", "transcription"]
streaming_chat = false
function_tools = false
input_audio = true

[[routes]]
id = "vllm-chat-text"
model_alias = "vllm-text-public"
operation = "chat"
adapter_id = "vllm-fixture"
upstream_id = "served-text"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = false

[[routes]]
id = "vllm-chat-audio"
model_alias = "vllm-audio-public"
operation = "chat"
adapter_id = "vllm-fixture"
upstream_id = "served-audio"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = true

[[routes]]
id = "vllm-transcription"
model_alias = "vllm-audio-public"
operation = "transcription"
adapter_id = "vllm-fixture"
upstream_id = "served-transcriber"
requires_streaming_chat = false
requires_function_tools = false

[[application_keys]]
id = "fixture-client"
secret_ref = "env:KANATA_TEST_KEY"
permissions = [
  {{ model_alias = "vllm-text-public", operation = "chat" }},
  {{ model_alias = "vllm-audio-public", operation = "chat" }},
  {{ model_alias = "vllm-audio-public", operation = "transcription" }}
]

[limits]
max_queue = 2
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = 2097152
max_extension_bytes = 8192

[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#
    );
    load_config(&contents)
}

fn load_config(contents: &str) -> ValidatedConfig {
    let id = NEXT_CONFIG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("kanata-vllm-audio-{id}.toml"));
    fs::write(&path, contents).unwrap_or_else(|_| panic!("write vLLM audio config"));
    let result =
        config::load(&path).unwrap_or_else(|error| panic!("load vLLM audio config: {error}"));
    let _ = fs::remove_file(path);
    result
}

pub fn route<'a>(config: &'a ValidatedConfig, route_id: &str) -> &'a ValidatedRoute {
    config
        .routes()
        .iter()
        .find(|route| route.identity().route_id == route_id)
        .unwrap_or_else(|| panic!("configured route {route_id}"))
}

pub fn routed(config: &ValidatedConfig, route_id: &str, request: Request) -> RoutedRequest {
    let route = route(config, route_id);
    RoutedRequest::new(
        RequestContext {
            request_id: "audio-fixture-request".into(),
            route: route.identity().clone(),
            trust_zone: TrustZone::Local,
            extensions: Extensions::default(),
        },
        request,
    )
    .unwrap_or_else(|error| panic!("routed audio request: {error:?}"))
}

pub fn chat_request(alias: &str, content: Vec<ChatContent>) -> Request {
    Request::Chat(ChatRequest {
        model: ModelAlias(alias.to_owned()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content,
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    })
}

pub fn text(value: impl Into<String>) -> ChatContent {
    ChatContent::Text { text: value.into() }
}

pub fn audio(format: InputAudioFormat, bytes: Vec<u8>) -> ChatContent {
    ChatContent::InputAudio {
        audio: ValidatedAudio::new(format, bytes).unwrap_or_else(|_| panic!("fixture audio")),
    }
}

pub fn transcription_request(
    alias: &str,
    media_type: &str,
    bytes: Vec<u8>,
    language: Option<String>,
    prompt: Option<String>,
) -> Request {
    Request::Transcription(TranscriptionRequest {
        model: ModelAlias(alias.to_owned()),
        file: ValidatedFile::new("voice.audio", media_type, bytes)
            .unwrap_or_else(|_| panic!("fixture transcription file")),
        language,
        prompt,
        extensions: Extensions::default(),
    })
}
