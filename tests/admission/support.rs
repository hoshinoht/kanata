#![allow(dead_code)]

use std::{
    collections::VecDeque,
    fs,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use axum::{
    body::{Body, to_bytes},
    http::Request,
    response::Response,
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
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore, mpsc};

pub struct Resolver;

impl SecretResolver for Resolver {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"test-key".to_vec())
    }
}

pub fn config(max_queue: u64, max_in_flight: u64, queue_ms: u64) -> ValidatedConfig {
    let mut contents = include_str!("../../tests/fixtures/config/example.toml").to_owned();
    contents = contents.replace("max_queue = 32", &format!("max_queue = {max_queue}"));
    contents = contents.replace(
        "max_in_flight = 8",
        &format!("max_in_flight = {max_in_flight}"),
    );
    contents = contents.replace("queue_ms = 1000", &format!("queue_ms = {queue_ms}"));
    let path = std::env::temp_dir().join(format!(
        "kanata-reliability-admission-{}-{}.toml",
        std::process::id(),
        CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("reliability config writes");
    let config = load(&path).expect("reliability config validates");
    let _ = fs::remove_file(path);
    config
}

static CONFIG_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

pub fn capabilities(config: &ValidatedConfig, id: &str) -> Capabilities {
    config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == id)
        .unwrap_or_else(|| panic!("configured adapter {id}"))
        .capabilities()
        .clone()
}

pub fn server(config: &ValidatedConfig, adapters: Vec<Arc<dyn Adapter>>) -> Arc<TwoPlaneServer> {
    Arc::new(
        TwoPlaneServer::from_validated_with_adapters(
            config,
            &Resolver,
            Readiness::new(true),
            adapters,
        )
        .expect("reliability server"),
    )
}

pub fn chat_request(model: &str) -> Request<Body> {
    chat_request_with_options(model, false, false, None)
}

pub fn chat_request_with_id(model: &str, request_id: &str) -> Request<Body> {
    chat_request_with_options(model, false, false, Some(request_id))
}

pub fn streaming_chat_request(model: &str, include_usage: bool) -> Request<Body> {
    chat_request_with_options(model, true, include_usage, None)
}

fn chat_request_with_options(
    model: &str,
    stream: bool,
    include_usage: bool,
    request_id: Option<&str>,
) -> Request<Body> {
    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "hello"}],
        "stream": stream,
    });
    if include_usage {
        body["stream_options"] = json!({"include_usage": true});
    }
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json");
    if let Some(request_id) = request_id {
        request = request.header("x-request-id", request_id);
    }
    request
        .body(Body::from(body.to_string()))
        .expect("chat request")
}

pub async fn response_json(response: Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body"),
    )
    .expect("response JSON")
}

pub fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = futures_util::task::noop_waker_ref();
    let mut context = Context::from_waker(waker);
    future.poll(&mut context)
}

#[derive(Clone)]
pub struct PendingProbe {
    inner: Arc<PendingInner>,
}

struct PendingInner {
    gate: Arc<Semaphore>,
    dispatches: Arc<AtomicUsize>,
    cancellations: Arc<AtomicUsize>,
    started: Notify,
    request_ids: Mutex<Vec<String>>,
}

pub fn pending_adapter(
    id: impl Into<String>,
    capabilities: Capabilities,
) -> (Arc<dyn Adapter>, PendingProbe) {
    let probe = PendingProbe {
        inner: Arc::new(PendingInner {
            gate: Arc::new(Semaphore::new(0)),
            dispatches: Arc::new(AtomicUsize::new(0)),
            cancellations: Arc::new(AtomicUsize::new(0)),
            started: Notify::new(),
            request_ids: Mutex::new(Vec::new()),
        }),
    };
    (
        Arc::new(PendingAdapter {
            id: id.into(),
            capabilities,
            probe: probe.clone(),
        }),
        probe,
    )
}

impl PendingProbe {
    pub fn dispatches(&self) -> usize {
        self.inner.dispatches.load(Ordering::SeqCst)
    }

    pub fn cancellations(&self) -> usize {
        self.inner.cancellations.load(Ordering::SeqCst)
    }

