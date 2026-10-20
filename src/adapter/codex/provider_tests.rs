use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use futures_util::StreamExt;
use http::Method;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;

use crate::{
    adapter::{Adapter, AdapterFuture, AdapterOutput, EventStream},
    config::{CodexAuthStore, ValidatedConfig},
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, Extensions, FinishReason,
        FunctionTool, GatewayError, ModelAlias, NormalizedEvent, Request as CoreRequest,
        RequestContext, RoutedRequest, ToolCall, ToolChoice, TrustZone, Usage,
    },
    test_support::{RequestRecord, load_config, loopback_listener, read_request, tls_fixture},
};

use super::{CodexAdapter, ExchangeFuture, RefreshExchange};
use crate::adapter::codex::auth::{
    Credential, CredentialStore, MAX_REFRESH_LOCK_WAIT, RefreshCoordinator, RefreshExchangeError,
    RefreshResponse,
};

const HOST: &str = "chatgpt.com";
const MEDIUM_ROUTE: &str = "codex-medium";
const LOW_ROUTE: &str = "codex-low";
const MEDIUM_MODEL: &str = "codex-chat";
const LOW_MODEL: &str = "codex-low:low";
const UPSTREAM_MODEL: &str = "fixture-codex-model";
const ACCESS_TOKEN: &str = "TEST_ONLY_ACCESS_TOKEN_NOT_SECRET_0001";
const REPLACEMENT_TOKEN: &str = "TEST_ONLY_REPLACEMENT_ACCESS_NOT_SECRET_0001";
const ACCOUNT_ID: &str = "TEST_ONLY_ACCOUNT_ID_NOT_SECRET_0001";
const REFRESH_TOKEN: &str = "TEST_ONLY_REFRESH_TOKEN_NOT_SECRET_0001";
const RESPONSE_EVENTS: &str = include_str!("../../../tests/fixtures/codex/responses-events.sse");
const NAMED_EVENTS: &str = include_str!("../../../tests/fixtures/codex/responses-events-named.sse");
const LIVE_SHAPE_EVENTS: &str =
    include_str!("../../../tests/fixtures/codex/responses-events-live-shape.sse");
const RESPONSE_REQUEST: &str = include_str!("../../../tests/fixtures/codex/responses-request.json");

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempState {
    root: PathBuf,
    state: PathBuf,
}

