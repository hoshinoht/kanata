mod labels;
pub(crate) mod logging;
mod metrics;
pub(crate) mod sanitize;

use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, AtomicU16, Ordering},
};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::{
    body::Body,
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use http::{HeaderMap, StatusCode};
use http_body::{Body as HttpBody, Frame, SizeHint};
use tracing::Instrument;

use crate::core::{ErrorKind, GatewayError, Operation};
use crate::server::ClientState;

use self::labels::{Endpoint, Outcome, Phase, StatusClass, error_labels};
use self::metrics::Metrics;

const NO_EXPLICIT_OUTCOME: u16 = 0;
const UNSET: &str = "-";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Listener {
    Private,
    Public,
}

impl Listener {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}

/// Response extension naming the client-facing error code; never serialized.
#[derive(Clone, Copy)]
pub(crate) struct ErrorCode(pub(crate) &'static str);

/// Request metadata safe to log.
pub(crate) struct RequestMeta {
    listener: Listener,
    method: String,
    path: Option<String>,
    client_ip: Option<IpAddr>,
}

#[derive(Default)]
struct Annotations {
    key: Option<String>,
    model: Option<String>,
    operation: Option<&'static str>,
    stream: bool,
    adapter: Option<String>,
    provider: Option<String>,
    error_code: Option<&'static str>,
    response_code: Option<&'static str>,
    reasoning_effort: Option<&'static str>,
    client_request_id: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Telemetry {
    metrics: Arc<Metrics>,
}

impl Telemetry {
    pub(crate) fn new() -> Self {
        Self {
            metrics: Arc::new(Metrics::new()),
        }
    }

    pub(crate) fn begin(&self, endpoint: Endpoint, meta: RequestMeta) -> CompletionGuard {
        self.metrics.start(endpoint);
        CompletionGuard {
            observer: Some(Observer {
                state: Arc::new(Observation {
                    metrics: self.metrics.clone(),
                    endpoint,
                    started: Instant::now(),
                    status: AtomicU16::new(0),
                    explicit: AtomicU16::new(NO_EXPLICIT_OUTCOME),
                    completed: AtomicBool::new(false),
                    request_id: random_request_id(),
                    meta,
                    annotations: Mutex::new(Annotations::default()),
                }),
            }),
        }
    }

    pub(crate) fn render(&self, live: bool, ready: bool) -> String {
        self.metrics.render(live, ready)
    }
}

#[derive(Clone)]
pub(crate) struct Observer {
    state: Arc<Observation>,
}

struct Observation {
    metrics: Arc<Metrics>,
    endpoint: Endpoint,
    started: Instant,
    status: AtomicU16,
    explicit: AtomicU16,
    completed: AtomicBool,
    request_id: String,
    meta: RequestMeta,
    annotations: Mutex<Annotations>,
}

impl Observer {
    pub(crate) fn record_error(&self, error: GatewayError) {
        let (outcome, phase) = error_labels(error);
        if !self.record(outcome, phase) {
            return;
        }
        let code = error.kind.mapping().code;
        let mut annotations = self.annotations();
        annotations.error_code = Some(code);
        let upstream = matches!(
            error.kind,
            ErrorKind::UpstreamFailure | ErrorKind::UpstreamUnavailable | ErrorKind::Timeout { .. }
        );
        if let (true, Some(adapter)) = (upstream, annotations.adapter.as_deref()) {
            tracing::warn!(
                target: "kanata::upstream",
                request_id = %self.state.request_id,
                adapter,
                provider = annotations.provider.as_deref().unwrap_or(UNSET),
                model = annotations.model.as_deref().unwrap_or(UNSET),
                error = code,
                phase = phase.as_str(),
                "upstream request failed",
            );
        }
    }

    pub(crate) fn record_draining(&self) {
        self.record(Outcome::Draining, Phase::None);
    }

    pub(crate) fn annotate_key(&self, key: &str) {
        self.annotations().key = Some(key.to_owned());
    }

    /// Call only once the alias matched a configured route.
    pub(crate) fn annotate_route(&self, model: &str, operation: Operation, stream: bool) {
        let mut annotations = self.annotations();
        annotations.model = Some(model.to_owned());
        annotations.operation = Some(match operation {
            Operation::Chat => "chat",
            Operation::Transcription => "transcription",
        });
        annotations.stream = stream;
    }

    pub(crate) fn annotate_adapter(&self, adapter: &str, provider: &str) {
        let mut annotations = self.annotations();
        annotations.adapter = Some(adapter.to_owned());
        annotations.provider = Some(provider.to_owned());
    }

    /// `id` must already be validated as a safe caller-supplied `x-request-id`.
    /// Recorded on the private listener only.
    pub(crate) fn annotate_client_request_id(&self, id: &str) {
        if self.state.meta.listener == Listener::Private {
            self.annotations().client_request_id = Some(id.to_owned());
        }
    }

    pub(crate) fn annotate_reasoning_effort(&self, effort: &'static str) {
        self.annotations().reasoning_effort = Some(effort);
    }

    fn annotations(&self) -> std::sync::MutexGuard<'_, Annotations> {
        self.state
            .annotations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, outcome: Outcome, phase: Phase) -> bool {
        let encoded = 1 + (outcome.index() * Phase::COUNT + phase.index()) as u16;
        self.state
            .explicit
            .compare_exchange(
                NO_EXPLICIT_OUTCOME,
                encoded,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn set_status(&self, status: StatusCode, code: Option<ErrorCode>) {
        self.state.status.store(status.as_u16(), Ordering::Release);
        if let Some(ErrorCode(code)) = code {
            self.annotations().response_code = Some(code);
        }
    }

    fn complete(&self, reason: CompletionReason) {
        if self.state.completed.swap(true, Ordering::AcqRel) {
            return;
        }
        let status = self.state.status.load(Ordering::Acquire);
        let status_class = StatusClass::from_status(status);
        let (outcome, phase) = self
            .explicit_labels()
            .unwrap_or_else(|| fallback_labels(status, reason));
        let duration = self.state.started.elapsed();
        self.state
            .metrics
            .finish(self.state.endpoint, outcome, status_class, phase, duration);
        let annotations = self.annotations();
        let key = annotations.key.as_deref().unwrap_or(UNSET);
        let model = annotations.model.as_deref().unwrap_or(UNSET);
        self.state.metrics.finish_keyed(key, model, status_class);
        let meta = &self.state.meta;
        tracing::info!(
            target: "kanata::access",
            request_id = %self.state.request_id,
            client_request_id = annotations.client_request_id.as_deref().unwrap_or(UNSET),
            listener = meta.listener.as_str(),
            method = %meta.method,
            endpoint = self.state.endpoint.as_str(),
            path = meta.path.as_deref().unwrap_or(UNSET),
            status,
            error = annotations
                .response_code
                .or(annotations.error_code)
                .unwrap_or(UNSET),
            outcome = outcome.as_str(),
            duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            key,
            model,
            operation = annotations.operation.unwrap_or(UNSET),
            stream = annotations.stream,
            reasoning_effort = annotations.reasoning_effort.unwrap_or(UNSET),
            adapter = annotations.adapter.as_deref().unwrap_or(UNSET),
            client_ip = %meta.client_ip.map_or_else(|| UNSET.to_owned(), |ip| ip.to_string()),
            "request completed",
        );
        drop(annotations);
        tracing::event!(
            target: "kanata::telemetry",
            tracing::Level::DEBUG,
            endpoint = self.state.endpoint.as_str(),
            outcome = outcome.as_str(),
            status_class = status_class.as_str(),
            phase = phase.as_str(),
            duration_ms = duration.as_secs_f64() * 1_000.0,
        );
    }

    fn explicit_labels(&self) -> Option<(Outcome, Phase)> {
        let encoded = self.state.explicit.load(Ordering::Acquire);
        if encoded == NO_EXPLICIT_OUTCOME {
            return None;
        }
        let value = usize::from(encoded - 1);
        let outcome = match value / Phase::COUNT {
            0 => Outcome::Success,
            1 => Outcome::ClientError,
            2 => Outcome::UpstreamError,
            3 => Outcome::InternalError,
            4 => Outcome::Timeout,
            5 => Outcome::Cancelled,
            6 => Outcome::Draining,
            _ => return None,
        };
        let phase = match value % Phase::COUNT {
            0 => Phase::None,
            1 => Phase::Queue,
            2 => Phase::Connect,
            3 => Phase::Headers,
            4 => Phase::FirstByte,
            5 => Phase::Idle,
            6 => Phase::Overall,
            _ => return None,
        };
        Some((outcome, phase))
    }
}

fn fallback_labels(status: u16, reason: CompletionReason) -> (Outcome, Phase) {
    match reason {
        CompletionReason::BodyError => (Outcome::InternalError, Phase::None),
        CompletionReason::CancelledBeforeHeaders | CompletionReason::BodyDrop
            if status == 0 || StatusClass::from_status(status) == StatusClass::Success =>
        {
            (Outcome::Cancelled, Phase::None)
        }
        _ => match status {
            408 => (Outcome::Cancelled, Phase::None),
            504 => (Outcome::Timeout, Phase::None),
            500 => (Outcome::InternalError, Phase::None),
            400..=499 => (Outcome::ClientError, Phase::None),
            502..=599 => (Outcome::UpstreamError, Phase::None),
            200..=399 => (Outcome::Success, Phase::None),
            _ => (Outcome::InternalError, Phase::None),
        },
    }
}

enum CompletionReason {
    CancelledBeforeHeaders,
    BodyEnd,
    BodyError,
    BodyDrop,
}

pub(crate) struct CompletionGuard {
    observer: Option<Observer>,
}

impl CompletionGuard {
    fn observer(&self) -> Observer {
        self.observer
            .as_ref()
            .cloned()
            .expect("telemetry guard observer")
    }

    fn wrap_response(mut self, response: Response) -> Response {
        let observer = self.observer.take().expect("telemetry guard observer");
        let (parts, body) = response.into_parts();
        observer.set_status(parts.status, parts.extensions.get::<ErrorCode>().copied());
        Response::from_parts(
            parts,
            Body::new(TelemetryBody {
                inner: Box::pin(body),
                observer: Some(observer),
            }),
        )
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if let Some(observer) = self.observer.as_ref() {
            observer.complete(CompletionReason::CancelledBeforeHeaders);
        }
    }
}

struct TelemetryBody {
    inner: Pin<Box<Body>>,
    observer: Option<Observer>,
}

impl HttpBody for TelemetryBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if this.inner.is_end_stream()
                    && let Some(observer) = this.observer.as_ref()
                {
                    observer.complete(CompletionReason::BodyEnd);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(observer) = this.observer.as_ref() {
                    observer.complete(CompletionReason::BodyError);
                }
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                if let Some(observer) = this.observer.as_ref() {
                    observer.complete(CompletionReason::BodyEnd);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        let end_stream = self.inner.is_end_stream();
        if end_stream && let Some(observer) = self.observer.as_ref() {
            observer.complete(CompletionReason::BodyEnd);
        }
        end_stream
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for TelemetryBody {
    fn drop(&mut self) {
        if let Some(observer) = self.observer.as_ref() {
            observer.complete(CompletionReason::BodyDrop);
        }
    }
}

pub(crate) async fn middleware(
    State(state): State<ClientState>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let endpoint = Endpoint::from_path(path);
    let listener = state.listener();
    let meta = RequestMeta {
        listener,
        method: sanitize::path(request.method().as_str()),
        path: (endpoint == Endpoint::Other).then(|| sanitize::path(path)),
        client_ip: match listener {
            Listener::Public => connecting_ip(request.headers()),
            Listener::Private => None,
        },
    };
    let guard = state.telemetry().begin(endpoint, meta);
    let observer = guard.observer();
    let span = tracing::info_span!("req", id = %observer.state.request_id);
    request.extensions_mut().insert(observer);
    guard.wrap_response(next.run(request).instrument(span).await)
}

/// Client address as asserted by the Cloudflare edge; only trusted on the public listener.
fn connecting_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let mut values = headers.get_all("cf-connecting-ip").iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()?.trim().parse().ok()
}

fn random_request_id() -> String {
    let mut bytes = [0u8; 6];
    if getrandom::fill(&mut bytes).is_err() {
        return UNSET.to_owned();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn observe_error(observer: Option<&Observer>, error: GatewayError) {
    if let Some(observer) = observer {
        observer.record_error(error);
    }
}

pub(crate) fn observe_draining(observer: Option<&Observer>) {
    if let Some(observer) = observer {
        observer.record_draining();
    }
}
