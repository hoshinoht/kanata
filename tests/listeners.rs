use std::{
    convert::Infallible,
    fs,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    body::{Body, Bytes, to_bytes},
    http::{HeaderValue, Request, StatusCode},
};
use futures_util::stream;
use kanata::adapter::{Adapter, AdapterFuture};
use kanata::auth::{SecretResolutionError, SecretResolver};
use kanata::config::{self, SecretReference};
use kanata::core::{Capabilities, ErrorKind, GatewayError, RoutedRequest};
use kanata::server::{Readiness, ServerPlan, TwoPlaneServer};
use tokio::sync::{Notify, Semaphore};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct MemoryResolver;

impl SecretResolver for MemoryResolver {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(b"test-key".to_vec())
    }
}

struct NoopAdapter {
    id: String,
    capabilities: Capabilities,
}

struct HeldAdapter {
    capabilities: Capabilities,
    dispatches: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: Arc<Semaphore>,
}

impl Adapter for HeldAdapter {
    fn id(&self) -> &str {
        "vllm-private"
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, _: RoutedRequest) -> AdapterFuture {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        self.started.notify_waiters();
        let release = self.release.clone();
        Box::pin(async move {
            let permit = release.acquire_owned().await.expect("release gate");
            permit.forget();
            Err(GatewayError {
                kind: ErrorKind::InvalidRequest,
            })
        })
    }
}

impl Adapter for NoopAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, _: RoutedRequest) -> AdapterFuture {
        Box::pin(async {
            Err(GatewayError {
                kind: ErrorKind::Internal,
            })
        })
    }
}

fn server() -> (TwoPlaneServer, Readiness) {
    server_from_config(
        config::load("tests/fixtures/config/example.toml").expect("example config validates"),
    )
}

fn server_from_config(config: config::ValidatedConfig) -> (TwoPlaneServer, Readiness) {
    let readiness = Readiness::new(false);
    let adapters = config
        .adapters()
        .iter()
        .map(|adapter| {
            Arc::new(NoopAdapter {
                id: adapter.id().into(),
                capabilities: adapter.capabilities().clone(),
            }) as Arc<dyn Adapter>
        })
        .collect();
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &MemoryResolver,
        readiness.clone(),
        adapters,
    )
    .expect("server builds");
    (server, readiness)
}

fn public_config(routes: &str, max_in_flight: u64, queue_ms: u64) -> config::ValidatedConfig {
    public_config_with_client_bind("0.0.0.0", routes, max_in_flight, queue_ms)
}

fn public_config_with_client_bind(
    client_bind: &str,
    routes: &str,
    max_in_flight: u64,
    queue_ms: u64,
) -> config::ValidatedConfig {
    let contents = fs::read_to_string("tests/fixtures/config/example.toml")
        .expect("example config reads")
        .replace(
            "bind = \"0.0.0.0\"",
            &format!("bind = \"{client_bind}\""),
        )
        .replace(
            "[listeners.admin]",
            "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
        )
        .replace(
            "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]",
            &format!(
                "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]\npublic_routes = {routes}"
            ),
        )
        .replace(
            "max_in_flight = 8",
            &format!("max_in_flight = {max_in_flight}"),
        )
        .replace("queue_ms = 1000", &format!("queue_ms = {queue_ms}"));
    let path = std::env::temp_dir().join(format!(
        "kanata-listeners-public-{}-{}.toml",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let result = config::load(&path);
    fs::remove_file(path).expect("fixture removes");
    result.expect("public config validates")
}

async fn reserved_listener_pair() -> (tokio::net::TcpListener, tokio::net::TcpListener) {
    loop {
        let client = tokio::net::TcpListener::bind("0.0.0.0:0")
            .await
            .expect("reserve wildcard client port");
        let admin = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve loopback admin port");
        if client.local_addr().expect("client address").port()
            != admin.local_addr().expect("admin address").port()
        {
            return (client, admin);
        }
    }
}

fn wildcard_client_config(client_port: u16, admin_port: u16) -> config::ValidatedConfig {
    let contents = fs::read_to_string("tests/fixtures/config/example.toml")
        .expect("example config reads")
        .replace("port = 8080", &format!("port = {client_port}"))
        .replace("port = 9090", &format!("port = {admin_port}"));
    let path = std::env::temp_dir().join(format!(
        "kanata-listeners-wildcard-{}-{}.toml",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("wildcard config fixture writes");
    let result = config::load(&path);
    fs::remove_file(path).expect("wildcard config fixture removes");
    result.expect("wildcard config remains schema-valid")
}

fn assert_wildcard_bind_rejected(error: std::io::Error) {
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "client listener bind must be concrete");
    assert!(!error.to_string().contains("0.0.0.0"));
}

fn public_chat_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model":"private-chat","messages":[{"role":"user","content":"held"}]}"#,
        ))
        .expect("chat request")
}