impl TempState {
    fn new() -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        let root = base.join(format!(
            "kanata-codex-provider-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let state = root.join("state");
        for directory in [&root, &state] {
            fs::create_dir(directory).expect("create fixture directory");
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .expect("secure fixture directory");
        }
        Self { root, state }
    }

    fn config(&self) -> ValidatedConfig {
        let state = self.state.to_string_lossy().replace('"', "\\\"");
        load_config(&format!(
            r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080
[listeners.admin]
bind = "127.0.0.1"
port = 18081
[publication]
tailnet_addresses = ["100.64.0.1"]
[codex_auth]
store = "file"
state_dir = "{state}"

[[adapters]]
id = "codex-fixture"
kind = "codex"
base_url = "https://chatgpt.com/backend-api/codex"
trust_zone = "external"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = true
function_tools = true
reasoning_control = true

[[routes]]
id = "{MEDIUM_ROUTE}"
model_alias = "{MEDIUM_MODEL}"
operation = "chat"
adapter_id = "codex-fixture"
upstream_id = "{UPSTREAM_MODEL}"
requires_streaming_chat = true
requires_function_tools = true

[[routes]]
id = "{LOW_ROUTE}"
model_alias = "{LOW_MODEL}"
operation = "chat"
adapter_id = "codex-fixture"
upstream_id = "fixture-low-model"
codex_reasoning_effort = "low"
requires_streaming_chat = true
requires_function_tools = true

[[application_keys]]
id = "fixture-owner"
secret_ref = "env:KANATA_TEST_OWNER_KEY"
owner = true
permissions = [
  {{ model_alias = "{MEDIUM_MODEL}", operation = "chat" }},
  {{ model_alias = "{LOW_MODEL}", operation = "chat" }}
]

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
        ))
    }

    async fn coordinator(&self) -> Arc<RefreshCoordinator> {
        let store = CredentialStore::new(CodexAuthStore::File, &self.state);
        let locked = store
            .lock(Duration::from_secs(1))
            .await
            .expect("lock fixture store");
        locked
            .save(&Credential::new(REFRESH_TOKEN, ACCOUNT_ID).expect("fixture credential"))
            .expect("seed fixture credential");
        drop(locked);

        let coordinator = Arc::new(
            RefreshCoordinator::new(store, Duration::from_secs(60), MAX_REFRESH_LOCK_WAIT)
                .expect("refresh coordinator"),
        );
        let token = coordinator
            .access_token(|credential| async move {
                RefreshResponse::new(
                    ACCESS_TOKEN,
                    SystemTime::now() + Duration::from_secs(3_600),
                    credential.account_id(),
                    None,
                )
                .map_err(|_| RefreshExchangeError)
            })
            .await
            .expect("prime fixture token");
        assert_eq!(token.account_id(), ACCOUNT_ID);
        coordinator
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A Codex adapter pinned to a loopback TLS fixture, with its seeded state directory.
struct Harness {
    config: ValidatedConfig,
    adapter: CodexAdapter,
    _temp: TempState,
}

type Served = JoinHandle<(Vec<RequestRecord>, bool)>;

impl Harness {
    async fn new(hostname: &str, exchange: RefreshExchange) -> (Self, TcpListener, TlsAcceptor) {
        let temp = TempState::new();
        let config = temp.config();
        let coordinator = temp.coordinator().await;
        let (listener, address) = loopback_listener().await;
        let (certificate, acceptor) = tls_fixture(hostname);
        let adapter = CodexAdapter::with_test_root(
            &config,
            "codex-fixture",
            coordinator,
            exchange,
            certificate,
            address,
        )
        .expect("Codex fixture adapter");
        let harness = Self {
            config,
            adapter,
            _temp: temp,
        };
        (harness, listener, acceptor)
    }

    async fn serving(exchange: RefreshExchange, responses: Vec<FixtureResponse>) -> (Self, Served) {
        let (harness, listener, acceptor) = Self::new(HOST, exchange).await;
        let server = tokio::spawn(serve_responses(listener, acceptor, responses));
        (harness, server)
    }

    fn execute(&self, route: &str, request: CoreRequest) -> AdapterFuture {
        self.adapter.execute(routed(&self.config, route, request))
    }

    fn medium(&self, streaming: bool, with_tool: bool) -> AdapterFuture {
        self.execute(MEDIUM_ROUTE, request(MEDIUM_MODEL, streaming, with_tool))
    }
}

/// Awaits the fixture server and asserts the adapter opened no unexpected connection.
async fn finish(server: Served) -> Vec<RequestRecord> {
    let (records, extra) = server.await.expect("fixture server");
    assert!(!extra);
    records
}

struct FixtureResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    fragment: bool,
    /// Declares one byte more than is sent, so the body never ends cleanly.
    unterminated: bool,
}

fn response(status: u16, content_type: &'static str, body: &[u8]) -> FixtureResponse {
    FixtureResponse {
        status,
        content_type,
        body: body.to_vec(),
        fragment: false,
        unterminated: false,
    }
}

fn sse(body: &[u8]) -> FixtureResponse {
    response(200, "text/event-stream", body)
}

fn counting_exchange() -> (Arc<AtomicUsize>, RefreshExchange) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let exchange: RefreshExchange = Arc::new(move |credential| {
        let calls = counter.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            RefreshResponse::new(
                REPLACEMENT_TOKEN,
                SystemTime::now() + Duration::from_secs(3_600),
                credential.account_id(),
                None,
            )
            .map_err(|_| RefreshExchangeError)
        }) as ExchangeFuture
    });
    (calls, exchange)
}

fn no_refresh() -> RefreshExchange {
    Arc::new(|_credential| Box::pin(async { Err(RefreshExchangeError) }) as ExchangeFuture)
}

