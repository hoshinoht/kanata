use std::{
    collections::BTreeMap,
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use rcgen::generate_simple_self_signed;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

use crate::{
    adapter::{Adapter, AdapterOutput},
    auth::{SecretResolutionError, SecretResolver, canonical_bearer_token},
    config::{self, SecretReference, ValidatedConfig},
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, Extensions, FinishReason,
        ModelAlias, Request as CoreRequest, RequestContext, RoutedRequest, ToolChoice, TrustZone,
        Usage,
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

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

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

struct RequestRecord {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn config_for(host: &str, port: u16) -> ValidatedConfig {
    config_with_streaming(host, port, false)
}

fn config_with_streaming(host: &str, port: u16, streaming_chat: bool) -> ValidatedConfig {
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

[[application_keys]]
id = "fixture-client"
secret_ref = "env:KANATA_TEST_CLIENT_KEY"
permissions = [{{ model_alias = "{PUBLIC_MODEL}", operation = "chat" }}]

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
    let path = std::env::temp_dir().join(format!("kanata-openrouter-tls-{id}.toml"));
    fs::write(&path, contents).unwrap_or_else(|_| panic!("write fixture config"));
    let config = config::load(&path).unwrap_or_else(|error| panic!("fixture config: {error}"));
    let _ = fs::remove_file(path);
    config
}

fn tls_fixture() -> (CertificateDer<'static>, TlsAcceptor) {
    let certified = generate_simple_self_signed(vec![FIXTURE_HOST.to_owned()])
        .unwrap_or_else(|_| panic!("fixture certificate"));
    let certificate = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)
        .unwrap_or_else(|_| panic!("server tls config"));
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    (certificate, acceptor)
}

fn adapter_for(
    config: &ValidatedConfig,
    certificate: CertificateDer<'static>,
    address: std::net::SocketAddr,
) -> OpenRouterAdapter {
    OpenRouterAdapter::from_config_with_tls_fixture(
        config,
        ADAPTER_ID,
        ROUTE_ID,
        &SyntheticResolver,
        certificate,
        address,
    )
    .unwrap_or_else(|error| panic!("OpenRouter fixture adapter: {error:?}"))
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
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias(PUBLIC_MODEL.into()),
        messages: vec![
            ChatMessage {
                role: ChatRole::System,
                content: vec![ChatContent::Text {
                    text: "Be concise.".into(),
                }],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatContent::Text {
                    text: "What is 1 + 1?".into(),
                }],
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatContent::Text { text: "2".into() }],
            },
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatContent::Text {
                    text: "Say hello".into(),
                }],
            },
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream,
        extensions: Extensions::default(),
        options: Default::default(),
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
fn empty_content_is_accepted_only_for_a_length_stop() {
    let mut payload: Value =
        serde_json::from_str(RESPONSE).unwrap_or_else(|_| panic!("response fixture"));
    payload["choices"][0]["message"]["content"] = json!("");
    payload["choices"][0]["message"]["reasoning"] = json!("thinking");
    payload["choices"][0]["message"]["reasoning_details"] = json!([{"type": "reasoning.text"}]);
    for (upstream, ok) in [("length", true), ("stop", false)] {
        payload["choices"][0]["finish_reason"] = json!(upstream);
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
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("fixture listener"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("fixture address"));
    let (certificate, acceptor) = tls_fixture();
    let config = config_for(FIXTURE_HOST, address.port());
    let adapter = adapter_for(&config, certificate, address);
    assert!(!format!("{adapter:?}").contains(TOKEN));
    let server = tokio::spawn(serve_response(
        listener,
        acceptor,
        200,
        "application/json; charset=utf-8",
        RESPONSE.as_bytes().to_vec(),
    ));

    let output = tokio::time::timeout(
        Duration::from_secs(3),
        adapter.execute(routed(&config, text_request())),
    )
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
        })
    );

    let record = server
        .await
        .unwrap_or_else(|_| panic!("fixture server task"))
        .unwrap_or_else(|_| panic!("fixture request"));
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
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("fixture listener"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("fixture address"));
    let (certificate, acceptor) = tls_fixture();
    let config = config_for("wrong.openrouter.fixture", address.port());
    let adapter = adapter_for(&config, certificate, address);
    let server = tokio::spawn(async move {
        let (socket, _) = listener
            .accept()
            .await
            .unwrap_or_else(|_| panic!("fixture accept"));
        acceptor.accept(socket).await.is_err()
    });

    assert_eq!(
        error_kind(adapter.execute(routed(&config, text_request())).await),
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
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("fixture listener"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("fixture address"));
    let (certificate, acceptor) = tls_fixture();
    let config = config_for(FIXTURE_HOST, address.port());
    let adapter = adapter_for(&config, certificate, address);
    let server = tokio::spawn(serve_response(
        listener,
        acceptor,
        429,
        "application/json",
        br#"{"error":"fixture-upstream-secret"}"#.to_vec(),
    ));

    let error = match adapter.execute(routed(&config, text_request())).await {
        Err(error) => error,
        Ok(AdapterOutput::Complete(_)) | Ok(AdapterOutput::Events(_)) => {
            panic!("unexpected status success")
        }
    };
    assert_eq!(error.kind, ErrorKind::RateLimited);
    assert!(!format!("{error:?}").contains("fixture-upstream-secret"));
    let record = server
        .await
        .unwrap_or_else(|_| panic!("fixture server task"))
        .unwrap_or_else(|_| panic!("fixture request"));
    assert_eq!(record.path, "/api/v1/chat/completions");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("fixture listener"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("fixture address"));
    let (certificate, acceptor) = tls_fixture();
    let config = config_for(FIXTURE_HOST, address.port());
    let adapter = adapter_for(&config, certificate, address);
    let server = tokio::spawn(serve_response(
        listener,
        acceptor,
        200,
        "application/json",
        br#"{"id":"fixture","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"private":"fixture-response-secret"}"#.to_vec(),
    ));
    let error = match adapter.execute(routed(&config, text_request())).await {
        Err(error) => error,
        Ok(AdapterOutput::Complete(_)) | Ok(AdapterOutput::Events(_)) => {
            panic!("unexpected malformed response success")
        }
    };
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);
    assert!(!format!("{error:?}").contains("fixture-response-secret"));
    server
        .await
        .unwrap_or_else(|_| panic!("fixture server task"))
        .unwrap_or_else(|_| panic!("fixture request"));
}

