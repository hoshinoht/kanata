use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::oneshot, task::JoinHandle};
use tokio_rustls::TlsAcceptor;

use crate::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    auth::{SecretResolutionError, SecretResolver, canonical_bearer_token},
    config::{SecretReference, ValidatedConfig},
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, Extensions, FinishReason,
        ModelAlias, Request as CoreRequest, RequestContext, RoutedRequest, ToolChoice, TrustZone,
        Usage,
    },
    test_support::{
        RequestRecord, load_config, loopback_listener, read_request, tls_fixture, wait_for_close,
    },
};

use super::OpenRouterAdapter;

const ADAPTER_ID: &str = "openrouter-fixture";
const ROUTE_ID: &str = "openrouter-chat";
const PUBLIC_MODEL: &str = "openrouter-public";
const UPSTREAM_MODEL: &str = "openai/gpt-4o-mini";
const FIXTURE_HOST: &str = "openrouter.fixture";
const TOKEN: &str = "fixture-openrouter-key";
const RESPONSE: &str = include_str!("../../../tests/fixtures/openrouter/chat-completion.json");

#[path = "tests/stream.rs"]
mod stream;

struct SyntheticResolver;

impl SecretResolver for SyntheticResolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        match reference {
            SecretReference::Env(name) if name == "TEST_OPENROUTER_KEY" => {
                Ok(TOKEN.as_bytes().to_vec())
            }
            _ => Err(SecretResolutionError),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Features {
    streaming_chat: bool,
    input_images: bool,
    allows_input_images: bool,
}

const STREAMING: Features = Features {
    streaming_chat: true,
    input_images: false,
    allows_input_images: false,
};

fn config_text(host: &str, port: u16, features: Features) -> String {
    let Features {
        streaming_chat,
        input_images,
        allows_input_images,
    } = features;
    format!(
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
id = "{ADAPTER_ID}"
kind = "openrouter"
base_url = "https://{host}:{port}/api/v1"
trust_zone = "external"
secret_ref = "env:TEST_OPENROUTER_KEY"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = {streaming_chat}
function_tools = false
input_audio = false
input_images = {input_images}
audio_streaming_chat = false
audio_function_tools = false

[[routes]]
id = "{ROUTE_ID}"
model_alias = "{PUBLIC_MODEL}"
operation = "chat"
adapter_id = "{ADAPTER_ID}"
upstream_id = "{UPSTREAM_MODEL}"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = false
allows_input_images = {allows_input_images}

[keys]
file = "kanata-unit-missing-keys.toml"

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
    )
}

/// An adapter whose configured `host` resolves to a loopback TLS fixture for `FIXTURE_HOST`.
struct Client {
    config: ValidatedConfig,
    adapter: OpenRouterAdapter,
}

impl Client {
    async fn new(host: &str, features: Features) -> (Self, TcpListener, TlsAcceptor) {
        let (listener, address) = loopback_listener().await;
        let (certificate, acceptor) = tls_fixture(FIXTURE_HOST);
        let config = load_config(&config_text(host, address.port(), features));
        let adapter = OpenRouterAdapter::from_config_with_tls_fixture(
            &config,
            ADAPTER_ID,
            ROUTE_ID,
            &SyntheticResolver,
            certificate,
            address,
        )
        .unwrap_or_else(|error| panic!("OpenRouter fixture adapter: {error:?}"));
        (Self { config, adapter }, listener, acceptor)
    }

    /// A non-streaming client whose upstream answers once with `status`.
    async fn responding(
        status: u16,
        content_type: &'static str,
        body: &[u8],
    ) -> (Self, JoinHandle<io::Result<RequestRecord>>) {
        let (client, listener, acceptor) = Self::new(FIXTURE_HOST, Features::default()).await;
        let server = tokio::spawn(serve_response(
            listener,
            acceptor,
            status,
            content_type,
            body.to_vec(),
        ));
        (client, server)
    }

    fn execute(&self, request: CoreRequest) -> AdapterFuture {
        self.adapter.execute(routed(&self.config, request))
    }
}

fn routed(config: &ValidatedConfig, request: CoreRequest) -> RoutedRequest {
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture-request".into(),
            route: config.routes()[0].identity().clone(),
            trust_zone: TrustZone::External,
            extensions: Extensions::default(),
        },
        request,
    )
    .unwrap_or_else(|error| panic!("routed fixture request: {error:?}"))
}