fn expect_err(result: Result<AdapterOutput, GatewayError>) -> GatewayError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("upstream failure accepted"),
    }
}

fn expect_events(output: AdapterOutput) -> EventStream {
    match output {
        AdapterOutput::Events(events) => events,
        AdapterOutput::Complete(_) => panic!("expected event stream"),
    }
}

fn request(model: &str, streaming: bool, with_tool: bool) -> CoreRequest {
    let tools = if with_tool {
        vec![FunctionTool {
            name: "lookup".into(),
            description: Some("TEST_ONLY_TOOL_DESCRIPTION_NOT_SECRET_0001".into()),
            parameters: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
                "additionalProperties": false
            }),
        }]
    } else {
        Vec::new()
    };
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias(model.into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatContent::Text {
                text: "TEST_ONLY_PROMPT_NOT_SECRET_0001".into(),
            }],
        }],
        tools,
        tool_choice: if with_tool {
            ToolChoice::Function {
                name: "lookup".into(),
            }
        } else {
            ToolChoice::Auto
        },
        stream: streaming,
        extensions: Extensions::default(),
        options: Default::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
    })
}

fn tool_history_request() -> CoreRequest {
    let message = |role, content| ChatMessage {
        role,
        content: vec![content],
    };
    CoreRequest::Chat(ChatRequest {
        model: ModelAlias(MEDIUM_MODEL.into()),
        messages: vec![
            message(
                ChatRole::User,
                ChatContent::Text {
                    text: "lookup".into(),
                },
            ),
            message(
                ChatRole::Assistant,
                ChatContent::ToolCall {
                    call: ToolCall {
                        id: "TEST_ONLY_CALL_ID_NOT_SECRET_0001".into(),
                        name: "lookup".into(),
                        arguments: "{}".into(),
                    },
                },
            ),
            message(
                ChatRole::Tool,
                ChatContent::ToolResult {
                    call_id: "TEST_ONLY_CALL_ID_NOT_SECRET_0001".into(),
                    content: "TEST_ONLY_TOOL_RESULT_NOT_SECRET_0001".into(),
                },
            ),
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: false,
        extensions: Extensions::default(),
        options: Default::default(),
        prompt_cache_key: None,
        include_encrypted_reasoning: false,
    })
}

fn routed(config: &ValidatedConfig, route_id: &str, request: CoreRequest) -> RoutedRequest {
    let route = config
        .routes()
        .iter()
        .find(|route| route.identity().route_id == route_id)
        .expect("configured route")
        .identity()
        .clone();
    RoutedRequest::new(
        RequestContext {
            request_id: "TEST_ONLY_REQUEST_ID_NOT_SECRET_0001".into(),
            route,
            trust_zone: TrustZone::External,
            extensions: Extensions::default(),
        },
        request,
    )
    .expect("routed fixture request")
}

async fn serve_responses(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    responses: Vec<FixtureResponse>,
) -> (Vec<RequestRecord>, bool) {
    let mut records = Vec::new();
    for response in responses {
        let (socket, _) = listener.accept().await.expect("accept fixture request");
        let mut socket = acceptor.accept(socket).await.expect("accept fixture TLS");
        records.push(
            read_request(&mut socket)
                .await
                .expect("read fixture request"),
        );
        let reason = match response.status {
            200 => "OK",
            401 => "Unauthorized",
            429 => "Too Many Requests",
            503 => "Service Unavailable",
            _ => "Fixture Status",
        };
        // An empty fixture content type omits the header entirely.
        let content_type = if response.content_type.is_empty() {
            String::new()
        } else {
            format!("content-type: {}\r\n", response.content_type)
        };
        let head = format!(
            "HTTP/1.1 {} {}\r\n{}content-length: {}\r\nconnection: close\r\n\r\n",
            response.status,
            reason,
            content_type,
            response.body.len() + usize::from(response.unterminated)
        );
        socket
            .write_all(head.as_bytes())
            .await
            .expect("response head");
        if response.fragment {
            for chunk in response.body.chunks(11) {
                socket.write_all(chunk).await.expect("fragment response");
                socket.flush().await.expect("flush fragment");
            }
        } else {
            socket
                .write_all(&response.body)
                .await
                .expect("response body");
        }
    }
    let extra_connection = tokio::time::timeout(Duration::from_millis(100), listener.accept())
        .await
        .is_ok();
    (records, extra_connection)
}

fn assert_codex_headers(record: &RequestRecord, token: &str) {
    assert_eq!(record.method, Method::POST.as_str());
    assert_eq!(record.path, "/backend-api/codex/responses");
    assert_eq!(record.headers["host"], HOST);
    assert_eq!(record.headers["authorization"], format!("Bearer {token}"));
    assert_eq!(record.headers["chatgpt-account-id"], ACCOUNT_ID);
    assert_eq!(record.headers["content-type"], "application/json");
    assert_eq!(record.headers["accept"], "text/event-stream");
    assert!(record.headers["user-agent"].starts_with("kanata/"));
    assert_eq!(record.headers.len(), 9);
}

#[tokio::test]
async fn missing_content_type_is_parsed_as_sse_but_explicit_html_is_rejected() {
    for (content_type, succeeds) in [
        ("", true),
        ("text/html; charset=utf-8", false),
        ("text/x!weird", false),
    ] {
        let (_, exchange) = counting_exchange();
        let (harness, server) = Harness::serving(
            exchange,
            vec![response(200, content_type, RESPONSE_EVENTS.as_bytes())],
        )
        .await;
        let output = harness.medium(false, false).await;
        assert_eq!(output.is_ok(), succeeds, "content type {content_type:?}");
        server.await.expect("fixture server");
    }
}

#[tokio::test]
async fn live_shape_stream_without_content_type_collects_and_streams() {
    for streaming in [false, true] {
        let mut fixture = response(200, "", LIVE_SHAPE_EVENTS.as_bytes());
        fixture.fragment = true;
        let (harness, server) = Harness::serving(no_refresh(), vec![fixture]).await;

        let output = harness
            .medium(streaming, true)
            .await
            .expect("live-shape output");
        let (content, finish_reason, usage) = match output {
            AdapterOutput::Complete(crate::core::Response::Chat(response)) => {
                assert_eq!(response.model, ModelAlias(MEDIUM_MODEL.into()));
                (
                    response.message.content,
                    response.finish_reason,
                    response.usage,
                )
            }
            AdapterOutput::Events(mut events) => {
                let mut text = String::new();
                let mut arguments = String::new();
                let mut completed = None;
                while let Some(event) = events.next().await {
                    match event.expect("normalized live-shape event") {
                        NormalizedEvent::ChatStarted { model } => {
                            assert_eq!(model, ModelAlias(MEDIUM_MODEL.into()));
                        }
                        NormalizedEvent::ChatTextDelta { text: delta } => text.push_str(&delta),
                        NormalizedEvent::ChatReasoningDelta { .. }
                        | NormalizedEvent::ChatReasoningItem { .. } => {}
                        NormalizedEvent::ChatToolCallDelta {
                            arguments_delta, ..
                        } => arguments.push_str(&arguments_delta),
                        NormalizedEvent::ChatCompleted {
                            finish_reason,
                            usage,
                        } => completed = Some((finish_reason, usage)),
                    }
                }
                let (finish_reason, usage) = completed.expect("completed event");
                (
                    vec![
                        ChatContent::Text { text },
                        ChatContent::ToolCall {
                            call: ToolCall {
                                id: "call_TEST_ONLY_CALL_ID_NOT_SECRET_0003".into(),
                                name: "lookup".into(),
                                arguments,
                            },
                        },
                    ],
                    finish_reason,
                    usage,
                )
            }
            AdapterOutput::Complete(_) => panic!("wrong output"),
        };
        assert_eq!(
            finish_reason,
            FinishReason::ToolCalls,
            "streaming={streaming}"
        );
        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: 120,
                output_tokens: 48,
                total_tokens: 168,
                reasoning_tokens: Some(32),
                cached_input_tokens: Some(64),
            })
        );
        assert_eq!(
            content,
            vec![
                ChatContent::Text {
                    text: "TEST_ONLY_LIVE_TEXT_NOT_SECRET_0001".into(),
                },
                ChatContent::ToolCall {
                    call: ToolCall {
                        id: "call_TEST_ONLY_CALL_ID_NOT_SECRET_0003".into(),
                        name: "lookup".into(),
                        arguments: "{\"query\":\"TEST_ONLY_LIVE_QUERY_NOT_SECRET_0001\"}".into(),
                    },
                },
            ]
        );
        assert_eq!(finish(server).await.len(), 1);
    }
}

