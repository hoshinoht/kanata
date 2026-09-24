use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};

use axum::{
    body::{Body, Bytes, to_bytes},
    http::Request,
    response::Response,
};
use futures_core::Stream;
use kanata::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    core::{Capabilities, GatewayError, NormalizedEvent, RoutedRequest},
    server::{Readiness, TwoPlaneServer},
};

pub type Event = Result<NormalizedEvent, GatewayError>;

#[derive(Clone)]
struct Plan {
    events: Vec<Event>,
    pending_first: bool,
    pending_after_first: bool,
}

#[derive(Clone)]
pub struct Probe {
    dispatches: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Probe {
    pub fn dispatches(&self) -> usize {
        self.dispatches.load(Ordering::SeqCst)
    }

    pub fn polls(&self) -> usize {
        self.polls.load(Ordering::SeqCst)
    }

    pub fn drops(&self) -> usize {
        self.drops.load(Ordering::SeqCst)
    }
}

struct SyntheticAdapter {
    id: String,
    capabilities: Capabilities,
    plan: Plan,
    probe: Probe,
}

impl Adapter for SyntheticAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, _: RoutedRequest) -> AdapterFuture {
        self.probe.dispatches.fetch_add(1, Ordering::SeqCst);
        let plan = self.plan.clone();
        let probe = self.probe.clone();
        Box::pin(async move {
            Ok(AdapterOutput::Events(Box::pin(PlannedStream {
                events: plan.events.into(),
                pending_first: plan.pending_first,
                pending_after_first: plan.pending_after_first,
                first_returned: false,
                polls: probe.polls,
                drops: probe.drops,
            })))
        })
    }
}

struct PlannedStream {
    events: VecDeque<Event>,
    pending_first: bool,
    pending_after_first: bool,
    first_returned: bool,
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Stream for PlannedStream {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let stream = self.get_mut();
        stream.polls.fetch_add(1, Ordering::SeqCst);
        if stream.pending_first || (stream.pending_after_first && stream.first_returned) {
            return Poll::Pending;
        }
        stream.first_returned = true;
        Poll::Ready(stream.events.pop_front())
    }
}

impl Drop for PlannedStream {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn server(events: Vec<Event>) -> (TwoPlaneServer, Probe) {
    server_with_options(events, false, false)
}

pub fn pending_first_server(events: Vec<Event>) -> (TwoPlaneServer, Probe) {
    server_with_options(events, true, false)
}

pub fn pending_after_first_server(events: Vec<Event>) -> (TwoPlaneServer, Probe) {
    server_with_options(events, false, true)
}

fn server_with_options(
    events: Vec<Event>,
    pending_first: bool,
    pending_after_first: bool,
) -> (TwoPlaneServer, Probe) {
    let config = crate::gateway::config();
    let adapter = Arc::new(SyntheticAdapter {
        id: "vllm-private".into(),
        capabilities: crate::gateway::capabilities(&config, "vllm-private"),
        plan: Plan {
            events,
            pending_first,
            pending_after_first,
        },
        probe: Probe {
            dispatches: Arc::new(AtomicUsize::new(0)),
            polls: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
        },
    });
    let probe = adapter.probe.clone();
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &crate::gateway::Resolver,
        Readiness::new(true),
        vec![adapter],
    )
    .expect("synthetic server");
    (server, probe)
}

pub fn request(body: &str) -> Request<Body> {
    crate::gateway::chat_request(body)
}

pub async fn response_body(response: Response) -> Bytes {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body")
}