    pub fn request_ids(&self) -> Vec<String> {
        self.inner
            .request_ids
            .lock()
            .expect("request IDs lock")
            .clone()
    }

    pub fn release(&self) {
        self.inner.gate.add_permits(1);
    }

    pub async fn wait_for_dispatch(&self, expected: usize) {
        while self.dispatches() < expected {
            self.inner.started.notified().await;
        }
    }
}

struct PendingAdapter {
    id: String,
    capabilities: Capabilities,
    probe: PendingProbe,
}

impl Adapter for PendingAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, request: RoutedRequest) -> AdapterFuture {
        let model = request.request().model_alias().0.clone();
        let request_id = request.context().request_id.clone();
        self.probe
            .inner
            .request_ids
            .lock()
            .expect("request IDs lock")
            .push(request_id);
        self.probe.inner.dispatches.fetch_add(1, Ordering::SeqCst);
        self.probe.inner.started.notify_waiters();
        let gate = self.probe.inner.gate.clone();
        let cancellations = self.probe.inner.cancellations.clone();
        Box::pin(async move {
            let mut cancellation = CancellationGuard {
                counter: cancellations,
                completed: false,
            };
            let permit = gate.acquire_owned().await.expect("pending gate open");
            permit.forget();
            cancellation.completed = true;
            Ok(AdapterOutput::Complete(CoreResponse::Chat(ChatResponse {
                model: ModelAlias(model),
                message: ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![ChatContent::Text { text: "ok".into() }],
                },
                finish_reason: FinishReason::Stop,
                usage: None,
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

pub type Event = Result<NormalizedEvent, GatewayError>;

#[derive(Clone)]
pub struct StreamProbe {
    inner: Arc<StreamInner>,
}

struct StreamInner {
    streams: Mutex<VecDeque<mpsc::UnboundedReceiver<Event>>>,
    dispatches: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

pub fn stream_adapter(
    id: impl Into<String>,
    capabilities: Capabilities,
) -> (Arc<dyn Adapter>, StreamProbe) {
    let probe = StreamProbe {
        inner: Arc::new(StreamInner {
            streams: Mutex::new(VecDeque::new()),
            dispatches: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
        }),
    };
    (
        Arc::new(StreamAdapter {
            id: id.into(),
            capabilities,
            probe: probe.clone(),
        }),
        probe,
    )
}

impl StreamProbe {
    pub fn prepare(&self) -> mpsc::UnboundedSender<Event> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.inner
            .streams
            .lock()
            .expect("stream plans lock")
            .push_back(receiver);
        sender
    }

    pub fn dispatches(&self) -> usize {
        self.inner.dispatches.load(Ordering::SeqCst)
    }

    pub fn drops(&self) -> usize {
        self.inner.drops.load(Ordering::SeqCst)
    }
}

struct StreamAdapter {
    id: String,
    capabilities: Capabilities,
    probe: StreamProbe,
}

impl Adapter for StreamAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, _: RoutedRequest) -> AdapterFuture {
        let receiver = self
            .probe
            .inner
            .streams
            .lock()
            .expect("stream plans lock")
            .pop_front()
            .expect("prepared stream");
        self.probe.inner.dispatches.fetch_add(1, Ordering::SeqCst);
        let drops = self.probe.inner.drops.clone();
        Box::pin(async move {
            Ok(AdapterOutput::Events(Box::pin(ChannelStream {
                receiver,
                drops,
            })))
        })
    }
}

struct ChannelStream {
    receiver: mpsc::UnboundedReceiver<Event>,
    drops: Arc<AtomicUsize>,
}

impl Stream for ChannelStream {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(context)
    }
}

impl Drop for ChannelStream {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn started(model: &str) -> Event {
    Ok(NormalizedEvent::ChatStarted {
        model: ModelAlias(model.into()),
    })
}

pub fn completed() -> Event {
    Ok(NormalizedEvent::ChatCompleted {
        finish_reason: FinishReason::Stop,
        usage: Some(kanata::core::Usage {
            input_tokens: 1,
            output_tokens: 1,
            total_tokens: 2,
        }),
    })
}