#[tokio::test]
async fn collection_returns_at_completed_without_waiting_for_body_end() {
    let mut fixture = sse(NAMED_EVENTS.as_bytes());
    fixture.unterminated = true;
    let (harness, server) = Harness::serving(no_refresh(), vec![fixture]).await;

    let output = harness
        .medium(false, true)
        .await
        .expect("completed before truncated body end");
    assert!(matches!(output, AdapterOutput::Complete(_)));
    server.await.expect("fixture server");
}

#[tokio::test]
async fn verified_https_request_uses_exact_codex_wire_and_collects_tool_response() {
    let (calls, exchange) = counting_exchange();
    let (harness, server) = Harness::serving(
        exchange,
        vec![response(
            200,
            "text/event-stream; charset=utf-8",
            RESPONSE_EVENTS.as_bytes(),
        )],
    )
    .await;
    assert!(!format!("{:?}", harness.adapter).contains(ACCESS_TOKEN));
    assert!(!format!("{:?}", harness.adapter).contains(ACCOUNT_ID));

    let output = harness
        .medium(false, true)
        .await
        .expect("Codex fixture output");
    let response = match output {
        AdapterOutput::Complete(crate::core::Response::Chat(response)) => response,
        AdapterOutput::Complete(_) | AdapterOutput::Events(_) => panic!("wrong output"),
    };
    assert_eq!(response.model, ModelAlias(MEDIUM_MODEL.into()));
    assert_eq!(response.message.role, ChatRole::Assistant);
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.message.content,
        vec![
            ChatContent::Text {
                text: "TEST_ONLY_TEXT_NOT_SECRET_0001".into(),
            },
            ChatContent::ToolCall {
                call: ToolCall {
                    id: "TEST_ONLY_CALL_ID_NOT_SECRET_0001".into(),
                    name: "lookup".into(),
                    arguments: "{\"query\":\"TEST_ONLY_QUERY_NOT_SECRET_0001\"}".into(),
                },
            },
        ]
    );
    assert_eq!(
        response.usage,
        Some(Usage {
            input_tokens: 4,
            output_tokens: 3,
            total_tokens: 7,
            reasoning_tokens: None,
            cached_input_tokens: None,
        })
    );

    let records = finish(server).await;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_codex_headers(record, ACCESS_TOKEN);
    let fixture: Value = serde_json::from_str(RESPONSE_REQUEST).expect("request fixture");
    let body: Value = serde_json::from_slice(&record.body).expect("request body");
    assert_eq!(body, fixture["body"]);
    assert_eq!(body["model"], UPSTREAM_MODEL);
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn named_fragmented_sse_stream_relays_text_tools_and_usage_without_execution() {
    let (calls, exchange) = counting_exchange();
    let mut fixture = sse(NAMED_EVENTS.as_bytes());
    fixture.fragment = true;
    let (harness, server) = Harness::serving(exchange, vec![fixture]).await;

    let mut events = expect_events(
        harness
            .medium(true, true)
            .await
            .expect("Codex stream output"),
    );
    let mut observed = Vec::new();
    while let Some(event) = events.next().await {
        observed.push(event.expect("normalized fixture event"));
    }
    assert!(observed.iter().any(|event| matches!(
        event,
        NormalizedEvent::ChatTextDelta { text } if text == "TEST_ONLY_TEXT_NOT_SECRET_0001"
    )));
    assert!(observed.iter().any(|event| matches!(
        event,
        NormalizedEvent::ChatToolCallDelta { call_id, name: Some(name), .. }
            if call_id == "TEST_ONLY_CALL_ID_NOT_SECRET_0001" && name == "lookup"
    )));
    assert!(observed.iter().any(|event| matches!(
        event,
        NormalizedEvent::ChatCompleted {
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                input_tokens: 4,
                output_tokens: 3,
                total_tokens: 7,
                reasoning_tokens: None,
                cached_input_tokens: None,
            })
        }
    )));
    let records = finish(server).await;
    assert_eq!(records.len(), 1);
    assert_codex_headers(&records[0], ACCESS_TOKEN);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_low_effort_route_preserves_alias_upstream_and_effort() {
    let body = RESPONSE_EVENTS.replace(UPSTREAM_MODEL, "fixture-low-model");
    let (harness, server) = Harness::serving(no_refresh(), vec![sse(body.as_bytes())]).await;

    let output = harness
        .execute(LOW_ROUTE, request(LOW_MODEL, false, false))
        .await
        .expect("low-effort response");
    assert!(matches!(output, AdapterOutput::Complete(_)));
    let records = finish(server).await;
    let payload: Value = serde_json::from_slice(&records[0].body).expect("request body");
    assert_eq!(payload["model"], "fixture-low-model");
    assert_eq!(payload["reasoning"]["effort"], "low");
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["store"], false);
}

