use std::{
    convert::Infallible,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    body::{Body, Bytes, to_bytes},
    http::{Request, header},
    response::Response,
};
use futures_util::stream;
use kanata::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    auth::{SecretResolutionError, SecretResolver},
    config::{SecretReference, ValidatedConfig, load},
    core::{
        Capabilities, ChatContent, ChatMessage, ChatResponse, ChatRole, FinishReason, Operation,
        Request as CoreRequest, Response as CoreResponse, RoutedRequest, TranscriptionResponse,
    },
    server::{Readiness, TwoPlaneServer},
};

pub(crate) const EXTENSION_KEY: &str = "io.kanata.trace";
pub(crate) const EXTENSIONS_JSON: &str =
    r#"{"io.kanata.trace":{"enabled":true,"labels":["contract"]}}"#;
pub(crate) const BOUNDARY: &str = "extension-boundary";
pub(crate) const MULTIPART_CONTENT_TYPE: &str =
    "multipart/form-data; boundary=\"extension-boundary\"";

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct Resolver;

impl SecretResolver for Resolver {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"test-key".to_vec())
    }
}

pub(crate) struct RecordingAdapter {
    pub(crate) requests: Arc<Mutex<Vec<RoutedRequest>>>,
    capabilities: Capabilities,
}

impl Adapter for RecordingAdapter {
    fn id(&self) -> &str {
        "vllm-private"
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, request: RoutedRequest) -> AdapterFuture {
        let response = match request.request() {
            CoreRequest::Chat(chat) => CoreResponse::Chat(ChatResponse {
                model: chat.model.clone(),
                message: ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![ChatContent::Text {
                        text: "chat response".into(),
                    }],
                },
                finish_reason: FinishReason::Stop,
                usage: None,
            }),
            CoreRequest::Transcription(_) => CoreResponse::Transcription(TranscriptionResponse {
                text: "transcribed".into(),
            }),
        };
        self.requests.lock().expect("lock").push(request);
        Box::pin(async move { Ok(AdapterOutput::Complete(response)) })
    }
}

pub(crate) fn example_contents() -> String {
    fs::read_to_string("tests/fixtures/config/example.toml").expect("example config")
}

pub(crate) fn config_contents(
    adapter_allowlist: &[&str],
    chat_allowlist: &[&str],
    transcription_allowlist: &[&str],
    max_extension_bytes: usize,
) -> String {
    let contents = example_contents();
    let contents = add_allowlist(contents, "vllm-private", adapter_allowlist);
    let contents = add_allowlist(contents, "vllm-chat", chat_allowlist);
    let contents = add_allowlist(contents, "vllm-transcription", transcription_allowlist);
    contents.replace(
        "max_extension_bytes = 8192",
        &format!("max_extension_bytes = {max_extension_bytes}"),
    )
}

pub(crate) fn load_contents(contents: String) -> Result<ValidatedConfig, String> {
    let path = std::env::temp_dir().join(format!(
        "kanata-extensions-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let result = load(&path).map_err(|error| error.to_string());
    fs::remove_file(path).expect("fixture removes");
    result
}

pub(crate) fn configured(
    adapter_allowlist: &[&str],
    chat_allowlist: &[&str],
    transcription_allowlist: &[&str],
    max_extension_bytes: usize,
) -> ValidatedConfig {
    load_contents(config_contents(
        adapter_allowlist,
        chat_allowlist,
        transcription_allowlist,
        max_extension_bytes,
    ))
    .expect("extension config")
}

fn add_allowlist(contents: String, id: &str, values: &[&str]) -> String {
    let marker = format!("id = \"{id}\"\n");
    let replacement = format!("{marker}extension_allowlist = {}\n", toml_array(values));
    assert!(contents.contains(&marker), "missing config marker {id}");
    contents.replacen(&marker, &replacement, 1)
}

fn toml_array(values: &[&str]) -> String {
    let values = values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{values}]")
}

pub(crate) fn server_from_config(
    config: &ValidatedConfig,
) -> (TwoPlaneServer, Arc<Mutex<Vec<RoutedRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let adapter: Arc<dyn Adapter> = Arc::new(RecordingAdapter {
        requests: requests.clone(),
        capabilities: Capabilities {
            operations: [Operation::Chat, Operation::Transcription]
                .into_iter()
                .collect(),
            streaming_chat: true,
            function_tools: true,
            ..Capabilities::default()
        },
    });
    let server = TwoPlaneServer::from_validated_with_adapters(
        config,
        &Resolver,
        Readiness::new(true),
        vec![adapter],
    )
    .expect("server");
    (server, requests)
}

pub(crate) fn server(
    adapter_allowlist: &[&str],
    chat_allowlist: &[&str],
    transcription_allowlist: &[&str],
    max_extension_bytes: usize,
) -> (TwoPlaneServer, Arc<Mutex<Vec<RoutedRequest>>>) {
    let config = configured(
        adapter_allowlist,
        chat_allowlist,
        transcription_allowlist,
        max_extension_bytes,
    );
    server_from_config(&config)
}

pub(crate) fn chat_request(extensions: &str) -> Request<Body> {
    let body = format!(
        r#"{{"model":"private-chat","messages":[{{"role":"user","content":"hello"}}],"extensions":{extensions}}}"#
    );
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, "Bearer test-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("request")
}

#[derive(Clone, Copy)]
pub(crate) enum Part<'a> {
    Field { name: &'a str, bytes: &'a [u8] },
}

pub(crate) fn multipart_with_file(extras: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    append_field(&mut body, "model", b"private-transcribe");
    append_file(&mut body, "file", "voice.wav", "audio/wav", b"audio");
    for part in extras {
        match part {
            Part::Field { name, bytes } => append_field(&mut body, name, bytes),
        }
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn append_field(body: &mut Vec<u8>, name: &str, bytes: &[u8]) {
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
}

fn append_file(body: &mut Vec<u8>, name: &str, filename: &str, content_type: &str, bytes: &[u8]) {
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
}

pub(crate) fn transcription_request(body: Vec<u8>) -> Request<Body> {
    let chunks = body
        .chunks(5)
        .map(|chunk| Ok::<Bytes, Infallible>(Bytes::copy_from_slice(chunk)))
        .collect::<Vec<_>>();
    Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header(header::AUTHORIZATION, "Bearer test-key")
        .header(header::CONTENT_TYPE, MULTIPART_CONTENT_TYPE)
        .body(Body::from_stream(stream::iter(chunks)))
        .expect("request")
}

pub(crate) async fn response_body(response: Response) -> Bytes {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
}
