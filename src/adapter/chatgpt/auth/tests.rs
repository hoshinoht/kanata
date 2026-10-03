use std::{
    collections::BTreeMap,
    fs, future,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

use super::record::{Identity, Session};
use super::*;

const JWKS: &[u8] = include_bytes!("../../../../tests/fixtures/chatgpt/test-only-jwks.json");
const PRIVATE_KEY: &[u8] = include_bytes!("../../../../tests/fixtures/chatgpt/test-only-rsa.pk8");
const CLIENT: &str = "oaiapp_test_only";
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "kanata-chatgpt-auth-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn claims(nonce: &str) -> Value {
    let now = unix_now().unwrap();
    json!({"iss":ISSUER,"sub":"test-only-subject","aud":CLIENT,"iat":now,"exp":now + 3600,
        "nonce":nonce,"email":"fixture@example.test"})
}
fn signed(claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"test-only-key"}"#);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
    let message = format!("{header}.{body}");
    let pair = RsaKeyPair::from_pkcs8(PRIVATE_KEY).unwrap();
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature))
}
fn token_response(nonce: &str) -> Value {
    json!({"access_token":"TEST_ONLY_ACCESS_REPLACEMENT", "refresh_token":"TEST_ONLY_REFRESH_REPLACEMENT",
        "id_token":signed(&claims(nonce)), "token_type":"Bearer", "expires_in":3600,
        "scope":"openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"})
}
fn registered(expires_in: u64) -> Registration {
    let now = unix_now().unwrap();
    Registration {
        client_id: CLIENT.into(),
        identity: Some(Identity {
            issuer: ISSUER.into(),
            subject: "test-only-subject".into(),
            email: Some("fixture@example.test".into()),
        }),
        session: Some(Session {
            access_token: "TEST_ONLY_ACCESS_ORIGINAL".into(),
            refresh_token: Some("TEST_ONLY_REFRESH_ORIGINAL".into()),
            id_token: signed(&claims("old-nonce")),
            scopes: vec![
                "resource.invoke".into(),
                "chatgpt.tokens.use.direct".into(),
                "offline_access".into(),
            ],
            saved_at: now - 100,
            expires_at: now + expires_in,
            earliest_refresh_at: None,
        }),
    }
}
async fn seed(manager: &AuthManager, registration: Registration) {
    let locked = manager.store.lock().await.unwrap();
    let mut state = State::new().unwrap();
    state.profiles.insert("default".into(), registration);
    state.active_profile = Some("default".into());
    locked.save(&state).unwrap();
}

