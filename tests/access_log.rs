#![allow(dead_code)]

#[path = "support/gateway.rs"]
mod gateway;

use std::future::Future;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    response::Response,
};
use kanata::core::ErrorKind;
use tracing::instrument::WithSubscriber;

use gateway as support;

const PROMPT: &str = "SYNTHETIC_PROMPT_MARKER";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture lock").clone()).expect("capture utf8")
    }

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

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("capture lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
    let config = support::config();
    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "codex-private",
            support::capabilities(&config, "codex-private"),
            support::chat_outcome("SYNTHETIC_OUTPUT"),
        )],
    );
    let body = r#"{"model":"codex-chat","messages":[{"role":"user","content":"x"}]}"#;
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