fn text_request() -> CoreRequest {
    text_request_with_stream(false)
}

fn text_request_with_stream(stream: bool) -> CoreRequest {
    let message = |role, text: &str| ChatMessage {
        role,
        content: vec![ChatContent::Text { text: text.into() }],
    };
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias(PUBLIC_MODEL.into()),
        messages: vec![
            message(ChatRole::System, "Be concise."),
            message(ChatRole::User, "What is 1 + 1?"),
            message(ChatRole::Assistant, "2"),
            message(ChatRole::User, "Say hello"),
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream,
        extensions: Extensions::default(),
        options: Default::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
    })
}

fn error_kind(result: Result<AdapterOutput, crate::core::GatewayError>) -> ErrorKind {
    match result {
        Err(error) => error.kind,
        Ok(AdapterOutput::Complete(_)) | Ok(AdapterOutput::Events(_)) => {
            panic!("unexpected adapter success")
        }
    }
}

async fn recorded(server: JoinHandle<io::Result<RequestRecord>>) -> RequestRecord {
    server
        .await
        .unwrap_or_else(|_| panic!("fixture server task"))
        .unwrap_or_else(|_| panic!("fixture request"))
}

/// An upstream that answers with `head` and an unfinished body, then waits for the client to close.
struct Stalled {
    request: oneshot::Receiver<RequestRecord>,
    closed: oneshot::Receiver<()>,
    accepted: Arc<AtomicUsize>,
    server: JoinHandle<()>,
}

impl Stalled {
    fn serve(listener: TcpListener, acceptor: TlsAcceptor, head: &'static [u8]) -> Self {
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_for_server = accepted.clone();
        let (request_tx, request) = oneshot::channel();
        let (closed_tx, closed) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener
                .accept()
                .await
                .unwrap_or_else(|_| panic!("fixture accept"));
            accepted_for_server.fetch_add(1, Ordering::SeqCst);
            let mut socket = acceptor
                .accept(socket)
                .await
                .unwrap_or_else(|_| panic!("fixture TLS accept"));
            let record = read_request(&mut socket)
                .await
                .unwrap_or_else(|_| panic!("fixture request"));
            socket
                .write_all(head)
                .await
                .unwrap_or_else(|_| panic!("partial response"));
            request_tx
                .send(record)
                .unwrap_or_else(|_| panic!("request signal"));
            wait_for_close(&mut socket).await;
            let _ = closed_tx.send(());
            if let Ok(Ok((socket, _))) =
                tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
            {
                accepted_for_server.fetch_add(1, Ordering::SeqCst);
                drop(socket);
            }
        });
        Self {
            request,
            closed,
            accepted,
            server,
        }
    }

    async fn request(&mut self) -> RequestRecord {
        tokio::time::timeout(Duration::from_secs(3), &mut self.request)
            .await
            .unwrap_or_else(|_| panic!("fixture request timeout"))
            .unwrap_or_else(|_| panic!("fixture request signal"))
    }

    /// Asserts the upstream saw the close and no replayed connection.
    async fn assert_closed_without_replay(self) {
        tokio::time::timeout(Duration::from_secs(2), self.closed)
            .await
            .unwrap_or_else(|_| panic!("upstream remained open"))
            .unwrap_or_else(|_| panic!("close signal"));
        self.server
            .await
            .unwrap_or_else(|_| panic!("fixture server task"));
        assert_eq!(self.accepted.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn canonical_bearer_is_bounded_and_rejects_injection_or_opaque_bytes() {
    let file = SecretReference::File(PathBuf::from("/fixture/token"));
    assert_eq!(
        canonical_bearer_token(&file, b"fixture-key\n".to_vec())
            .unwrap_or_else(|_| panic!("LF token")),
        "fixture-key"
    );
    assert_eq!(
        canonical_bearer_token(&file, b"fixture-key\r\n".to_vec())
            .unwrap_or_else(|_| panic!("CRLF terminal newline")),
        "fixture-key"
    );
    for invalid in [
        b"fixture-key\n\n".to_vec(),
        b"fixture-key\r\nX-Leak: value".to_vec(),
        b"fixture-key\r".to_vec(),
        vec![0xff],
        vec![b'a'; 4097],
    ] {
        assert!(canonical_bearer_token(&file, invalid).is_err());
    }
    assert!(
        canonical_bearer_token(&SecretReference::Env("TEST".into()), b"key\n".to_vec()).is_err()
    );
}

#[test]
fn documented_non_tool_finish_reasons_normalize_to_the_public_contract() {
    let mut payload: Value =
        serde_json::from_str(RESPONSE).unwrap_or_else(|_| panic!("response fixture"));
    for (upstream, expected) in [
        ("length", FinishReason::Length),
        ("content_filter", FinishReason::ContentFilter),
    ] {
        payload["choices"][0]["finish_reason"] = json!(upstream);
        let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| panic!("response json"));
        let response = super::response::decode(&bytes, ModelAlias(PUBLIC_MODEL.into()))
            .unwrap_or_else(|_| panic!("normalized finish reason"));
        assert_eq!(response.finish_reason, expected);
    }
}

#[test]
fn current_response_metadata_fields_are_accepted() {
    let mut payload: Value =
        serde_json::from_str(RESPONSE).unwrap_or_else(|_| panic!("response fixture"));
    payload["provider"] = json!("OpenAI");
    payload["system_fingerprint"] = Value::Null;
    let choice = &mut payload["choices"][0];
    choice["logprobs"] = Value::Null;
    choice["native_finish_reason"] = json!("completed");
    choice["message"]["refusal"] = Value::Null;
    choice["message"]["reasoning"] = Value::Null;
    let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| panic!("response json"));
    assert!(super::response::decode(&bytes, ModelAlias(PUBLIC_MODEL.into())).is_ok());
}

