use std::sync::atomic::{AtomicUsize, Ordering};

use kanata::{
    adapter::Adapter,
    config::{self, ValidatedConfig},
    core::{
        ChatOptions, JsonSchemaFormat, ReasoningEffort, Request as CoreRequest, ResponseFormat,
        SamplingOptions, Temperature, TopP,
    },
};
use serde_json::{Value, json};

use crate::support::{MockServer, ResponseSpec, TEXT_RESPONSE, adapter, routed, text_request};

const OPTIONS_REQUEST: &str = include_str!("../fixtures/ollama/chat-options-request.json");
static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

fn options_config(address: &str) -> ValidatedConfig {
    let contents = include_str!("../fixtures/config/example.toml")
        .replace(
            "http://ollama.invalid:11434",
            &format!("http://{address}/v1"),
        )
        .replacen(
            "function_tools = true\n",
            "function_tools = true\nstructured_output = true\nsampling_controls = true\nreasoning_control = true\n",
            1,
        );
    let path = std::env::temp_dir().join(format!(
        "kanata-ollama-options-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, contents).expect("config writes");
    let result = config::load(&path);
    let _ = std::fs::remove_file(path);
    result.expect("options config loads")
}

fn options_request() -> CoreRequest {
    let CoreRequest::Chat(mut chat) = text_request() else {
        unreachable!()
    };
    chat.options = ChatOptions {
        response_format: Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaFormat::new(
                "greeting".into(),
                None,
                json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
                Some(true),
            )
            .expect("schema"),
        }),
        sampling: SamplingOptions {
            temperature: Some(Temperature::new(0.25).expect("temperature")),
            top_p: Some(TopP::new(0.5).expect("top_p")),
            seed: Some(42),
        },
        max_output_tokens: Some(128),
        max_output_tokens_param: Default::default(),
        reasoning_effort: Some(ReasoningEffort::None),
        enable_thinking: None,
    };
    CoreRequest::Chat(chat)
}

async fn sent_body(request: CoreRequest) -> Value {
    let mut mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
    let config = options_config(&mock.address);
    adapter(&config)
        .execute(routed(&config, request))
        .await
        .expect("adapter succeeds");
    mock.finish().await;
    let record = mock.requests.lock().expect("request lock")[0].clone();
    serde_json::from_slice(&record.body).expect("request json")
}

#[tokio::test]
async fn chat_options_are_encoded_and_absent_options_are_omitted() {
    let expected: Value = serde_json::from_str(OPTIONS_REQUEST).expect("fixture json");
    assert_eq!(sent_body(options_request()).await, expected);

    let plain = sent_body(text_request()).await;
    for key in [
        "response_format",
        "temperature",
        "top_p",
        "seed",
        "max_tokens",
        "reasoning_effort",
    ] {
        assert!(plain.get(key).is_none(), "{key} must be omitted");
    }
}
