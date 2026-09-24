use std::{
    future::Future,
    io,
    net::{SocketAddr, ToSocketAddrs},
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

use hyper_util::client::legacy::connect::dns::Name;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::{JoinError, JoinHandle},
};
use tower::Service;

const MAX_RESOLUTIONS: usize = 16;

type Lookup = Arc<dyn Fn(String) -> io::Result<Vec<SocketAddr>> + Send + Sync + 'static>;
type AcquireFuture =
    Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, tokio::sync::AcquireError>> + Send>>;

static DNS_PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Clone)]
pub(super) struct Resolver {
    permits: Arc<Semaphore>,
    lookup: Lookup,
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Resolver")
    }
}

pub(super) fn production() -> Resolver {
    Resolver {
        permits: DNS_PERMITS
            .get_or_init(|| Arc::new(Semaphore::new(MAX_RESOLUTIONS)))
            .clone(),
        lookup: Arc::new(|host| {
            (host.as_str(), 0)
                .to_socket_addrs()
                .map(|addresses| addresses.collect())
        }),
    }
}

#[cfg(test)]
pub(super) fn test_resolver<F>(capacity: usize, lookup: F) -> Resolver
where
    F: Fn(String) -> io::Result<Vec<SocketAddr>> + Send + Sync + 'static,
{
    Resolver {
        permits: Arc::new(Semaphore::new(capacity)),
        lookup: Arc::new(lookup),
    }
}

impl Service<Name> for Resolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = ResolveFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        ResolveFuture::new(
            self.permits.clone(),
            self.lookup.clone(),
            name.as_str().to_owned(),
        )
    }
}

pub(super) struct ResolveFuture {
    permits: Option<AcquireFuture>,
    lookup: Option<Lookup>,
    host: Option<String>,
    task: Option<AbortOnDrop<io::Result<Vec<SocketAddr>>>>,
    done: bool,
}

impl ResolveFuture {
    fn new(permits: Arc<Semaphore>, lookup: Lookup, host: String) -> Self {
        Self {
            permits: Some(Box::pin(permits.acquire_owned())),
            lookup: Some(lookup),
            host: Some(host),
            task: None,
            done: false,
        }
    }
}

impl Future for ResolveFuture {
    type Output = io::Result<std::vec::IntoIter<SocketAddr>>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.done {
            panic!("resolver future polled after completion");
        }
        if this.task.is_none() {
            let permit = match this.permits.as_mut() {
                Some(future) => match future.as_mut().poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(permit)) => permit,
                    Poll::Ready(Err(error)) => {
                        this.done = true;
                        return Poll::Ready(Err(io::Error::other(error)));
                    }
                },
                None => unreachable!("resolver permit future missing"),
            };
            this.permits = None;
            let lookup = this
                .lookup
                .take()
                .unwrap_or_else(|| unreachable!("lookup missing"));
            let host = this
                .host
                .take()
                .unwrap_or_else(|| unreachable!("host missing"));
            let task = tokio::task::spawn_blocking(move || {
                let result = lookup(host);
                drop(permit);
                result
            });
            this.task = Some(AbortOnDrop::new(task));
        }

        let task = this
            .task
            .as_mut()
            .unwrap_or_else(|| unreachable!("resolver task missing"));
        match Pin::new(task).poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(addresses)) => {
                this.done = true;
                Poll::Ready(addresses.map(Vec::into_iter))
            }
            Poll::Ready(Err(error)) => {
                this.done = true;
                Poll::Ready(Err(join_error(error)))
            }
        }
    }
}

struct AbortOnDrop<T> {
    handle: Option<JoinHandle<T>>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }
}

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(handle) = self.handle.as_mut() else {
            panic!("resolver task polled after completion");
        };
        match Pin::new(handle).poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.handle.take();
                Poll::Ready(result)
            }
        }
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

