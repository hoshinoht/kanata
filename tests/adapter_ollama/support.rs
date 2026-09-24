#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::body::Body;
use http::{Request, StatusCode, header};
use kanata::{
    adapter::{AdapterOutput, ollama::OllamaAdapter},
    auth::{SecretResolutionError, SecretResolver},
    config::{self, ValidatedConfig},
    core::{
        Capabilities, ChatContent, ChatMessage, ChatRequest, ChatRole, ModelAlias, Operation,
        Request as CoreRequest, RequestContext, Response as CoreResponse, RouteIdentity,
        RoutedRequest, ToolCall, ToolChoice, TrustZone,
    },
    server::TwoPlaneServer,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

pub const TEXT_RESPONSE: &str = include_str!("../fixtures/ollama/chat-text-response.json");
pub const TOOLS_RESPONSE: &str = include_str!("../fixtures/ollama/chat-tools-response.json");
pub const TEXT_STREAM: &str = include_str!("../fixtures/ollama/chat-stream-text.sse");
pub const SEPARATE_USAGE_STREAM: &str =
    include_str!("../fixtures/ollama/chat-stream-text-separate-usage.sse");
pub const TOOLS_STREAM: &str = include_str!("../fixtures/ollama/chat-stream-tools.sse");

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

pub struct Resolver;

impl SecretResolver for Resolver {
    fn resolve(&self, _: &config::SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"test-key".to_vec())
    }
}

#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

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
    let id = NEXT_CONFIG.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("kanata-ollama-{id}.toml"));
    fs::write(&path, contents).unwrap_or_else(|_| panic!("write config"));
    let result = config::load(&path).unwrap_or_else(|_| panic!("load config"));
    let _ = fs::remove_file(path);
    result
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

pub async fn read_request(socket: &mut TcpStream) -> std::io::Result<RequestRecord> {
    let mut bytes = Vec::new();
    let head_end;
    loop {
        let mut chunk = [0_u8; 1024];
        let count = socket.read(&mut chunk).await?;
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "headers",
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            head_end = index + 4;
            break;
        }
        if bytes.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers",
            ));
        }
    }
    let head = std::str::from_utf8(&bytes[..head_end - 4])
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "headers"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "request line"))?;
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "path"))?
        .to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "header",
            ));
        };
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers
        .get("content-length")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "length"))?
        .parse::<usize>()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "length"))?;
    let mut body = bytes.split_off(head_end);
    if body.len() > length {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body"));
    }
    let already_read = body.len();
    body.resize(length, 0);
    if already_read < length {
        socket
            .read_exact(&mut body[already_read..])
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "body"))?;
    }
    Ok(RequestRecord {
        path,
        headers,
        body,
    })
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
    if spec.wait_for_close && !spec.write_body_before_close {
        Ok(())
    } else if spec.chunks.is_empty() {
        socket.write_all(&spec.body).await
    } else {
        for chunk in &spec.chunks {
            socket.write_all(chunk).await?;
        }
        Ok(())
    }
}

async fn wait_for_eof(socket: &mut TcpStream) {
    let mut buffer = [0_u8; 1024];
    while socket.read(&mut buffer).await.unwrap_or(0) != 0 {}
}

pub fn route_identity() -> RouteIdentity {
    RouteIdentity::new(
        "ollama-chat",
        "concrete-model",
        ModelAlias("local-chat".into()),
        Operation::Chat,
    )
}