fn unpolled_request(
    path: &str,
    authorization: Option<HeaderValue>,
    duplicate_authorization: bool,
) -> (Request<Body>, Arc<AtomicBool>) {
    let polled = Arc::new(AtomicBool::new(false));
    let observed = polled.clone();
    let body = Body::from_stream(stream::poll_fn(move |_| {
        observed.store(true, Ordering::SeqCst);
        Poll::Ready(None::<Result<Bytes, Infallible>>)
    }));
    let mut builder = Request::builder().method("POST").uri(path);
    if let Some(authorization) = authorization {
        builder = builder.header("authorization", authorization);
    }
    let mut request = builder.body(body).expect("request");
    if duplicate_authorization {
        request
            .headers_mut()
            .append("authorization", HeaderValue::from_static("Bearer test-key"));
    }
    (request, polled)
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = futures_util::task::noop_waker_ref();
    future.poll(&mut Context::from_waker(waker))
}

fn restricted_server() -> (TwoPlaneServer, Readiness) {
    let contents =
        fs::read_to_string("tests/fixtures/config/example.toml").expect("example config reads");
    let contents = contents.replace(
        "permissions = [\n  { model_alias = \"local-chat\", operation = \"chat\" },\n  { model_alias = \"private-chat\", operation = \"chat\" },\n  { model_alias = \"private-transcribe\", operation = \"transcription\" },\n  { model_alias = \"remote-chat\", operation = \"chat\" },\n  { model_alias = \"codex-chat\", operation = \"chat\" },\n]",
        "permissions = [{ model_alias = \"local-chat\", operation = \"chat\" }]",
    );
    let path = std::env::temp_dir().join(format!(
        "kanata-listeners-{}-{}.toml",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let config = config::load(&path).expect("restricted config validates");
    fs::remove_file(path).expect("fixture removes");
    server_from_config(config)
}

#[tokio::test]
async fn client_models_are_authenticated_and_permission_filtered() {
    let (server, _) = server();
    let response = server
        .client_oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()["www-authenticate"],
        "Bearer realm=\"kanata\""
    );
    let unauthorized_body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");

    let response = server
        .client_oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer wrong-key")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
        unauthorized_body
    );

    let mut duplicate = Request::builder()
        .uri("/v1/models")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .expect("request");
    duplicate
        .headers_mut()
        .append("authorization", HeaderValue::from_static("Bearer test-key"));
    let malformed = [
        Request::builder()
            .uri("/v1/models")
            .header("authorization", "Basic test-key")
            .body(Body::empty())
            .expect("request"),
        Request::builder()
            .uri("/v1/models")
            .header("authorization", format!("Bearer {}", "x".repeat(4097)))
            .body(Body::empty())
            .expect("request"),
        Request::builder()
            .uri("/v1/models")
            .header("authorization", "Bearer comma,value")
            .body(Body::empty())
            .expect("request"),
        Request::builder()
            .uri("/v1/models")
            .header("authorization", "Bearer middle=padding")
            .body(Body::empty())
            .expect("request"),
        Request::builder()
            .uri("/v1/models")
            .header(
                "authorization",
                HeaderValue::from_bytes(b"Bearer \xff").expect("header"),
            )
            .body(Body::empty())
            .expect("request"),
        duplicate,
    ];
    for request in malformed {
        let response = server.client_oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()["www-authenticate"],
            "Bearer realm=\"kanata\""
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body"),
            unauthorized_body
        );
    }

    let response = server
        .client_oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer test-key")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body")
                .to_vec()
        )
        .expect("utf8"),
        "{\"data\":[{\"created\":0,\"id\":\"codex-chat\",\"kanata\":{\"context_tokens\":null,\"function_tools\":true,\"input_audio\":false,\"operations\":[\"chat\"],\"reasoning_control\":false,\"reasoning_efforts\":null,\"sampling_controls\":false,\"streaming\":true,\"structured_output\":false,\"trust_zone\":\"external\"},\"object\":\"model\",\"owned_by\":\"kanata\"},{\"created\":0,\"id\":\"local-chat\",\"kanata\":{\"context_tokens\":null,\"function_tools\":true,\"input_audio\":false,\"operations\":[\"chat\"],\"reasoning_control\":false,\"reasoning_efforts\":null,\"sampling_controls\":false,\"streaming\":true,\"structured_output\":false,\"trust_zone\":\"local\"},\"object\":\"model\",\"owned_by\":\"kanata\"},{\"created\":0,\"id\":\"private-chat\",\"kanata\":{\"context_tokens\":null,\"function_tools\":true,\"input_audio\":false,\"operations\":[\"chat\"],\"reasoning_control\":false,\"reasoning_efforts\":null,\"sampling_controls\":false,\"streaming\":true,\"structured_output\":false,\"trust_zone\":\"private_network\"},\"object\":\"model\",\"owned_by\":\"kanata\"},{\"created\":0,\"id\":\"private-transcribe\",\"kanata\":{\"context_tokens\":null,\"function_tools\":false,\"input_audio\":false,\"operations\":[\"transcription\"],\"reasoning_control\":false,\"reasoning_efforts\":null,\"sampling_controls\":false,\"streaming\":false,\"structured_output\":false,\"trust_zone\":\"private_network\"},\"object\":\"model\",\"owned_by\":\"kanata\"},{\"created\":0,\"id\":\"remote-chat\",\"kanata\":{\"context_tokens\":null,\"function_tools\":false,\"input_audio\":false,\"operations\":[\"chat\"],\"reasoning_control\":false,\"reasoning_efforts\":null,\"sampling_controls\":false,\"streaming\":true,\"structured_output\":false,\"trust_zone\":\"external\"},\"object\":\"model\",\"owned_by\":\"kanata\"}],\"object\":\"list\"}"
    );

    let (restricted, _) = restricted_server();
    let response = restricted
        .client_oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer test-key")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body"),
        "{\"data\":[{\"created\":0,\"id\":\"local-chat\",\"kanata\":{\"context_tokens\":null,\"function_tools\":true,\"input_audio\":false,\"operations\":[\"chat\"],\"reasoning_control\":false,\"reasoning_efforts\":null,\"sampling_controls\":false,\"streaming\":true,\"structured_output\":false,\"trust_zone\":\"local\"},\"object\":\"model\",\"owned_by\":\"kanata\"}],\"object\":\"list\"}"
    );
}

