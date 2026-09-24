#![allow(dead_code)]

use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use kanata::{
    auth::{SecretResolutionError, SecretResolver},
    config::{self, SecretReference, ValidatedConfig},
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, Extensions, ModelAlias, Operation,
        Request as CoreRequest, RequestContext, RouteIdentity, RoutedRequest, ToolChoice,
        TrustZone,
    },
};

pub const ADAPTER_ID: &str = "openrouter-fixture";
pub const ROUTE_ID: &str = "openrouter-chat";
pub const PUBLIC_MODEL: &str = "openrouter-public";
pub const UPSTREAM_MODEL: &str = "openai/gpt-4o-mini";
pub const TOKEN: &str = "fixture-openrouter-key";

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

pub struct SyntheticResolver;

impl SecretResolver for SyntheticResolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        match reference {
            SecretReference::Env(name) if name == "TEST_OPENROUTER_KEY" => {
                Ok(TOKEN.as_bytes().to_vec())
            }
            _ => Err(SecretResolutionError),
        }
    }
}

pub fn config_for(address: &str) -> ValidatedConfig {
    config_with(address, "chat", "chat", false, false, false)
}

pub fn config_with(
    address: &str,
    operations: &str,
    route_operation: &str,
    streaming: bool,
    tools: bool,
    input_audio: bool,
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
id = "{ADAPTER_ID}"
kind = "openrouter"
base_url = "{address}"
trust_zone = "external"
secret_ref = "env:TEST_OPENROUTER_KEY"
[adapters.capabilities]
operations = ["{operations}"]
streaming_chat = {streaming}
function_tools = {tools}
input_audio = {input_audio}
audio_streaming_chat = false
audio_function_tools = false

[[routes]]
id = "{ROUTE_ID}"
model_alias = "{PUBLIC_MODEL}"
operation = "{route_operation}"
adapter_id = "{ADAPTER_ID}"
upstream_id = "{UPSTREAM_MODEL}"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = false

[[application_keys]]
id = "fixture-client"
secret_ref = "env:KANATA_TEST_CLIENT_KEY"
permissions = [{{ model_alias = "{PUBLIC_MODEL}", operation = "{route_operation}" }}]

[limits]
max_queue = 2
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = 1048576
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
    let id = NEXT_CONFIG.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("kanata-openrouter-fixture-{id}.toml"));
    fs::write(&path, contents).unwrap_or_else(|_| panic!("write test config"));
    let result = config::load(&path).unwrap_or_else(|error| panic!("load test config: {error}"));
    let _ = fs::remove_file(path);
    result
}

pub fn adapter(config: &ValidatedConfig) -> kanata::adapter::openrouter::OpenRouterAdapter {
    kanata::adapter::openrouter::OpenRouterAdapter::from_config(
        config,
        ADAPTER_ID,
        ROUTE_ID,
        &SyntheticResolver,
    )
    .unwrap_or_else(|error| panic!("OpenRouter adapter: {error:?}"))
}

pub fn text_request() -> CoreRequest {
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias(PUBLIC_MODEL.into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatContent::Text {
                text: "Say hello".into(),
            }],
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    })
}

pub fn routed(config: &ValidatedConfig, request: CoreRequest) -> RoutedRequest {
    routed_with(
        config.routes()[0].identity().clone(),
        TrustZone::External,
        Extensions::default(),
        request,
    )
}

pub fn routed_with(
    route: RouteIdentity,
    trust_zone: TrustZone,
    extensions: Extensions,
    request: CoreRequest,
) -> RoutedRequest {
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture-request".into(),
            route,
            trust_zone,
            extensions,
        },
        request,
    )
    .unwrap_or_else(|error| panic!("routed request: {error:?}"))
}

pub fn transcription_route() -> RouteIdentity {
    RouteIdentity::new(
        ROUTE_ID,
        UPSTREAM_MODEL,
        ModelAlias(PUBLIC_MODEL.into()),
        Operation::Transcription,
    )
}
