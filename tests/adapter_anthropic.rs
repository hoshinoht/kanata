//! `kind = "anthropic"`: Claude Messages API with a Console key, private-only. Wire behaviour
//! against a loopback TLS fixture lives in `crates/kanata-adapters/src/anthropic/tests.rs`.

#[path = "support/scratch.rs"]
mod scratch;

use std::time::Duration;

use kanata::{
    adapter::{Adapter, AdapterOutput, anthropic::AnthropicAdapter},
    auth::{SecretResolutionError, SecretResolver},
    config::{Plane, ProviderKind, SecretReference, ValidatedConfig},
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, Extensions, FunctionTool,
        InputAudioFormat, ModelAlias, ReasoningEffort, Request, RequestContext, ResponseFormat,
        RoutedRequest, ToolCall, ToolChoice, TrustZone, ValidatedAudio,
    },
};

/// A published Ollama route next to an Anthropic effort family.
fn source(extra: &str) -> String {
    scratch::compact_config(
        r#"[{ model_alias = "local", operation = "chat" }]"#,
        &format!(
            r#"[[adapters]]
id = "ollama"
kind = "ollama"
base_url = "http://127.0.0.1:9"
trust_zone = "local"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = false
function_tools = false
[[adapters]]
id = "anthropic"
kind = "anthropic"
base_url = "https://api.anthropic.com/v1"
trust_zone = "external"
secret_ref = "env:ANTHROPIC_KEY"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = true
function_tools = true
structured_output = true
reasoning_control = true
{extra}
[models.chat.local]
adapter_id = "ollama"
upstream_id = "qwen3:0.6b"
[models.chat.claude]
adapter_id = "anthropic"
upstream_id = "claude-opus-5-5"
default_effort = "medium"
efforts = ["low", "high"]"#
        ),
        &[
            scratch::key_record(
                "mixed",
                "mixed-key",
                "permissions = [\n  { model_alias = \"local\", operation = \"chat\" },\n  { model_alias = \"claude\", operation = \"chat\" },\n]",
            ),
            scratch::key_record(
                "public-only",
                "public-only-key",
                "permissions = [{ model_alias = \"local\", operation = \"chat\" }]",
            ),
        ]
        .concat(),
    )
}

use scratch::load_text as load;

fn error(text: &str) -> String {
    load(text).unwrap_err().to_string()
}

struct Resolver;

impl SecretResolver for Resolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        match reference {
            SecretReference::Env(name) if name == "ANTHROPIC_KEY" => {
                Ok(b"sk-ant-fixture-not-secret".to_vec())
            }
            _ => Err(SecretResolutionError),
        }
    }
}

#[test]
fn config_is_private_only_with_validated_settings() {
    let config = load(&source("")).unwrap();
    let adapter = &config.adapters()[1];
    assert_eq!(adapter.kind(), ProviderKind::Anthropic);
    assert!(adapter.kind().is_private_only());
    assert!(!adapter.kind().applies_route_effort_pin());
    let settings = adapter.anthropic().unwrap();
    assert_eq!(settings.workspace_id(), None);
    assert_eq!(settings.default_max_output_tokens(), 16_000);
    assert!(config.adapters()[0].anthropic().is_none());

    let config = load(&source(
        "[adapters.anthropic]\nworkspace_id = \"wrkspc_01Fixture\"\ndefault_max_output_tokens = 64000",
    ))
    .unwrap();
    let settings = config.adapters()[1].anthropic().unwrap();
    assert_eq!(settings.workspace_id(), Some("wrkspc_01Fixture"));
    assert_eq!(settings.default_max_output_tokens(), 64_000);
}

#[test]
fn config_rejects_publication_and_invalid_adapters() {
    let base = source("");
    for alias in ["claude", "claude:high"] {
        let published = base.replace(
            "public_routes = [{ model_alias = \"local\", operation = \"chat\" }]",
            &format!("public_routes = [{{ model_alias = \"{alias}\", operation = \"chat\" }}]"),
        );
        assert_ne!(published, base);
        assert_eq!(
            error(&published),
            "config error at publication.public_routes[0]: anthropic_not_public"
        );
    }
    for (from, to, expected) in [
        (
            "https://api.anthropic.com/v1",
            "https://anthropic-proxy.invalid/v1",
            "adapters[1].base_url: anthropic_pinned_origin_required",
        ),
        (
            "secret_ref = \"env:ANTHROPIC_KEY\"\n",
            "",
            "adapters[1].secret_ref: required",
        ),
        (
            "operations = [\"chat\"]\nstreaming_chat = true",
            "operations = [\"chat\", \"transcription\"]\nstreaming_chat = true",
            "adapters[1].capabilities.operations: anthropic_chat_only",
        ),
        (
            "trust_zone = \"external\"",
            "trust_zone = \"private_network\"",
            "adapters[1].trust_zone: provider_zone_mismatch",
        ),
        (
            "reasoning_control = true",
            "reasoning_control = true\nsampling_controls = true",
            "adapters[1].capabilities.sampling_controls: unsupported_by_adapter_kind",
        ),
        (
            "reasoning_control = true",
            "reasoning_control = true\ninput_audio = true",
            "adapters[1].capabilities.input_audio: unsupported_by_adapter_kind",
        ),
        (
            "reasoning_control = true",
            "reasoning_control = true\n[adapters.anthropic]\nworkspace_id = \"team\"",
            "adapters[1].anthropic.workspace_id: invalid_workspace_id",
        ),
        (
            "function_tools = false\n",
            "function_tools = false\n[adapters.anthropic]\ndefault_max_output_tokens = 1024\n",
            "adapters[0].anthropic: anthropic_only",
        ),
        (
            "efforts = [\"low\", \"high\"]",
            "efforts = [\"minimal\", \"high\"]",
            "efforts: unsupported_effort",
        ),
    ] {
        let text = base.replacen(from, to, 1);
        assert_ne!(text, base, "{from}");
        let message = error(&text);
        assert!(message.ends_with(expected), "{message}");
    }
}