#[tokio::test]
async fn cancelling_an_inflight_https_request_closes_it_without_replay() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("fixture listener"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("fixture address"));
    let (certificate, acceptor) = tls_fixture();
    let config = config_for(FIXTURE_HOST, address.port());
    let adapter = adapter_for(&config, certificate, address);
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_for_server = accepted.clone();
    let (request_tx, request_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener
            .accept()
            .await
            .unwrap_or_else(|_| panic!("fixture accept"));
        accepted_for_server.fetch_add(1, Ordering::SeqCst);
        let mut socket = acceptor
            .accept(socket)
            .await
            .unwrap_or_else(|_| panic!("fixture tls accept"));
        let record = read_request_inner(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("fixture request"));
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4096\r\nconnection: close\r\n\r\n{",
            )
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

    let request = routed(&config, text_request());
    let task = tokio::spawn(async move { adapter.execute(request).await });
    let record = tokio::time::timeout(Duration::from_secs(3), request_rx)
        .await
        .unwrap_or_else(|_| panic!("fixture request timeout"))
        .unwrap_or_else(|_| panic!("fixture request signal"));
    assert_eq!(record.path, "/api/v1/chat/completions");
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(2), closed_rx)
        .await
        .unwrap_or_else(|_| panic!("upstream remained open after cancellation"))
        .unwrap_or_else(|_| panic!("close signal"));
    server
        .await
        .unwrap_or_else(|_| panic!("fixture server task"));
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
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
    let record = read_request_inner(&mut socket).await?;
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

async fn read_request_inner<S: AsyncRead + Unpin>(socket: &mut S) -> io::Result<RequestRecord> {
    let mut bytes = Vec::new();
    let head_end = loop {
        let mut chunk = [0_u8; 2048];
        let count = socket.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request headers",
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
    };
    let head = std::str::from_utf8(&bytes[..head_end - 4])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "headers"))?;
    let mut lines = head.split("\r\n");
    let mut request_parts = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request line"))?
        .split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "method"))?
        .to_owned();
    let path = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path"))?
        .to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "header"));
        };
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers
        .get("content-length")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "content length"))?
        .parse::<usize>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "content length"))?;
    let mut body = bytes.split_off(head_end);
    if body.len() > length {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request body"));
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

async fn wait_for_close<S: AsyncRead + Unpin>(socket: &mut S) {
    let mut buffer = [0_u8; 1024];
    loop {
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
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
            },
            max_output_tokens: Some(64),
            max_output_tokens_param: Default::default(),
            reasoning_effort: Some(ReasoningEffort::Max),
            enable_thinking: None,
        },
        extensions: Extensions::default(),
    };
    let payload = super::request::encode(&chat, UPSTREAM_MODEL, false).expect("encodes");
    let expected: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/openrouter/chat-options-request.json"
    ))
    .expect("fixture json");
    assert_eq!(serde_json::to_value(payload).expect("serializes"), expected);
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
    };
    let payload = super::request::encode(&chat, UPSTREAM_MODEL, true).expect("encodes");
    assert_eq!(
        serde_json::to_value(payload).expect("serializes"),
        json!({
            "model": UPSTREAM_MODEL,
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}}]},
                {"role": "tool", "content": "sunny", "tool_call_id": "call-1"}
            ],
            "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}],
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
            "stream": true,
            "provider": {"allow_fallbacks": false}
        })
    );
}
