use std::future::Future;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use axum::{
    body::{Body, to_bytes},
    http::{HeaderValue, Request, StatusCode, header},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use kanata::core::{ErrorKind, GatewayError, ModelAlias, NormalizedEvent};
use serde_json::json;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::fmt::MakeWriter;

use crate::{gateway as support, sse_support};

const CHAT_BODY: &str =
    r#"{"model":"private-chat","messages":[{"role":"user","content":"SYNTHETIC_PROMPT"}]}"#;
const MARKERS: [&str; 10] = [
    "SYNTHETIC_SECRET",
    "SYNTHETIC_ACCOUNT",
    "SYNTHETIC_PROMPT",
    "SYNTHETIC_OUTPUT",
    "SYNTHETIC_AUDIO",
    "SYNTHETIC_FILENAME",
    "SYNTHETIC_TOOL",
    "SYNTHETIC_HEADER",
    "SYNTHETIC_REQUEST_ID",
    "SYNTHETIC_QUERY",
];

#[derive(Clone, Default)]
struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Capture {
    fn writer(&self) -> CaptureWriter {
        CaptureWriter(self.bytes.clone())
    }

    fn text(&self) -> String {
        String::from_utf8(self.bytes.lock().expect("capture lock").clone()).expect("capture utf8")
    }
}

#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Write for CaptureWriter {
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

async fn captured<F>(capture: &Capture, future: F)
where
    F: Future<Output = ()>,
{
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_target(true)
        .with_writer(capture.writer())
        .finish();
    future.with_subscriber(subscriber).await;
}

fn chat_request_with_markers() -> Request<Body> {
    let mut request = support::chat_request_with(
        CHAT_BODY,
        Some(support::CHAT_CONTENT_TYPE),
        Some("Bearer test-key"),
        &["SYNTHETIC_REQUEST_ID"],
    );
    request.headers_mut().insert(
        "x-account-id",
        HeaderValue::from_static("SYNTHETIC_ACCOUNT"),
    );
    request.headers_mut().insert(
        "x-synthetic-header",
        HeaderValue::from_static("SYNTHETIC_HEADER"),
    );
    request
        .headers_mut()
        .insert("x-secret", HeaderValue::from_static("SYNTHETIC_SECRET"));
    request
        .headers_mut()
        .insert("x-tool-name", HeaderValue::from_static("SYNTHETIC_TOOL"));
    *request.uri_mut() = "/v1/chat/completions?account=SYNTHETIC_QUERY"
        .parse()
        .expect("chat query");
    request
}

fn transcription_request_with_markers() -> Request<Body> {
    const BOUNDARY: &str = "redaction-boundary";
    let mut body = Vec::new();
    field(&mut body, BOUNDARY, "model", b"private-transcribe");
    field(&mut body, BOUNDARY, "prompt", b"SYNTHETIC_PROMPT");
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"SYNTHETIC_FILENAME.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\nSYNTHETIC_AUDIO\r\n");
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header(header::AUTHORIZATION, "Bearer test-key")
        .header("x-secret", "SYNTHETIC_SECRET")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary=\"{BOUNDARY}\""),
        )
        .body(Body::from(body))
        .expect("multipart request")
}

fn field(body: &mut Vec<u8>, boundary: &str, name: &str, value: &[u8]) {
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(value);
    body.extend_from_slice(b"\r\n");
}

fn assert_redacted(capture: &Capture) {
    let text = capture.text();
    for marker in MARKERS {
        assert!(!text.contains(marker), "telemetry leaked {marker}: {text}");
    }
    let events: Vec<_> = text
        .lines()
        .filter(|line| line.contains("kanata::telemetry"))
        .collect();
    assert_eq!(events.len(), 1, "completion event count: {text}");
    let event = events[0];
    for field in [
        "endpoint=",
        "outcome=",
        "status_class=",
        "phase=",
        "duration_ms=",
    ] {
        assert!(event.contains(field), "missing {field}: {event}");
    }
    assert!(!event.contains("request"));
}

#[tokio::test]
async fn success_error_sse_and_multipart_telemetry_are_metadata_only() {
    let config = support::config();
    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(&config, "vllm-private"),
            support::chat_outcome("SYNTHETIC_OUTPUT"),
        )],
    );
    let capture = Capture::default();
    captured(&capture, async {
        let response = server
            .client_oneshot(chat_request_with_markers())
            .await
            .expect("chat response");
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("chat body");
        assert!(String::from_utf8_lossy(&body).contains("SYNTHETIC_OUTPUT"));
    })
    .await;
    assert_redacted(&capture);

    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(&config, "vllm-private"),
            support::Outcome::WrongModel("SYNTHETIC_OUTPUT".into()),
        )],
    );
    let capture = Capture::default();
    captured(&capture, async {
        let response = server
            .client_oneshot(chat_request_with_markers())
            .await
            .expect("error response");
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body");
        assert!(!String::from_utf8_lossy(&body).contains("SYNTHETIC_OUTPUT"));
    })
    .await;
    assert_redacted(&capture);

    let (server, _) = sse_support::server(vec![
        Ok(NormalizedEvent::ChatStarted {
            model: ModelAlias("private-chat".into()),
        }),
        Err(GatewayError {
            kind: ErrorKind::UpstreamFailure,
        }),
    ]);
    let capture = Capture::default();
    captured(&capture, async {
        let response = server
            .client_oneshot(sse_support::request(CHAT_BODY))
            .await
            .expect("sse response");
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("sse body");
        assert!(String::from_utf8_lossy(&body).contains("upstream_failure"));
    })
    .await;
    assert_redacted(&capture);

    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(&config, "vllm-private"),
            support::Outcome::Transcription,
        )],
    );
    let capture = Capture::default();
    captured(&capture, async {
        let response = server
            .client_oneshot(transcription_request_with_markers())
            .await
            .expect("transcription response");
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("transcription body");
        assert!(String::from_utf8_lossy(&body).contains("transcribed"));
    })
    .await;
    assert_redacted(&capture);
}

#[tokio::test]
async fn inline_audio_payload_is_absent_from_success_response_and_telemetry() {
    let config = crate::audio_support::config_with_audio(1_048_576, 1024);
    let (server, requests) = support::server_with(&config, crate::audio_support::adapters(&config));
    let encoded_audio = STANDARD.encode(b"SYNTHETIC_AUDIO");
    let body = json!({
        "model": "private-chat",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "input_audio",
                "input_audio": {"data": &encoded_audio, "format": "wav"}
            }]
        }]
    })
    .to_string();
    let request = support::chat_request_with(
        &body,
        Some(support::CHAT_CONTENT_TYPE),
        Some("Bearer test-key"),
        &[],
    );
    let capture = Capture::default();
    captured(&capture, async {
        let response = server.client_oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        assert!(!String::from_utf8_lossy(&body).contains(&encoded_audio));
    })
    .await;
    assert_redacted(&capture);
    assert!(!capture.text().contains(&encoded_audio));
    assert_eq!(support::recorded_len(&requests), 1);
}
