#![allow(dead_code)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    body::{Body, Bytes, to_bytes},
    http::{Request, header},
    response::Response,
};
use kanata::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    auth::{SecretResolutionError, SecretResolver},
    config::{SecretReference, ValidatedConfig, load},
    core::{
        Capabilities, ChatMessage, ChatResponse, ErrorKind, FinishReason, GatewayError, ModelAlias,
        Operation, Request as CoreRequest, Response as CoreResponse, RoutedRequest, Usage,
    },
    server::{Readiness, ServerBuildError, TwoPlaneServer},
};
use serde_json::Value;

pub const CHAT_CONTENT_TYPE: &str = "application/json";
pub const MULTIPART_BOUNDARY: &str = "gateway-boundary";
pub const MULTIPART_CONTENT_TYPE: &str = "multipart/form-data; boundary=\"gateway-boundary\"";
pub type Requests = Arc<Mutex<Vec<RoutedRequest>>>;
pub type ServerWithRequests = (TwoPlaneServer, Requests);

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

pub struct Resolver;

impl SecretResolver for Resolver {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"test-key".to_vec())
    }
}

#[derive(Clone)]
pub enum Outcome {
    Chat {
        model: Option<ModelAlias>,
        message: ChatMessage,
        finish_reason: FinishReason,
        usage: Option<Usage>,
    },
    WrongModel(String),
    WrongResponse,
    InvalidAssistant,
    Transcription,
    Error(ErrorKind),
}

pub struct AdapterSpec {
    pub id: String,
    pub capabilities: Capabilities,
    pub outcome: Outcome,
}

pub fn adapter_spec(
    id: impl Into<String>,
    capabilities: Capabilities,
    outcome: Outcome,
) -> AdapterSpec {
    AdapterSpec {
        id: id.into(),
        capabilities,
        outcome,
    }
}

struct RecordingAdapter {
    id: String,
    capabilities: Capabilities,
    outcome: Outcome,
    requests: Requests,
}

impl Adapter for RecordingAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, request: RoutedRequest) -> AdapterFuture {
        let request_model = request.request().model_alias().clone();
        let result = match self.outcome.clone() {
            Outcome::Chat {
                model,
                message,
                finish_reason,
                usage,
            } => Ok(AdapterOutput::Complete(CoreResponse::Chat(ChatResponse {
                model: model.unwrap_or(request_model),
                message,
                finish_reason,
                usage,
            }))),
            Outcome::WrongModel(model) => {
                Ok(AdapterOutput::Complete(CoreResponse::Chat(ChatResponse {
                    model: ModelAlias(model),
                    message: assistant_text("UPSTREAM_RESPONSE_MARKER"),
                    finish_reason: FinishReason::Stop,
                    usage: None,
                })))
            }
            Outcome::WrongResponse => Ok(AdapterOutput::Complete(CoreResponse::Transcription(
                kanata::core::TranscriptionResponse {
                    text: "UPSTREAM_RESPONSE_MARKER".into(),
                },
            ))),
            Outcome::InvalidAssistant => {
                Ok(AdapterOutput::Complete(CoreResponse::Chat(ChatResponse {
                    model: request_model,
                    message: ChatMessage {
                        role: kanata::core::ChatRole::User,
                        content: vec![kanata::core::ChatContent::Text {
                            text: "UPSTREAM_RESPONSE_MARKER".into(),
                        }],
                    },
                    finish_reason: FinishReason::Stop,
                    usage: None,
                })))
            }
            Outcome::Transcription => Ok(AdapterOutput::Complete(CoreResponse::Transcription(
                kanata::core::TranscriptionResponse {
                    text: "transcribed".into(),
                },
            ))),
            Outcome::Error(kind) => Err(GatewayError { kind }),
        };
        self.requests.lock().expect("request lock").push(request);
        Box::pin(async move { result })
    }
}

pub fn all_capabilities() -> Capabilities {
    Capabilities {
        operations: [Operation::Chat, Operation::Transcription]
            .into_iter()
            .collect(),
        streaming_chat: true,
        function_tools: true,
        ..Capabilities::default()
    }
}

pub fn config() -> ValidatedConfig {
    load("tests/fixtures/config/example.toml").expect("example config")
}