#[test]
fn public_plane_drops_anthropic_routes_adapters_and_scoped_keys() {
    let dir = scratch::config_dir("anthropic", &source(""));
    let file = dir.join("config.toml");
    let file = file.to_str().unwrap();
    let check = |plane: &str| {
        kanata::cli::run(["check", "--config", file, "--plane", plane].map(str::to_owned))
    };
    for plane in ["private", "public"] {
        assert_eq!(check(plane), Ok(Some("configuration valid".to_owned())));
    }
    let (_, public) = kanata::config::load_plane(file, Plane::Public).unwrap();
    assert!(
        public
            .adapters()
            .iter()
            .all(|adapter| adapter.kind() != ProviderKind::Anthropic)
    );
    let aliases: Vec<_> = public
        .routes()
        .iter()
        .map(|route| route.identity().selector.model_alias.0.as_str())
        .collect();
    assert_eq!(aliases, ["local"]);
    let keys: Vec<_> = public.keys().iter().map(|key| key.id()).collect();
    assert_eq!(keys, ["public-only"]);

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["routes", "--config", file])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for alias in ["claude", "claude:low", "claude:high"] {
        let row: Vec<_> = text
            .lines()
            .find(|line| line.starts_with(&format!("{alias} ")))
            .expect("row")
            .split_whitespace()
            .take(4)
            .collect();
        assert_eq!(row, [alias, "chat", "anthropic", "never"]);
    }
}

fn routed(config: &ValidatedConfig, chat: ChatRequest) -> RoutedRequest {
    let route = config
        .routes()
        .iter()
        .find(|route| route.identity().selector.model_alias.0 == "claude")
        .unwrap();
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture".into(),
            route: route.identity().clone(),
            trust_zone: TrustZone::External,
            extensions: Extensions::default(),
        },
        Request::Chat(chat),
    )
    .unwrap()
}

fn chat(messages: Vec<ChatMessage>) -> ChatRequest {
    ChatRequest {
        model: ModelAlias("claude".into()),
        messages,
        tools: vec![FunctionTool {
            name: "lookup".into(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
        }],
        tool_choice: ToolChoice::Auto,
        stream: false,
        options: Default::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
        extensions: Extensions::default(),
    }
}

fn user(text: &str) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: vec![ChatContent::Text { text: text.into() }],
    }
}

fn tool_call(arguments: &str) -> ChatMessage {
    ChatMessage {
        role: ChatRole::Assistant,
        content: vec![ChatContent::ToolCall {
            call: ToolCall {
                id: "toolu_1".into(),
                name: "lookup".into(),
                arguments: arguments.into(),
            },
        }],
    }
}

#[tokio::test]
async fn invalid_requests_are_rejected_before_any_network_call() {
    let config = load(&source("")).unwrap();
    let adapter = AnthropicAdapter::from_config(&config, "anthropic", &Resolver).unwrap();
    let system_late = chat(vec![
        user("Hi"),
        ChatMessage {
            role: ChatRole::System,
            content: vec![ChatContent::Text {
                text: "Late instructions".into(),
            }],
        },
    ]);
    let mut json_object = chat(vec![user("Hi")]);
    json_object.options.response_format = Some(ResponseFormat::JsonObject);
    let mut minimal = chat(vec![user("Hi")]);
    minimal.options.reasoning_effort = Some(ReasoningEffort::Minimal);
    let audio = chat(vec![ChatMessage {
        role: ChatRole::User,
        content: vec![ChatContent::InputAudio {
            audio: ValidatedAudio::new(InputAudioFormat::Wav, vec![1, 2, 3]).unwrap(),
        }],
    }]);
    for (request, expected) in [
        (
            chat(vec![user("Hi"), tool_call("[1]")]),
            ErrorKind::InvalidRequest,
        ),
        (
            chat(vec![user("Hi"), tool_call("{not json")]),
            ErrorKind::InvalidRequest,
        ),
        (system_late, ErrorKind::InvalidRequest),
        (json_object, ErrorKind::InvalidRequest),
        (minimal, ErrorKind::InvalidRequest),
        (audio, ErrorKind::UnsupportedOperation),
    ] {
        // A request that reached the network would not resolve within this bound.
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            adapter.execute(routed(&config, request)),
        )
        .await
        .expect("local rejection");
        let kind = match result {
            Err(error) => error.kind,
            Ok(AdapterOutput::Complete(_) | AdapterOutput::Events(_)) => panic!("dispatched"),
        };
        assert_eq!(kind, expected);
    }
}
