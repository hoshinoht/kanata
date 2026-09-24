use std::sync::atomic::{AtomicUsize, Ordering};

use kanata::config::{ValidatedConfig, load};

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn config_with_audio(max_body_bytes: usize, max_audio_bytes: usize) -> ValidatedConfig {
    const CAPABILITIES: &str = "operations = [\"chat\", \"transcription\"]\nstreaming_chat = true\nfunction_tools = true\n\n[[adapters]]\nid = \"openrouter-remote\"";
    const AUDIO_CAPABILITIES: &str = "operations = [\"chat\", \"transcription\"]\nstreaming_chat = true\nfunction_tools = true\ninput_audio = true\n\n[[adapters]]\nid = \"openrouter-remote\"";
    const ROUTE: &str = "requires_streaming_chat = true\nrequires_function_tools = true\n\n[[routes]]\nid = \"vllm-transcription\"";
    const AUDIO_ROUTE: &str = "requires_streaming_chat = true\nrequires_function_tools = true\nallows_input_audio = true\n\n[[routes]]\nid = \"vllm-transcription\"";

    let mut contents =
        std::fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
    assert!(contents.contains(CAPABILITIES));
    contents = contents.replacen(CAPABILITIES, AUDIO_CAPABILITIES, 1);
    assert!(contents.contains(ROUTE));
    contents = contents.replacen(ROUTE, AUDIO_ROUTE, 1);
    assert!(contents.contains("max_body_bytes = 1048576"));
    contents = contents.replace(
        "max_body_bytes = 1048576",
        &format!("max_body_bytes = {max_body_bytes}"),
    );
    assert!(contents.contains("max_audio_bytes = 26214400"));
    contents = contents.replace(
        "max_audio_bytes = 26214400",
        &format!("max_audio_bytes = {max_audio_bytes}"),
    );

    let path = std::env::temp_dir().join(format!(
        "kanata-chat-audio-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, contents).expect("audio config writes");
    let result = load(&path);
    std::fs::remove_file(path).expect("audio config removes");
    result.expect("audio config validates")
}

pub(crate) fn adapters(config: &ValidatedConfig) -> Vec<crate::support::AdapterSpec> {
    ["vllm-private", "ollama-local"]
        .into_iter()
        .map(|id| {
            crate::support::adapter_spec(
                id,
                crate::support::capabilities(config, id),
                crate::support::chat_outcome("mock audio response"),
            )
        })
        .collect()
}