#[tokio::test]
async fn request_reasoning_effort_must_match_the_route_effort() {
    let body = RESPONSE_EVENTS.replace(UPSTREAM_MODEL, "fixture-low-model");
    let (harness, server) = Harness::serving(no_refresh(), vec![sse(body.as_bytes())]).await;
    let with_options = |configure: fn(&mut crate::core::ChatOptions)| {
        let CoreRequest::Chat(mut chat) = request(LOW_MODEL, false, false) else {
            unreachable!()
        };
        configure(&mut chat.options);
        CoreRequest::Chat(chat)
    };

    for (configure, kind) in [
        (
            (|options: &mut crate::core::ChatOptions| {
                options.reasoning_effort = Some(crate::core::ReasoningEffort::High);
            }) as fn(&mut crate::core::ChatOptions),
            ErrorKind::InvalidRequest,
        ),
        (
            (|options: &mut crate::core::ChatOptions| {
                options.reasoning_effort = Some(crate::core::ReasoningEffort::Xhigh);
            }) as fn(&mut crate::core::ChatOptions),
            ErrorKind::InvalidRequest,
        ),
        (
            |options: &mut crate::core::ChatOptions| options.sampling.seed = Some(1),
            ErrorKind::UnsupportedOperation,
        ),
    ] {
        let error = expect_err(harness.execute(LOW_ROUTE, with_options(configure)).await);
        assert_eq!(error.kind, kind);
    }

    harness
        .execute(
            LOW_ROUTE,
            with_options(|options| {
                options.reasoning_effort = Some(crate::core::ReasoningEffort::Low);
            }),
        )
        .await
        .expect("matching effort response");
    let records = finish(server).await;
    assert_eq!(records.len(), 1);
    let payload: Value = serde_json::from_slice(&records[0].body).expect("request body");
    assert_eq!(payload["reasoning"]["effort"], "low");
}

