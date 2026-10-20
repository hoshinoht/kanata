#![allow(dead_code)]

use crate::scratch;

use kanata::{
    auth::{SecretResolutionError, SecretResolver},
    config::{SecretReference, ValidatedConfig},
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
    let body = format!(
        r#"[[adapters]]
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
"#
    );
    scratch::load_valid(&scratch::fixture_text(
        &body,
        &format!("{{ model_alias = \"{PUBLIC_MODEL}\", operation = \"{route_operation}\" }}"),
        1_048_576,
    ))
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
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
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
