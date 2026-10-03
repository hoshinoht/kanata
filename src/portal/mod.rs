mod service;
#[cfg(test)]
mod tests;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

use crate::config::ValidatedConfig;
use crate::keys::cli::RouteChoice;

const USAGE: &str = "usage: kanata portal --config <path> [--port <port>]";
const BODY_LIMIT: usize = 32 * 1024;
const SESSION_DURATION: Duration = Duration::from_secs(3600);
const LOGIN_DURATION: Duration = Duration::from_secs(600);
const REQUEST_DURATION: Duration = Duration::from_secs(10);
const STYLE: &str = include_str!("style.css");
const SCRIPT: &str = include_str!("app.js");
const ROUTE_GROUPS: &str = include_str!("route-groups.js");
const LOGO: &str = include_str!("../server/guide/relay.svg");
const FAVICON: &str = include_str!("../server/guide/favicon.svg");
const FONT: &[u8] = include_bytes!("../server/guide/fonts/MapleMono-Regular.woff2");
const FONT_LICENSE: &str = include_str!("../server/guide/fonts/OFL.txt");

#[derive(Clone)]
struct Portal {
    service: Arc<service::KeyService>,
    authority: String,
    origin: String,
    login: Arc<Mutex<Login>>,
    mutations: Arc<tokio::sync::Semaphore>,
}

struct Login {
    bootstrap: Option<[u8; 32]>,
    deadline: Instant,
    failures: u8,
    session: Option<([u8; 32], Instant)>,
}

pub(crate) async fn run(
    arguments: &[String],
    catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
    output: &mut impl FnMut(String),
) -> Result<String, String> {
    let mut path = None;
    let mut port = None;
    for pair in arguments.chunks(2) {
        match pair {
            [flag, value] if flag == "--config" && path.is_none() => {
                path = Some(PathBuf::from(value));
            }
            [flag, value] if flag == "--port" && port.is_none() => {
                port = Some(value.parse::<u16>().map_err(|_| USAGE.to_owned())?);
            }
            _ => return Err(USAGE.into()),
        }
    }
    let path = path.ok_or(USAGE)?;
    service::validate_location(&path)?;
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port.unwrap_or(9091)))
        .await
        .map_err(|_| "cannot bind private portal to 127.0.0.1; choose another --port")?;
    let address = listener
        .local_addr()
        .map_err(|_| "portal address unavailable")?;
    let (portal, code) = Portal::new(path, catalog, address.port())?;
    output(format!(
        "Private key portal: {}\nOne-use login code (expires in 10 minutes): {code}\nOpen this address on this device and enter the code. Keep this terminal private.\nPress Enter for a new login code, or Ctrl-C to stop. Sessions expire after one hour.",
        portal.origin
    ));
    let (send, renewals) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(line) if line.trim().is_empty() => {
                    if send.send(()).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    serve(listener, portal, renewals, output).await?;
    Ok(String::new())
}

async fn serve(
    listener: tokio::net::TcpListener,
    portal: Portal,
    mut renewals: tokio::sync::mpsc::UnboundedReceiver<()>,
    output: &mut impl FnMut(String),
) -> Result<(), String> {
    let router = portal.clone().router();
    let capacity = Arc::new(tokio::sync::Semaphore::new(32));
    let mut connections = tokio::task::JoinSet::new();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(()) = renewals.recv() => {
                match portal.renew_code() {
                    Ok(code) => output(format!("One-use login code (expires in 10 minutes): {code}")),
                    Err(error) => output(error),
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let (socket, peer) = accepted.map_err(|_| "portal accept failed")?;
                if !matches!(peer, SocketAddr::V4(peer) if peer.ip().is_loopback()) {
                    continue;
                }
                let Ok(permit) = capacity.clone().try_acquire_owned() else { continue };
                let service = TowerToHyperService::new(router.clone());
                connections.spawn(async move {
                    let _permit = permit;
                    let _ = hyper::server::conn::http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(5))
                        .keep_alive(false)
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

impl Portal {
    fn new(
        path: PathBuf,
        catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
        port: u16,
    ) -> Result<(Self, String), String> {
        let code = random_secret()?;
        Ok((
            Self {
                service: Arc::new(service::KeyService { path, catalog }),
                authority: format!("127.0.0.1:{port}"),
                origin: format!("http://127.0.0.1:{port}"),
                login: Arc::new(Mutex::new(Login {
                    bootstrap: Some(digest(&code)),
                    deadline: Instant::now() + LOGIN_DURATION,
                    failures: 0,
                    session: None,
                })),
                mutations: Arc::new(tokio::sync::Semaphore::new(1)),
            },
            code,
        ))
    }

    fn router(self) -> Router {
        Router::new()
            .route("/", get(page))
            .route("/app.js", get(script))
            .route("/route-groups.js", get(route_groups))
            .route("/style.css", get(style))
            .route("/favicon.svg", get(favicon))
            .route("/fonts/MapleMono-Regular.woff2", get(font))
            .route("/api/login", post(login))
            .route("/api/snapshot", post(snapshot))
            .route("/api/change", post(change))
            .route("/api/logout", post(logout))
            .route("/api/login-code", post(login_code))
            .fallback(|| async { failure(StatusCode::NOT_FOUND, "Not found") })
            .layer(middleware::from_fn_with_state(self.clone(), guard))
            .with_state(self)
    }

    fn authenticated(&self, headers: &HeaderMap) -> bool {
        let mut tokens = headers.get_all(header::AUTHORIZATION).iter();
        let Some(token) = tokens
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };
        if tokens.next().is_some() {
            return false;
        }
        let Ok(login) = self.login.lock() else {
            return false;
        };
        login
            .session
            .is_some_and(|(expected, expires)| Instant::now() < expires && equal(&expected, token))
    }

    fn renew_code(&self) -> Result<String, String> {
        let code = random_secret()?;
        let mut login = self.login.lock().map_err(|_| "Cannot renew login code")?;
        login.bootstrap = Some(digest(&code));
        login.deadline = Instant::now() + LOGIN_DURATION;
        login.failures = 0;
        Ok(code)
    }
}

async fn guard(State(portal): State<Portal>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let api = request.uri().path().starts_with("/api/");
    let valid_host = exactly(headers, header::HOST.as_str(), &portal.authority)
        && request
            .uri()
            .authority()
            .is_none_or(|value| value.as_str() == portal.authority)
        && request.uri().query().is_none();
    let browser = !api
        || (request.method() == Method::POST
            && exactly(headers, header::ORIGIN.as_str(), &portal.origin)
            && exactly(headers, "x-kanata-portal", "1")
            && headers
                .get("sec-fetch-site")
                .is_none_or(|value| value == "same-origin")
            && exactly(headers, header::CONTENT_TYPE.as_str(), "application/json"));
    let mut response = if !valid_host || !browser {
        failure(
            StatusCode::FORBIDDEN,
            "This portal accepts requests only from its local browser origin",
        )
    } else if api && request.uri().path() != "/api/login" && !portal.authenticated(headers) {
        failure(
            StatusCode::UNAUTHORIZED,
            "Unlock this portal with the code from its terminal",
        )
    } else {
        match tokio::time::timeout(REQUEST_DURATION, next.run(request)).await {
            Ok(response) => response,
            Err(_) => failure(
                StatusCode::REQUEST_TIMEOUT,
                "Portal request timed out; refresh before retrying a change",
            ),
        }
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; img-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    ));
    response
}

fn exactly(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    let mut values = headers.get_all(name).iter();
    values.next().is_some_and(|value| value == expected) && values.next().is_none()
}

async fn page() -> Html<String> {
    Html(
        include_str!("index.html")
            .replace("{{LOGO}}", LOGO)
            .replace("{{FONT_LICENSE}}", FONT_LICENSE),
    )
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        SCRIPT,
    )
}
async fn style() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE)
}
async fn route_groups() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        ROUTE_GROUPS,
    )
}
async fn favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], FAVICON)
}
async fn font() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/woff2")], FONT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    code: String,
}

