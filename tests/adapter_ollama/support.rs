#![allow(dead_code)]

#[path = "../support/http.rs"]
mod http_mock;
use crate::scratch;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::body::Body;
use http::{Request, StatusCode, header};
use kanata::{
    adapter::{AdapterOutput, ollama::OllamaAdapter},
    config::ValidatedConfig,
    core::{
        Capabilities, ChatContent, ChatMessage, ChatRequest, ChatRole, ModelAlias, Operation,
        Request as CoreRequest, RequestContext, Response as CoreResponse, RouteIdentity,
        RoutedRequest, ToolCall, ToolChoice, TrustZone,
    },
    server::TwoPlaneServer,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

use http_mock::wait_for_eof;
pub use http_mock::{RequestRecord, read_request};
#[allow(unused_imports)]
pub use scratch::load_text;

pub const TEXT_RESPONSE: &str = include_str!("../fixtures/ollama/chat-text-response.json");
pub const TOOLS_RESPONSE: &str = include_str!("../fixtures/ollama/chat-tools-response.json");
pub const TEXT_STREAM: &str = include_str!("../fixtures/ollama/chat-stream-text.sse");
pub const SEPARATE_USAGE_STREAM: &str =
    include_str!("../fixtures/ollama/chat-stream-text-separate-usage.sse");
pub const TOOLS_STREAM: &str = include_str!("../fixtures/ollama/chat-stream-tools.sse");

pub struct MockServer {
    pub address: String,
    pub requests: Arc<Mutex<Vec<RequestRecord>>>,
    pub once: Arc<AtomicUsize>,
    headers_seen: Option<oneshot::Receiver<()>>,
    response_headers: Option<oneshot::Receiver<()>>,
    closed: Option<oneshot::Receiver<()>>,
    task: Option<JoinHandle<()>>,
}

pub struct ResponseSpec {
    pub status: StatusCode,
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
    pub chunks: Vec<Vec<u8>>,
    pub wait_for_close: bool,
    pub write_body_before_close: bool,
}

impl ResponseSpec {
    pub fn json(body: &str) -> Self {
        Self {
            status: StatusCode::OK,
            content_type: Some("application/json; charset=utf-8"),
            body: body.as_bytes().to_vec(),
            chunks: Vec::new(),
            wait_for_close: false,
            write_body_before_close: false,
        }
    }

    pub fn status(status: StatusCode) -> Self {
        Self {
            status,
            content_type: Some("application/json"),
            body: b"{\"error\":\"fixture marker\"}".to_vec(),
            chunks: Vec::new(),
            wait_for_close: false,
            write_body_before_close: false,
        }
    }

    pub fn event_stream(body: &str, split: usize) -> Self {
        let split = split.max(1);
        Self {
            status: StatusCode::OK,
            content_type: Some("text/event-stream"),
            body: Vec::new(),
            chunks: body.as_bytes().chunks(split).map(<[u8]>::to_vec).collect(),
            wait_for_close: false,
            write_body_before_close: false,
        }
    }
}

impl MockServer {
    pub async fn once(spec: ResponseSpec) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|_| panic!("mock listener"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|_| panic!("mock address"))
            .to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let once = Arc::new(AtomicUsize::new(0));
        let (headers_tx, headers_seen) = oneshot::channel();
        let (response_headers_tx, response_headers) = oneshot::channel();
        let (closed_tx, closed) = oneshot::channel();
        let requests_for_task = requests.clone();
        let once_for_task = once.clone();
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_request(&mut socket).await;
            if let Ok(request) = request {
                once_for_task.fetch_add(1, Ordering::SeqCst);
                requests_for_task
                    .lock()
                    .unwrap_or_else(|_| panic!("request lock"))
                    .push(request);
                let _ = headers_tx.send(());
                write_response(&mut socket, &spec, response_headers_tx)
                    .await
                    .unwrap_or_else(|_| panic!("mock response"));
                if spec.wait_for_close {
                    wait_for_eof(&mut socket).await;
                    let _ = closed_tx.send(());
                }
            }
        });
        Self {
            address,
            requests,
            once,
            headers_seen: Some(headers_seen),
            response_headers: Some(response_headers),
            closed: Some(closed),
            task: Some(task),
        }
    }

    pub async fn wait_for_headers(&mut self) {
        self.headers_seen
            .take()
            .unwrap_or_else(|| panic!("headers receiver"))
            .await
            .unwrap_or_else(|_| panic!("headers signal"));
    }

    pub async fn wait_for_close(&mut self) {
        self.closed
            .take()
            .unwrap_or_else(|| panic!("close receiver"))
            .await
            .unwrap_or_else(|_| panic!("close signal"));
    }

    pub async fn wait_for_response_headers(&mut self) {
        self.response_headers
            .take()
            .unwrap_or_else(|| panic!("response headers receiver"))
            .await
            .unwrap_or_else(|_| panic!("response headers signal"));
    }

    pub async fn finish(&mut self) {
        self.task
            .take()
            .unwrap_or_else(|| panic!("mock task"))
            .await
            .unwrap_or_else(|_| panic!("mock task join"));
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub fn config_for(address: &str, streaming: bool, tools: bool) -> ValidatedConfig {
    config_for_with_timeouts(address, streaming, tools, None, None)
}

pub fn config_for_with_timeouts(
    address: &str,
    streaming: bool,
    tools: bool,
    first_byte_ms: Option<u64>,
    idle_ms: Option<u64>,
) -> ValidatedConfig {
    let mut contents = include_str!("../../tests/fixtures/config/example.toml").to_owned();
    contents = contents.replace(
        "http://ollama.invalid:11434",
        &format!("http://{address}/v1"),
    );
    contents = contents.replacen(
        "streaming_chat = true",
        &format!("streaming_chat = {streaming}"),
        1,
    );
    contents = contents.replacen(
        "function_tools = true",
        &format!("function_tools = {tools}"),
        1,
    );
    contents = contents.replacen(
        "requires_streaming_chat = true",
        &format!("requires_streaming_chat = {streaming}"),
        1,
    );
    contents = contents.replacen(
        "requires_function_tools = true",
        &format!("requires_function_tools = {tools}"),
        1,
    );
    if let Some(first_byte_ms) = first_byte_ms {
        contents = contents.replacen(
            "first_byte_ms = 15000",
            &format!("first_byte_ms = {first_byte_ms}"),
            1,
        );
    }
    if let Some(idle_ms) = idle_ms {
        contents = contents.replacen("idle_ms = 30000", &format!("idle_ms = {idle_ms}"), 1);
    }
    scratch::load_text(&contents).unwrap_or_else(|_| panic!("load config"))
}

pub fn adapter(config: &ValidatedConfig) -> OllamaAdapter {
    OllamaAdapter::new(&config.adapters()[0], config.timeouts(), config.limits())
        .unwrap_or_else(|_| panic!("ollama adapter"))
}

pub fn routed(config: &ValidatedConfig, request: CoreRequest) -> RoutedRequest {
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture-request".into(),
            route: config.routes()[0].identity().clone(),
            trust_zone: TrustZone::Local,
            extensions: Default::default(),
        },
        request,
    )
    .unwrap_or_else(|_| panic!("routed request"))
}