#[test]
fn empty_content_is_an_empty_reply_unless_filtered_or_refused() {
    let mut payload: Value =
        serde_json::from_str(RESPONSE).unwrap_or_else(|_| panic!("response fixture"));
    payload["choices"][0]["message"]["content"] = json!("");
    payload["choices"][0]["message"]["reasoning"] = json!("thinking");
    payload["choices"][0]["message"]["reasoning_details"] = json!([{"type": "reasoning.text"}]);
    for (upstream, refusal, ok) in [
        ("length", Value::Null, true),
        ("stop", Value::Null, true),
        ("stop", json!("I can't help with that."), false),
        ("content_filter", Value::Null, false),
    ] {
        payload["choices"][0]["finish_reason"] = json!(upstream);
        payload["choices"][0]["message"]["refusal"] = refusal;
        let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| panic!("response json"));
        let result = super::response::decode(&bytes, ModelAlias(PUBLIC_MODEL.into()));
        match result {
            Ok(response) if ok => assert!(response.message.content.is_empty()),
            Err(error) if !ok => assert_eq!(error.kind, ErrorKind::UpstreamFailure),
            _ => panic!("unexpected result for {upstream}"),
        }
    }
}

#[test]
fn malformed_required_usage_still_fails_with_known_metadata_present() {
    let mut payload: Value =
        serde_json::from_str(RESPONSE).unwrap_or_else(|_| panic!("response fixture"));
    payload["usage"]
        .as_object_mut()
        .unwrap_or_else(|| panic!("usage object"))
        .remove("total_tokens");
    let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| panic!("response json"));
    let error = super::response::decode(&bytes, ModelAlias(PUBLIC_MODEL.into()))
        .err()
        .unwrap_or_else(|| panic!("missing required usage accepted"));
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);
}

