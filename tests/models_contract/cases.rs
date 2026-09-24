use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use serde_json::Value;

use crate::support::{
    adapter_spec, capabilities, chat_outcome, config, config_with_public_routes, models_request,
    response_json, server_with, server_without_adapters, try_server_with,
};

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

fn ids(value: &Value) -> Vec<&str> {
    value["data"]
        .as_array()
        .expect("model data")
        .iter()
        .map(|model| model["id"].as_str().expect("model id"))
        .collect()
}

#[tokio::test]
async fn models_are_sorted_deduplicated_authorized_and_bound() {
    let config = config();
    let capabilities = capabilities(&config, "vllm-private");
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities,
            chat_outcome("unused"),
        )],
    );
    let response = server
        .client_oneshot(models_request())
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let value = response_json(response).await;
    assert_eq!(value["object"], "list");
    assert_eq!(
        ids(&value),
        vec!["private-chat", "private-transcribe"],
        "unbound routes must not be advertised"
    );
    for model in value["data"].as_array().expect("models") {
        assert_eq!(model["object"], "model");
        assert_eq!(model["created"], 0);
        assert_eq!(model["owned_by"], "kanata");
    }
}

#[tokio::test]
async fn models_keep_exact_key_permissions_with_a_bound_adapter() {
    let source = fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
    let old = "permissions = [\n  { model_alias = \"local-chat\", operation = \"chat\" },\n  { model_alias = \"private-chat\", operation = \"chat\" },\n  { model_alias = \"private-transcribe\", operation = \"transcription\" },\n  { model_alias = \"remote-chat\", operation = \"chat\" },\n  { model_alias = \"codex-chat\", operation = \"chat\" },\n]";
    let contents = source.replace(
        old,
        "permissions = [{ model_alias = \"private-chat\", operation = \"chat\" }]",
    );
    assert_ne!(contents, source);
    let path = std::env::temp_dir().join(format!(
        "kanata-models-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture");
    let config = kanata::config::load(&path).expect("restricted config");
    fs::remove_file(path).expect("fixture cleanup");

    let capabilities = capabilities(&config, "vllm-private");
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities,
            chat_outcome("unused"),
        )],
    );
    let response = server
        .client_oneshot(models_request())
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(ids(&response_json(response).await), vec!["private-chat"]);
}

#[tokio::test]
async fn models_only_constructor_hides_unbound_routes() {
    let server = server_without_adapters(&config());
    let response = server
        .client_oneshot(models_request())
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(ids(&response_json(response).await).is_empty());
}

#[tokio::test]
async fn public_models_default_empty_and_intersect_exact_public_routes() {
    let config = config_with_public_routes(&[]);
    let capabilities = capabilities(&config, "vllm-private");
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities.clone(),
            chat_outcome("unused"),
        )],
    );
    let private = server
        .client_oneshot(models_request())
        .await
        .expect("private models response");
    assert_eq!(private.status(), StatusCode::OK);
    assert_eq!(
        ids(&response_json(private).await),
        ["private-chat", "private-transcribe"]
    );

    let public = server
        .public_oneshot(models_request())
        .await
        .expect("configured public router");
    assert_eq!(public.status(), StatusCode::OK);
    assert!(ids(&response_json(public).await).is_empty());

    let config = config_with_public_routes(&[("private-chat", "chat")]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities,
            chat_outcome("unused"),
        )],
    );
    let public = server
        .public_oneshot(models_request())
        .await
        .expect("configured public router");
    assert_eq!(public.status(), StatusCode::OK);
    assert_eq!(ids(&response_json(public).await), ["private-chat"]);

    let config = config_with_public_routes(&[("private-transcribe", "transcription")]);
    let asr_capabilities = crate::support::capabilities(&config, "vllm-private");
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            asr_capabilities,
            chat_outcome("unused"),
        )],
    );
    let public = server
        .public_oneshot(models_request())
        .await
        .expect("configured public router");
    assert_eq!(public.status(), StatusCode::OK);
    assert_eq!(ids(&response_json(public).await), ["private-transcribe"]);
}

#[test]
fn duplicate_missing_and_mismatched_adapter_bindings_fail_closed() {
    let config = config();
    let capabilities = capabilities(&config, "vllm-private");
    assert!(
        try_server_with(
            &config,
            vec![
                adapter_spec("vllm-private", capabilities.clone(), chat_outcome("unused")),
                adapter_spec("vllm-private", capabilities.clone(), chat_outcome("unused")),
            ],
        )
        .is_err()
    );
    assert!(
        try_server_with(
            &config,
            vec![adapter_spec(
                "missing-adapter",
                capabilities.clone(),
                chat_outcome("unused")
            )],
        )
        .is_err()
    );

    let mut mismatched = capabilities;
    mismatched.function_tools = false;
    assert!(
        try_server_with(
            &config,
            vec![adapter_spec(
                "vllm-private",
                mismatched,
                chat_outcome("unused")
            )],
        )
        .is_err()
    );
}

