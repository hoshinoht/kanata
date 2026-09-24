use std::{
    convert::Infallible, future::Future, future::pending, io, net::SocketAddr, sync::Arc,
    time::Duration,
};

use axum::{Router, body::Body};
use hyper::{body::Incoming, server::conn::http1};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
};
use tower::{ServiceExt, util::BoxCloneSyncService};

use super::{Readiness, TwoPlaneServer, shutdown};
use crate::routing::admission::Admission;

pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

const CLIENT_CONNECTION_LIMIT: usize = 256;
const ADMIN_CONNECTION_LIMIT: usize = 32;
/// Time a client has to send a complete request head, including on idle keep-alive connections.
pub const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
type ConnectionService =
    BoxCloneSyncService<http::Request<Incoming>, http::Response<Body>, Infallible>;

pub struct BoundTwoPlaneServer {
    plan: super::ServerPlan,
    client_listener: TcpListener,
    public_listener: Option<TcpListener>,
    admin_listener: TcpListener,
    client: Router,
    public: Option<super::PublicClientService>,
    admin: Router,
    admission: Arc<Admission>,
    readiness: Readiness,
    header_read_timeout: Duration,
}

impl TwoPlaneServer {
    pub async fn bind(self) -> io::Result<BoundTwoPlaneServer> {
        validate_client_bind(self.plan)?;
        let client = TcpListener::bind(self.plan.client_addr()).await?;
        let public = match self.plan.public_addr() {
            Some(address) => Some(TcpListener::bind(address).await?),
            None => None,
        };
        let admin = TcpListener::bind(self.plan.admin_addr()).await?;
        self.with_bound_listeners_and_public(client, public, admin)
    }

    pub fn with_bound_listeners(
        self,
        client_listener: TcpListener,
        admin_listener: TcpListener,
    ) -> io::Result<BoundTwoPlaneServer> {
        self.with_bound_listeners_and_public(client_listener, None, admin_listener)
    }

    pub fn with_bound_listeners_and_public(
        self,
        client_listener: TcpListener,
        public_listener: Option<TcpListener>,
        admin_listener: TcpListener,
    ) -> io::Result<BoundTwoPlaneServer> {
        validate_client_bind(self.plan)?;
        let client_addr = client_listener.local_addr()?;
        let public_addr = public_listener
            .as_ref()
            .map(TcpListener::local_addr)
            .transpose()?;
        let admin_addr = admin_listener.local_addr()?;
        if !self.plan.admin_addr().ip().is_loopback() || !admin_addr.ip().is_loopback() {
            return Err(invalid_listener("admin listener must be loopback"));
        }
        if client_addr != self.plan.client_addr() {
            return Err(invalid_listener(
                "client listener does not match server plan",
            ));
        }
        if public_addr != self.plan.public_addr() {
            return Err(invalid_listener(
                "public listener does not match server plan",
            ));
        }
        if admin_addr != self.plan.admin_addr() {
            return Err(invalid_listener(
                "admin listener does not match server plan",
            ));
        }
        Ok(BoundTwoPlaneServer {
            plan: self.plan,
            client_listener,
            public_listener,
            admin_listener,
            client: self.client,
            public: self.public,
            admin: self.admin,
            admission: self.admission,
            readiness: self.readiness,
            header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT,
        })
    }
}

impl BoundTwoPlaneServer {
    pub fn client_addr(&self) -> SocketAddr {
        self.plan.client_addr()
    }

    pub fn public_addr(&self) -> Option<SocketAddr> {
        self.plan.public_addr()
    }

    pub fn admin_addr(&self) -> SocketAddr {
        self.plan.admin_addr()
    }

    /// Overrides how long a client may take to send a request head.
    pub fn with_header_read_timeout(mut self, timeout: Duration) -> Self {
        self.header_read_timeout = timeout;
        self
    }

    pub async fn serve_until<S>(self, shutdown: S, grace: Duration) -> io::Result<()>
    where
        S: Future,
    {
        serve_until(self, shutdown, grace).await
    }
}

async fn serve_until<S>(bound: BoundTwoPlaneServer, shutdown: S, grace: Duration) -> io::Result<()>
where
    S: Future,
{
    shutdown::validate_grace(grace)?;
    let mut runtime = Runtime::new(bound);
    let result = runtime.run(shutdown, grace).await;
    runtime.connections.abort_and_join().await;
    result
}

struct Runtime {
    client_listener: TcpListener,
    public_listener: Option<TcpListener>,
    admin_listener: TcpListener,
    client: Router,
    public: Option<super::PublicClientService>,
    admin: Router,
    admission: Arc<Admission>,
    readiness: Readiness,
    header_read_timeout: Duration,
    client_slots: Arc<Semaphore>,
    admin_slots: Arc<Semaphore>,
    drain_tx: watch::Sender<bool>,
    drain_rx: watch::Receiver<bool>,
    connections: shutdown::ConnectionSets,
}