#[tokio::test]
async fn verified_https_chat_sends_explicit_model_no_fallback_and_normalizes_fixture() {
    let (client, server) =
        Client::responding(200, "application/json; charset=utf-8", RESPONSE.as_bytes()).await;
    assert!(!format!("{:?}", client.adapter).contains(TOKEN));

    let output = tokio::time::timeout(Duration::from_secs(3), client.execute(text_request()))
        .await
        .unwrap_or_else(|_| panic!("OpenRouter fixture timeout"))
        .unwrap_or_else(|error| panic!("OpenRouter fixture response: {error:?}"));
    let response = match output {
        AdapterOutput::Complete(crate::core::Response::Chat(response)) => response,
        AdapterOutput::Complete(_) | AdapterOutput::Events(_) => panic!("wrong output operation"),
    };
    assert_eq!(response.model, ModelAlias(PUBLIC_MODEL.into()));
    assert_eq!(response.message.role, ChatRole::Assistant);
    assert_eq!(
        response.message.content,
        vec![ChatContent::Text {
            text: "fixture response".into()
        }]
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.usage,
        Some(Usage {
            input_tokens: 11,
            output_tokens: 7,
            total_tokens: 18,
            reasoning_tokens: Some(0),
            cached_input_tokens: Some(2),
        })
    );

    let record = recorded(server).await;
    assert_eq!(record.method, "POST");
    assert_eq!(record.path, "/api/v1/chat/completions");
    assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(record.headers["accept"], "application/json");
    assert_eq!(record.headers["content-type"], "application/json");
    let payload: Value =
        serde_json::from_slice(&record.body).unwrap_or_else(|_| panic!("fixture request json"));
    assert_eq!(payload["model"], UPSTREAM_MODEL);
    assert_eq!(payload["stream"], false);
    assert_eq!(payload["provider"], json!({"allow_fallbacks": false}));
    assert_eq!(
        payload["messages"],
        json!([
            {"role":"system", "content":"Be concise."},
            {"role":"user", "content":"What is 1 + 1?"},
            {"role":"assistant", "content":"2"},
            {"role":"user", "content":"Say hello"}
        ])
    );
    let object = payload
        .as_object()
        .unwrap_or_else(|| panic!("request object"));
    assert_eq!(object.len(), 4);
    assert!(!object.contains_key("models"));
}

#[tokio::test]
async fn mismatched_fixture_hostname_is_rejected_by_tls_verification() {
    let (client, listener, acceptor) =
        Client::new("wrong.openrouter.fixture", Features::default()).await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener
            .accept()
            .await
            .unwrap_or_else(|_| panic!("fixture accept"));
        acceptor.accept(socket).await.is_err()
    });

    assert_eq!(
        error_kind(client.execute(text_request()).await),
        ErrorKind::UpstreamUnavailable
    );
    assert!(
        server
            .await
            .unwrap_or_else(|_| panic!("fixture server task"))
    );
}

#[tokio::test]
async fn upstream_status_and_malformed_body_are_redacted_and_never_retried() {
    for (status, body, kind, secret) in [
        (
            429,
            &br#"{"error":"fixture-upstream-secret"}"#[..],
            ErrorKind::RateLimited,
            "fixture-upstream-secret",
        ),
        (
            200,
            br#"{"id":"fixture","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"private":"fixture-response-secret"}"#,
            ErrorKind::UpstreamFailure,
            "fixture-response-secret",
        ),
    ] {
        let (client, server) = Client::responding(status, "application/json", body).await;
        let error = match client.execute(text_request()).await {
            Err(error) => error,
            Ok(AdapterOutput::Complete(_)) | Ok(AdapterOutput::Events(_)) => {
                panic!("unexpected success for status {status}")
            }
        };
        assert_eq!(error.kind, kind);
        assert!(!format!("{error:?}").contains(secret));
        assert_eq!(recorded(server).await.path, "/api/v1/chat/completions");
    }
}

