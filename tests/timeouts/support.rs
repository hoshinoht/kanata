use std::{
    collections::VecDeque,
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    body::{Body, Bytes},
    http::Request,
};
use futures_core::Stream;
use kanata::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    auth::{SecretResolutionError, SecretResolver},
    config::{SecretReference, ValidatedConfig, load},
    core::{
        Capabilities, ChatContent, ChatMessage, ChatResponse, ChatRole, FinishReason, GatewayError,
        ModelAlias, NormalizedEvent, Response as CoreResponse, RoutedRequest,
    },
    server::{Readiness, TwoPlaneServer},
};
use tokio::sync::{Notify, Semaphore, mpsc};

pub fn config(queue_ms: u64, first_byte_ms: u64, idle_ms: u64, overall_ms: u64) -> ValidatedConfig {
    config_with_limits(32, 8, queue_ms, first_byte_ms, idle_ms, overall_ms)
}

pub fn config_with_limits(
    max_queue: u64,
    max_in_flight: u64,
    queue_ms: u64,
    first_byte_ms: u64,
    idle_ms: u64,
    overall_ms: u64,
) -> ValidatedConfig {
    let contents = include_str!("../../tests/fixtures/config/example.toml")
        .replace("max_queue = 32", &format!("max_queue = {max_queue}"))
        .replace(
            "max_in_flight = 8",
            &format!("max_in_flight = {max_in_flight}"),
        )
        .replace("queue_ms = 1000", &format!("queue_ms = {queue_ms}"))
        .replace("connect_ms = 5000", &format!("connect_ms = {overall_ms}"))
        .replace("headers_ms = 10000", &format!("headers_ms = {overall_ms}"))
        .replace(
            "first_byte_ms = 15000",
            &format!("first_byte_ms = {first_byte_ms}"),
        )
        .replace("idle_ms = 30000", &format!("idle_ms = {idle_ms}"))
        .replace("overall_ms = 60000", &format!("overall_ms = {overall_ms}"));
    let path = std::env::temp_dir().join(format!(
        "kanata-timeouts-{}-{}.toml",
        std::process::id(),
        CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, contents).expect("config write");
    let config = load(&path).expect("config");
    let _ = std::fs::remove_file(path);
    config
}

static CONFIG_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"test-key".to_vec())
    }
}

#[derive(Clone)]
pub struct Probe {
    pub drops: Arc<AtomicUsize>,
    dispatches: Arc<AtomicUsize>,
    started: Arc<Notify>,
    gate: Arc<Semaphore>,
}

impl Probe {
    pub fn dispatches(&self) -> usize {
        self.dispatches.load(Ordering::SeqCst)
    }

    pub async fn wait_for_dispatch(&self, expected: usize) {
        while self.dispatches() < expected {
            self.started.notified().await;
        }
    }

    pub fn release(&self) {
        self.gate.add_permits(1);
    }
}

pub type Event = Result<NormalizedEvent, GatewayError>;

pub enum Mode {
    Pending,
    Events(mpsc::UnboundedReceiver<Event>),
    BoundedEvents(mpsc::Receiver<Event>),
    DelayedEvents {
        delay_ms: u64,
        receiver: mpsc::Receiver<Event>,
    },
}

pub fn bounded_events(capacity: usize) -> (mpsc::Sender<Event>, Mode) {
    assert!(capacity > 0);
    let (sender, receiver) = mpsc::channel(capacity);
    (sender, Mode::BoundedEvents(receiver))
}

pub fn delayed_events(capacity: usize, delay_ms: u64) -> (mpsc::Sender<Event>, Mode) {
    assert!(capacity > 0);
    let (sender, receiver) = mpsc::channel(capacity);
    (sender, Mode::DelayedEvents { delay_ms, receiver })
}

pub struct TestAdapter {
    capabilities: Capabilities,
    modes: std::sync::Mutex<VecDeque<Mode>>,
    probe: Probe,
}

