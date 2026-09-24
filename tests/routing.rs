use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use kanata::config;
use kanata::core::{ModelAlias, Operation, RouteSelector};
use kanata::routing::Registry;

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

fn example() -> String {
    fs::read_to_string("tests/fixtures/config/example.toml").expect("example exists")
}

fn check(contents: String) -> Result<config::ValidatedConfig, String> {
    let path = std::env::temp_dir().join(format!(
        "kanata-routing-{}-{}.toml",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let result = config::load(&path).map_err(|error| error.to_string());
    fs::remove_file(path).expect("fixture removes");
    result
}

#[test]
fn registry_retains_exact_route_and_adapter_identity() {
    let config = check(example()).expect("example validates");
    let registry = Registry::from_validated(&config);
    let route = registry
        .resolve(&RouteSelector {
            model_alias: ModelAlias("codex-chat".into()),
            operation: Operation::Chat,
        })
        .expect("exact route");
    assert_eq!(route.identity.route_id, "codex-chat");
    assert_eq!(route.identity.upstream_id, "gpt-5-codex");
    assert_eq!(route.adapter_id, "codex-private");
    assert_eq!(registry.routes().count(), 5);
    assert_eq!(config.listeners().client().port(), 8080);
    assert_eq!(config.application_keys()[0].permissions().len(), 5);
    assert_eq!(config.limits().max_queue(), 32);
    assert_eq!(config.timeouts().overall_ms(), 60_000);
}

#[test]
fn aliases_do_not_wildcard_or_suffix_match() {
    let config = check(example()).expect("example validates");
    let registry = Registry::from_validated(&config);
    for alias in ["codex-chat-extra", "chat", "codex-*"] {
        assert!(
            registry
                .resolve(&RouteSelector {
                    model_alias: ModelAlias(alias.into()),
                    operation: Operation::Chat
                })
                .is_none()
        );
    }
}

#[test]
fn codex_effort_alias_routes_only_by_its_exact_selector_and_upstream_id() {
    let config = config::load("config/personal.example.toml").expect("personal config validates");
    let registry = Registry::from_validated(&config);
    let low = registry
        .resolve(&RouteSelector {
            model_alias: ModelAlias("gpt-6-luna:low".into()),
            operation: Operation::Chat,
        })
        .expect("configured effort alias");

    assert_eq!(low.identity.route_id, "codex-gpt-6-luna-low");
    assert_eq!(low.identity.upstream_id, "gpt-6-luna");
    for alias in ["gpt-6-luna", "gpt-6-luna:medium"] {
        assert!(
            registry
                .resolve(&RouteSelector {
                    model_alias: ModelAlias(alias.into()),
                    operation: Operation::Chat,
                })
                .is_none()
        );
    }
    assert!(
        registry
            .resolve(&RouteSelector {
                model_alias: ModelAlias("gpt-6-luna:low".into()),
                operation: Operation::Transcription,
            })
            .is_none()
    );
}

#[test]
fn invalid_routes_never_build_a_registry() {
    let duplicate = check(example().replace(
        "model_alias = \"remote-chat\"",
        "model_alias = \"local-chat\"",
    ));
    assert_eq!(
        duplicate.unwrap_err(),
        "config error at routes[3]: duplicate_selector"
    );
    let missing =
        check(example().replace("adapter_id = \"ollama-local\"", "adapter_id = \"missing\""));
    assert_eq!(
        missing.unwrap_err(),
        "config error at routes[0].adapter_id: missing_adapter"
    );
    let mismatch = check(example().replace(
        "operations = [\"chat\"]\nstreaming_chat = true\nfunction_tools = true",
        "operations = [\"chat\"]\nstreaming_chat = false\nfunction_tools = true",
    ));
    assert_eq!(
        mismatch.unwrap_err(),
        "config error at routes[0].requires_streaming_chat: unsupported_by_adapter"
    );
    let tools = check(example().replace(
        "operations = [\"chat\"]\nstreaming_chat = true\nfunction_tools = true",
        "operations = [\"chat\"]\nstreaming_chat = true\nfunction_tools = false",
    ));
    assert_eq!(
        tools.unwrap_err(),
        "config error at routes[0].requires_function_tools: unsupported_by_adapter"
    );
}

#[test]
fn rejects_codex_transcription_and_wildcard_aliases() {
    let codex = check(example().replace(
        "id = \"codex-private\"\nkind = \"codex\"\nbase_url = \"https://chatgpt.invalid/backend-api/codex\"\ntrust_zone = \"external\"\n[adapters.capabilities]\noperations = [\"chat\"]",
        "id = \"codex-private\"\nkind = \"codex\"\nbase_url = \"https://chatgpt.invalid/backend-api/codex\"\ntrust_zone = \"external\"\n[adapters.capabilities]\noperations = [\"chat\", \"transcription\"]",
    ));
    assert_eq!(
        codex.unwrap_err(),
        "config error at adapters[3].capabilities.operations: codex_chat_only"
    );
    let wildcard =
        check(example().replace("model_alias = \"local-chat\"", "model_alias = \"local-*\""));
    assert_eq!(
        wildcard.unwrap_err(),
        "config error at routes[0].model_alias: invalid_exact_alias"
    );
}

#[test]
fn audio_route_permissions_are_explicit_and_intersect_adapter_capabilities() {
    let unsupported = check(example().replace(
        "upstream_id = \"meta-llama/Meta-Llama-3.1-8B-Instruct\"\nrequires_streaming_chat = true",
        "upstream_id = \"meta-llama/Meta-Llama-3.1-8B-Instruct\"\nallows_input_audio = true\nrequires_streaming_chat = true",
    ));
    assert_eq!(
        unsupported.unwrap_err(),
        "config error at routes[1].allows_input_audio: unsupported_by_adapter"
    );

    let unsupported_audio_stream = example()
        .replace(
            "function_tools = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
            "function_tools = true\ninput_audio = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
        )
        .replace(
            "upstream_id = \"meta-llama/Meta-Llama-3.1-8B-Instruct\"\nrequires_streaming_chat = true",
            "upstream_id = \"meta-llama/Meta-Llama-3.1-8B-Instruct\"\nallows_input_audio = true\nallows_audio_streaming_chat = true\nrequires_streaming_chat = true",
        );
    assert_eq!(
        check(unsupported_audio_stream).unwrap_err(),
        "config error at routes[1].allows_audio_streaming_chat: unsupported_by_adapter"
    );

    let configured = example()
        .replace(
            "function_tools = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
            "function_tools = true\ninput_audio = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
        )
        .replace(
            "upstream_id = \"meta-llama/Meta-Llama-3.1-8B-Instruct\"\nrequires_streaming_chat = true",
            "upstream_id = \"meta-llama/Meta-Llama-3.1-8B-Instruct\"\nallows_input_audio = true\nrequires_streaming_chat = true",
        );
    let config = check(configured).expect("explicit audio route validates");
    let registry = Registry::from_validated(&config);
    let route = registry
        .resolve(&RouteSelector {
            model_alias: ModelAlias("private-chat".into()),
            operation: Operation::Chat,
        })
        .expect("configured route");
    assert!(route.capabilities.input_audio);
    assert!(!route.capabilities.audio_streaming_chat);
    assert!(!route.capabilities.audio_function_tools);
    assert!(route.capabilities.streaming_chat);
    assert!(route.capabilities.function_tools);
}

#[test]
fn multiple_vllm_transcription_modes_keep_exact_adapter_and_route_identity() {
    let mut contents = example().replace(
        "  { model_alias = \"codex-chat\", operation = \"chat\" },\n]",
        "  { model_alias = \"codex-chat\", operation = \"chat\" },\n  { model_alias = \"private-audio-chat\", operation = \"chat\" },\n  { model_alias = \"private-audio-transcribe\", operation = \"transcription\" },\n]",
    );
    contents.push_str(
        r#"

[[adapters]]
id = "vllm-audio-chat"
kind = "vllm"
base_url = "http://vllm-audio-chat.invalid:8000"
trust_zone = "private_network"
transcription_mode = "audio_chat"
[adapters.capabilities]
operations = ["chat", "transcription"]
streaming_chat = false
function_tools = false
input_audio = true

[[routes]]
id = "vllm-audio-chat-route"
model_alias = "private-audio-chat"
operation = "chat"
adapter_id = "vllm-audio-chat"
upstream_id = "audio-chat-model"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = true

[[routes]]
id = "vllm-audio-transcription-route"
model_alias = "private-audio-transcribe"
operation = "transcription"
adapter_id = "vllm-audio-chat"
upstream_id = "audio-chat-model"
requires_streaming_chat = false
requires_function_tools = false
"#,
    );
    let config = check(contents).expect("both independent vLLM configs validate");
    let registry = Registry::from_validated(&config);
    let native = registry
        .resolve(&RouteSelector {
            model_alias: ModelAlias("private-transcribe".into()),
            operation: Operation::Transcription,
        })
        .expect("native ASR route");
    let audio_chat = registry
        .resolve(&RouteSelector {
            model_alias: ModelAlias("private-audio-transcribe".into()),
            operation: Operation::Transcription,
        })
        .expect("audio-chat transcription route");
    let chat = registry
        .resolve(&RouteSelector {
            model_alias: ModelAlias("private-audio-chat".into()),
            operation: Operation::Chat,
        })
        .expect("audio chat route");

    assert_eq!(native.adapter_id, "vllm-private");
    assert_eq!(native.base_url.as_str(), "http://vllm.invalid:8000/");
    assert_eq!(audio_chat.adapter_id, "vllm-audio-chat");
    assert_eq!(
        audio_chat.base_url.as_str(),
        "http://vllm-audio-chat.invalid:8000/"
    );
    assert!(chat.capabilities.input_audio);
    assert_eq!(
        config.adapters()[1].transcription_mode(),
        Some(config::VllmTranscriptionMode::NativeAsr)
    );
    assert_eq!(
        config.adapters()[4].transcription_mode(),
        Some(config::VllmTranscriptionMode::AudioChat)
    );
    assert!(
        registry
            .resolve(&RouteSelector {
                model_alias: ModelAlias("private-audio-transcribe-extra".into()),
                operation: Operation::Transcription,
            })
            .is_none()
    );
}
