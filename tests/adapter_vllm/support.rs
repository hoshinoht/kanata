#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use kanata::{
    adapter::{AdapterOutput, vllm::VllmAdapter},
    config::{self, ValidatedConfig},
    core::{
        Capabilities, ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, Extensions,
        FinishReason, FunctionTool, ModelAlias, Operation, Request as CoreRequest, RequestContext,
        Response as CoreResponse, RouteIdentity, RoutedRequest, ToolCall, ToolChoice,
        TranscriptionRequest, TrustZone, ValidatedFile,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

pub const TEXT_RESPONSE: &str = include_str!("../fixtures/vllm/chat-text-response.json");
pub const INVALID_USAGE_RESPONSE: &str =
    include_str!("../fixtures/vllm/chat-inconsistent-usage.json");
pub const UNEXPECTED_RESPONSE: &str = include_str!("../fixtures/vllm/chat-unexpected-field.json");

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub struct MockServer {
    pub address: String,
    pub requests: Arc<Mutex<Vec<RequestRecord>>>,
    pub calls: Arc<AtomicUsize>,
    request_seen: Option<oneshot::Receiver<()>>,
    response_sent: Option<oneshot::Receiver<()>>,
    client_closed: Option<oneshot::Receiver<()>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Clone)]
enum Reply {
    Complete {
        status: u16,
        content_type: String,
        body: Vec<u8>,
    },
    Incomplete,
}

impl MockServer {
    pub async fn json(body: &str) -> Self {
        Self::start(Reply::Complete {
            status: 200,
            content_type: "application/json; charset=utf-8".to_owned(),
            body: body.as_bytes().to_vec(),
        })
        .await
    }

    pub async fn json_many(body: &str, request_count: usize) -> Self {
        assert!(request_count > 0, "mock request count");
        Self::start_many(
            Reply::Complete {
                status: 200,
                content_type: "application/json; charset=utf-8".to_owned(),
                body: body.as_bytes().to_vec(),
            },
            request_count,
        )
        .await
    }

    pub async fn status(status: u16, body: &str) -> Self {
        Self::start(Reply::Complete {
            status,
            content_type: "application/json; charset=utf-8".to_owned(),
            body: body.as_bytes().to_vec(),
        })
        .await
    }

    pub async fn response(status: u16, content_type: &str, body: &str) -> Self {
        Self::start(Reply::Complete {
            status,
            content_type: content_type.to_owned(),
            body: body.as_bytes().to_vec(),
        })
        .await
    }

    pub async fn incomplete_body() -> Self {
        Self::start(Reply::Incomplete).await
    }

    async fn start(reply: Reply) -> Self {
        Self::start_many(reply, 1).await
    }

    async fn start_many(reply: Reply, request_count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|_| panic!("mock listener"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|_| panic!("mock address"))
            .to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let requests_for_task = requests.clone();
        let calls_for_task = calls.clone();
        let (request_tx, request_seen) = oneshot::channel();
        let (response_tx, response_sent) = oneshot::channel();
        let (closed_tx, client_closed) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut request_tx = Some(request_tx);
            let mut response_tx = Some(response_tx);
            for _ in 0..request_count {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let request = read_request(&mut socket)
                    .await
                    .unwrap_or_else(|_| panic!("mock request"));
                calls_for_task.fetch_add(1, Ordering::SeqCst);
                requests_for_task
                    .lock()
                    .unwrap_or_else(|_| panic!("request lock"))
                    .push(request);
                if let Some(sender) = request_tx.take() {
                    let _ = sender.send(());
                }

                match reply.clone() {
                    Reply::Complete {
                        status,
                        content_type,
                        body,
                    } => {
                        write_response(&mut socket, status, &content_type, &body)
                            .await
                            .unwrap_or_else(|_| panic!("mock response"));
                        if let Some(sender) = response_tx.take() {
                            let _ = sender.send(());
                        }
                    }
                    Reply::Incomplete => {
                        socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 64\r\nconnection: close\r\n\r\n{",
                            )
                            .await
                            .unwrap_or_else(|_| panic!("mock partial response"));
                        if let Some(sender) = response_tx.take() {
                            let _ = sender.send(());
                        }
                        wait_for_eof(&mut socket).await;
                        let _ = closed_tx.send(());
                        break;
                    }
                }
            }
        });
        Self {
            address,
            requests,
            calls,
            request_seen: Some(request_seen),
            response_sent: Some(response_sent),
            client_closed: Some(client_closed),
            task: Some(task),
        }
    }

    pub async fn wait_for_request(&mut self) {
        self.request_seen
            .take()
            .unwrap_or_else(|| panic!("request receiver"))
            .await
            .unwrap_or_else(|_| panic!("request signal"));
    }

    pub async fn wait_for_response(&mut self) {
        self.response_sent
            .take()
            .unwrap_or_else(|| panic!("response receiver"))
            .await
            .unwrap_or_else(|_| panic!("response signal"));
    }

    pub async fn wait_for_client_close(&mut self) {
        self.client_closed
            .take()
            .unwrap_or_else(|| panic!("close receiver"))
            .await
            .unwrap_or_else(|_| panic!("close signal"));
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

pub fn config_for(address: &str) -> ValidatedConfig {
    config_with(address, Operation::Chat, false, false, false)
}

pub fn config_with(
    address: &str,
    operation: Operation,
    streaming: bool,
    tools: bool,
    secret_ref: bool,
) -> ValidatedConfig {
    let operation = match operation {
        Operation::Chat => "chat",
        Operation::Transcription => "transcription",
    };
    let secret_ref = if secret_ref {
        "secret_ref = \"env:VLLM_KEY\"\n"
    } else {
        ""
    };
    // Credentials are only sent over HTTPS.
    let scheme = if secret_ref.is_empty() {
        "http"
    } else {
        "https"
    };
    let transcription_mode = if operation == "transcription" {
        "transcription_mode = \"native_asr\"\n"
    } else {
        ""
    };
    let contents = format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080

[listeners.admin]
bind = "127.0.0.1"
port = 18081

[publication]
tailnet_addresses = ["100.64.0.1"]

[[adapters]]
id = "vllm-fixture"
kind = "vllm"
base_url = "{scheme}://{address}/v1"
trust_zone = "local"
{transcription_mode}{secret_ref}[adapters.capabilities]
operations = ["{operation}"]
streaming_chat = {streaming}
function_tools = {tools}

[[routes]]
id = "vllm-chat"
model_alias = "vllm-public"
operation = "{operation}"
adapter_id = "vllm-fixture"
upstream_id = "served-checkpoint-alias"
requires_streaming_chat = false
requires_function_tools = false

[[application_keys]]
id = "fixture-client"
secret_ref = "env:KANATA_TEST_KEY"
permissions = [{{ model_alias = "vllm-public", operation = "{operation}" }}]

[limits]
max_queue = 2
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = 1048576
max_extension_bytes = 8192

[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#
    );
    let id = NEXT_CONFIG.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("kanata-vllm-fixture-{id}.toml"));
    fs::write(&path, contents).unwrap_or_else(|_| panic!("write test config"));
    let result = config::load(&path).unwrap_or_else(|error| panic!("load test config: {error}"));
    let _ = fs::remove_file(path);
    result
}

