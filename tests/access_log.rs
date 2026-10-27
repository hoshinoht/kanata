#![allow(dead_code)]

#[path = "support/gateway.rs"]
mod gateway;
#[path = "support/scratch.rs"]
mod scratch;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    response::Response,
};
use kanata::core::ErrorKind;
use tracing::instrument::WithSubscriber;

use gateway::{self as support, Capture};

const PROMPT: &str = "SYNTHETIC_PROMPT_MARKER";

trait AccessLine {
    /// The single `kanata::access` line.
    fn access_line(&self) -> String;
}

impl AccessLine for Capture {
    fn access_line(&self) -> String {
        let text = self.text();
        let lines: Vec<_> = text
            .lines()
            .filter(|line| line.contains("kanata::access"))
            .collect();
        assert_eq!(lines.len(), 1, "access line count: {text}");
        lines[0].to_owned()
    }
}

async fn captured<F>(future: F) -> Capture
where
    F: Future<Output = Response>,
{
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    async {
        let response = future.await;
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
    }
    .with_subscriber(subscriber)
    .await;
    capture
}

fn chat_body() -> String {
    format!(r#"{{"model":"private-chat","messages":[{{"role":"user","content":"{PROMPT}"}}]}}"#)
}

fn chat_with_ip(ip: &str) -> Request<Body> {
    let mut request = support::chat_request(&chat_body());
    request
        .headers_mut()
        .insert("cf-connecting-ip", ip.parse().expect("header"));
    request
}

fn assert_fields(line: &str, fields: &[&str]) {
    for field in fields {
        assert!(line.contains(field), "missing {field}: {line}");
    }
}

#[tokio::test]
async fn private_request_logs_key_and_model_but_not_secrets_or_client_ip() {
    let config = support::config();
    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(&config, "vllm-private"),
            support::chat_outcome("SYNTHETIC_OUTPUT"),
        )],
    );
    let capture = captured(async {
        server
            .client_oneshot(chat_with_ip("203.0.113.7"))
            .await
            .expect("response")
    })
    .await;
    let line = capture.access_line();
    assert_fields(
        &line,
        &[
            "listener=\"private\"",
            "method=POST",
            "endpoint=\"chat\"",
            "status=200",
            "error=\"-\"",
            "key=\"personal-client\"",
            "model=\"private-chat\"",
            "operation=\"chat\"",
            "stream=false",
            "adapter=\"vllm-private\"",
            "client_ip=-",
            "client_request_id=\"-\"",
            "reasoning_effort=\"-\"",
        ],
    );
    let text = capture.text();
    for secret in [
        "test-key",
        "Bearer",
        PROMPT,
        "SYNTHETIC_OUTPUT",
        "203.0.113.7",
    ] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
}

#[tokio::test]
async fn caller_request_id_and_route_pinned_reasoning_effort_are_logged() {
    let example =
        std::fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
    let pinned = example
        .replacen(
            "function_tools = true\n\n[[routes]]",
            "function_tools = true\nreasoning_control = true\n\n[[routes]]",
            1,
        )
        .replace(
            "upstream_id = \"gpt-5\"\n",
            "upstream_id = \"gpt-5\"\nreasoning_effort = \"medium\"\n",
        );
    assert_ne!(pinned, example);
    let config = support::load_valid(&pinned);
    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "chatgpt-private",
            support::capabilities(&config, "chatgpt-private"),
            support::chat_outcome("SYNTHETIC_OUTPUT"),
        )],
    );
    let body = r#"{"model":"chatgpt-chat","messages":[{"role":"user","content":"x"}]}"#;
    let capture = captured(async {
        server
            .client_oneshot(support::chat_request_with(
                body,
                Some(support::CHAT_CONTENT_TYPE),
                Some("Bearer test-key"),
                &["caller-42"],
            ))
            .await
            .expect("response")
    })
    .await;
    assert_fields(
        &capture.access_line(),
        &[
            "status=200",
            "client_request_id=\"caller-42\"",
            "reasoning_effort=\"medium\"",
        ],
    );
}

#[tokio::test]
async fn public_request_does_not_log_the_caller_request_id() {
    let config = support::config_with_public_routes(&[("private-chat", "chat")]);
    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(&config, "vllm-private"),
            support::chat_outcome("SYNTHETIC_OUTPUT"),
        )],
    );
    let capture = captured(async {
        server
            .public_oneshot(support::chat_request_with(
                &chat_body(),
                Some(support::CHAT_CONTENT_TYPE),
                Some("Bearer test-key"),
                &["caller-42"],
            ))
            .await
            .expect("public router")
    })
    .await;
    assert_fields(
        &capture.access_line(),
        &[
            "listener=\"public\"",
            "status=200",
            "client_request_id=\"-\"",
        ],
    );
    assert!(!capture.text().contains("caller-42"));
}