async fn login(State(portal): State<Portal>, body: Body) -> Response {
    let Ok(bytes) = to_bytes(body, 4096).await else {
        return failure(StatusCode::BAD_REQUEST, "Invalid login request");
    };
    let Ok(request) = serde_json::from_slice::<LoginRequest>(&bytes) else {
        return failure(StatusCode::BAD_REQUEST, "Invalid login request");
    };
    let Ok(token) = random_secret() else {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, "Cannot create session");
    };
    let Ok(mut login) = portal.login.lock() else {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, "Cannot unlock portal");
    };
    if Instant::now() >= login.deadline || login.failures >= 10 || login.bootstrap.is_none() {
        return failure(
            StatusCode::UNAUTHORIZED,
            "This code is unavailable or expired; press Enter in the portal terminal for a new code",
        );
    }
    if !login
        .bootstrap
        .is_some_and(|expected| equal(&expected, &request.code))
    {
        login.failures = login.failures.saturating_add(1);
        return failure(StatusCode::UNAUTHORIZED, "Incorrect login code");
    }
    login.bootstrap = None;
    login.session = Some((digest(&token), Instant::now() + SESSION_DURATION));
    Json(json!({"ok": true, "session": token})).into_response()
}

async fn snapshot(State(portal): State<Portal>) -> Response {
    let service = portal.service.clone();
    match tokio::task::spawn_blocking(move || service.snapshot()).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => failure(StatusCode::BAD_REQUEST, &error),
        Err(_) => failure(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read keys"),
    }
}

async fn change(State(portal): State<Portal>, body: Body) -> Response {
    let Ok(bytes) = to_bytes(body, BODY_LIMIT).await else {
        return failure(StatusCode::BAD_REQUEST, "Key change is too large");
    };
    let Ok(request) = serde_json::from_slice::<service::Change>(&bytes) else {
        return failure(StatusCode::BAD_REQUEST, "Invalid key change");
    };
    let Ok(permit) = portal.mutations.clone().try_acquire_owned() else {
        return failure(
            StatusCode::CONFLICT,
            "Another key change is running; refresh and try again",
        );
    };
    let service = portal.service.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        service.change(request)
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => failure(StatusCode::BAD_REQUEST, &error),
        Err(_) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot change key; refresh before retrying",
        ),
    }
}

async fn logout(State(portal): State<Portal>) -> Response {
    if let Ok(mut login) = portal.login.lock() {
        login.session = None;
    }
    Json(json!({"ok": true})).into_response()
}

async fn login_code(State(portal): State<Portal>) -> Response {
    match portal.renew_code() {
        Ok(code) => {
            Json(json!({"code":code,"expires_in":LOGIN_DURATION.as_secs()})).into_response()
        }
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

fn random_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| "operating system randomness is unavailable")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}
fn equal(expected: &[u8; 32], secret: &str) -> bool {
    bool::from(expected.ct_eq(&digest(secret)))
}
fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}