#[tokio::test]
async fn one_pre_output_401_refreshes_and_replays_exactly_once() {
    let (calls, exchange) = counting_exchange();
    let (harness, server) = Harness::serving(
        exchange,
        vec![
            response(
                401,
                "application/json",
                br#"{"error":"TEST_ONLY_REJECTED"}"#,
            ),
            sse(RESPONSE_EVENTS.as_bytes()),
        ],
    )
    .await;

    let output = harness
        .medium(false, false)
        .await
        .expect("replayed Codex response");
    assert!(matches!(output, AdapterOutput::Complete(_)));
    let records = finish(server).await;
    assert_eq!(records.len(), 2);
    assert_codex_headers(&records[0], ACCESS_TOKEN);
    assert_codex_headers(&records[1], REPLACEMENT_TOKEN);
    assert_eq!(records[0].body, records[1].body);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn upstream_rejections_are_redacted_and_bounded_in_refreshes_and_requests() {
    // (responses, expected kind, redacted detail, upstream requests, refreshes)
    let cases: [(Vec<FixtureResponse>, ErrorKind, &str, usize, usize); 2] = [
        // A second 401 never triggers a third request.
        (
            vec![
                response(401, "application/json", br#"{"error":"TEST_ONLY_FIRST"}"#),
                response(401, "application/json", br#"{"error":"TEST_ONLY_SECOND"}"#),
            ],
            ErrorKind::UpstreamUnavailable,
            "TEST_ONLY_SECOND",
            2,
            1,
        ),
        // Rate limits are static: no refresh and no retry.
        (
            vec![response(
                429,
                "application/json",
                br#"{"error":"TEST_ONLY_UPSTREAM_DETAIL"}"#,
            )],
            ErrorKind::RateLimited,
            "TEST_ONLY_UPSTREAM_DETAIL",
            1,
            0,
        ),
    ];
    for (responses, kind, detail, requests, refreshes) in cases {
        let (calls, exchange) = counting_exchange();
        let (harness, server) = Harness::serving(exchange, responses).await;

        let error = expect_err(harness.medium(false, false).await);
        assert_eq!(error.kind, kind);
        assert!(!format!("{error:?}").contains(detail));
        assert_eq!(finish(server).await.len(), requests);
        assert_eq!(calls.load(Ordering::SeqCst), refreshes);
    }
}

#[tokio::test]
async fn parser_failure_after_output_does_not_refresh_or_replay() {
    let (calls, exchange) = counting_exchange();
    let malformed = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"created_at\":1,\"model\":\"fixture-codex-model\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"visible\"}\n\n",
        "data: {not-json}\n\n"
    );
    let (harness, server) = Harness::serving(exchange, vec![sse(malformed.as_bytes())]).await;

    let mut events = expect_events(
        harness
            .medium(true, false)
            .await
            .expect("stream response headers"),
    );
    let mut saw_error = false;
    while let Some(event) = events.next().await {
        if let Err(error) = event {
            assert_eq!(error.kind, ErrorKind::UpstreamFailure);
            saw_error = true;
            break;
        }
    }
    assert!(saw_error);
    assert_eq!(finish(server).await.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dropping_downstream_stream_aborts_tls_upstream_without_replay() {
    let (calls, exchange) = counting_exchange();
    let (harness, listener, acceptor) = Harness::new(HOST, exchange).await;
    let prefix = RESPONSE_EVENTS
        .split("data: {\"type\":\"response.output_item.added\"")
        .next()
        .expect("text output prefix");
    let (sent_tx, sent_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept fixture request");
        let mut socket = acceptor.accept(socket).await.expect("accept fixture TLS");
        let record = read_request(&mut socket)
            .await
            .expect("read fixture request");
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 4096\r\nconnection: close\r\n\r\n{prefix}"
                )
                .as_bytes(),
            )
            .await
            .expect("write partial response");
        let _ = sent_tx.send(record);
        let mut buffer = [0_u8; 1024];
        let closed = loop {
            match tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buffer)).await {
                Ok(Ok(0)) | Ok(Err(_)) => break true,
                Err(_) => break false,
                Ok(Ok(_)) => {}
            }
        };
        let extra = tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_ok();
        let _ = closed_tx.send((closed, extra));
    });

    let mut events = expect_events(
        harness
            .medium(true, false)
            .await
            .expect("stream response headers"),
    );
    let record = tokio::time::timeout(Duration::from_secs(2), sent_rx)
        .await
        .expect("fixture request timeout")
        .expect("fixture request record");
    assert_codex_headers(&record, ACCESS_TOKEN);
    assert!(matches!(
        events.next().await,
        Some(Ok(NormalizedEvent::ChatStarted { .. }))
    ));
    assert!(matches!(
        events.next().await,
        Some(Ok(NormalizedEvent::ChatTextDelta { .. }))
    ));
    drop(events);

    let (closed, extra) = tokio::time::timeout(Duration::from_secs(2), closed_rx)
        .await
        .expect("upstream close timeout")
        .expect("close signal");
    assert!(closed);
    assert!(!extra);
    server.await.expect("fixture server");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pinned_hostname_rejects_a_trusted_but_mismatched_tls_certificate() {
    let (harness, listener, acceptor) = Harness::new("not-chatgpt.fixture", no_refresh()).await;
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept TLS client");
        acceptor.accept(socket).await.is_err()
    });

    let error = expect_err(harness.medium(false, false).await);
    assert_eq!(error.kind, ErrorKind::UpstreamUnavailable);
    assert!(server.await.expect("TLS server result"));
}