type RequestLog = Arc<Mutex<Vec<(String, BTreeMap<String, String>)>>>;
struct Fixture {
    address: SocketAddr,
    certificate: CertificateDer<'static>,
    task: tokio::task::JoinHandle<()>,
    requests: RequestLog,
    tokens: Arc<Mutex<Value>>,
    token_status: Arc<AtomicU16>,
    revoke_status: Arc<AtomicU16>,
    pause_exchange: Arc<AtomicBool>,
    exchange_arrived: Arc<Notify>,
    release_exchange: Arc<Notify>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["auth.openai.com".into()]).unwrap();
        let certificate = cert.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let tokens = Arc::new(Mutex::new(token_response("fixture-nonce")));
        let token_status = Arc::new(AtomicU16::new(200));
        let revoke_status = Arc::new(AtomicU16::new(200));
        let pause_exchange = Arc::new(AtomicBool::new(false));
        let exchange_arrived = Arc::new(Notify::new());
        let release_exchange = Arc::new(Notify::new());
        let (log, response, status, revocation, pause, arrived, release) = (
            requests.clone(),
            tokens.clone(),
            token_status.clone(),
            revoke_status.clone(),
            pause_exchange.clone(),
            exchange_arrived.clone(),
            release_exchange.clone(),
        );
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(stream).await.unwrap();
                let (head, body) = read_request(&mut stream).await;
                let path = head
                    .lines()
                    .next()
                    .unwrap()
                    .split(' ')
                    .nth(1)
                    .unwrap()
                    .to_owned();
                assert!(
                    head.to_ascii_lowercase()
                        .contains("host: auth.openai.com\r\n")
                );
                assert!(!head.to_ascii_lowercase().contains("authorization:"));
                let fields = url::form_urlencoded::parse(&body)
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect::<BTreeMap<_, _>>();
                log.lock().unwrap().push((path.clone(), fields));
                let (status, body) = match path.as_str() {
                    "/.well-known/openid-configuration" => (
                        200,
                        serde_json::to_vec(&json!({"issuer":ISSUER,
                        "jwks_uri":"https://auth.openai.com/.well-known/jwks.json",
                        "revocation_endpoint":"https://auth.openai.com/api/accounts/oauth/revoke",
                        "id_token_signing_alg_values_supported":["RS256"]}))
                        .unwrap(),
                    ),
                    "/.well-known/jwks.json" => (200, JWKS.to_vec()),
                    "/api/accounts/oauth/token" => {
                        assert!(
                            head.to_ascii_lowercase()
                                .contains("content-type: application/x-www-form-urlencoded\r\n")
                        );
                        if pause.load(Ordering::SeqCst) {
                            arrived.notify_one();
                            release.notified().await;
                        }
                        (
                            status.load(Ordering::SeqCst),
                            serde_json::to_vec(&*response.lock().unwrap()).unwrap(),
                        )
                    }
                    "/api/accounts/oauth/revoke" => {
                        if pause.load(Ordering::SeqCst) {
                            arrived.notify_one();
                            release.notified().await;
                        }
                        (revocation.load(Ordering::SeqCst), Vec::new())
                    }
                    _ => panic!("unexpected fixture path"),
                };
                let header = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
        });
        Self {
            address,
            certificate,
            task,
            requests,
            tokens,
            token_status,
            revoke_status,
            pause_exchange,
            exchange_arrived,
            release_exchange,
        }
    }
    fn manager(&self, directory: &Temp) -> AuthManager {
        let config = super::super::tests::config();
        AuthManager {
            store: store::Store::new(&directory.0),
            client: Arc::new(net::Client::with_test_root(
                config.timeouts(),
                self.certificate.clone(),
                self.address,
            )),
        }
    }
    fn count(&self, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .count()
    }
}
async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 1024];
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(bytes.len() < 32 * 1024);
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .unwrap()
        .1
        .trim()
        .parse::<usize>()
        .unwrap();
    while bytes.len() < header_end + length {
        let mut buffer = [0; 1024];
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&buffer[..read]);
    }
    (headers, bytes[header_end..].to_vec())
}
async fn callback(url: &url::Url, overrides: &[(&str, &str)]) -> String {
    let params: BTreeMap<_, _> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut redirect = url::Url::parse(&params["redirect_uri"]).unwrap();
    let mut values = BTreeMap::from([
        ("state", params["state"].as_str()),
        ("code", "TEST_ONLY_CODE"),
        ("client_id", CLIENT),
    ]);
    for (key, value) in overrides {
        values.insert(*key, *value);
    }
    redirect.query_pairs_mut().extend_pairs(values);
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
        .await
        .unwrap();
    let request = format!(
        "GET {}?{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        redirect.path(),
        redirect.query().unwrap(),
        redirect.port().unwrap()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).await.unwrap();
    body
}

