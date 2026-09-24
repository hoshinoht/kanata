use std::collections::BTreeSet;

use serde_json::Value;

const REFRESH_REQUEST: &str = include_str!("fixtures/codex/refresh-request.json");
const REFRESH_RESPONSE: &str = include_str!("fixtures/codex/refresh-response.json");
const RESPONSES_REQUEST: &str = include_str!("fixtures/codex/responses-request.json");
const RESPONSES_EVENTS: &str = include_str!("fixtures/codex/responses-events.sse");

// These literals intentionally pin the private Codex contract for deliberate fixture updates.
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const PUBLIC_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

fn json_fixture(source: &str) -> Value {
    serde_json::from_str(source).expect("fixture should be valid JSON")
}

fn object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("fixture value should be an object")
        .keys()
        .cloned()
        .collect()
}

fn sse_events() -> Vec<Value> {
    RESPONSES_EVENTS
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|payload| *payload != "[DONE]")
        .map(|payload| serde_json::from_str(payload).expect("SSE event should be valid JSON"))
        .collect()
}

#[test]
fn refresh_fixture_matches_request_and_rotation_shape() {
    let request = json_fixture(REFRESH_REQUEST);
    assert_eq!(request["method"], "POST");
    assert_eq!(request["url"], TOKEN_URL);
    assert_eq!(request["headers"]["Content-Type"], "application/json");
    assert_eq!(
        object_keys(&request["body"]),
        BTreeSet::from([
            "client_id".to_owned(),
            "grant_type".to_owned(),
            "refresh_token".to_owned(),
        ])
    );
    assert_eq!(request["body"]["client_id"], PUBLIC_CODEX_CLIENT_ID);
    assert_eq!(request["body"]["grant_type"], "refresh_token");

    let response = json_fixture(REFRESH_RESPONSE);
    assert_eq!(
        object_keys(&response),
        BTreeSet::from([
            "access_token".to_owned(),
            "expires_in".to_owned(),
            "refresh_token".to_owned(),
        ])
    );
    assert_eq!(response["expires_in"], 3600);
    assert!(
        response["access_token"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(
        response["refresh_token"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
}

#[test]
fn responses_fixture_matches_private_endpoint_headers_and_flags() {
    let request = json_fixture(RESPONSES_REQUEST);
    assert_eq!(request["method"], "POST");
    assert_eq!(request["url"], CODEX_URL);

    let headers = request["headers"]
        .as_object()
        .expect("headers should be an object");
    for name in [
        "Authorization",
        "ChatGPT-Account-ID",
        "Content-Type",
        "Accept",
    ] {
        assert!(
            headers.contains_key(name),
            "required header {name} is absent"
        );
    }
    assert!(
        headers["Authorization"]
            .as_str()
            .is_some_and(|value| value.starts_with("Bearer TEST_ONLY_"))
    );
    assert!(
        headers["ChatGPT-Account-ID"]
            .as_str()
            .is_some_and(|value| value.starts_with("TEST_ONLY_"))
    );
    assert_eq!(headers["Content-Type"], "application/json");
    assert_eq!(headers["Accept"], "text/event-stream");

    let body = &request["body"];
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert!(
        body["input"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tool_choice"]["type"], "function");
}

#[test]
fn responses_sse_fixture_covers_text_tool_and_usage_events() {
    let events = sse_events();
    let types: BTreeSet<&str> = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert!(types.contains("response.created"));
    assert!(types.contains("response.output_text.delta"));
    assert!(types.contains("response.output_item.added"));
    assert!(types.contains("response.function_call_arguments.delta"));
    assert!(types.contains("response.function_call_arguments.done"));
    assert!(types.contains("response.completed"));
    assert!(RESPONSES_EVENTS.contains("data: [DONE]\n"));

    let text = events
        .iter()
        .find(|event| event["type"] == "response.output_text.delta")
        .expect("text delta event should exist");
    assert_eq!(text["delta"], "TEST_ONLY_TEXT_NOT_SECRET_0001");

    let tool = events
        .iter()
        .find(|event| event["type"] == "response.function_call_arguments.done")
        .expect("tool argument event should exist");
    assert_eq!(
        tool["arguments"],
        "{\"query\":\"TEST_ONLY_QUERY_NOT_SECRET_0001\"}"
    );

    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("completion event should exist");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 4);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 3);
    assert_eq!(completed["response"]["usage"]["total_tokens"], 7);
}

#[test]
fn fixtures_pin_public_metadata_but_keep_credentials_and_content_synthetic() {
    let all = [
        REFRESH_REQUEST,
        REFRESH_RESPONSE,
        RESPONSES_REQUEST,
        RESPONSES_EVENTS,
    ]
    .join("\n");
    assert!(all.contains(PUBLIC_CODEX_CLIENT_ID));
    assert!(all.contains("TEST_ONLY_"));
    for marker in [
        "access-secret",
        "refresh-secret",
        "service-secret",
        "sk-",
        "eyJ",
    ] {
        assert!(
            !all.contains(marker),
            "fixture contains live-looking marker {marker}"
        );
    }
    for value in [
        "TEST_ONLY_REFRESH_TOKEN_NOT_SECRET_0001",
        "TEST_ONLY_ACCESS_TOKEN_NOT_SECRET_0001",
        "TEST_ONLY_ROTATED_REFRESH_TOKEN_NOT_SECRET_0001",
        "TEST_ONLY_ACCOUNT_ID_NOT_SECRET_0001",
    ] {
        assert!(
            all.contains(value),
            "fixture is missing synthetic marker {value}"
        );
    }
}