#[tokio::test]
async fn public_request_logs_cloudflare_client_ip_only_when_it_parses() {
    let config = support::config_with_public_routes(&[("private-chat", "chat")]);
    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(&config, "vllm-private"),
            support::chat_outcome("SYNTHETIC_OUTPUT"),
        )],
    );
    let capture = captured(async {
        server
            .public_oneshot(chat_with_ip("203.0.113.7"))
            .await
            .expect("public router")
    })
    .await;
    assert_fields(
        &capture.access_line(),
        &[
            "listener=\"public\"",
            "client_ip=203.0.113.7",
            "key=\"personal-client\"",
        ],
    );

    let capture = captured(async {
        server
            .public_oneshot(chat_with_ip("not-an-ip SYNTHETIC_HEADER"))
            .await
            .expect("public router")
    })
    .await;
    assert_fields(&capture.access_line(), &["client_ip=-"]);
    assert!(!capture.text().contains("SYNTHETIC_HEADER"));
}

#[tokio::test]
async fn unknown_path_is_sanitized_and_truncated_and_unknown_model_is_unset() {
    let config = support::config();
    let server = support::server_without_adapters(&config);
    let long = format!("/v1/{}", "a".repeat(100));
    let capture = captured(async {
        server
            .client_oneshot(
                Request::builder()
                    .uri(format!("{long}?token=SYNTHETIC_QUERY"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
    })
    .await;
    let line = capture.access_line();
    assert_fields(
        &line,
        &[
            "endpoint=\"other\"",
            &format!("path=\"{}\"", &long[..64]),
            "status=404",
            "key=\"-\"",
        ],
    );
    assert!(!line.contains(&long[..65]), "{line}");
    assert!(!line.contains("SYNTHETIC_QUERY"), "{line}");

    let capture = captured(async {
        server
            .client_oneshot(support::chat_request(
                r#"{"model":"SYNTHETIC_UNKNOWN_MODEL","messages":[{"role":"user","content":"x"}]}"#,
            ))
            .await
            .expect("response")
    })
    .await;
    let line = capture.access_line();
    assert_fields(&line, &["model=\"-\"", "error=\"permission_denied\""]);
    assert!(!capture.text().contains("SYNTHETIC_UNKNOWN_MODEL"));
}

#[tokio::test]
async fn upstream_failure_warns_with_adapter_and_code_but_not_prompt() {
    let config = support::config();
    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(&config, "vllm-private"),
            support::Outcome::Error(ErrorKind::UpstreamFailure),
        )],
    );
    let capture = captured(async {
        let response = server
            .client_oneshot(support::chat_request(&chat_body()))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        response
    })
    .await;
    let text = capture.text();
    let warn = text
        .lines()
        .find(|line| line.contains("upstream request failed"))
        .unwrap_or_else(|| panic!("missing warn: {text}"));
    assert_fields(
        warn,
        &[
            "WARN",
            "adapter=\"vllm-private\"",
            "provider=\"vllm\"",
            "error=\"upstream_failure\"",
        ],
    );
    assert_fields(
        &capture.access_line(),
        &["status=502", "error=\"upstream_failure\""],
    );
    assert!(!text.contains(PROMPT), "{text}");
}

/// Like [`captured`], also returning the response status and its one `x-request-id`.
async fn captured_id<F>(future: F) -> (Capture, StatusCode, String)
where
    F: Future<Output = Response>,
{
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let sink = slot.clone();
    let capture = captured(async move {
        let response = future.await;
        let ids: Vec<_> = response
            .headers()
            .get_all("x-request-id")
            .iter()
            .map(|value| value.to_str().expect("ascii id").to_owned())
            .collect();
        *sink.lock().unwrap() = Some((response.status(), ids));
        response
    })
    .await;
    let (status, ids) = slot.lock().unwrap().take().expect("response seen");
    assert_eq!(ids.len(), 1, "one x-request-id: {ids:?}");
    (capture, status, ids.into_iter().next().unwrap())
}

fn assert_generated(id: &str) {
    assert_eq!(id.len(), 32, "{id}");
    assert!(
        id.bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
        "{id}"
    );
}

fn chat_with_ids(ids: &[&str]) -> Request<Body> {
    support::chat_request_with(
        &chat_body(),
        Some(support::CHAT_CONTENT_TYPE),
        Some("Bearer test-key"),
        ids,
    )
}

fn models_with_id(id: &str) -> Request<Body> {
    let mut request = support::models_request();
    request
        .headers_mut()
        .insert("x-request-id", id.parse().expect("header"));
    request
}

fn private_chat_server(
    config: &kanata::config::ValidatedConfig,
    outcome: support::Outcome,
) -> support::ServerWithRequests {
    support::server_with(
        config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(config, "vllm-private"),
            outcome,
        )],
    )
}

