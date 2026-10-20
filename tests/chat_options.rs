#[path = "support/scratch.rs"]
mod scratch;
#[path = "support/gateway.rs"]
mod support;

use axum::http::StatusCode;
use kanata::config::ValidatedConfig;
use kanata::core::{
    JsonSchemaFormat, MAX_RESPONSE_SCHEMA_BYTES, MAX_RESPONSE_SCHEMA_DEPTH, Penalty,
    ReasoningEffort, ResponseFormat, Temperature, TopP,
};
use serde_json::{Value, json};

use support::{
    adapter_spec, capabilities, chat_outcome, chat_request, config, core_chat, load_text,
    recorded_len, response_json, server_with, take_request,
};

const OLLAMA_CAPABILITIES: &str = "id = \"ollama-local\"\nkind = \"ollama\"\nbase_url = \"http://ollama.invalid:11434\"\ntrust_zone = \"local\"\n[adapters.capabilities]\noperations = [\"chat\"]\nstreaming_chat = true\nfunction_tools = true\n";
const CODEX_CAPABILITIES_END: &str = "function_tools = true\n\n[[routes]]";

fn option_config() -> ValidatedConfig {
    let contents = std::fs::read_to_string("tests/fixtures/config/example.toml")
        .expect("example config")
        .replace(
            OLLAMA_CAPABILITIES,
            &format!(
                "{OLLAMA_CAPABILITIES}structured_output = true\nsampling_controls = true\nreasoning_control = true\n"
            ),
        )
        .replacen(
            CODEX_CAPABILITIES_END,
            "function_tools = true\nreasoning_control = true\n\n[[routes]]",
            1,
        )
        .replacen(
            "upstream_id = \"llama3.2:latest\"\n",
            "upstream_id = \"llama3.2:latest\"\nmax_output_tokens = 4096\n",
            1,
        );
    let config = load_text(&contents).expect("option config validates");
    assert!(capabilities(&config, "ollama-local").structured_output);
    config
}

/// The local route served by `fm serve`, which declares sampling controls.
fn apple_fm_config() -> ValidatedConfig {
    let contents = std::fs::read_to_string("tests/fixtures/config/example.toml")
        .expect("example config")
        .replace("http://ollama.invalid:11434", "http://127.0.0.1:9/v1")
        .replacen("kind = \"ollama\"", "kind = \"apple_fm\"", 1)
        .replacen(
            "function_tools = true\n",
            "function_tools = false\nsampling_controls = true\n",
            1,
        )
        .replacen(
            "requires_function_tools = true",
            "requires_function_tools = false",
            1,
        );
    load_text(&contents).expect("apple_fm config validates")
}

fn vllm_config(reasoning_control: bool) -> ValidatedConfig {
    let mut contents =
        std::fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
    if reasoning_control {
        contents = contents.replacen(
            "function_tools = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
            "function_tools = true\nreasoning_control = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
            1,
        );
    }
    let config = load_text(&contents).expect("vllm config validates");
    assert_eq!(
        capabilities(&config, "vllm-private").reasoning_control,
        reasoning_control
    );
    config
}

fn vllm_server(config: &ValidatedConfig) -> support::ServerWithRequests {
    server_with(
        config,
        vec![adapter_spec(
            "vllm-private",
            capabilities(config, "vllm-private"),
            chat_outcome("ok"),
        )],
    )
}

fn server(config: &ValidatedConfig) -> support::ServerWithRequests {
    server_with(
        config,
        vec![
            adapter_spec(
                "ollama-local",
                capabilities(config, "ollama-local"),
                chat_outcome("ok"),
            ),
            adapter_spec(
                "codex-private",
                capabilities(config, "codex-private"),
                chat_outcome("ok"),
            ),
        ],
    )
}

fn body(model: &str, options: Value) -> String {
    let mut body = json!({"model": model, "messages": [{"role": "user", "content": "x"}]});
    body.as_object_mut()
        .expect("object")
        .extend(options.as_object().expect("options object").clone());
    body.to_string()
}

fn nested(depth: usize) -> Value {
    (1..depth).fold(json!({}), |inner, _| json!({ "properties": inner }))
}

async fn assert_rejected(
    server: &kanata::server::TwoPlaneServer,
    model: &str,
    options: Value,
    param: Value,
) {
    let response = server
        .client_oneshot(chat_request(&body(model, options.clone())))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{options}");
    let value = response_json(response).await;
    assert_eq!(value["error"]["code"], "invalid_request", "{options}");
    assert_eq!(value["error"]["param"], param, "{options}");
}

