#![allow(dead_code)]

#[path = "../support/http.rs"]
mod http_mock;
use crate::scratch;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use kanata::{
    adapter::{AdapterOutput, vllm::VllmAdapter},
    config::ValidatedConfig,
    core::{
        Capabilities, ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, Extensions,
        FinishReason, FunctionTool, ModelAlias, Operation, Request as CoreRequest, RequestContext,
        Response as CoreResponse, RouteIdentity, RoutedRequest, ToolCall, ToolChoice,
        TranscriptionRequest, TrustZone, ValidatedFile,
    },
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};

use http_mock::wait_for_eof;
pub use http_mock::{RequestRecord, read_request};

pub const TEXT_RESPONSE: &str = include_str!("../fixtures/vllm/chat-text-response.json");
pub const INVALID_USAGE_RESPONSE: &str =
    include_str!("../fixtures/vllm/chat-inconsistent-usage.json");
pub const UNEXPECTED_RESPONSE: &str = include_str!("../fixtures/vllm/chat-unexpected-field.json");

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
    // Credentials need HTTPS unless the host is a loopback IP literal; keep fixtures on HTTPS.
    let scheme = if secret_ref { "https" } else { "http" };
    config_at(
        &format!("{scheme}://{address}/v1"),
        "local",
        operation,
        streaming,
        tools,
        secret_ref,
    )
}

pub fn config_at(
    base_url: &str,
    trust_zone: &str,
    operation: Operation,
    streaming: bool,
    tools: bool,
    secret_ref: bool,
) -> ValidatedConfig {
    load_config(&config_text_at(
        base_url, trust_zone, operation, streaming, tools, secret_ref,
    ))
}

/// Config text behind `config_at`, for tests that add adapter fields.
pub fn config_text_at(
    base_url: &str,
    trust_zone: &str,
    operation: Operation,
    streaming: bool,
    tools: bool,
    secret_ref: bool,
) -> String {
    let operation = operation.as_str();
    let secret_ref = if secret_ref {
        "secret_ref = \"env:VLLM_KEY\"\n"
    } else {
        ""
    };
    let transcription_mode = if operation == "transcription" {
        "transcription_mode = \"native_asr\"\n"
    } else {
        ""
    };
    scratch::fixture_text(
        &format!(
            r#"[[adapters]]
id = "vllm-fixture"
kind = "vllm"
base_url = "{base_url}"
trust_zone = "{trust_zone}"
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
"#
        ),
        &format!("{{ model_alias = \"vllm-public\", operation = \"{operation}\" }}"),
        1_048_576,
    )
}

pub fn load_config(contents: &str) -> ValidatedConfig {
    scratch::load_valid(contents)
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
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
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
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
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
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
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

pub async fn write_response<S: AsyncWrite + Unpin>(
    socket: &mut S,
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