#[tokio::test]
async fn private_listener_echoes_a_valid_caller_request_id_and_generates_otherwise() {
    let config = support::config();
    let (server, requests) = private_chat_server(&config, support::chat_outcome("ok"));

    let (capture, status, id) = captured_id(async {
        server
            .client_oneshot(chat_with_ids(&["caller-42"]))
            .await
            .unwrap()
    })
    .await;
    assert_eq!((status, id.as_str()), (StatusCode::OK, "caller-42"));
    assert_fields(&capture.access_line(), &["client_request_id=\"caller-42\""]);
    assert_eq!(
        support::take_request(&requests).context().request_id,
        "caller-42"
    );

    let (capture, status, id) =
        captured_id(async { server.client_oneshot(chat_with_ids(&[])).await.unwrap() }).await;
    assert_eq!(status, StatusCode::OK);
    assert_generated(&id);
    assert_fields(
        &capture.access_line(),
        &[&format!("request_id={id} "), "client_request_id=\"-\""],
    );

    // Unsafe ids are replaced; inference requests still reject them.
    let (capture, status, id) = captured_id(async {
        server
            .client_oneshot(models_with_id("bad/id"))
            .await
            .unwrap()
    })
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_generated(&id);
    assert_fields(
        &capture.access_line(),
        &[&format!("request_id={id} "), "client_request_id=\"-\""],
    );
    for ids in [&["bad/id"][..], &["one", "two"]] {
        let (_, status, id) =
            captured_id(async { server.client_oneshot(chat_with_ids(ids)).await.unwrap() }).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_generated(&id);
    }
}

#[tokio::test]
async fn public_listener_never_uses_the_caller_request_id() {
    let config = support::config_with_public_routes(&[("private-chat", "chat")]);
    let (server, requests) = private_chat_server(&config, support::chat_outcome("ok"));
    let (capture, status, id) = captured_id(async {
        server
            .public_oneshot(chat_with_ids(&["caller-42"]))
            .await
            .unwrap()
    })
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_generated(&id);
    assert_fields(
        &capture.access_line(),
        &[&format!("request_id={id} "), "client_request_id=\"-\""],
    );
    assert!(!capture.text().contains("caller-42"));
    assert_ne!(
        support::take_request(&requests).context().request_id,
        "caller-42"
    );

    // An id the private listener would reject is ignored here.
    let (_, status, id) = captured_id(async {
        server
            .public_oneshot(chat_with_ids(&["bad/id"]))
            .await
            .unwrap()
    })
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_generated(&id);
}

#[tokio::test]
async fn request_id_is_set_on_auth_rejections_guide_unknown_paths_and_streams() {
    let config = support::config_with_public_routes(&[("private-chat", "chat")]);
    let (server, _) = private_chat_server(
        &config,
        support::Outcome::Stream(vec![
            kanata::core::NormalizedEvent::ChatStarted {
                model: kanata::core::ModelAlias("private-chat".into()),
            },
            kanata::core::NormalizedEvent::ChatTextDelta { text: "hi".into() },
            kanata::core::NormalizedEvent::ChatCompleted {
                finish_reason: kanata::core::FinishReason::Stop,
                usage: None,
            },
        ]),
    );
    let get = |path: &str| {
        Request::builder()
            .uri(path)
            .header("x-request-id", "caller-42")
            .body(Body::empty())
            .expect("request")
    };
    for (path, expected) in [
        ("/v1/models", StatusCode::FORBIDDEN),
        ("/v1", StatusCode::OK),
        ("/v1/unknown", StatusCode::FORBIDDEN),
    ] {
        let (capture, status, id) =
            captured_id(async { server.public_oneshot(get(path)).await.unwrap() }).await;
        assert_eq!(status, expected, "{path}");
        assert_generated(&id);
        assert_fields(&capture.access_line(), &[&format!("request_id={id} ")]);
    }
    for (path, expected) in [
        ("/v1/models", StatusCode::UNAUTHORIZED),
        ("/v1/unknown", StatusCode::NOT_FOUND),
    ] {
        let (_, status, id) =
            captured_id(async { server.client_oneshot(get(path)).await.unwrap() }).await;
        assert_eq!((status, id.as_str()), (expected, "caller-42"), "{path}");
    }

    let body =
        r#"{"model":"private-chat","stream":true,"messages":[{"role":"user","content":"x"}]}"#;
    let stream = || {
        support::chat_request_with(
            body,
            Some(support::CHAT_CONTENT_TYPE),
            Some("Bearer test-key"),
            &[],
        )
    };
    let (capture, status, id) =
        captured_id(async { server.client_oneshot(stream()).await.unwrap() }).await;
    assert_eq!(status, StatusCode::OK);
    assert_generated(&id);
    assert_fields(
        &capture.access_line(),
        &[&format!("request_id={id} "), "stream=true"],
    );
    let (_, status, id) =
        captured_id(async { server.public_oneshot(stream()).await.unwrap() }).await;
    assert_eq!(status, StatusCode::OK);
    assert_generated(&id);
}
