#![allow(dead_code)]

#[path = "../admission/support.rs"]
mod admission;
use crate::scratch;

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use kanata::{
    adapter::{Adapter, AdapterFuture},
    auth::{SecretResolutionError, SecretResolver},
    config::{SecretReference, ValidatedConfig},
    core::{Capabilities, NormalizedEvent, RoutedRequest},
    server::{Readiness, TwoPlaneServer},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, Semaphore, mpsc, oneshot},
    task::JoinHandle,
};

use admission::{CancellationGuard, ok_chat};
pub use admission::{Event, StreamProbe};

pub struct Resolver;

impl SecretResolver for Resolver {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"test-key".to_vec())
    }
}

pub struct Running {
    pub client_addr: std::net::SocketAddr,
    pub admin_addr: std::net::SocketAddr,
    pub readiness: Readiness,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<()>>,
}

impl Running {
    pub fn signal(&mut self) {
        let _ = self.stop.take().expect("shutdown sender").send(());
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn join(self) -> io::Result<()> {
        self.task.await.expect("server task")
    }
}

pub async fn pending_server(grace: Duration) -> (Running, PendingProbe) {
    let (adapter, probe) = pending_adapter().await;
    (start(adapter, grace).await, probe)
}

pub async fn stream_server(
    grace: Duration,
) -> (Running, StreamProbe, mpsc::UnboundedSender<Event>) {
    let (adapter, probe, sender) = stream_adapter();
    (start(adapter, grace).await, probe, sender)
}

pub async fn start(adapter: Arc<dyn Adapter>, grace: Duration) -> Running {
    let (server, client_listener, admin_listener, readiness) = parts(adapter).await;
    let client_addr = client_listener.local_addr().expect("client address");
    let admin_addr = admin_listener.local_addr().expect("admin address");
    let bound = server
        .with_bound_listeners(client_listener, admin_listener)
        .expect("validated listener pair");
    let (stop, shutdown) = oneshot::channel();
    let task = tokio::spawn(async move {
        bound
            .serve_until(
                async move {
                    let _ = shutdown.await;
                },
                grace,
            )
            .await
    });
    Running {
        client_addr,
        admin_addr,
        readiness,
        stop: Some(stop),
        task,
    }
}

pub async fn parts(
    adapter: Arc<dyn Adapter>,
) -> (TwoPlaneServer, TcpListener, TcpListener, Readiness) {
    let client_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("client listener");
    let admin_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("admin listener");
    let client_addr = client_listener.local_addr().expect("client address");
    let admin_addr = admin_listener.local_addr().expect("admin address");
    let config = config(client_addr.port(), admin_addr.port());
    let readiness = Readiness::new(true);
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &Resolver,
        readiness.clone(),
        vec![adapter],
    )
    .expect("server builds");
    (server, client_listener, admin_listener, readiness)
}

fn config(client_port: u16, admin_port: u16) -> ValidatedConfig {
    let contents = include_str!("../../tests/fixtures/config/example.toml")
        .replace("bind = \"0.0.0.0\"", "bind = \"127.0.0.1\"")
        .replace("port = 8080", &format!("port = {client_port}"))
        .replace("port = 9090", &format!("port = {admin_port}"))
        .replace("max_in_flight = 8", "max_in_flight = 1");
    scratch::load_valid(&contents)
}

pub async fn connected(addr: std::net::SocketAddr) -> TcpStream {
    TcpStream::connect(addr).await.expect("connect")
}

pub async fn send(stream: &mut TcpStream, request: impl AsRef<[u8]>) {
    stream
        .write_all(request.as_ref())
        .await
        .expect("request write");
}

pub async fn response(stream: &mut TcpStream) -> RawResponse {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.expect("response read");
    parse_response(bytes)
}

pub async fn response_prefix(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let count = stream
            .read(&mut buffer)
            .await
            .expect("response prefix read");
        assert!(count > 0, "connection closed before response prefix");
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n")
            && bytes.windows(5).any(|window| window == b"data:")
        {
            return bytes;
        }
    }
}

#[derive(Debug)]
pub struct RawResponse {
    pub status: u16,
    pub bytes: Vec<u8>,
}

impl RawResponse {
    pub fn body_json(&self) -> serde_json::Value {
        let marker = self
            .bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response headers")
            + 4;
        serde_json::from_slice(&self.bytes[marker..]).expect("response json")
    }