#[tokio::test]
async fn cancelling_an_inflight_https_request_closes_it_without_replay() {
    let (client, listener, acceptor) = Client::new(FIXTURE_HOST, Features::default()).await;
    let mut upstream = Stalled::serve(
        listener,
        acceptor,
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4096\r\nconnection: close\r\n\r\n{",
    );

    let task = tokio::spawn(async move { client.execute(text_request()).await });
    assert_eq!(upstream.request().await.path, "/api/v1/chat/completions");
    task.abort();
    let _ = task.await;
    upstream.assert_closed_without_replay().await;
}

async fn serve_response(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
) -> io::Result<RequestRecord> {
    let (socket, _) = listener.accept().await?;
    let mut socket = acceptor.accept(socket).await.map_err(io::Error::other)?;
    let record = read_request(&mut socket).await?;
    let reason = if status == 200 {
        "OK"
    } else {
        "Too Many Requests"
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len(),
    );
    socket.write_all(response.as_bytes()).await?;
    socket.write_all(&body).await?;
    Ok(record)
}

#[test]
fn chat_options_encode_with_reasoning_as_an_effort_object() {
    use crate::core::{
        ChatOptions, JsonSchemaFormat, ReasoningEffort, ResponseFormat, SamplingOptions,
        Temperature, TopP,
    };

    let chat = ChatRequest {
        model: ModelAlias(PUBLIC_MODEL.into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatContent::Text {
                text: "Say hello".into(),
            }],
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        options: ChatOptions {
            response_format: Some(ResponseFormat::JsonSchema {
                json_schema: JsonSchemaFormat::new(
                    "greeting".into(),
                    Some("A greeting.".into()),
                    json!({"type": "object", "properties": {"text": {"type": "string"}}}),
                    None,
                )
                .expect("schema"),
            }),
            sampling: SamplingOptions {
                temperature: Some(Temperature::new(0.7).expect("temperature")),
                top_p: Some(TopP::new(0.9).expect("top_p")),
                seed: Some(-1),
                stop: Vec::new(),
                presence_penalty: None,
                frequency_penalty: None,
            },
            max_output_tokens: Some(64),
            max_output_tokens_param: Default::default(),
            stop_param: Default::default(),
            reasoning_effort: Some(ReasoningEffort::Max),
            enable_thinking: None,
        },
        extensions: Extensions::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
    };
    let payload = super::request::encode(&chat, UPSTREAM_MODEL, false).expect("encodes");
    let expected: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/openrouter/chat-options-request.json"
    ))
    .expect("fixture json");
    assert_eq!(serde_json::to_value(payload).expect("serializes"), expected);

    let mut chat = chat;
    chat.options.sampling.stop = vec!["END".into(), "STOP".into()];
    chat.options.sampling.presence_penalty = Some(crate::core::Penalty::new(1.5).expect("penalty"));
    chat.options.sampling.frequency_penalty =
        Some(crate::core::Penalty::new(-2.0).expect("penalty"));
    let payload = serde_json::to_value(
        super::request::encode(&chat, UPSTREAM_MODEL, false).expect("encodes"),
    )
    .expect("serializes");
    assert_eq!(payload["stop"], json!(["END", "STOP"]));
    assert_eq!(payload["presence_penalty"], 1.5);
    assert_eq!(payload["frequency_penalty"], -2.0);
}

#[test]
fn input_audio_encodes_as_content_parts_and_text_stays_a_string() {
    use crate::core::{InputAudioFormat, ValidatedAudio};
    let chat = ChatRequest {
        model: ModelAlias(PUBLIC_MODEL.into()),
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: vec![ChatContent::Text {
                    text: "Be brief.".into(),
                }],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![
                    ChatContent::Text {
                        text: "What is said?".into(),
                    },
                    ChatContent::InputAudio {
                        audio: ValidatedAudio::new(InputAudioFormat::Wav, b"RIFF".to_vec())
                            .expect("audio"),
                    },
                ],
            },
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
    };
    let payload = serde_json::to_value(
        super::request::encode(&chat, "mistralai/voxtral-small-24b-2507", false).expect("encodes"),
    )
    .expect("serializes");
    assert_eq!(
        payload["messages"],
        json!([
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": [
                {"type": "text", "text": "What is said?"},
                {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}}
            ]}
        ])
    );
}

