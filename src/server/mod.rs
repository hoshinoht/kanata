use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{FromRequestParts, State},
    http::{HeaderValue, Request, StatusCode, request::Parts},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use tower::{ServiceBuilder, ServiceExt, util::BoxCloneSyncService};

use crate::adapter::Adapter;
use crate::auth::{ApplicationAuth, AuthBuildError, AuthContext, SecretResolver};
use crate::config::ValidatedConfig;
use crate::core::RouteSelector;
use crate::routing::{Registry, admission::Admission};

mod runtime;
mod shutdown;

pub use runtime::{BoundTwoPlaneServer, DEFAULT_SHUTDOWN_GRACE};

#[derive(Clone, Default)]
pub struct Readiness {
    ready: Arc<AtomicBool>,
    draining: Arc<AtomicBool>,
}

impl Readiness {
    pub fn new(ready: bool) -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(ready)),
            draining: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.draining.load(Ordering::Acquire)
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }

    pub(crate) fn set_draining(&self) {
        self.draining.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct ClientState {
    auth: Arc<ApplicationAuth>,
    registry: Arc<Registry>,
    admission: Arc<Admission>,
    adapters: Arc<BTreeMap<String, Arc<dyn Adapter>>>,
    public_routes: Option<Arc<Vec<RouteSelector>>>,
    max_body_bytes: usize,
    max_audio_chat_body_bytes: usize,
    max_audio_bytes: usize,
    max_extension_bytes: usize,
    first_byte_ms: u64,
    idle_ms: u64,
    overall_ms: u64,
    request_sequence: Arc<std::sync::atomic::AtomicU64>,
    telemetry: Arc<crate::telemetry::Telemetry>,
    listener: crate::telemetry::Listener,
}

impl ClientState {
    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    pub(crate) fn admission(&self) -> &Admission {
        &self.admission
    }

    pub(crate) fn adapter(&self, id: &str) -> Option<Arc<dyn Adapter>> {
        self.adapters.get(id).cloned()
    }
    pub(crate) fn route_is_bound(&self, route: &crate::routing::RouteEntry) -> bool {
        self.adapters.contains_key(&route.adapter_id)
    }
    pub(crate) fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }
    pub(crate) fn max_audio_chat_body_bytes(&self) -> usize {
        self.max_audio_chat_body_bytes
    }
    pub(crate) fn max_audio_bytes(&self) -> usize {
        self.max_audio_bytes
    }
    pub(crate) fn max_extension_bytes(&self) -> usize {
        self.max_extension_bytes
    }
    pub(crate) fn first_byte_ms(&self) -> u64 {
        self.first_byte_ms
    }
    pub(crate) fn idle_ms(&self) -> u64 {
        self.idle_ms
    }
    pub(crate) fn overall_ms(&self) -> u64 {
        self.overall_ms
    }
    pub(crate) fn next_request_id(&self) -> String {
        format!(
            "req_{:016x}",
            self.request_sequence.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub(crate) fn telemetry(&self) -> &Arc<crate::telemetry::Telemetry> {
        &self.telemetry
    }

    pub(crate) fn listener(&self) -> crate::telemetry::Listener {
        self.listener
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerPlan {
    client_addr: SocketAddr,
    public_addr: Option<SocketAddr>,
    admin_addr: SocketAddr,
}

impl ServerPlan {
    pub fn from_validated(config: &ValidatedConfig) -> Result<Self, ServerPlanError> {
        let client = config.listeners().client();
        let public = config.listeners().public();
        let admin = config.listeners().admin();
        if !admin.bind().is_loopback() {
            return Err(ServerPlanError::AdminNotLoopback);
        }
        Ok(Self {
            client_addr: SocketAddr::new(client.bind(), client.port()),
            public_addr: public.map(|listener| SocketAddr::new(listener.bind(), listener.port())),
            admin_addr: SocketAddr::new(admin.bind(), admin.port()),
        })
    }

    pub fn client_addr(self) -> SocketAddr {
        self.client_addr
    }

    pub fn public_addr(self) -> Option<SocketAddr> {
        self.public_addr
    }

    pub fn admin_addr(self) -> SocketAddr {
        self.admin_addr
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlanError {
    AdminNotLoopback,
}

impl fmt::Display for ServerPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("admin listener must be loopback")
    }
}

impl std::error::Error for ServerPlanError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerBuildError {
    Auth(AuthBuildError),
    Plan(ServerPlanError),
    AdmissionLimits,
    AdapterBindings,
}

impl fmt::Display for ServerBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(error) => error.fmt(formatter),
            Self::Plan(error) => error.fmt(formatter),
            Self::AdmissionLimits => formatter.write_str("admission limits are not representable"),
            Self::AdapterBindings => {
                formatter.write_str("adapter bindings do not match validated configuration")
            }
        }
    }
}

impl std::error::Error for ServerBuildError {}

pub struct TwoPlaneServer {
    plan: ServerPlan,
    client: Router,
    public: Option<PublicClientService>,
    admin: Router,
    admission: Arc<Admission>,
    readiness: Readiness,
}

type PublicClientService = BoxCloneSyncService<Request<Body>, Response, Infallible>;

impl TwoPlaneServer {
    pub fn from_validated(
        config: &ValidatedConfig,
        resolver: &impl SecretResolver,
        readiness: Readiness,
    ) -> Result<Self, ServerBuildError> {
        Self::build(config, resolver, readiness, Router::new(), Vec::new())
    }

    pub fn from_validated_with_adapters(
        config: &ValidatedConfig,
        resolver: &impl SecretResolver,
        readiness: Readiness,
        adapters: Vec<Arc<dyn Adapter>>,
    ) -> Result<Self, ServerBuildError> {
        Self::build(config, resolver, readiness, Router::new(), adapters)
    }

    #[allow(dead_code)]
    pub(crate) fn from_validated_with_client_routes(
        config: &ValidatedConfig,
        resolver: &impl SecretResolver,
        readiness: Readiness,
        client_routes: Router<ClientState>,
    ) -> Result<Self, ServerBuildError> {
        Self::build(config, resolver, readiness, client_routes, Vec::new())
    }

    fn build(
        config: &ValidatedConfig,
        resolver: &impl SecretResolver,
        readiness: Readiness,
        client_routes: Router<ClientState>,
        adapters: Vec<Arc<dyn Adapter>>,
    ) -> Result<Self, ServerBuildError> {
        let auth =
            ApplicationAuth::from_validated(config, resolver).map_err(ServerBuildError::Auth)?;
        let plan = ServerPlan::from_validated(config).map_err(ServerBuildError::Plan)?;
        let mut bound = BTreeMap::new();
        for adapter in adapters {
            if bound.insert(adapter.id().to_owned(), adapter).is_some() {
                return Err(ServerBuildError::AdapterBindings);
            }
        }
        if !bound.iter().all(|(id, adapter)| {
            config.adapters().iter().any(|configured| {
                configured.id() == id && configured.capabilities() == adapter.capabilities()
            })
        }) {
            return Err(ServerBuildError::AdapterBindings);
        }
        let registry = Arc::new(Registry::from_validated(config));
        let admission = Arc::new(
            Admission::from_config(&registry, config)
                .map_err(|_| ServerBuildError::AdmissionLimits)?,
        );
        let telemetry = Arc::new(crate::telemetry::Telemetry::new());
        let client_state = ClientState {
            auth: Arc::new(auth),
            registry: registry.clone(),
            admission: admission.clone(),
            adapters: Arc::new(bound),
            public_routes: None,
            max_body_bytes: usize::try_from(config.limits().max_body_bytes())
                .map_err(|_| ServerBuildError::AdapterBindings)?,
            max_audio_chat_body_bytes: usize::try_from(config.limits().max_audio_chat_body_bytes())
                .map_err(|_| ServerBuildError::AdapterBindings)?,
            max_audio_bytes: usize::try_from(config.limits().max_audio_bytes())
                .map_err(|_| ServerBuildError::AdapterBindings)?,
            max_extension_bytes: usize::try_from(config.limits().max_extension_bytes())
                .map_err(|_| ServerBuildError::AdapterBindings)?,
            first_byte_ms: config.timeouts().first_byte_ms(),
            idle_ms: config.timeouts().idle_ms(),
            overall_ms: config.timeouts().overall_ms(),
            request_sequence: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            telemetry: telemetry.clone(),
            listener: crate::telemetry::Listener::Private,
        };
        let client = client_router(client_routes, client_state.clone());
        let public = config.listeners().public().map(|_| {
            let mut public_state = client_state.clone();
            public_state.listener = crate::telemetry::Listener::Public;
            public_state.public_routes =
                Some(Arc::new(config.publication().public_routes().to_vec()));
            public_client_router(public_state)
        });
        Ok(Self {
            plan,
            client,
            public,
            admin: admin_router(readiness.clone(), admission.clone(), telemetry),
            admission,
            readiness,
        })
    }

    pub fn plan(&self) -> ServerPlan {
        self.plan
    }

    pub async fn client_oneshot(&self, request: Request<Body>) -> Result<Response, Infallible> {
        self.client.clone().oneshot(request).await
    }

    pub async fn public_oneshot(&self, request: Request<Body>) -> Option<Response> {
        let public = self.public.as_ref()?;
        match public.clone().oneshot(request).await {
            Ok(response) => Some(response),
            Err(error) => match error {},
        }
    }

    pub async fn admin_oneshot(&self, request: Request<Body>) -> Result<Response, Infallible> {
        self.admin.clone().oneshot(request).await
    }

    pub async fn bind_and_serve(self) -> std::io::Result<()> {
        self.bind()
            .await?
            .serve_until(std::future::pending::<()>(), DEFAULT_SHUTDOWN_GRACE)
            .await
    }
}

pub(crate) struct Authenticated {
    context: AuthContext,
    public_routes: Option<Arc<Vec<RouteSelector>>>,
}

impl Authenticated {
    pub(crate) fn key_identity(&self) -> &str {
        self.context.key_identity()
    }

    #[allow(dead_code)]
    pub(crate) fn authorize(&self, selector: &RouteSelector) -> Result<(), ForbiddenResponse> {
        self.context
            .authorize(selector)
            .map_err(|_| ForbiddenResponse)?;
        if self
            .public_routes
            .as_ref()
            .is_some_and(|routes| !routes.iter().any(|route| route == selector))
        {
            return Err(ForbiddenResponse);
        }
        Ok(())
    }
}

impl FromRequestParts<ClientState> for Authenticated {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ClientState,
    ) -> Result<Self, Self::Rejection> {
        let context = parts
            .extensions
            .get::<PublicAuthContext>()
            .map(|authenticated| authenticated.0.clone())
            .or_else(|| state.auth.authenticate_headers(&parts.headers).ok())
            .ok_or_else(unauthorized)?;
        if let Some(observer) = parts.extensions.get::<crate::telemetry::Observer>() {
            observer.annotate_key(context.key_identity());
        }
        Ok(Self {
            context,
            public_routes: state.public_routes.clone(),
        })
    }
}

