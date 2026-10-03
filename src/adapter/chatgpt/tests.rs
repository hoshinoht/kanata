use super::*;
use crate::core::{
    ChatContent, ChatMessage, ChatRequest, ChatRole, Extensions, FunctionTool, ModelAlias,
    RequestContext, ToolCall, ToolChoice,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
fn load(contents: &str) -> Result<ValidatedConfig, crate::config::ConfigError> {
    let root = std::env::temp_dir().join(format!(
        "kanata-chatgpt-config-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("config.toml");
    std::fs::write(&path, contents).unwrap();
    let result = crate::config::load(&path);
    std::fs::remove_dir_all(root).unwrap();
    result
}
pub(super) fn config() -> ValidatedConfig {
    load(&legacy_template()).unwrap()
}
fn legacy_template() -> String {
    let mut template: toml::Value =
        toml::from_str(include_str!("../../../config/chatgpt.example.toml")).unwrap();
    let routes = template["routes"].as_array_mut().unwrap();
    routes.truncate(1);
    routes[0].as_table_mut().unwrap().remove("reasoning_effort");
    for adapter in template["adapters"].as_array_mut().unwrap() {
        adapter["capabilities"]
            .as_table_mut()
            .unwrap()
            .remove("reasoning_control");
    }
    toml::to_string(&template).unwrap()
}
pub(super) fn config_with_effort(effort: crate::core::ReasoningEffort) -> ValidatedConfig {
    let text = legacy_template()
        .replace(
            "\nfunction_tools = true\n",
            "\nfunction_tools = true\nreasoning_control = true\n",
        )
        .replace(
            "upstream_id = \"replace-with-account-model-slug\"",
            &format!(
                "upstream_id = \"fixture-model\"\nreasoning_effort = \"{}\"",
                effort.as_str()
            ),
        );
    load(&text).unwrap()
}
pub(super) fn chat() -> ChatRequest {
    ChatRequest {
        model: ModelAlias("chatgpt-chat".into()),
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: vec![ChatContent::Text {
                    text: "Be concise".into(),
                }],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatContent::Text {
                    text: "Fixture question".into(),
                }],
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatContent::ToolCall {
                    call: ToolCall {
                        id: "call_1".into(),
                        name: "lookup".into(),
                        arguments: "{}".into(),
                    },
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: vec![ChatContent::ToolResult {
                    call_id: "call_1".into(),
                    content: "Fixture result".into(),
                }],
            },
        ],
        tools: vec![
            FunctionTool {
                name: "lookup".into(),
                description: None,
                parameters: json!({"type":"object"}),
            },
            FunctionTool {
                name: "other".into(),
                description: None,
                parameters: json!({"type":"object"}),
            },
        ],
        tool_choice: ToolChoice::Function {
            name: "lookup".into(),
        },
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    }
}
pub(super) fn routed(config: &ValidatedConfig, chat: ChatRequest) -> RoutedRequest {
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture".into(),
            route: config.routes()[0].identity().clone(),
            trust_zone: TrustZone::External,
            extensions: Extensions::default(),
        },
        Request::Chat(chat),
    )
    .unwrap()
}

#[test]
fn namespaced_payload_keeps_full_history_and_rejects_unsupported_options() {
    let config = config();
    let routes = vec![request::RouteBinding::from_route(&config.routes()[0])];
    let caps = config.adapters()[0].capabilities();
    let mut responses_chat = chat();
    responses_chat.options.max_output_tokens_param = crate::core::MaxTokensParam::MaxOutputTokens;
    let encoded = request::encode(&routed(&config, responses_chat), caps, &routes).unwrap();
    assert_eq!(encoded["store"], false);
    assert_eq!(encoded["stream"], true);
    assert_eq!(encoded["instructions"], "Be concise");
    assert_eq!(encoded["input"][1]["namespace"], "kanata");
    assert_eq!(encoded["input"][2]["type"], "function_call_output");
    assert_eq!(encoded["tools"][0]["type"], "namespace");
    assert_eq!(encoded["tools"][0]["tools"].as_array().unwrap().len(), 1);
    assert_eq!(encoded["tool_choice"], "required");
    for key in [
        "reasoning",
        "temperature",
        "top_p",
        "max_output_tokens",
        "previous_response_id",
    ] {
        assert!(encoded.get(key).is_none());
    }
    assert!(request::encode(&routed(&config, chat()), caps, &[]).is_err());
    let mut request = chat();
    request.options.sampling.temperature = Some(crate::core::Temperature::new(0.5).unwrap());
    assert!(request::encode(&routed(&config, request), caps, &routes).is_err());
}