pub fn adapter(config: &ValidatedConfig) -> VllmAdapter {
    VllmAdapter::new(&config.adapters()[0], config.timeouts(), config.limits())
        .unwrap_or_else(|error| panic!("vLLM adapter: {error:?}"))
}

pub fn routed(config: &ValidatedConfig, request: CoreRequest) -> RoutedRequest {
    routed_with_zone(config, request, TrustZone::Local)
}

pub fn routed_with_zone(
    config: &ValidatedConfig,
    request: CoreRequest,
    trust_zone: TrustZone,
) -> RoutedRequest {
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture-request".into(),
            route: config.routes()[0].identity().clone(),
            trust_zone,
            extensions: Extensions::default(),
        },
        request,
    )
    .unwrap_or_else(|error| panic!("routed request: {error:?}"))
}

pub fn routed_transcription(request: CoreRequest) -> RoutedRequest {
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture-request".into(),
            route: RouteIdentity::new(
                "vllm-transcription",
                "served-checkpoint",
                ModelAlias("vllm-public".into()),
                Operation::Transcription,
            ),
            trust_zone: TrustZone::Local,
            extensions: Extensions::default(),
        },
        request,
    )
    .unwrap_or_else(|error| panic!("routed transcription: {error:?}"))
}

pub fn text_request() -> CoreRequest {
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias("vllm-public".into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatContent::Text {
                text: "Say hello".into(),
            }],
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    })
}

pub fn tool_request() -> CoreRequest {
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias("vllm-public".into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatContent::Text {
                text: "Use a function".into(),
            }],
        }],
        tools: vec![FunctionTool {
            name: "lookup".into(),
            description: None,
            parameters: serde_json::json!({"type":"object"}),
        }],
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    })
}

pub fn tool_history_request() -> CoreRequest {
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias("vllm-public".into()),
        messages: vec![ChatMessage {
            role: ChatRole::Assistant,
            content: vec![ChatContent::ToolCall {
                call: ToolCall {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{}".into(),
                },
            }],
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
    })
}

pub fn transcription_request() -> CoreRequest {
    CoreRequest::Transcription(TranscriptionRequest {
        model: ModelAlias("vllm-public".into()),
        file: ValidatedFile::new("voice.wav", "audio/wav", b"fixture".to_vec())
            .unwrap_or_else(|_| panic!("test file")),
        language: None,
        prompt: None,
        extensions: Extensions::default(),
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

pub fn error_kind(result: Result<AdapterOutput, kanata::core::GatewayError>) -> ErrorKind {
    match result {
        Err(error) => error.kind,
        Ok(AdapterOutput::Complete(_)) => panic!("unexpected complete response"),
        Ok(AdapterOutput::Events(_)) => panic!("unexpected event stream"),
    }
}

pub fn expected_capabilities() -> Capabilities {
    Capabilities::new([Operation::Chat])
}

pub fn expected_text() -> Vec<ChatContent> {
    vec![ChatContent::Text {
        text: "fixture response".into(),
    }]
}

pub fn fixture_finish_reason() -> FinishReason {
    FinishReason::Stop
}

async fn read_request(socket: &mut TcpStream) -> std::io::Result<RequestRecord> {
    let mut bytes = Vec::new();
    let head_end = loop {
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
            break index + 4;
        }
        if bytes.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers",
            ));
        }
    };
    let head = std::str::from_utf8(&bytes[..head_end - 4])
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "headers"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "method"))?
        .to_owned();
    let path = request_parts
        .next()
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
        socket.read_exact(&mut body[already_read..]).await?;
    }
    Ok(RequestRecord {
        method,
        path,
        headers,
        body,
    })
}

async fn write_response(
    socket: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Fixture",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len(),
    );
    socket.write_all(response.as_bytes()).await?;
    socket.write_all(body).await
}

async fn wait_for_eof(socket: &mut TcpStream) {
    let mut buffer = [0_u8; 1024];
    while socket.read(&mut buffer).await.unwrap_or(0) != 0 {}
}