#[test]
fn signed_identity_requires_signature_issuer_audience_nonce_and_valid_times() {
    let claims = claims("nonce");
    let now = unix_now().unwrap();
    assert!(jwt::verify(&signed(&claims), JWKS, CLIENT, Some("nonce"), "access", now).is_ok());
    for (key, value) in [
        ("iss", json!("https://attacker.invalid")),
        ("aud", json!("other-client")),
        ("sub", json!("")),
        ("nonce", json!("wrong")),
        ("exp", json!(now - 1)),
        ("iat", json!(now + 600)),
        ("nbf", json!(now + 600)),
        ("azp", json!("other-client")),
    ] {
        let mut invalid = claims.clone();
        invalid[key] = value;
        assert!(
            jwt::verify(
                &signed(&invalid),
                JWKS,
                CLIENT,
                Some("nonce"),
                "access",
                now
            )
            .is_err()
        );
    }
    let token = signed(&claims);
    let mut parts = token.split('.').map(str::to_owned).collect::<Vec<_>>();
    parts[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":"attacker"})).unwrap());
    assert!(jwt::verify(&parts.join("."), JWKS, CLIENT, Some("nonce"), "access", now).is_err());
    let bad_key = serde_json::from_slice::<Value>(JWKS).unwrap();
    let duplicate = json!({"keys":[bad_key["keys"][0],bad_key["keys"][0]]});
    assert!(
        jwt::verify(
            &token,
            &serde_json::to_vec(&duplicate).unwrap(),
            CLIENT,
            Some("nonce"),
            "access",
            now
        )
        .is_err()
    );
    let mut multiple = claims.clone();
    multiple["aud"] = json!([CLIENT, "other"]);
    assert!(
        jwt::verify(
            &signed(&multiple),
            JWKS,
            CLIENT,
            Some("nonce"),
            "access",
            now
        )
        .is_err()
    );
    multiple["azp"] = json!(CLIENT);
    assert!(
        jwt::verify(
            &signed(&multiple),
            JWKS,
            CLIENT,
            Some("nonce"),
            "access",
            now
        )
        .is_ok()
    );
}

#[tokio::test]
async fn signed_login_binds_pkce_callback_identity_and_persists_registration() {
    let fixture = Fixture::new().await;
    let temp = Temp::new();
    let manager = fixture.manager(&temp);
    let flow = manager.begin_login("default").await.unwrap();
    let url = url::Url::parse(flow.authorization_url()).unwrap();
    let params = url.query_pairs().collect::<BTreeMap<_, _>>();
    assert_eq!(params["client_id"], "dynamic_agent_client");
    assert_eq!(params["agent_name_hint"], "Kanata");
    assert!(!params.contains_key("id_token_hint"));
    assert!(!params.contains_key("code_verifier"));
    *fixture.tokens.lock().unwrap() = token_response(&params["nonce"]);
    let host = params["ext_agent_host_id"].to_string();
    let challenge = params["code_challenge"].to_string();
    let task = tokio::spawn(flow.finish(future::pending()));
    assert!(
        callback(&url, &[("state", "wrong")])
            .await
            .starts_with("HTTP/1.1 400")
    );
    assert!(callback(&url, &[]).await.starts_with("HTTP/1.1 200"));
    let status = task.await.unwrap().unwrap();
    assert!(status.plan_enabled);
    assert!(status.signed_in);
    {
        let requests = fixture.requests.lock().unwrap();
        let fields = &requests
            .iter()
            .find(|(p, _)| p == "/api/accounts/oauth/token")
            .unwrap()
            .1;
        assert_eq!(fields["client_id"], CLIENT);
        assert_eq!(fields["resource"], net::RESOURCE);
        assert_eq!(fields["redirect_uri"], params["redirect_uri"]);
        use sha2::{Digest, Sha256};
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(fields["code_verifier"].as_bytes())),
            challenge
        );
    }
    let restarted = fixture.manager(&temp);
    assert_eq!(
        restarted.access_token().await.unwrap().bearer(),
        "TEST_ONLY_ACCESS_REPLACEMENT"
    );
    let next = restarted.begin_login("default").await.unwrap();
    let nexturl = url::Url::parse(next.authorization_url()).unwrap();
    let nextparams = nexturl.query_pairs().collect::<BTreeMap<_, _>>();
    assert_eq!(nextparams["client_id"], CLIENT);
    assert_eq!(nextparams["ext_agent_host_id"], host);
    assert!(host.starts_with("urn:uuid:"));
    assert_eq!(host.len(), 45);
    assert!(!nextparams.contains_key("agent_name_hint"));
    assert_ne!(nextparams["state"], params["state"]);
    assert!(matches!(
        next.finish(async {}).await,
        Err(AuthError::Cancelled)
    ));
    assert_eq!(
        fs::metadata(temp.0.join("chatgpt-v1.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let status = serde_json::to_string(&restarted.status().await.unwrap()).unwrap();
    assert!(!status.contains("TEST_ONLY"));
}

#[tokio::test]
async fn refresh_is_serialized_across_instances_and_survives_caller_cancellation() {
    let fixture = Fixture::new().await;
    let temp = Temp::new();
    let manager = fixture.manager(&temp);
    seed(&manager, registered(1)).await;
    fixture.pause_exchange.store(true, Ordering::SeqCst);
    let other = fixture.manager(&temp);
    let cancelled = tokio::spawn(async move { other.access_token().await });
    tokio::time::timeout(Duration::from_secs(5), fixture.exchange_arrived.notified())
        .await
        .unwrap();
    cancelled.abort();
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let other = fixture.manager(&temp);
        tasks.push(tokio::spawn(async move {
            other.access_token().await.unwrap().bearer().to_owned()
        }));
    }
    fixture.release_exchange.notify_one();
    for task in tasks {
        assert_eq!(task.await.unwrap(), "TEST_ONLY_ACCESS_REPLACEMENT");
    }
    assert_eq!(fixture.count("/api/accounts/oauth/token"), 1);
    {
        let requests = fixture.requests.lock().unwrap();
        let form = &requests
            .iter()
            .find(|(path, _)| path == "/api/accounts/oauth/token")
            .unwrap()
            .1;
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], "TEST_ONLY_REFRESH_ORIGINAL");
        assert!(!form.contains_key("scope"));
    }
    let locked = manager.store.lock().await.unwrap();
    let state = locked.load().unwrap().unwrap();
    assert_eq!(
        state.profiles["default"]
            .session
            .as_ref()
            .unwrap()
            .refresh_token
            .as_deref(),
        Some("TEST_ONLY_REFRESH_REPLACEMENT")
    );
}