fn join_error(error: JoinError) -> io::Error {
    if error.is_cancelled() {
        io::Error::new(io::ErrorKind::Interrupted, error)
    } else {
        io::Error::other(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use http::Uri;
    use hyper_util::client::legacy::connect::HttpConnector;
    use tokio::{sync::mpsc, task::JoinHandle};
    use tower::Service;

    use super::{Resolver, test_resolver};

    fn name(host: &str) -> super::Name {
        host.parse().unwrap_or_else(|_| panic!("resolver name"))
    }

    #[derive(Clone)]
    struct Gate {
        state: Arc<(Mutex<bool>, Condvar)>,
        starts: mpsc::UnboundedSender<()>,
        finishes: mpsc::UnboundedSender<()>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl Gate {
        fn lookup(&self, _: String) -> io::Result<Vec<SocketAddr>> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            self.starts.send(()).unwrap_or_else(|_| panic!("start"));
            let (lock, wake) = &*self.state;
            let mut released = lock.lock().unwrap_or_else(|_| panic!("gate lock"));
            while !*released {
                released = wake.wait(released).unwrap_or_else(|_| panic!("gate wait"));
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.finishes.send(()).unwrap_or_else(|_| panic!("finish"));
            Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 80)])
        }

        fn release(&self) {
            let (lock, wake) = &*self.state;
            *lock.lock().unwrap_or_else(|_| panic!("gate lock")) = true;
            wake.notify_all();
        }
    }

    fn gate() -> (
        Gate,
        mpsc::UnboundedReceiver<()>,
        mpsc::UnboundedReceiver<()>,
    ) {
        let (starts_tx, starts_rx) = mpsc::unbounded_channel();
        let (finishes_tx, finishes_rx) = mpsc::unbounded_channel();
        let gate = Gate {
            state: Arc::new((Mutex::new(false), Condvar::new())),
            starts: starts_tx,
            finishes: finishes_tx,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        };
        (gate, starts_rx, finishes_rx)
    }

    fn spawn_resolution(
        resolver: Resolver,
        host: &'static str,
    ) -> JoinHandle<io::Result<Vec<SocketAddr>>> {
        tokio::spawn(async move {
            let mut resolver = resolver;
            resolver
                .call(name(host))
                .await
                .map(|addresses| addresses.collect())
        })
    }

    #[tokio::test]
    async fn production_capacity_is_shared_and_capped_without_default_gai() {
        let (gate, mut starts, mut finishes) = gate();
        let resolver = test_resolver(2, {
            let gate = gate.clone();
            move |host| gate.lookup(host)
        });
        let first = spawn_resolution(resolver.clone(), "one.test");
        let second = spawn_resolution(resolver.clone(), "two.test");
        starts.recv().await.unwrap_or_else(|| panic!("first start"));
        starts
            .recv()
            .await
            .unwrap_or_else(|| panic!("second start"));
        assert_eq!(gate.peak.load(Ordering::SeqCst), 2);

        let third = spawn_resolution(resolver, "three.test");
        tokio::task::yield_now().await;
        assert!(starts.try_recv().is_err());
        gate.release();
        finishes.recv().await.unwrap_or_else(|| panic!("finish"));
        finishes.recv().await.unwrap_or_else(|| panic!("finish"));
        starts.recv().await.unwrap_or_else(|| panic!("third start"));
        assert!(first.await.unwrap_or_else(|_| panic!("first")).is_ok());
        assert!(second.await.unwrap_or_else(|_| panic!("second")).is_ok());
        assert!(third.await.unwrap_or_else(|_| panic!("third")).is_ok());
    }

    #[tokio::test]
    async fn canceled_started_jobs_hold_permits_until_blocking_lookup_returns() {
        let (gate, mut starts, mut finishes) = gate();
        let resolver = test_resolver(2, {
            let gate = gate.clone();
            move |host| gate.lookup(host)
        });
        let first = spawn_resolution(resolver.clone(), "one.test");
        let second = spawn_resolution(resolver.clone(), "two.test");
        starts.recv().await.unwrap_or_else(|| panic!("first start"));
        starts
            .recv()
            .await
            .unwrap_or_else(|| panic!("second start"));
        first.abort();
        second.abort();
        let third = spawn_resolution(resolver, "three.test");
        tokio::task::yield_now().await;
        assert!(starts.try_recv().is_err());
        gate.release();
        finishes.recv().await.unwrap_or_else(|| panic!("finish"));
        finishes.recv().await.unwrap_or_else(|| panic!("finish"));
        starts.recv().await.unwrap_or_else(|| panic!("third start"));
        let _ = first.await;
        let _ = second.await;
        gate.release();
        finishes
            .recv()
            .await
            .unwrap_or_else(|| panic!("third finish"));
        third
            .await
            .unwrap_or_else(|_| panic!("third"))
            .unwrap_or_else(|_| panic!("lookup"));
        assert_eq!(gate.peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn canceled_queued_waiter_does_not_start_a_lookup() {
        let (gate, mut starts, mut finishes) = gate();
        let resolver = test_resolver(1, {
            let gate = gate.clone();
            move |host| gate.lookup(host)
        });
        let first = spawn_resolution(resolver.clone(), "one.test");
        starts.recv().await.unwrap_or_else(|| panic!("first start"));
        let queued = spawn_resolution(resolver, "queued.test");
        tokio::task::yield_now().await;
        queued.abort();
        gate.release();
        finishes.recv().await.unwrap_or_else(|| panic!("finish"));
        assert!(first.await.unwrap_or_else(|_| panic!("first")).is_ok());
        assert!(starts.try_recv().is_err());
    }

    #[tokio::test]
    async fn custom_resolver_is_used_by_http_connector() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|_| panic!("listener"));
        let address = listener.local_addr().unwrap_or_else(|_| panic!("address"));
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_resolver = calls.clone();
        let resolver = test_resolver(1, move |_| {
            calls_for_resolver.fetch_add(1, Ordering::SeqCst);
            Ok(vec![address])
        });
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap_or_else(|_| panic!("accept"));
            drop(socket);
        });
        let mut connector = HttpConnector::new_with_resolver(resolver);
        let _io = connector
            .call(Uri::from_static("http://resolver.test/"))
            .await
            .unwrap_or_else(|_| panic!("connect"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.await.unwrap_or_else(|_| panic!("server"));
    }
}