#[tokio::test]
async fn tool_history_is_relayed_but_never_executed_by_the_adapter() {
    let (harness, server) =
        Harness::serving(no_refresh(), vec![sse(RESPONSE_EVENTS.as_bytes())]).await;

    let output = harness
        .execute(MEDIUM_ROUTE, tool_history_request())
        .await
        .expect("tool history response");
    assert!(matches!(output, AdapterOutput::Complete(_)));
    let records = finish(server).await;
    let payload: Value = serde_json::from_slice(&records[0].body).expect("request body");
    assert_eq!(payload["input"][1]["type"], "function_call");
    assert_eq!(payload["input"][2]["type"], "function_call_output");
    assert_eq!(
        payload["input"][2]["output"],
        "TEST_ONLY_TOOL_RESULT_NOT_SECRET_0001"
    );
}

#[derive(Clone, Default)]
struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);

impl LogCapture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("log lock").clone()).expect("log utf8")
    }
}

impl io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn captured_failure(fixture: FixtureResponse, streaming: bool) -> String {
    use tracing::instrument::WithSubscriber;

    let (harness, server) = Harness::serving(no_refresh(), vec![fixture]).await;
    let capture = LogCapture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    async {
        match harness.medium(streaming, false).await {
            Ok(AdapterOutput::Events(mut events)) => while events.next().await.is_some() {},
            Ok(AdapterOutput::Complete(_)) => panic!("failure fixture completed"),
            Err(_) => {}
        }
    }
    .with_subscriber(subscriber)
    .await;
    let _ = server.await;
    capture.text()
}