#[derive(Clone)]
struct PublicAuthContext(AuthContext);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) struct ForbiddenResponse;

impl IntoResponse for ForbiddenResponse {
    fn into_response(self) -> Response {
        error_response(
            StatusCode::FORBIDDEN,
            "Permission denied",
            "permission_error",
            "permission_denied",
        )
    }
}

fn client_router(routes: Router<ClientState>, state: ClientState) -> Router {
    let middleware_state = state.clone();
    client_route_tree(routes)
        .layer(middleware::from_fn_with_state(
            middleware_state,
            crate::telemetry::middleware,
        ))
        .with_state(state)
}

fn client_route_tree(routes: Router<ClientState>) -> Router<ClientState> {
    Router::new()
        .nest(
            "/v1",
            Router::new()
                .route("/models", get(list_models))
                .merge(crate::api::routes())
                .merge(routes),
        )
        .fallback(not_found)
}

fn public_client_router(state: ClientState) -> PublicClientService {
    let auth_state = state.clone();
    let telemetry_state = state.clone();
    let router = client_route_tree(Router::new()).with_state(state);
    BoxCloneSyncService::new(
        ServiceBuilder::new()
            .layer(middleware::from_fn_with_state(
                telemetry_state,
                crate::telemetry::middleware,
            ))
            .layer(middleware::from_fn_with_state(
                auth_state,
                authenticate_public_request,
            ))
            .service(router),
    )
}