impl Adapter for TestAdapter {
    fn id(&self) -> &str {
        "vllm-private"
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    fn execute(&self, _: RoutedRequest) -> AdapterFuture {
        self.probe.dispatches.fetch_add(1, Ordering::SeqCst);
        self.probe.started.notify_one();
        let mode = {
            let mut modes = self.modes.lock().expect("modes");
            if matches!(modes.front(), Some(Mode::Pending)) {
                Mode::Pending
            } else {
                modes.pop_front().expect("request mode")
            }
        };
        match mode {
            Mode::Pending => {
                let probe = self.probe.clone();
                Box::pin(async move {
                    let mut cancellation = CancellationGuard {
                        counter: probe.drops.clone(),
                        completed: false,
                    };
                    let permit = probe
                        .gate
                        .clone()
                        .acquire_owned()
                        .await
                        .expect("pending gate open");
                    permit.forget();
                    cancellation.completed = true;
                    Ok(AdapterOutput::Complete(CoreResponse::Chat(ChatResponse {
                        model: ModelAlias("private-chat".into()),
                        message: ChatMessage {
                            role: ChatRole::Assistant,
                            content: vec![ChatContent::Text { text: "ok".into() }],
                        },
                        finish_reason: FinishReason::Stop,
                        usage: None,
                    })))
                })
            }
            Mode::Events(receiver) => self.events(EventReceiver::Unbounded(receiver)),
            Mode::BoundedEvents(receiver) => self.events(EventReceiver::Bounded(receiver)),
            Mode::DelayedEvents { delay_ms, receiver } => {
                let drops = self.probe.drops.clone();
                Box::pin(async move {
                    let mut cancellation = CancellationGuard {
                        counter: drops.clone(),
                        completed: false,
                    };
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    cancellation.completed = true;
                    Ok(AdapterOutput::Events(Box::pin(Receiver {
                        receiver: EventReceiver::Bounded(receiver),
                        drops,
                    })))
                })
            }
        }
    }
}

impl TestAdapter {
    fn events(&self, receiver: EventReceiver) -> AdapterFuture {
        let drops = self.probe.drops.clone();
        Box::pin(async move {
            Ok(AdapterOutput::Events(Box::pin(Receiver {
                receiver,
                drops,
            })))
        })
    }
}

struct CancellationGuard {
    counter: Arc<AtomicUsize>,
    completed: bool,
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

enum EventReceiver {
    Unbounded(mpsc::UnboundedReceiver<Event>),
    Bounded(mpsc::Receiver<Event>),
}

struct Receiver {
    receiver: EventReceiver,
    drops: Arc<AtomicUsize>,
}

impl Stream for Receiver {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.receiver {
            EventReceiver::Unbounded(receiver) => receiver.poll_recv(cx),
            EventReceiver::Bounded(receiver) => receiver.poll_recv(cx),
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn server(config: &ValidatedConfig, mode: Mode) -> (TwoPlaneServer, Probe) {
    server_with_modes(config, vec![mode])
}

pub fn server_with_modes(config: &ValidatedConfig, modes: Vec<Mode>) -> (TwoPlaneServer, Probe) {
    let capabilities = config
        .adapters()
        .iter()
        .find(|a| a.id() == "vllm-private")
        .expect("adapter")
        .capabilities()
        .clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let probe = Probe {
        drops: drops.clone(),
        dispatches: Arc::new(AtomicUsize::new(0)),
        started: Arc::new(Notify::new()),
        gate: Arc::new(Semaphore::new(0)),
    };
    let adapter = Arc::new(TestAdapter {
        capabilities,
        modes: std::sync::Mutex::new(modes.into()),
        probe: probe.clone(),
    });
    (
        TwoPlaneServer::from_validated_with_adapters(
            config,
            &Resolver,
            Readiness::new(true),
            vec![adapter],
        )
        .expect("server"),
        probe,
    )
}

pub fn request(stream: bool) -> Request<Body> {
    request_with_options(stream, false)
}

pub fn request_with_options(stream: bool, include_usage: bool) -> Request<Body> {
    let stream_options = include_usage.then_some(r#","stream_options":{"include_usage":true}"#);
    let body = format!(
        r#"{{"model":"private-chat","messages":[{{"role":"user","content":"hi"}}],"stream":{stream}{}}}"#,
        stream_options.unwrap_or_default()
    );
    request_with_body(Body::from(body))
}

pub fn trickling_request() -> (Request<Body>, mpsc::Sender<Bytes>, BodyProbe) {
    let (sender, receiver) = mpsc::channel(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let body = Body::from_stream(RequestBody {
        receiver,
        drops: drops.clone(),
    });
    (request_with_body(body), sender, BodyProbe { drops })
}

pub fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = futures_util::task::noop_waker_ref();
    let mut context = Context::from_waker(waker);
    future.poll(&mut context)
}

pub struct BodyProbe {
    drops: Arc<AtomicUsize>,
}

impl BodyProbe {
    pub fn drops(&self) -> usize {
        self.drops.load(Ordering::SeqCst)
    }
}

struct RequestBody {
    receiver: mpsc::Receiver<Bytes>,
    drops: Arc<AtomicUsize>,
}

impl Stream for RequestBody {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx).map(|value| value.map(Ok))
    }
}

impl Drop for RequestBody {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn request_with_body(body: Body) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(body)
        .expect("request")
}