    pub fn body_text(&self) -> &str {
        let marker = self
            .bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response headers")
            + 4;
        std::str::from_utf8(&self.bytes[marker..]).expect("response text")
    }
}

fn parse_response(bytes: Vec<u8>) -> RawResponse {
    let line_end = bytes
        .windows(2)
        .position(|window| window == b"\r\n")
        .expect("status line");
    let status = std::str::from_utf8(&bytes[..line_end])
        .expect("status utf8")
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("status number");
    RawResponse { status, bytes }
}

pub fn chat_request() -> String {
    let body =
        r#"{"model":"private-chat","messages":[{"role":"user","content":"hello"}],"stream":false}"#;
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

pub fn draining_request_without_body() -> &'static str {
    "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-key\r\nContent-Type: application/json\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n"
}

pub fn invalid_auth_request_without_body() -> &'static str {
    "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer wrong-key\r\nContent-Type: application/json\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n"
}

pub fn models_request() -> &'static str {
    "GET /v1/models HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-key\r\nConnection: close\r\n\r\n"
}

pub fn admin_request(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
}

pub fn streaming_request() -> String {
    let body =
        r#"{"model":"private-chat","messages":[{"role":"user","content":"hello"}],"stream":true}"#;
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-key\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

#[derive(Clone)]
pub struct PendingProbe {
    inner: Arc<PendingInner>,
}

struct PendingInner {
    gate: Arc<Semaphore>,
    dispatches: AtomicUsize,
    cancellations: Arc<AtomicUsize>,
    started: Notify,
}

pub async fn pending_adapter() -> (Arc<dyn Adapter>, PendingProbe) {
    let probe = PendingProbe {
        inner: Arc::new(PendingInner {
            gate: Arc::new(Semaphore::new(0)),
            dispatches: AtomicUsize::new(0),
            cancellations: Arc::new(AtomicUsize::new(0)),
            started: Notify::new(),
        }),
    };
    let adapter = Arc::new(PendingAdapter {
        probe: probe.clone(),
    });
    (adapter, probe)
}

impl PendingProbe {
    pub async fn wait_for_dispatch(&self, expected: usize) {
        while self.dispatches() < expected {
            self.inner.started.notified().await;
        }
    }

    pub fn dispatches(&self) -> usize {
        self.inner.dispatches.load(Ordering::SeqCst)
    }

    pub fn cancellations(&self) -> usize {
        self.inner.cancellations.load(Ordering::SeqCst)
    }

    pub fn release(&self) {
        self.inner.gate.add_permits(1);
    }
}

struct PendingAdapter {
    probe: PendingProbe,
}

impl Adapter for PendingAdapter {
    fn id(&self) -> &str {
        "vllm-private"
    }

    fn capabilities(&self) -> &Capabilities {
        static CAPABILITIES: std::sync::OnceLock<Capabilities> = std::sync::OnceLock::new();
        CAPABILITIES.get_or_init(capabilities)
    }

    fn execute(&self, request: RoutedRequest) -> AdapterFuture {
        let model = request.request().model_alias().0.clone();
        let probe = self.probe.clone();
        Box::pin(async move {
            probe.inner.dispatches.fetch_add(1, Ordering::SeqCst);
            probe.inner.started.notify_waiters();
            let mut cancellation = CancellationGuard {
                counter: probe.inner.cancellations.clone(),
                completed: false,
            };
            let permit = probe
                .inner
                .gate
                .clone()
                .acquire_owned()
                .await
                .expect("gate");
            permit.forget();
            cancellation.completed = true;
            Ok(ok_chat(model))
        })
    }
}

pub fn stream_adapter() -> (Arc<dyn Adapter>, StreamProbe, mpsc::UnboundedSender<Event>) {
    let (adapter, probe) = admission::stream_adapter("vllm-private", capabilities());
    let sender = probe.prepare();
    (adapter, probe, sender)
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        operations: [kanata::core::Operation::Chat]
            .into_iter()
            .chain([kanata::core::Operation::Transcription])
            .collect(),
        streaming_chat: true,
        function_tools: true,
        ..Capabilities::default()
    }
}

pub fn started() -> Event {
    admission::started("private-chat")
}

pub fn text(value: &str) -> Event {
    Ok(NormalizedEvent::ChatTextDelta { text: value.into() })
}