fn load_variant(replacements: &[(&str, &str)]) -> kanata::config::ValidatedConfig {
    let mut contents =
        fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
    for (old, new) in replacements {
        assert!(contents.contains(old), "fixture contains {old}");
        contents = contents.replace(old, new);
    }
    let path = std::env::temp_dir().join(format!(
        "kanata-models-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture");
    let config = kanata::config::load(&path).expect("variant config");
    fs::remove_file(path).expect("fixture cleanup");
    config
}

fn entry<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["data"]
        .as_array()
        .expect("model data")
        .iter()
        .find(|model| model["id"] == id)
        .unwrap_or_else(|| panic!("model {id} listed"))
}

#[tokio::test]
async fn models_report_kanata_capabilities_per_alias() {
    let config = load_variant(&[
        (
            "trust_zone = \"local\"\n[adapters.capabilities]\noperations = [\"chat\"]\n",
            "trust_zone = \"local\"\n[adapters.capabilities]\noperations = [\"chat\"]\nstructured_output = true\nsampling_controls = true\nreasoning_control = true\n",
        ),
        (
            "backend-api/codex\"\ntrust_zone = \"external\"\n[adapters.capabilities]\n",
            "backend-api/codex\"\ntrust_zone = \"external\"\n[adapters.capabilities]\nreasoning_control = true\n",
        ),
        (
            "upstream_id = \"llama3.2:latest\"\n",
            "upstream_id = \"llama3.2:latest\"\ncontext_tokens = 16384\n",
        ),
    ]);
    let specs = ["ollama-local", "codex-private"]
        .into_iter()
        .map(|id| adapter_spec(id, capabilities(&config, id), chat_outcome("unused")))
        .collect();
    let (server, _) = server_with(&config, specs);
    let value = response_json(
        server
            .client_oneshot(models_request())
            .await
            .expect("response"),
    )
    .await;
    assert_eq!(ids(&value), ["codex-chat", "local-chat"]);
    assert_eq!(
        entry(&value, "local-chat")["kanata"],
        serde_json::json!({
            "operations": ["chat"], "structured_output": true, "sampling_controls": true,
            "reasoning_control": true, "function_tools": true, "streaming": true,
            "input_audio": false, "trust_zone": "local", "reasoning_efforts": null,
            "context_tokens": 16384,
            "admission": {
                "max_in_flight": 8, "max_queue": 32, "queue_ms": 1000,
                "adapter_max_in_flight": null
            }
        })
    );
    assert_eq!(
        entry(&value, "codex-chat")["kanata"],
        serde_json::json!({
            "operations": ["chat"], "structured_output": false, "sampling_controls": false,
            "reasoning_control": true, "function_tools": true, "streaming": true,
            "input_audio": false, "trust_zone": "external",
            "reasoning_efforts": ["low", "medium", "high"], "context_tokens": null,
            "admission": {
                "max_in_flight": 8, "max_queue": 32, "queue_ms": 1000,
                "adapter_max_in_flight": null
            }
        })
    );
}

#[tokio::test]
async fn models_merge_operations_for_one_alias() {
    let config = load_variant(&[
        (
            "model_alias = \"private-transcribe\"\noperation",
            "model_alias = \"private-chat\"\noperation",
        ),
        (
            "{ model_alias = \"private-transcribe\", operation = \"transcription\" }",
            "{ model_alias = \"private-chat\", operation = \"transcription\" }",
        ),
    ]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities(&config, "vllm-private"),
            chat_outcome("unused"),
        )],
    );
    let value = response_json(
        server
            .client_oneshot(models_request())
            .await
            .expect("response"),
    )
    .await;
    assert_eq!(ids(&value), ["private-chat"]);
    let kanata = &entry(&value, "private-chat")["kanata"];
    assert_eq!(
        kanata["operations"],
        serde_json::json!(["chat", "transcription"])
    );
    assert_eq!(kanata["trust_zone"], "private_network");
    assert_eq!(kanata["streaming"], true);
}

#[tokio::test]
async fn public_models_carry_same_kanata_object_without_admission() {
    let config = config_with_public_routes(&[("private-chat", "chat")]);
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities(&config, "vllm-private"),
            chat_outcome("unused"),
        )],
    );
    let private = response_json(
        server
            .client_oneshot(models_request())
            .await
            .expect("private"),
    )
    .await;
    let public = response_json(
        server
            .public_oneshot(models_request())
            .await
            .expect("public router"),
    )
    .await;
    assert_eq!(ids(&public), ["private-chat"]);
    let mut private_kanata = entry(&private, "private-chat")["kanata"].clone();
    assert!(
        private_kanata
            .as_object_mut()
            .expect("kanata object")
            .remove("admission")
            .is_some()
    );
    assert_eq!(entry(&public, "private-chat")["kanata"], private_kanata);
}
