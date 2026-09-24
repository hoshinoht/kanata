use std::{
    convert::Infallible,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
};

use axum::{
    body::{Body, Bytes, to_bytes},
    http::{Request, header},
};
use futures_util::stream;
use kanata::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    auth::{SecretResolutionError, SecretResolver},
    config::{SecretReference, ValidatedConfig, load},
    core::{Capabilities, Operation, Response, RoutedRequest, TranscriptionResponse},
    server::{Readiness, TwoPlaneServer},
};

pub(crate) const BOUNDARY: &str = "contract-boundary";
pub(crate) const MULTIPART_CONTENT_TYPE: &str =
    "multipart/form-data; boundary=\"contract-boundary\"";
pub(crate) const MODEL: &[u8] = b"private-transcribe";
pub(crate) const AUDIO: &[u8] = &[0xff, 0x00, b'\r', b'\n'];
pub(crate) const MODEL_MARKER: &str = "MODEL_MARKER";
pub(crate) const FILE_MARKER: &str = "FILE_MARKER";
pub(crate) const LANGUAGE_MARKER: &str = "LANGUAGE_MARKER";
pub(crate) const PROMPT_MARKER: &str = "PROMPT_MARKER";
pub(crate) const FORMAT_MARKER: &str = "FORMAT_MARKER";
pub(crate) const UNKNOWN_FIELD_MARKER: &str = "UNKNOWN_FIELD_MARKER";
pub(crate) const HEADER_MARKER: &str = "HEADER_MARKER";
pub(crate) const REDACTION_MARKERS: &[&str] = &[
    "MODEL_MARKER",
    "FILE_MARKER",
    "LANGUAGE_MARKER",
    "PROMPT_MARKER",
    "FORMAT_MARKER",
    "UNKNOWN_FIELD_MARKER",
    "HEADER_MARKER",
    "FILENAME_MARKER",
    "UNKNOWN_ROUTE_MARKER",
    "EMPTY_FILE_MARKER",
    "TRUNCATED_MARKER",
];

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"test-key".to_vec())
    }
}

pub(crate) struct RecordingAdapter {
    pub(crate) requests: Arc<Mutex<Vec<RoutedRequest>>>,
    pub(crate) capabilities: Capabilities,
}
impl Adapter for RecordingAdapter {
    fn id(&self) -> &str {
        "vllm-private"
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    fn execute(&self, request: RoutedRequest) -> AdapterFuture {
        self.requests.lock().expect("lock").push(request);
        Box::pin(async {
            Ok(AdapterOutput::Complete(Response::Transcription(
                TranscriptionResponse {
                    text: "transcribed".into(),
                },
            )))
        })
    }
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

pub(crate) fn server() -> (TwoPlaneServer, Arc<Mutex<Vec<RoutedRequest>>>) {
    let config = load("tests/fixtures/config/example.toml").expect("config");
    server_from_config(&config)
}

#[derive(Clone, Copy)]
pub(crate) enum Part<'a> {
    Field {
        name: &'a str,
        bytes: &'a [u8],
    },
    File {
        name: &'a str,
        filename: Option<&'a str>,
        content_type: Option<&'a str>,
        bytes: &'a [u8],
    },
}

pub(crate) fn multipart_body(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part {
            Part::Field { name, bytes } => {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                );
                body.extend_from_slice(bytes);
            }
            Part::File {
                name,
                filename,
                content_type,
                bytes,
            } => {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"").as_bytes(),
                );
                if let Some(filename) = filename {
                    body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
                }
                body.extend_from_slice(b"\r\n");
                if let Some(content_type) = content_type {
                    body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
                }
                body.extend_from_slice(b"\r\n");
                body.extend_from_slice(bytes);
            }
        }
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

pub(crate) fn multipart_with_file<'a>(
    model: &'a [u8],
    filename: Option<&'a str>,
    content_type: Option<&'a str>,
    audio: &'a [u8],
    extras: &[Part<'a>],
) -> Vec<u8> {
    let mut parts = Vec::with_capacity(extras.len() + 2);
    parts.push(Part::Field {
        name: "model",
        bytes: model,
    });
    parts.push(Part::File {
        name: "file",
        filename,
        content_type,
        bytes: audio,
    });
    parts.extend_from_slice(extras);
    multipart_body(&parts)
}

pub(crate) fn transcription_request(
    body: Vec<u8>,
    authorization: Option<&str>,
    content_type: Option<&str>,
    chunk_size: usize,
) -> Request<Body> {
    let chunks = body
        .chunks(chunk_size)
        .map(|chunk| Ok::<Bytes, Infallible>(Bytes::copy_from_slice(chunk)))
        .collect::<Vec<_>>();
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions");
    if let Some(authorization) = authorization {
        builder = builder.header(header::AUTHORIZATION, authorization);
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    builder
        .body(Body::from_stream(stream::iter(chunks)))
        .expect("request")
}

pub(crate) fn config_with_limits(max_body: usize, max_audio: usize) -> ValidatedConfig {
    let contents = fs::read_to_string("tests/fixtures/config/example.toml")
        .expect("example config")
        .replace(
            "max_body_bytes = 1048576",
            &format!("max_body_bytes = {max_body}"),
        )
        .replace(
            "max_audio_bytes = 26214400",
            &format!("max_audio_bytes = {max_audio}"),
        );
    let path = std::env::temp_dir().join(format!(
        "kanata-transcription-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let result = load(&path);
    fs::remove_file(path).expect("fixture removes");
    result.expect("bounded config")
}

pub(crate) fn public_config(public_routes: &str) -> ValidatedConfig {
    let contents = fs::read_to_string("tests/fixtures/config/example.toml")
        .expect("example config")
        .replace(
            "[listeners.admin]",
            "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
        )
        .replace(
            "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]",
            &format!(
                "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]\npublic_routes = {public_routes}"
            ),
        );
    let path = std::env::temp_dir().join(format!(
        "kanata-transcription-public-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let result = load(&path);
    fs::remove_file(path).expect("fixture removes");
    result.expect("public config")
}

pub(crate) async fn response_body(response: axum::response::Response) -> Bytes {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
}

pub(crate) fn unpolled_request(
    polled: Arc<AtomicBool>,
    authorization: Option<&str>,
) -> Request<Body> {
    let observed = polled.clone();
    let body = Body::from_stream(stream::poll_fn(move |_| {
        observed.store(true, Ordering::SeqCst);
        Poll::Ready(None::<Result<Bytes, Infallible>>)
    }));
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header(header::CONTENT_TYPE, MULTIPART_CONTENT_TYPE);
    if let Some(authorization) = authorization {
        builder = builder.header(header::AUTHORIZATION, authorization);
    }
    builder.body(body).expect("request")
}