async fn authenticate_public_request(
    State(state): State<ClientState>,
    mut request: Request<Body>,
    next: middleware::Next,
) -> Response {
    let Ok(context) = state.auth.authenticate_headers(request.headers()) else {
        return ForbiddenResponse.into_response();
    };
    request.extensions_mut().insert(PublicAuthContext(context));
    next.run(request).await
}

#[derive(Clone)]
struct AdminState {
    readiness: Readiness,
    admission: Arc<Admission>,
    telemetry: Arc<crate::telemetry::Telemetry>,
}

fn admin_router(
    readiness: Readiness,
    admission: Arc<Admission>,
    telemetry: Arc<crate::telemetry::Telemetry>,
) -> Router {
    Router::new()
        .route("/live", get(live))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .fallback(not_found)
        .with_state(AdminState {
            readiness,
            admission,
            telemetry,
        })
}

async fn list_models(auth: Authenticated, State(state): State<ClientState>) -> Response {
    let mut aliases: std::collections::BTreeMap<_, Vec<&crate::routing::RouteEntry>> =
        std::collections::BTreeMap::new();
    for route in state
        .registry()
        .routes()
        .filter(|route| auth.authorize(&route.identity.selector).is_ok())
        .filter(|route| state.route_is_bound(route))
    {
        aliases
            .entry(route.identity.selector.model_alias.0.clone())
            .or_default()
            .push(route);
    }
    // Capacity settings are published to private clients only.
    let publish_admission = state.listener() == crate::telemetry::Listener::Private;
    let limits = state.admission().limits();
    let data: Vec<_> = aliases
        .into_iter()
        .map(|(alias, routes)| {
            let mut kanata = model_capabilities(&routes);
            if publish_admission {
                let adapter = routes
                    .iter()
                    .find(|route| route.identity.selector.operation == crate::core::Operation::Chat)
                    .or(routes.first())
                    .and_then(|route| state.admission().adapter_max_in_flight(&route.adapter_id));
                kanata["admission"] = json!({
                    "max_in_flight": limits.max_in_flight,
                    "max_queue": limits.max_queue,
                    "queue_ms": limits.queue_ms,
                    "adapter_max_in_flight": adapter,
                });
            }
            json!({
                "id": alias,
                "object": "model",
                "created": 0,
                "owned_by": "kanata",
                "kanata": kanata,
            })
        })
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

/// Kanata extension for a model entry; `routes` are the alias's authorized, bound routes.
fn model_capabilities(routes: &[&crate::routing::RouteEntry]) -> serde_json::Value {
    let mut operations: Vec<_> = routes
        .iter()
        .map(|route| route.identity.selector.operation)
        .collect();
    operations.sort();
    let chat = routes
        .iter()
        .find(|route| route.identity.selector.operation == crate::core::Operation::Chat);
    let caps = chat.map(|route| &route.capabilities);
    let flag = |get: fn(&crate::core::Capabilities) -> bool| caps.is_some_and(get);
    let reasoning_control = flag(|c| c.reasoning_control);
    let reasoning_efforts = chat
        .filter(|_| reasoning_control)
        .and_then(|route| route.provider_kind.restricted_reasoning_efforts());
    let trust_zone = chat.or(routes.first()).map(|route| route.trust_zone);
    json!({
        "operations": operations,
        "structured_output": flag(|c| c.structured_output),
        "sampling_controls": flag(|c| c.sampling_controls),
        "reasoning_control": reasoning_control,
        "function_tools": flag(|c| c.function_tools),
        "streaming": flag(|c| c.streaming_chat),
        "input_audio": flag(|c| c.input_audio),
        "trust_zone": trust_zone,
        "reasoning_efforts": reasoning_efforts,
        "context_tokens": chat.and_then(|route| route.context_tokens),
    })
}

pub(crate) fn unauthorized() -> Response {
    let mut response = error_response(
        StatusCode::UNAUTHORIZED,
        "Invalid authentication credentials",
        "authentication_error",
        "invalid_api_key",
    );
    response.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"kanata\""),
    );
    response
}

pub(crate) fn error_response(
    status: StatusCode,
    message: &'static str,
    error_type: &'static str,
    code: &'static str,
) -> Response {
    let mut response = (
        status,
        Json(
            json!({"error": {"message": message, "type": error_type, "param": null, "code": code}}),
        ),
    )
        .into_response();
    response
        .extensions_mut()
        .insert(crate::telemetry::ErrorCode(code));
    response
}

async fn live() -> &'static str {
    "live\n"
}

async fn ready(State(state): State<AdminState>) -> Response {
    if state.readiness.is_ready() && !state.admission.is_closed() {
        (StatusCode::OK, "ready\n").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready\n").into_response()
    }
}

async fn metrics(State(state): State<AdminState>) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        {
            let mut body = state.telemetry.render(
                true,
                state.readiness.is_ready() && !state.admission.is_closed(),
            );
            body.push_str(&format!(
                "# TYPE kanata_circuit_breakers_open gauge\nkanata_circuit_breakers_open {}\n",
                state.admission.open_breakers()
            ));
            body
        },
    )
        .into_response()
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}