#[tokio::test]
async fn options_are_decoded_and_normalized_into_core() {
    let config = option_config();
    let (server, requests) = server(&config);
    let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
    let response = server
        .client_oneshot(chat_request(&body(
            "local-chat",
            json!({
                "response_format": {"type": "json_schema", "json_schema": {
                    "name": "reply_v1", "schema": schema, "strict": true, "description": "d"
                }},
                "temperature": 0.2,
                "top_p": 1,
                "seed": -7,
                "max_completion_tokens": 256,
                "reasoning_effort": "xhigh",
                "stop": ["END", "STOP"],
                "presence_penalty": -0.5,
                "frequency_penalty": 2
            }),
        )))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let options = core_chat(take_request(&requests)).options;
    assert_eq!(
        options.response_format,
        Some(ResponseFormat::JsonSchema {
            json_schema: JsonSchemaFormat::new(
                "reply_v1".into(),
                Some("d".into()),
                schema,
                Some(true)
            )
            .expect("valid schema"),
        })
    );
    assert_eq!(
        options.sampling.temperature,
        Some(Temperature::new(0.2).unwrap())
    );
    assert_eq!(options.sampling.top_p, Some(TopP::new(1.0).unwrap()));
    assert_eq!(options.sampling.seed, Some(-7));
    assert_eq!(options.max_output_tokens, Some(256));
    assert_eq!(options.reasoning_effort, Some(ReasoningEffort::Xhigh));
    assert_eq!(
        options.sampling.stop,
        vec!["END".to_owned(), "STOP".to_owned()]
    );
    assert_eq!(
        options.sampling.presence_penalty,
        Some(Penalty::new(-0.5).unwrap())
    );
    assert_eq!(
        options.sampling.frequency_penalty,
        Some(Penalty::new(2.0).unwrap())
    );

    let single = server
        .client_oneshot(chat_request(&body("local-chat", json!({"stop": "END"}))))
        .await
        .expect("response");
    assert_eq!(single.status(), StatusCode::OK);
    assert_eq!(
        core_chat(take_request(&requests)).options.sampling.stop,
        vec!["END".to_owned()]
    );
}

#[tokio::test]
async fn out_of_contract_options_name_the_offending_param() {
    let config = option_config();
    let (server, requests) = server(&config);
    let too_large = json!({"type": "object", "description": "x".repeat(MAX_RESPONSE_SCHEMA_BYTES)});
    let schema_format = |schema: Value| json!({"response_format": {"type": "json_schema", "json_schema": {"name": "n", "schema": schema}}});
    let cases = [
        (json!({"temperature": 2.01}), "temperature"),
        (json!({"temperature": "1"}), "temperature"),
        (json!({"top_p": 0}), "top_p"),
        (json!({"seed": 1.5}), "seed"),
        (json!({"max_tokens": 0}), "max_tokens"),
        (json!({"max_tokens": 1_048_577}), "max_tokens"),
        (
            json!({"max_tokens": 5, "max_completion_tokens": 5}),
            "max_completion_tokens",
        ),
        (json!({"reasoning_effort": "extreme"}), "reasoning_effort"),
        (json!({"reasoning_effort": null}), "reasoning_effort"),
        (json!({"response_format": null}), "response_format"),
        (
            json!({"response_format": {"type": "json_object", "extra": 1}}),
            "response_format",
        ),
        (
            json!({"response_format": {"type": "json_schema", "json_schema": {"name": "n", "schema": {}, "extra": 1}}}),
            "response_format",
        ),
        (
            json!({"response_format": {"type": "json_schema", "json_schema": {"name": "bad name", "schema": {}}}}),
            "response_format",
        ),
        (schema_format(json!([])), "response_format"),
        (
            schema_format(nested(MAX_RESPONSE_SCHEMA_DEPTH + 1)),
            "response_format",
        ),
        (schema_format(too_large), "response_format"),
        (json!({"stop": ""}), "stop"),
        (json!({"stop": null}), "stop"),
        (json!({"stop": [1]}), "stop"),
        (json!({"stop": ["a", "b", "c", "d", "e"]}), "stop"),
        (json!({"presence_penalty": "1"}), "presence_penalty"),
        (json!({"frequency_penalty": 2.01}), "frequency_penalty"),
    ];
    for (options, param) in cases {
        assert_rejected(&server, "local-chat", options, json!(param)).await;
    }

    let accepted = server
        .client_oneshot(chat_request(&body(
            "local-chat",
            schema_format(nested(MAX_RESPONSE_SCHEMA_DEPTH)),
        )))
        .await
        .expect("response");
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(recorded_len(&requests), 1);
}

#[tokio::test]
async fn options_without_a_declared_capability_are_rejected_before_dispatch() {
    let config = config();
    let (server, requests) = server(&config);
    for (options, param) in [
        (
            json!({"response_format": {"type": "json_object"}}),
            "response_format",
        ),
        (json!({"top_p": 0.5}), "top_p"),
        (json!({"reasoning_effort": "low"}), "reasoning_effort"),
    ] {
        assert_rejected(&server, "local-chat", options, json!(param)).await;
    }
    assert_eq!(recorded_len(&requests), 0);

    let text = server
        .client_oneshot(chat_request(&body(
            "local-chat",
            json!({"response_format": {"type": "text"}}),
        )))
        .await
        .expect("response");
    assert_eq!(text.status(), StatusCode::OK);
}