#[tokio::test]
async fn declined_plan_permission_terminal_refresh_and_remote_logout_are_explicit() {
    let fixture = Fixture::new().await;
    let temp = Temp::new();
    let manager = fixture.manager(&temp);
    let mut registration = registered(3600);
    registration.session.as_mut().unwrap().scopes = vec!["openid".into()];
    seed(&manager, registration).await;
    assert!(matches!(
        manager.access_token().await,
        Err(AuthError::PermissionDenied)
    ));
    assert_eq!(fixture.count("/api/accounts/oauth/token"), 0);
    seed(&manager, registered(1)).await;
    *fixture.tokens.lock().unwrap() =
        json!({"error":"invalid_grant","error_description":"DO_NOT_EXPOSE_PROVIDER_TEXT"});
    fixture.token_status.store(400, Ordering::SeqCst);
    assert!(matches!(
        manager.access_token().await,
        Err(AuthError::InvalidGrant)
    ));
    let status = manager.status().await.unwrap();
    assert!(!status.profiles[0].signed_in);
    assert_eq!(status.profiles[0].client_id, CLIENT);
    assert!(status.active_profile.is_none());
    seed(&manager, registered(3600)).await;
    let before = manager
        .store
        .lock()
        .await
        .unwrap()
        .load()
        .unwrap()
        .unwrap()
        .host_id;
    let outcome = manager.logout(None).await.unwrap();
    assert!(outcome.remote_revocation_confirmed);
    let after = manager.store.lock().await.unwrap().load().unwrap().unwrap();
    assert_eq!(after.host_id, before);
    assert!(after.profiles["default"].session.is_none());
    assert!(after.profiles["default"].identity.is_some());
    assert!(matches!(
        manager.access_token().await,
        Err(AuthError::NotSignedIn)
    ));
    {
        let log = fixture.requests.lock().unwrap();
        let form = &log
            .iter()
            .find(|(path, _)| path == "/api/accounts/oauth/revoke")
            .unwrap()
            .1;
        assert_eq!(form["client_id"], CLIENT);
        assert_eq!(form["token_type_hint"], "refresh_token");
    }
    seed(&manager, registered(3600)).await;
    fixture.revoke_status.store(503, Ordering::SeqCst);
    let outcome = manager.logout(None).await.unwrap();
    assert!(!outcome.remote_revocation_confirmed);
    assert_eq!(fixture.count("/api/accounts/oauth/revoke"), 4);
}