pub fn text_request() -> CoreRequest {
    text_request_with_stream(false)
}

pub fn text_request_with_stream(stream: bool) -> CoreRequest {
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias("local-chat".into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatContent::Text {
                text: "Say hello".into(),
            }],
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream,
        extensions: Default::default(),
        options: Default::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
    })
}

pub fn tool_request(choice: ToolChoice) -> CoreRequest {
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias("local-chat".into()),
        messages: vec![
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatContent::Text {
                    text: "Look this up".into(),
                }],
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatContent::ToolCall {
                    call: ToolCall {
                        id: "call_history_1".into(),
                        name: "lookup".into(),
                        arguments: "{\"q\":\"history\"}".into(),
                    },
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: vec![ChatContent::ToolResult {
                    call_id: "call_history_1".into(),
                    content: "history result".into(),
                }],
            },
        ],
        tools: vec![kanata::core::FunctionTool {
            name: "lookup".into(),
            description: Some("Look up a fixture value".into()),
            parameters: serde_json::json!({"type":"object","properties":{"q":{"type":"string"}}}),
        }],
        tool_choice: choice,
        stream: false,
        extensions: Default::default(),
        options: Default::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
    })
}

pub fn take_chat(
    result: Result<AdapterOutput, kanata::core::GatewayError>,
) -> kanata::core::ChatResponse {
    match result.unwrap_or_else(|error| panic!("adapter error: {error:?}")) {
        AdapterOutput::Complete(CoreResponse::Chat(response)) => response,
        AdapterOutput::Complete(_) => panic!("wrong response operation"),
        AdapterOutput::Events(_) => panic!("unexpected event stream"),
    }
}

pub fn error_kind(
    result: Result<AdapterOutput, kanata::core::GatewayError>,
) -> kanata::core::ErrorKind {
    match result {
        Err(error) => error.kind,
        Ok(AdapterOutput::Complete(_)) => panic!("unexpected complete response"),
        Ok(AdapterOutput::Events(_)) => panic!("unexpected event stream"),
    }
}

pub fn capabilities(config: &ValidatedConfig) -> Capabilities {
    config.adapters()[0].capabilities().clone()
}

pub async fn api_request(server: &TwoPlaneServer, body: &str) -> axum::response::Response {
    server
        .client_oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::AUTHORIZATION, "Bearer test-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap_or_else(|_| panic!("api request")),
        )
        .await
        .unwrap_or_else(|_| panic!("api response"))
}

async fn write_response(
    socket: &mut TcpStream,
    spec: &ResponseSpec,
    response_headers: oneshot::Sender<()>,
) -> std::io::Result<()> {
    let reason = spec.status.canonical_reason().unwrap_or("Fixture");
    let body_len = if spec.chunks.is_empty() {
        spec.body.len()
    } else {
        spec.chunks.iter().map(Vec::len).sum()
    };
    let content_length = if spec.wait_for_close {
        body_len + 1
    } else {
        body_len
    };
    let mut response = format!(
        "HTTP/1.1 {} {}\r\ncontent-length: {}\r\nconnection: close\r\n",
        spec.status.as_u16(),
        reason,
        content_length
    );
    if let Some(content_type) = spec.content_type {
        response.push_str(&format!("content-type: {content_type}\r\n"));
    }
    response.push_str("\r\n");
    socket.write_all(response.as_bytes()).await?;
    let _ = response_headers.send(());
    // Body write errors mean the client closed early.
    if !spec.wait_for_close || spec.write_body_before_close {
        if spec.chunks.is_empty() {
            let _ = socket.write_all(&spec.body).await;
        }
        for chunk in &spec.chunks {
            if socket.write_all(chunk).await.is_err() {
                break;
            }
        }
    }
    Ok(())
}

pub fn route_identity() -> RouteIdentity {
    RouteIdentity::new(
        "ollama-chat",
        "concrete-model",
        ModelAlias("local-chat".into()),
        Operation::Chat,
    )
}