#[tokio::test]
async fn upstream_error_status_logs_code_without_prompt_token_or_message() {
    let text = captured_failure(
        response(
            502,
            "application/json",
            br#"{"error":{"code":"server_is_overloaded","type":"server_error","message":"TEST_ONLY_UPSTREAM_DETAIL"}}"#,
        ),
        false,
    )
    .await;
    let warn = text
        .lines()
        .find(|line| line.contains("upstream error response"))
        .unwrap_or_else(|| panic!("missing upstream warn: {text}"));
    for field in [
        "WARN",
        "adapter=codex-fixture",
        "provider=\"codex\"",
        "status=502",
        "code=\"server_is_overloaded\"",
        "error_type=\"server_error\"",
    ] {
        assert!(warn.contains(field), "missing {field}: {warn}");
    }
    for secret in [
        "TEST_ONLY_PROMPT_NOT_SECRET_0001",
        ACCESS_TOKEN,
        ACCOUNT_ID,
        "TEST_ONLY_UPSTREAM_DETAIL",
    ] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
}

#[tokio::test]
async fn failed_response_event_logs_event_and_provider_code() {
    let failed = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"created_at\":1,\"model\":\"fixture-codex-model\"}}\n\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"r1\",\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"TEST_ONLY_UPSTREAM_DETAIL\"}}}\n\n"
    );
    let text = captured_failure(sse(failed.as_bytes()), true).await;
    let warn = text
        .lines()
        .find(|line| line.contains("upstream stream failed"))
        .unwrap_or_else(|| panic!("missing stream warn: {text}"));
    assert!(warn.contains("event=\"response.failed\""), "{warn}");
    assert!(warn.contains("code=\"rate_limit_exceeded\""), "{warn}");
    assert!(!text.contains("TEST_ONLY_UPSTREAM_DETAIL"), "{text}");
    assert!(!text.contains("TEST_ONLY_PROMPT_NOT_SECRET_0001"), "{text}");
}