#[tokio::test]
async fn storage_and_discovery_reject_unsafe_paths_and_malformed_state() {
    for value in [
        "http://auth.openai.com/jwks",
        "https://attacker.invalid/jwks",
        "https://auth.openai.com:8443/jwks",
        "https://user@auth.openai.com/jwks",
        "https://auth.openai.com/jwks?x=1",
        "https://auth.openai.com/jwks#fragment",
        "https://auth.openai.com/%2e%2e/jwks",
    ] {
        assert!(net::discovered_endpoint(value).is_err());
    }
    let temp = Temp::new();
    let store = store::Store::new(&temp.0);
    let locked = store.lock().await.unwrap();
    locked.save(&State::new().unwrap()).unwrap();
    drop(locked);
    let path = temp.0.join("chatgpt-v1.json");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.lock().await.unwrap().load().is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, "malformed").unwrap();
    assert!(store.lock().await.unwrap().load().is_err());
    fs::remove_file(&path).unwrap();
    let outside = Temp::new();
    let target = outside.0.join("sentinel");
    fs::write(&target, "untouched").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(store.lock().await.unwrap().load().is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
}

#[tokio::test]
async fn callbacks_and_returning_identity_cannot_overwrite_selected_registration() {
    let fixture = Fixture::new().await;
    let temp = Temp::new();
    let manager = fixture.manager(&temp);
    let flow = manager.begin_login("new").await.unwrap();
    let url = url::Url::parse(flow.authorization_url()).unwrap();
    let task = tokio::spawn(flow.finish(future::pending()));
    callback(&url, &[("error", "access_denied")]).await;
    assert!(matches!(task.await.unwrap(), Err(AuthError::ConsentDenied)));
    assert_eq!(fixture.count("/api/accounts/oauth/token"), 0);
    let flow = manager.begin_login("new").await.unwrap();
    let url = url::Url::parse(flow.authorization_url()).unwrap();
    *fixture.tokens.lock().unwrap() = json!({"error":"invalid_grant"});
    fixture.token_status.store(400, Ordering::SeqCst);
    let task = tokio::spawn(flow.finish(future::pending()));
    callback(&url, &[]).await;
    assert!(matches!(task.await.unwrap(), Err(AuthError::InvalidGrant)));
    let retained = manager.status().await.unwrap();
    assert_eq!(retained.profiles[0].client_id, CLIENT);
    assert!(!retained.profiles[0].signed_in);
    assert!(retained.active_profile.is_none());
    let returning = manager.begin_login("new").await.unwrap();
    assert!(returning.authorization_url().contains(CLIENT));
    drop(returning);

    seed(&manager, registered(3600)).await;
    let flow = manager.begin_login("default").await.unwrap();
    let url = url::Url::parse(flow.authorization_url()).unwrap();
    let nonce = url
        .query_pairs()
        .find(|(key, _)| key == "nonce")
        .unwrap()
        .1
        .into_owned();
    let mut response = token_response(&nonce);
    let mut changed = claims(&nonce);
    changed["sub"] = json!("different-subject");
    response["id_token"] = json!(signed(&changed));
    *fixture.tokens.lock().unwrap() = response;
    fixture.token_status.store(200, Ordering::SeqCst);
    let task = tokio::spawn(flow.finish(future::pending()));
    assert!(
        callback(&url, &[("client_id", "wrong_client")])
            .await
            .starts_with("HTTP/1.1 400")
    );
    callback(&url, &[]).await;
    assert!(matches!(
        task.await.unwrap(),
        Err(AuthError::InvalidIdentity)
    ));
    assert_eq!(
        manager.access_token().await.unwrap().bearer(),
        "TEST_ONLY_ACCESS_ORIGINAL"
    );

    let flow = manager.begin_login("default").await.unwrap();
    let url = url::Url::parse(flow.authorization_url()).unwrap();
    let nonce = url
        .query_pairs()
        .find(|(key, _)| key == "nonce")
        .unwrap()
        .1
        .into_owned();
    let mut response = token_response(&nonce);
    response["scope"] = json!("openid profile email");
    *fixture.tokens.lock().unwrap() = response;
    let task = tokio::spawn(flow.finish(future::pending()));
    callback(&url, &[]).await;
    let status = task.await.unwrap().unwrap();
    assert!(status.signed_in);
    assert!(!status.plan_enabled);
    assert!(matches!(
        manager.access_token().await,
        Err(AuthError::PermissionDenied)
    ));
}