impl Runtime {
    fn new(bound: BoundTwoPlaneServer) -> Self {
        let (drain_tx, drain_rx) = watch::channel(false);
        Self {
            client_listener: bound.client_listener,
            public_listener: bound.public_listener,
            admin_listener: bound.admin_listener,
            client: bound.client,
            public: bound.public,
            admin: bound.admin,
            admission: bound.admission,
            readiness: bound.readiness,
            header_read_timeout: bound.header_read_timeout,
            client_slots: Arc::new(Semaphore::new(CLIENT_CONNECTION_LIMIT)),
            admin_slots: Arc::new(Semaphore::new(ADMIN_CONNECTION_LIMIT)),
            drain_tx,
            drain_rx,
            connections: shutdown::ConnectionSets::new(),
        }
    }

    async fn run<S>(&mut self, shutdown: S, grace: Duration) -> io::Result<()>
    where
        S: Future,
    {
        let mut shutdown = Box::pin(shutdown);
        loop {
            self.connections.reap_completed()?;
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    tracing::info!(
                        target: "kanata::lifecycle",
                        grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
                        "shutdown requested; draining",
                    );
                    self.start_draining();
                    let deadline = shutdown::grace_deadline(grace)?;
                    return self.drain_until(deadline).await;
                }
                result = self.connections.client.join_next(), if !self.connections.client.is_empty() => {
                    complete(result)?;
                }
                result = self.connections.client_rejections.join_next(), if !self.connections.client_rejections.is_empty() => {
                    complete(result)?;
                }
                result = self.connections.admin.join_next(), if !self.connections.admin.is_empty() => {
                    complete(result)?;
                }
                accepted = accept_with_slot(&self.client_listener, &self.client_slots) => {
                    if let Some((socket, permit)) = accepted? {
                        self.spawn_client(socket, permit);
                    }
                }
                accepted = accept_optional_with_slot(self.public_listener.as_ref(), &self.client_slots) => {
                    if let Some((socket, permit)) = accepted? {
                        self.spawn_public(socket, permit);
                    }
                }
                accepted = accept_with_slot(&self.admin_listener, &self.admin_slots) => {
                    if let Some((socket, permit)) = accepted? {
                        self.spawn_admin(socket, permit);
                    }
                }
            }
        }
    }

    fn start_draining(&self) {
        self.admission.close();
        self.readiness.set_draining();
        self.readiness.set_ready(false);
        let _ = self.drain_tx.send(true);
    }

    async fn drain_until(&mut self, deadline: tokio::time::Instant) -> io::Result<()> {
        loop {
            self.connections.reap_completed()?;
            if self.connections.client.is_empty()
                && self.connections.client_rejections.is_empty()
                && self.connections.admin.is_empty()
            {
                tracing::info!(target: "kanata::lifecycle", "drain complete");
                return Ok(());
            }
            let sleep = tokio::time::sleep_until(deadline);
            tokio::pin!(sleep);
            tokio::select! {
                biased;
                _ = &mut sleep => {
                    tracing::warn!(
                        target: "kanata::lifecycle",
                        open_connections = self.connections.client.len()
                            + self.connections.client_rejections.len()
                            + self.connections.admin.len(),
                        "drain grace expired; aborting open connections",
                    );
                    return Ok(());
                }
                result = self.connections.client.join_next(), if !self.connections.client.is_empty() => {
                    complete(result)?;
                }
                result = self.connections.client_rejections.join_next(), if !self.connections.client_rejections.is_empty() => {
                    complete(result)?;
                }
                result = self.connections.admin.join_next(), if !self.connections.admin.is_empty() => {
                    complete(result)?;
                }
                accepted = accept_with_slot(&self.admin_listener, &self.admin_slots) => {
                    if let Some((socket, permit)) = accepted? {
                        self.spawn_admin_rejection(socket, permit);
                    }
                }
                accepted = accept_with_slot(&self.client_listener, &self.client_slots) => {
                    if let Some((socket, permit)) = accepted? {
                        self.spawn_client_rejection(socket, permit);
                    }
                }
                accepted = accept_optional_with_slot(self.public_listener.as_ref(), &self.client_slots) => {
                    if let Some((socket, permit)) = accepted? {
                        self.spawn_public_rejection(socket, permit);
                    }
                }
            }
        }
    }

    fn spawn_client(&mut self, socket: TcpStream, permit: OwnedSemaphorePermit) {
        let service = router_connection_service(self.client.clone());
        let drain = self.drain_rx.clone();
        let header_timeout = self.header_read_timeout;
        self.connections.client.spawn(async move {
            let _permit = permit;
            serve_connection(socket, service, drain, true, header_timeout).await;
        });
    }

    fn spawn_public(&mut self, socket: TcpStream, permit: OwnedSemaphorePermit) {
        let Some(public) = self.public.clone() else {
            return;
        };
        let service = public_connection_service(public);
        let drain = self.drain_rx.clone();
        let header_timeout = self.header_read_timeout;
        self.connections.client.spawn(async move {
            let _permit = permit;
            serve_connection(socket, service, drain, true, header_timeout).await;
        });
    }

    fn spawn_admin(&mut self, socket: TcpStream, permit: OwnedSemaphorePermit) {
        let service = router_connection_service(self.admin.clone());
        let drain = self.drain_rx.clone();
        let header_timeout = self.header_read_timeout;
        self.connections.admin.spawn(async move {
            let _permit = permit;
            serve_connection(socket, service, drain, true, header_timeout).await;
        });
    }

    fn spawn_client_rejection(&mut self, socket: TcpStream, permit: OwnedSemaphorePermit) {
        let service = router_connection_service(self.client.clone());
        let drain = self.drain_rx.clone();
        let header_timeout = self.header_read_timeout;
        self.connections.client_rejections.spawn(async move {
            let _permit = permit;
            serve_connection(socket, service, drain, false, header_timeout).await;
        });
    }

    fn spawn_public_rejection(&mut self, socket: TcpStream, permit: OwnedSemaphorePermit) {
        let Some(public) = self.public.clone() else {
            return;
        };
        let service = public_connection_service(public);
        let drain = self.drain_rx.clone();
        let header_timeout = self.header_read_timeout;
        self.connections.client_rejections.spawn(async move {
            let _permit = permit;
            serve_connection(socket, service, drain, false, header_timeout).await;
        });
    }

    fn spawn_admin_rejection(&mut self, socket: TcpStream, permit: OwnedSemaphorePermit) {
        let service = router_connection_service(self.admin.clone());
        let drain = self.drain_rx.clone();
        let header_timeout = self.header_read_timeout;
        self.connections.admin.spawn(async move {
            let _permit = permit;
            serve_connection(socket, service, drain, false, header_timeout).await;
        });
    }
}