#[tokio::test]
async fn router_planes_are_isolated_and_readiness_is_local() {
    let (server, readiness) = server();
    for path in ["/live", "/ready", "/metrics", "/admin"] {
        let response = server
            .client_oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let response = server
        .admin_oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let live = server
        .admin_oneshot(
            Request::builder()
                .uri("/live")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(live.status(), StatusCode::OK);
    let not_ready = server
        .admin_oneshot(
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    readiness.set_ready(true);
    let ready = server
        .admin_oneshot(
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(ready.status(), StatusCode::OK);
    let metrics = server
        .admin_oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(metrics.status(), StatusCode::OK);
    let body = to_bytes(metrics.into_body(), usize::MAX)
        .await
        .expect("body");
    assert!(body.starts_with(b"# TYPE kanata_process_live gauge\nkanata_process_live 1\n"));
    assert!(
        body.windows(b"kanata_process_ready 1\n".len())
            .any(|window| { window == b"kanata_process_ready 1\n" })
    );
    assert!(
        body.windows(b"kanata_requests_started_total".len())
            .any(|window| { window == b"kanata_requests_started_total" })
    );
    assert!(
        server
            .public_oneshot(Request::new(Body::empty()))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn public_auth_precedes_routes_fallback_and_body_reads() {
    let config = public_config("[]", 8, 1_000);
    let (server, _) = server_from_config(config);
    let invalid_authorizations = [
        (None, false),
        (Some(HeaderValue::from_static("Basic test-key")), false),
        (Some(HeaderValue::from_static("Bearer ")), false),
        (Some(HeaderValue::from_static("Bearer wrong-key")), false),
        (
            Some(HeaderValue::from_bytes(b"Bearer \xff").expect("header")),
            false,
        ),
        (Some(HeaderValue::from_static("Bearer test-key")), true),
    ];
    let mut expected_body = None;
    for path in [
        "/v1/chat/completions",
        "/v1/audio/transcriptions",
        "/arbitrary/path",
        "/live",
    ] {
        for (authorization, duplicate) in invalid_authorizations.clone() {
            let (request, polled) = unpolled_request(path, authorization, duplicate);
            let response = server
                .public_oneshot(request)
                .await
                .expect("configured public router");
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
            assert!(response.headers().get("www-authenticate").is_none());
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("response body");
            if let Some(expected) = &expected_body {
                assert_eq!(&body, expected, "{path}");
            } else {
                expected_body = Some(body);
            }
            assert!(!polled.load(Ordering::SeqCst), "body polled for {path}");
        }
    }

    for path in ["/arbitrary/path", "/live"] {
        let response = server
            .public_oneshot(
                Request::builder()
                    .uri(path)
                    .header("authorization", "Bearer test-key")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("configured public router");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert!(response.headers().get("www-authenticate").is_none());
    }

    let metrics = server
        .admin_oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("admin metrics");
    let metrics = to_bytes(metrics.into_body(), usize::MAX)
        .await
        .expect("metrics body");
    let metrics = String::from_utf8(metrics.to_vec()).expect("metrics text");
    assert!(
        metrics.contains("kanata_requests_finished_total{endpoint=\"other\",outcome=\"client_error\",status_class=\"4xx\",timeout_phase=\"none\"}"),
        "public auth rejections share metadata telemetry"
    );
}

#[tokio::test(start_paused = true)]
async fn public_and_private_routers_share_route_admission_capacity() {
    let config = public_config(
        r#"[{ model_alias = "private-chat", operation = "chat" }]"#,
        1,
        10,
    );
    let dispatches = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let adapter: Arc<dyn Adapter> = Arc::new(HeldAdapter {
        capabilities: config.adapters()[1].capabilities().clone(),
        dispatches: dispatches.clone(),
        started: started.clone(),
        release: release.clone(),
    });
    let server = Arc::new(
        TwoPlaneServer::from_validated_with_adapters(
            &config,
            &MemoryResolver,
            Readiness::new(true),
            vec![adapter],
        )
        .expect("shared-capacity server"),
    );

    let first_server = server.clone();
    let first = tokio::spawn(async move {
        first_server
            .public_oneshot(public_chat_request())
            .await
            .expect("configured public router")
    });
    while dispatches.load(Ordering::SeqCst) == 0 {
        started.notified().await;
    }

    let mut second = Box::pin(server.client_oneshot(public_chat_request()));
    assert!(matches!(poll_once(second.as_mut()), Poll::Pending));
    tokio::time::advance(Duration::from_millis(10)).await;
    let second = match poll_once(second.as_mut()) {
        Poll::Ready(Ok(response)) => response,
        Poll::Ready(Err(error)) => match error {},
        Poll::Pending => panic!("queued request should time out"),
    };
    assert_eq!(second.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);

    release.add_permits(1);
    let first = first.await.expect("public request task");
    assert_eq!(first.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn listener_plan_uses_validated_addresses_without_host_publication() {
    let config =
        config::load("tests/fixtures/config/example.toml").expect("example config validates");
    let plan = ServerPlan::from_validated(&config).expect("plan builds");
    assert_eq!(plan.client_addr(), "0.0.0.0:8080".parse().expect("address"));
    assert_eq!(
        plan.admin_addr(),
        "127.0.0.1:9090".parse().expect("address")
    );
    assert!(plan.admin_addr().ip().is_loopback());
    assert_eq!(plan.public_addr(), None);
}

#[tokio::test]
async fn wildcard_client_bind_is_rejected_before_runtime_socket_binding() {
    let (client_listener, admin_listener) = reserved_listener_pair().await;
    let client_addr = client_listener.local_addr().expect("client address");
    let admin_addr = admin_listener.local_addr().expect("admin address");
    let config = wildcard_client_config(client_addr.port(), admin_addr.port());

    let (server, _) = server_from_config(config.clone());
    assert_wildcard_bind_rejected(server.bind().await.err().expect("bind rejected"));

    let (server, _) = server_from_config(config.clone());
    assert_wildcard_bind_rejected(server.bind_and_serve().await.unwrap_err());

    let (server, _) = server_from_config(config);
    assert_wildcard_bind_rejected(
        server
            .with_bound_listeners_and_public(client_listener, None, admin_listener)
            .err()
            .expect("prebound wildcard plan rejected"),
    );

    assert!(
        tokio::net::TcpListener::bind(client_addr).await.is_ok(),
        "rejected prebound client socket was released"
    );
    assert!(
        tokio::net::TcpListener::bind(admin_addr).await.is_ok(),
        "rejected prebound admin socket was released"
    );
}