pub fn config_with_public_routes(routes: &[(&str, &str)]) -> ValidatedConfig {
    let mut contents =
        std::fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
    contents = contents.replace(
        "[listeners.admin]",
        "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
    );
    let routes = routes
        .iter()
        .map(|(alias, operation)| {
            format!("{{ model_alias = \"{alias}\", operation = \"{operation}\" }}")
        })
        .collect::<Vec<_>>()
        .join(",\n  ");
    contents = contents.replace(
        "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]",
        &format!(
            "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]\npublic_routes = [{routes}]"
        ),
    );
    let path = std::env::temp_dir().join(format!(
        "kanata-gateway-public-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, contents).expect("fixture writes");
    let result = load(&path);
    std::fs::remove_file(path).expect("fixture removes");
    result.expect("public config validates")
}

pub fn capabilities(config: &ValidatedConfig, id: &str) -> Capabilities {
    config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == id)
        .unwrap_or_else(|| panic!("adapter {id} is configured"))
        .capabilities()
        .clone()
}

pub fn server_with(config: &ValidatedConfig, specs: Vec<AdapterSpec>) -> ServerWithRequests {
    try_server_with(config, specs).expect("server")
}

pub fn try_server_with(
    config: &ValidatedConfig,
    specs: Vec<AdapterSpec>,
) -> Result<ServerWithRequests, ServerBuildError> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let adapters: Vec<Arc<dyn Adapter>> = specs
        .into_iter()
        .map(|spec| {
            Arc::new(RecordingAdapter {
                id: spec.id,
                capabilities: spec.capabilities,
                outcome: spec.outcome,
                requests: requests.clone(),
            }) as Arc<dyn Adapter>
        })
        .collect();
    let server = TwoPlaneServer::from_validated_with_adapters(
        config,
        &Resolver,
        Readiness::new(true),
        adapters,
    )?;
    Ok((server, requests))
}

pub fn server_without_adapters(config: &ValidatedConfig) -> TwoPlaneServer {
    TwoPlaneServer::from_validated(config, &Resolver, Readiness::new(true)).expect("server")
}

pub fn assistant_text(text: &str) -> ChatMessage {
    ChatMessage {
        role: kanata::core::ChatRole::Assistant,
        content: vec![kanata::core::ChatContent::Text { text: text.into() }],
    }
}

pub fn chat_request(body: &str) -> Request<Body> {
    chat_request_with(body, Some(CHAT_CONTENT_TYPE), Some("Bearer test-key"), &[])
}

pub fn chat_request_with(
    body: &str,
    content_type: Option<&str>,
    authorization: Option<&str>,
    request_ids: &[&str],
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions");
    if let Some(authorization) = authorization {
        builder = builder.header(header::AUTHORIZATION, authorization);
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    let mut request = builder.body(Body::from(body.to_owned())).expect("request");
    for request_id in request_ids {
        request
            .headers_mut()
            .append("x-request-id", (*request_id).parse().expect("request id"));
    }
    request
}

pub fn models_request() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header(header::AUTHORIZATION, "Bearer test-key")
        .body(Body::empty())
        .expect("request")
}

pub fn transcription_request(model: &str) -> Request<Body> {
    let mut body = Vec::new();
    append_field(&mut body, "model", model.as_bytes());
    append_file(&mut body, "voice.wav", "audio/wav", b"audio");
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header(header::AUTHORIZATION, "Bearer test-key")
        .header(header::CONTENT_TYPE, MULTIPART_CONTENT_TYPE)
        .body(Body::from(body))
        .expect("request")
}

fn append_field(body: &mut Vec<u8>, name: &str, value: &[u8]) {
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(value);
    body.extend_from_slice(b"\r\n");
}

fn append_file(body: &mut Vec<u8>, filename: &str, content_type: &str, value: &[u8]) {
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(value);
    body.extend_from_slice(b"\r\n");
}

pub async fn response_body(response: Response) -> Bytes {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body")
}

pub async fn response_json(response: Response) -> Value {
    serde_json::from_slice(&response_body(response).await).expect("json response")
}

pub fn recorded_len(requests: &Requests) -> usize {
    requests.lock().expect("request lock").len()
}

pub fn take_request(requests: &Requests) -> RoutedRequest {
    requests
        .lock()
        .expect("request lock")
        .pop()
        .expect("request")
}

pub fn chat_outcome(text: &str) -> Outcome {
    Outcome::Chat {
        model: None,
        message: assistant_text(text),
        finish_reason: FinishReason::Stop,
        usage: Some(Usage {
            input_tokens: 11,
            output_tokens: 7,
            total_tokens: 18,
        }),
    }
}

pub fn request_model(request: &RoutedRequest) -> &ModelAlias {
    request.request().model_alias()
}

pub fn core_chat(request: RoutedRequest) -> kanata::core::ChatRequest {
    let (_, request) = request.into_parts();
    let CoreRequest::Chat(request) = request else {
        panic!("chat request")
    };
    request
}