#[test]
fn token_response_requires_bounded_expiry_and_rotated_refresh_token() {
    let now = unix_now().unwrap();
    for (field, value) in [
        ("expires_in", json!(0)),
        ("expires_in", json!(86_401)),
        ("expires_in", json!(-1)),
        ("earliest_refresh_at", json!(now + 4000)),
        ("token_type", json!("MAC")),
        ("access_token", json!("bad\nvalue")),
        ("refresh_token", Value::Null),
    ] {
        let mut response = token_response("nonce");
        response[field] = value;
        let parsed = serde_json::from_value::<net::TokenResponse>(response);
        assert!(parsed.is_err() || parsed.unwrap().session(now, None, true).is_err());
    }
    let mut response = token_response("nonce");
    response.as_object_mut().unwrap().remove("id_token");
    let parsed = serde_json::from_value::<net::TokenResponse>(response.clone()).unwrap();
    assert!(parsed.session(now, None, false).is_err());
    let parsed = serde_json::from_value::<net::TokenResponse>(response).unwrap();
    assert_eq!(
        parsed
            .session(now, Some("RETAINED_TEST_ID"), true)
            .unwrap()
            .id_token,
        "RETAINED_TEST_ID"
    );
    let parsed = serde_json::from_value::<net::TokenResponse>(token_response("nonce")).unwrap();
    assert!(parsed.session(u64::MAX, None, false).is_err());
}

#[tokio::test]
async fn clock_rollback_and_earliest_refresh_time_do_not_dispatch_refresh() {
    let fixture = Fixture::new().await;
    let temp = Temp::new();
    let manager = fixture.manager(&temp);
    let mut registration = registered(3600);
    registration.session.as_mut().unwrap().saved_at = unix_now().unwrap() + 100;
    seed(&manager, registration).await;
    assert!(matches!(
        manager.access_token().await,
        Err(AuthError::Clock)
    ));
    let mut registration = registered(30);
    registration.session.as_mut().unwrap().earliest_refresh_at = Some(unix_now().unwrap() + 20);
    seed(&manager, registration).await;
    assert_eq!(
        manager.access_token().await.unwrap().bearer(),
        "TEST_ONLY_ACCESS_ORIGINAL"
    );
    assert_eq!(fixture.count("/api/accounts/oauth/token"), 0);
}

#[tokio::test]
async fn cancelled_logout_finishes_revocation_and_preserves_recovery_until_then() {
    let fixture = Fixture::new().await;
    let temp = Temp::new();
    let manager = fixture.manager(&temp);
    seed(&manager, registered(3600)).await;
    fixture.pause_exchange.store(true, Ordering::SeqCst);
    let other = fixture.manager(&temp);
    let task = tokio::spawn(async move { other.logout(None).await });
    tokio::time::timeout(Duration::from_secs(5), fixture.exchange_arrived.notified())
        .await
        .unwrap();
    task.abort();
    let during = State::decode(&fs::read(temp.0.join("chatgpt-v1.json")).unwrap()).unwrap();
    assert!(during.active_profile.is_none());
    assert!(during.profiles["default"].session.is_some());
    fixture.release_exchange.notify_one();
    let status = manager.status().await.unwrap();
    assert!(!status.profiles[0].signed_in);
    assert!(status.active_profile.is_none());
    assert_eq!(fixture.count("/api/accounts/oauth/revoke"), 1);
}