#[tokio::test]
async fn max_output_tokens_are_dropped_on_a_route_without_sampling_controls() {
    let config = config();
    let (server, requests) = server(&config);
    let response = server
        .client_oneshot(chat_request(&body(
            "codex-chat",
            json!({"max_completion_tokens": 16}),
        )))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        core_chat(take_request(&requests)).options.max_output_tokens,
        None
    );
}

#[tokio::test]
async fn output_above_the_route_cap_is_rejected_under_its_wire_name() {
    let config = option_config();
    let (server, requests) = server(&config);
    assert_rejected(
        &server,
        "local-chat",
        json!({"max_tokens": 4097}),
        json!("max_tokens"),
    )
    .await;
    assert_rejected(
        &server,
        "local-chat",
        json!({"max_completion_tokens": 4097}),
        json!("max_completion_tokens"),
    )
    .await;
    assert_eq!(recorded_len(&requests), 0);

    let accepted = server
        .client_oneshot(chat_request(&body(
            "local-chat",
            json!({"max_tokens": 4096}),
        )))
        .await
        .expect("response");
    assert_eq!(accepted.status(), StatusCode::OK);
}

#[tokio::test]
async fn reasoning_effort_outside_the_route_backend_range_is_rejected() {
    let config = option_config();
    let (server, requests) = server(&config);
    assert_rejected(
        &server,
        "codex-chat",
        json!({"reasoning_effort": "xhigh"}),
        json!("reasoning_effort"),
    )
    .await;
    assert_rejected(
        &server,
        "codex-chat",
        json!({"reasoning_effort": "low"}),
        json!("reasoning_effort"),
    )
    .await;
    assert_rejected(&server, "codex-chat", json!({"seed": 1}), json!("seed")).await;
    for param in ["stop", "presence_penalty", "frequency_penalty"] {
        let value = if param == "stop" {
            json!("END")
        } else {
            json!(0)
        };
        assert_rejected(&server, "codex-chat", json!({ param: value }), json!(param)).await;
    }
    assert_eq!(recorded_len(&requests), 0);

    let accepted = server
        .client_oneshot(chat_request(&body(
            "codex-chat",
            json!({"reasoning_effort": "medium"}),
        )))
        .await
        .expect("response");
    assert_eq!(accepted.status(), StatusCode::OK);
}

#[tokio::test]
async fn route_cap_defaults_missing_tokens_and_preserves_smaller_values() {
    let config = option_config();
    let (server, requests) = server(&config);
    for (options, expected) in [
        (json!({}), 4096),
        (json!({"max_tokens": 64}), 64),
        (json!({"max_completion_tokens": 128}), 128),
    ] {
        let response = server
            .client_oneshot(chat_request(&body("local-chat", options)))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let routed = take_request(&requests);
        assert_eq!(core_chat(routed).options.max_output_tokens, Some(expected));
    }
}

#[tokio::test]
async fn stop_and_penalties_are_rejected_where_the_backend_cannot_honour_them() {
    let config = apple_fm_config();
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "ollama-local",
            capabilities(&config, "ollama-local"),
            chat_outcome("ok"),
        )],
    );
    for (options, param) in [
        (json!({"stop": "END"}), "stop"),
        (json!({"presence_penalty": 1}), "presence_penalty"),
        (json!({"frequency_penalty": 1}), "frequency_penalty"),
    ] {
        assert_rejected(&server, "local-chat", options, json!(param)).await;
    }
    assert_eq!(recorded_len(&requests), 0);
    let accepted = server
        .client_oneshot(chat_request(&body(
            "local-chat",
            json!({"temperature": 0.5}),
        )))
        .await
        .expect("response");
    assert_eq!(accepted.status(), StatusCode::OK);
}

#[tokio::test]
async fn vllm_reasoning_effort_needs_reasoning_control_and_excludes_the_thinking_switch() {
    let config = vllm_config(false);
    let (server, requests) = vllm_server(&config);
    assert_rejected(
        &server,
        "private-chat",
        json!({"reasoning_effort": "low"}),
        json!("reasoning_effort"),
    )
    .await;
    assert_eq!(recorded_len(&requests), 0);

    let config = vllm_config(true);
    let (server, requests) = vllm_server(&config);
    assert_rejected(
        &server,
        "private-chat",
        json!({"reasoning_effort": "high", "chat_template_kwargs": {"enable_thinking": false}}),
        json!("chat_template_kwargs"),
    )
    .await;
    assert_eq!(recorded_len(&requests), 0);
    for (options, expected) in [
        (
            json!({"reasoning_effort": "high"}),
            Some(ReasoningEffort::High),
        ),
        (json!({}), None),
    ] {
        let response = server
            .client_oneshot(chat_request(&body("private-chat", options)))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let chat = core_chat(take_request(&requests));
        assert_eq!(chat.options.reasoning_effort, expected);
    }
}