#[test]
fn catalog_keeps_server_order_and_only_visible_models() {
    let models = decode_models(br#"{"models":[{"slug":"b","display_name":"Second","visibility":"list"},{"slug":"hidden","display_name":"Hidden","visibility":"hidden"},{"slug":"a","display_name":"First","visibility":"list"}]}"#).unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.slug.as_str())
            .collect::<Vec<_>>(),
        ["b", "a"]
    );
    assert!(decode_models(br#"{"data":[{"id":"wrong-schema"}]}"#).is_err());
    assert!(
        decode_models(
            br#"{"models":[{"slug":"b","display_name":"bad\nname","visibility":"list"}]}"#
        )
        .is_err()
    );
}

#[test]
fn config_pins_origin_capabilities_and_public_boundary() {
    let template = include_str!("../../../config/chatgpt.example.toml");
    for text in [template.replace("https://api.openai.com/v1", "https://example.com/v1"),
        template.replace("function_tools = true", "function_tools = true\nsampling_controls = true"),
        template.replace("operations = [\"chat\"]", "operations = [\"transcription\"]"),
        template.replace("[publication]", "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[publication]\npublic_routes = [{ model_alias = \"chatgpt-chat\", operation = \"chat\" }]")] {
        assert!(load(&text).is_err());
    }
    let template = template.replace(
        "[publication]",
        "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[publication]",
    );
    let config = load(&template)
        .unwrap()
        .for_plane(crate::config::Plane::Public)
        .unwrap();
    assert!(config.chatgpt_auth().is_none());
    assert!(config.adapters().is_empty());
    assert!(config.routes().is_empty());
}

#[test]
fn public_process_excludes_keys_with_mixed_private_provider_scopes() {
    let text = include_str!("../../../tests/fixtures/config/example.toml")
        .replace("[codex_auth]\nstore = \"keyring\"", "[chatgpt_auth]")
        .replace("kind = \"codex\"", "kind = \"chatgpt\"")
        .replace("https://chatgpt.invalid/backend-api/codex", "https://api.openai.com/v1")
        .replace("owner = true", "owner = false")
        .replace("[publication]", "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[publication]\npublic_routes = [{ model_alias = \"local-chat\", operation = \"chat\" }]");
    let full = load(&text).unwrap();
    assert_eq!(full.application_keys().len(), 1);
    let public = full.for_plane(crate::config::Plane::Public).unwrap();
    assert_eq!(public.routes().len(), 1);
    assert!(public.chatgpt_auth().is_none());
    assert!(public.application_keys().is_empty());
    assert!(
        public
            .adapters()
            .iter()
            .all(|adapter| !adapter.kind().is_private_only())
    );
}

#[test]
fn pinned_reasoning_payload_supports_standard_efforts_and_rejects_overrides() {
    use crate::core::ReasoningEffort;
    for effort in [
        ReasoningEffort::None,
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Xhigh,
        ReasoningEffort::Max,
    ] {
        let config = config_with_effort(effort);
        let bindings = vec![request::RouteBinding::from_route(&config.routes()[0])];
        let caps = config.adapters()[0].capabilities();
        for explicit in [None, Some(effort)] {
            let mut chat = chat();
            chat.options.reasoning_effort = explicit;
            let payload = request::encode(&routed(&config, chat), caps, &bindings).unwrap();
            assert_eq!(payload["reasoning"]["effort"], effort.as_str());
            assert_eq!(payload["store"], false);
            assert_eq!(payload["stream"], true);
            assert_eq!(payload["tools"][0]["type"], "namespace");
        }
        let mut chat = chat();
        chat.options.reasoning_effort = Some(if effort == ReasoningEffort::High {
            ReasoningEffort::Low
        } else {
            ReasoningEffort::High
        });
        assert!(request::encode(&routed(&config, chat), caps, &bindings).is_err());
    }
    let config = config();
    let mut chat = chat();
    chat.options.reasoning_effort = Some(ReasoningEffort::Low);
    assert!(
        request::encode(
            &routed(&config, chat),
            config.adapters()[0].capabilities(),
            &[request::RouteBinding::from_route(&config.routes()[0])]
        )
        .is_err()
    );
}

#[test]
fn catalog_reports_account_efforts_defaults_and_unsupported_levels() {
    let models = decode_models(br#"{"models":[{"slug":"fixture","display_name":"Fixture","visibility":"list","supported_reasoning_levels":[{"effort":"low","description":"Fixture"},{"effort":"medium"},{"effort":"max"},{"effort":"ultra"}],"default_reasoning_level":"low"}]}"#).unwrap();
    assert_eq!(
        models[0].supported_reasoning_efforts,
        Some(vec![
            crate::core::ReasoningEffort::Low,
            crate::core::ReasoningEffort::Medium,
            crate::core::ReasoningEffort::Max
        ])
    );
    assert_eq!(
        models[0].default_reasoning_effort,
        Some(crate::core::ReasoningEffort::Low)
    );
    assert_eq!(models[0].unsupported_reasoning_efforts, ["ultra"]);
    for levels in [
        json!([{"effort":"low"},{"effort":"low"}]),
        json!([{"effort":"bad\nlevel"}]),
    ] {
        assert!(decode_models(&serde_json::to_vec(&json!({"models":[{"slug":"fixture","display_name":"Fixture","visibility":"list","supported_reasoning_levels":levels}]})).unwrap()).is_err());
    }
}

#[test]
fn reasoning_config_requires_explicit_matching_private_route_pins() {
    let legacy = legacy_template();
    let enabled = legacy.replace(
        "\nfunction_tools = true\n",
        "\nfunction_tools = true\nreasoning_control = true\n",
    );
    assert!(
        load(&enabled)
            .unwrap_err()
            .to_string()
            .contains("reasoning_effort: required")
    );
    let pinned = enabled.replace(
        "upstream_id = \"replace-with-account-model-slug\"",
        "upstream_id = \"fixture-model\"\nreasoning_effort = \"medium\"",
    );
    let config = load(&pinned).unwrap();
    assert_eq!(config.routes()[0].pinned_reasoning_effort(), Some("medium"));
    for changed in [
        pinned.replace("reasoning_control = true", "reasoning_control = false"),
        pinned.replace(
            "model_alias = \"chatgpt-chat\"",
            "model_alias = \"chatgpt-chat:low\"",
        ),
        pinned.replace(
            "reasoning_effort = \"medium\"",
            "reasoning_effort = \"ultra\"",
        ),
        pinned.replace(
            "reasoning_effort = \"medium\"",
            "codex_reasoning_effort = \"medium\"",
        ),
    ] {
        assert!(load(&changed).is_err());
    }
}