async fn accept_optional_with_slot(
    listener: Option<&TcpListener>,
    slots: &Arc<Semaphore>,
) -> io::Result<Option<(TcpStream, OwnedSemaphorePermit)>> {
    match listener {
        Some(listener) => accept_with_slot(listener, slots).await,
        None => pending().await,
    }
}

async fn accept_with_slot(
    listener: &TcpListener,
    slots: &Arc<Semaphore>,
) -> io::Result<Option<(TcpStream, OwnedSemaphorePermit)>> {
    let (socket, _) = listener
        .accept()
        .await
        .map_err(|error| io::Error::new(error.kind(), "listener accept failed"))?;
    let Some(permit) = slots.clone().try_acquire_owned().ok() else {
        return Ok(None);
    };
    Ok(Some((socket, permit)))
}

async fn serve_connection(
    socket: TcpStream,
    service: ConnectionService,
    mut drain: watch::Receiver<bool>,
    keep_alive: bool,
    header_timeout: Duration,
) {
    let service = TowerToHyperService::new(service);
    let mut connection = Box::pin(
        http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(header_timeout)
            .keep_alive(keep_alive)
            .serve_connection(TokioIo::new(socket), service),
    );
    if !keep_alive {
        let _ = (&mut connection).await;
        return;
    }
    if *drain.borrow() {
        connection.as_mut().graceful_shutdown();
        let _ = (&mut connection).await;
        return;
    }
    loop {
        tokio::select! {
            result = &mut connection => {
                let _ = result;
                return;
            }
            changed = drain.changed() => {
                if changed.is_err() {
                    return;
                }
                if *drain.borrow() {
                    connection.as_mut().graceful_shutdown();
                    let _ = (&mut connection).await;
                    return;
                }
            }
        }
    }
}

fn router_connection_service(router: Router) -> ConnectionService {
    BoxCloneSyncService::new(
        router
            .into_service::<Body>()
            .map_request(|request: http::Request<Incoming>| request.map(Body::new)),
    )
}

fn public_connection_service(public: super::PublicClientService) -> ConnectionService {
    BoxCloneSyncService::new(
        public.map_request(|request: http::Request<Incoming>| request.map(Body::new)),
    )
}

fn complete(result: Option<Result<(), tokio::task::JoinError>>) -> io::Result<()> {
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(shutdown::join_error(&error)),
        None => Ok(()),
    }
}

fn invalid_listener(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn validate_client_bind(plan: super::ServerPlan) -> io::Result<()> {
    if plan.client_addr().ip().is_unspecified() {
        return Err(invalid_listener("client listener bind must be concrete"));
    }
    Ok(())
}