#[test]
fn transcription_encodes_base64_json_and_decodes_text_only() {
    use crate::core::{TranscriptionRequest, ValidatedFile};
    let transcription = TranscriptionRequest {
        model: ModelAlias(PUBLIC_MODEL.into()),
        file: ValidatedFile::new("clip.mp3", "audio/mpeg", b"ID3".to_vec()).expect("file"),
        language: Some("en".into()),
        prompt: None,
        extensions: Extensions::default(),
    };
    let payload = serde_json::to_value(
        super::request::encode_transcription(
            &transcription,
            "mistralai/voxtral-small-24b-2507-stt",
        )
        .expect("encodes"),
    )
    .expect("serializes");
    assert_eq!(
        payload,
        json!({
            "model": "mistralai/voxtral-small-24b-2507-stt",
            "input_audio": {"data": "SUQz", "format": "mp3"},
            "language": "en",
            "response_format": "json"
        })
    );

    let decoded = super::response::decode_transcription(
        br#"{"text":" hello ","usage":{"seconds":1.2,"cost":0.0001}}"#,
    )
    .expect("decodes");
    assert_eq!(decoded.text, "hello");
    assert!(super::response::decode_transcription(br#"{"text":"  "}"#).is_err());
    assert!(super::response::decode_transcription(br#"{"text":"x","extra":1}"#).is_err());
}

#[test]
fn tool_call_completion_decodes() {
    const TOOL: &str =
        include_str!("../../../tests/fixtures/openrouter/chat-completion-tool-call.json");
    let chat =
        super::response::decode(TOOL.as_bytes(), ModelAlias("voxtral".into())).expect("tool call");
    assert_eq!(chat.finish_reason, FinishReason::ToolCalls);
    assert!(matches!(
        chat.message.content.as_slice(),
        [ChatContent::ToolCall { call }]
            if call.id == "call-fixture" && call.name == "get_weather"
                && call.arguments == r#"{"city": "Singapore"}"#
    ));
}

#[test]
fn tools_and_tool_history_encode() {
    use crate::core::{FunctionTool, ToolCall};
    let chat = ChatRequest {
        model: ModelAlias(PUBLIC_MODEL.into()),
        messages: vec![
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatContent::Text {
                    text: "weather?".into(),
                }],
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ChatContent::ReasoningText {
                        text: "check the forecast".into(),
                    },
                    ChatContent::Text {
                        text: String::new(),
                    },
                    ChatContent::ToolCall {
                        call: ToolCall {
                            id: "call-1".into(),
                            name: "get_weather".into(),
                            arguments: "{}".into(),
                        },
                    },
                ],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: vec![ChatContent::ToolResult {
                    call_id: "call-1".into(),
                    content: "sunny".into(),
                }],
            },
        ],
        tools: vec![FunctionTool {
            name: "get_weather".into(),
            description: None,
            parameters: json!({"type": "object"}),
        }],
        tool_choice: ToolChoice::Function {
            name: "get_weather".into(),
        },
        stream: true,
        options: Default::default(),
        extensions: Extensions::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
    };
    let payload = super::request::encode(&chat, UPSTREAM_MODEL, true).expect("encodes");
    assert_eq!(
        serde_json::to_value(payload).expect("serializes"),
        json!({
            "model": UPSTREAM_MODEL,
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}}], "reasoning": "check the forecast"},
                {"role": "tool", "content": "sunny", "tool_call_id": "call-1"}
            ],
            "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}],
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
            "stream": true,
            "provider": {"allow_fallbacks": false}
        })
    );
}

#[tokio::test]
async fn image_input_requires_route_permission_and_uses_inline_https_payload() {
    const IMAGE: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl2n3QAAAAASUVORK5CYII=";
    for allowed in [false, true] {
        let features = Features {
            input_images: true,
            allows_input_images: allowed,
            ..Features::default()
        };
        let (client, listener, acceptor) = Client::new(FIXTURE_HOST, features).await;
        let CoreRequest::Chat(mut chat) = text_request() else {
            panic!("chat fixture")
        };
        chat.messages = vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatContent::InputImage {
                image: crate::core::ValidatedImage::from_data_url(IMAGE.to_owned()).unwrap(),
            }],
        }];
        if !allowed {
            let result = client.execute(CoreRequest::Chat(chat)).await;
            assert_eq!(error_kind(result), ErrorKind::UnsupportedOperation);
            continue;
        }
        let server = tokio::spawn(serve_response(
            listener,
            acceptor,
            200,
            "application/json",
            RESPONSE.as_bytes().to_vec(),
        ));
        let response = client.execute(CoreRequest::Chat(chat)).await.unwrap();
        assert!(matches!(
            response,
            AdapterOutput::Complete(crate::core::Response::Chat(_))
        ));
        let record = recorded(server).await;
        let payload: Value = serde_json::from_slice(&record.body).unwrap();
        assert_eq!(
            payload["messages"][0]["content"],
            json!([{"type": "image_url", "image_url": {"url": IMAGE}}])
        );
        assert_eq!(payload["model"], UPSTREAM_MODEL);
    }
}
