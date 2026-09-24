#![allow(dead_code)]

use std::{fs, io, sync::Arc, time::Duration};

use axum::http::StatusCode;
use kanata::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    config::{ValidatedConfig, load},
    core::{
        Capabilities, ChatContent, ChatMessage, ChatResponse, ChatRole, FinishReason,
        Response as CoreResponse, RoutedRequest, Usage,
    },
    server::{Readiness, TwoPlaneServer},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

use crate::gateway::Resolver;

static CONFIG_SEQUENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Running {
    client_addr: std::net::SocketAddr,
    admin_addr: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<()>>,
}

impl Running {
    async fn stop(mut self) {
        let _ = self.stop.take().expect("shutdown sender").send(());
        self.task
            .await
            .expect("server task")
            .expect("server result");
    }
}

struct FixedAdapter {
    capabilities: Capabilities,
    delay: Duration,
}

impl Adapter for FixedAdapter {
    fn id(&self) -> &str {
        "vllm-private"
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, request: RoutedRequest) -> AdapterFuture {
        let model = request.request().model_alias().clone();
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(AdapterOutput::Complete(CoreResponse::Chat(ChatResponse {
                model,
                message: ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![ChatContent::Text {
                        text: "socket-ok".into(),
                    }],
                },
                finish_reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    total_tokens: 2,
                }),
            })))
        })
    }
}

#[derive(Debug)]
struct RawResponse {
    status: StatusCode,
    headers: String,
    body: Vec<u8>,
}

async fn start() -> Running {
    start_with(None, Duration::ZERO).await
}

async fn start_with(header_read_timeout: Option<Duration>, delay: Duration) -> Running {
    let client_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("client listener");
    let admin_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("admin listener");
    let client_addr = client_listener.local_addr().expect("client address");
    let admin_addr = admin_listener.local_addr().expect("admin address");
    let config = config(client_addr.port(), admin_addr.port());
    let capabilities = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "vllm-private")
        .expect("vllm config")
        .capabilities()
        .clone();
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &Resolver,
        Readiness::new(true),
        vec![Arc::new(FixedAdapter {
            capabilities,
            delay,
        })],
    )
    .expect("server builds");
    let mut bound = server
        .with_bound_listeners(client_listener, admin_listener)
        .expect("validated listener pair");
    if let Some(timeout) = header_read_timeout {
        bound = bound.with_header_read_timeout(timeout);
    }
    let (stop, shutdown) = oneshot::channel();
    let task = tokio::spawn(async move {
        bound
            .serve_until(
                async move {
                    let _ = shutdown.await;
                },
                Duration::from_secs(30),
            )
            .await
    });
    Running {
        client_addr,
        admin_addr,
        stop: Some(stop),
        task,
    }
}

fn config(client_port: u16, admin_port: u16) -> ValidatedConfig {
    let contents = include_str!("../../tests/fixtures/config/example.toml")
        .replace("bind = \"0.0.0.0\"", "bind = \"127.0.0.1\"")
        .replace("port = 8080", &format!("port = {client_port}"))
        .replace("port = 9090", &format!("port = {admin_port}"));
    let path = std::env::temp_dir().join(format!(
        "kanata-telemetry-socket-{}-{}.toml",
        std::process::id(),
        CONFIG_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("telemetry config writes");
    let config = load(&path).expect("telemetry config validates");
    let _ = fs::remove_file(path);
    config
}

async fn request(address: std::net::SocketAddr, request: impl AsRef<[u8]>) -> RawResponse {
    let mut stream = TcpStream::connect(address).await.expect("socket connect");
    stream
        .write_all(request.as_ref())
        .await
        .expect("socket request");
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .expect("socket response");
    parse_response(bytes)
}

fn parse_response(bytes: Vec<u8>) -> RawResponse {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response headers")
        + 4;
    let header_bytes = &bytes[..header_end];
    let header_text = String::from_utf8_lossy(header_bytes).into_owned();
    let status = header_text
        .lines()
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse::<u16>()
        .expect("status number");
    RawResponse {
        status: StatusCode::from_u16(status).expect("status code"),
        headers: header_text,
        body: bytes[header_end..].to_vec(),
    }
}

fn chat_request(authorization: Option<&str>) -> String {
    let body = r#"{"model":"private-chat","messages":[{"role":"user","content":"hello"}]}"#;
    let authorization = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n{authorization}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn empty_404_request() -> &'static str {
    "GET /v1/does-not-exist HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
}

fn admin_metrics_request() -> &'static str {
    "GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
}

#[tokio::test]
async fn real_http1_known_length_and_empty_responses_finish_successfully() {
    let server = start().await;

    let success = request(server.client_addr, chat_request(Some("Bearer test-key"))).await;
    assert_eq!(success.status, StatusCode::OK);
    assert!(success.headers.contains("content-length:"));
    assert!(String::from_utf8_lossy(&success.body).contains("socket-ok"));

    let unauthorized = request(server.client_addr, chat_request(None)).await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);
    assert!(unauthorized.headers.contains("content-length:"));
    let body: serde_json::Value = serde_json::from_slice(&unauthorized.body).expect("auth json");
    assert_eq!(body["error"]["code"], "invalid_api_key");

    let not_found = request(server.client_addr, empty_404_request()).await;
    assert_eq!(not_found.status, StatusCode::NOT_FOUND);

    let metrics = request(server.admin_addr, admin_metrics_request()).await;
    assert_eq!(metrics.status, StatusCode::OK);
    let metrics = String::from_utf8(metrics.body).expect("metrics utf8");
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"success\",status_class=\"2xx\",timeout_phase=\"none\"} 1"
    ));
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"client_error\",status_class=\"4xx\",timeout_phase=\"none\"} 1"
    ));
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"other\",outcome=\"client_error\",status_class=\"4xx\",timeout_phase=\"none\"} 1"
    ));
    assert!(metrics.contains("kanata_requests_inflight{endpoint=\"chat\"} 0"));
    assert!(metrics.contains("kanata_requests_inflight{endpoint=\"other\"} 0"));

    server.stop().await;
}

#[tokio::test]
async fn incomplete_request_head_is_closed_after_header_timeout() {
    let server = start_with(Some(Duration::from_millis(300)), Duration::ZERO).await;
    let mut stream = TcpStream::connect(server.client_addr)
        .await
        .expect("connect");
    stream
        .write_all(b"POST /v1/chat/completions HTTP/1.1\r\nhost: kanata\r\n")
        .await
        .expect("partial head");

    let started = std::time::Instant::now();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
        .await
        .expect("server closes a stalled connection")
        .ok();
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert!(response.is_empty() || response.starts_with(b"HTTP/1.1 408"));

    server.stop().await;
}

#[tokio::test]
async fn header_timeout_does_not_cut_a_slow_response() {
    let server = start_with(Some(Duration::from_millis(200)), Duration::from_millis(600)).await;
    let response = request(server.client_addr, chat_request(Some("Bearer test-key"))).await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&response.body).contains("socket-ok"));
    server.stop().await;
}
