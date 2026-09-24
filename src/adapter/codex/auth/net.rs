use std::{future::Future, time::Duration};

use futures_util::StreamExt;
use http::Method;
use serde::{Deserialize, Serialize};

use crate::{
    adapter::diagnostics::token_refresh_failed,
    adapter::transport::{
        AUTH_RESPONSE_BYTES, Accept, Endpoint, ResponseContentType, Transport, TransportRequest,
    },
    config::ValidatedTimeouts,
    core::GatewayError,
};

use super::{
    Credential, DeviceAuthorization, DeviceCodeRequest, DevicePollFailure, DevicePollRequest,
    DevicePollResponse, LoginError, LoginExchangeFailure, LoginTokenResponse, RefreshExchangeError,
    RefreshResponse, normalize_authorization_code_response, normalize_refresh_response,
};

const AUTH_HOST: &str = "auth.openai.com";
const PROVIDER: &str = "codex";
const REQUEST_BODY_BYTES: usize = 16 * 1024;

pub(crate) struct CodexAuthClient {
    transport: Transport,
    overall_timeout: Duration,
}

impl CodexAuthClient {
    /// Construct the fixed-issuer auth client without making a network request.
    pub(crate) fn new(timeouts: &ValidatedTimeouts) -> Result<Self, GatewayError> {
        Ok(Self {
            transport: Transport::new_pinned_https(AUTH_HOST, timeouts)?,
            overall_timeout: Duration::from_millis(timeouts.overall_ms()),
        })
    }

    /// Request and validate the first-stage device code.
    pub(crate) async fn request_device_authorization<C>(
        &self,
        cancellation: C,
    ) -> Result<DeviceAuthorization, LoginError>
    where
        C: Future<Output = ()>,
    {
        let code_request = DeviceCodeRequest::new()?;
        let body = ClientIdRequest {
            client_id: super::CODEX_CLIENT_ID,
        };
        let request = TransportRequest::json(
            Method::POST,
            endpoint(&["api", "accounts", "deviceauth", "usercode"])
                .map_err(|_| LoginError::InvalidDeviceResponse)?,
            &body,
            None,
            Some(Accept::Json),
            REQUEST_BODY_BYTES,
            AUTH_RESPONSE_BYTES,
        )
        .map_err(|_| LoginError::InvalidDeviceResponse)?;

        let response = tokio::select! {
            biased;
            _ = cancellation => return Err(LoginError::Cancelled),
            response = self.execute(request) => {
                response.map_err(|_| LoginError::InvalidDeviceResponse)?
            }
        };
        if response.status != 200 || response.content_type != Some(ResponseContentType::Json) {
            return Err(LoginError::InvalidDeviceResponse);
        }
        code_request.accept_response(&response.body)
    }

    /// Send one device authorization poll; the login coordinator owns its interval and deadline.
    pub(crate) async fn poll_device_authorization(
        &self,
        request: DevicePollRequest,
    ) -> Result<DevicePollResponse, DevicePollFailure> {
        let payload = request.body().as_bytes();
        if payload.len() > REQUEST_BODY_BYTES {
            return Err(DevicePollFailure);
        }
        let body: DevicePollPayload =
            serde_json::from_slice(payload).map_err(|_| DevicePollFailure)?;
        let request = TransportRequest::sensitive_json(
            Method::POST,
            endpoint(&["api", "accounts", "deviceauth", "token"]).map_err(|_| DevicePollFailure)?,
            &body,
            REQUEST_BODY_BYTES,
            AUTH_RESPONSE_BYTES,
        )
        .map_err(|_| DevicePollFailure)?;
        let response = self.execute(request).await.map_err(|_| DevicePollFailure)?;
        if (200..300).contains(&response.status)
            && response.content_type != Some(ResponseContentType::Json)
        {
            return Err(DevicePollFailure);
        }
        Ok(DevicePollResponse::new(response.status, response.body))
    }

    /// Exchange the validated one-time authorization code at the pinned token endpoint.
    pub(crate) async fn exchange_authorization_code(
        &self,
        request: super::AuthorizationCodeRequest,
    ) -> Result<LoginTokenResponse, LoginExchangeFailure> {
        let form = request.form_body();
        let request = TransportRequest::form_encoded(
            Method::POST,
            endpoint(&["oauth", "token"]).map_err(|_| LoginExchangeFailure)?,
            form.as_str(),
            REQUEST_BODY_BYTES,
            AUTH_RESPONSE_BYTES,
        )
        .map_err(|_| LoginExchangeFailure)?;
        let response = self
            .execute(request)
            .await
            .map_err(|_| LoginExchangeFailure)?;
        if response.status != 200 || response.content_type != Some(ResponseContentType::Json) {
            return Err(LoginExchangeFailure);
        }
        normalize_authorization_code_response(&response.body).map_err(|_| LoginExchangeFailure)
    }

    /// Refresh using only the stored refresh token and account identity.
    pub(crate) async fn refresh(
        &self,
        credential: Credential,
    ) -> Result<RefreshResponse, RefreshExchangeError> {
        let body = RefreshRequest {
            client_id: super::CODEX_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: credential.refresh_token(),
        };
        let request = TransportRequest::sensitive_json(
            Method::POST,
            endpoint(&["oauth", "token"]).map_err(|_| RefreshExchangeError)?,
            &body,
            REQUEST_BODY_BYTES,
            AUTH_RESPONSE_BYTES,
        )
        .map_err(|_| RefreshExchangeError)?;
        let response = self.execute(request).await.map_err(|_| {
            token_refresh_failed(PROVIDER, "transport", None, None);
            RefreshExchangeError
        })?;
        if response.status != 200 {
            token_refresh_failed(
                PROVIDER,
                "http_status",
                Some(response.status),
                Some(&response.body),
            );
            return Err(RefreshExchangeError);
        }
        // Success bodies carry tokens, so later failures log only a category.
        if response.content_type != Some(ResponseContentType::Json) {
            token_refresh_failed(PROVIDER, "content_type", Some(response.status), None);
            return Err(RefreshExchangeError);
        }
        normalize_refresh_response(&response.body, credential.account_id()).map_err(|_| {
            token_refresh_failed(PROVIDER, "invalid_response", Some(response.status), None);
            RefreshExchangeError
        })
    }

    async fn execute(&self, request: TransportRequest) -> Result<AuthResponse, NetFailure> {
        tokio::time::timeout(self.overall_timeout, async {
            let mut response = self
                .transport
                .execute(request)
                .await
                .map_err(|_| NetFailure)?;
            let status = response.status;
            let content_type = response.content_type;
            let mut body = Vec::new();
            while let Some(chunk) = response.body.next().await {
                let chunk = chunk.map_err(|_| NetFailure)?;
                let next_len = body.len().checked_add(chunk.len()).ok_or(NetFailure)?;
                if next_len > AUTH_RESPONSE_BYTES {
                    return Err(NetFailure);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(AuthResponse {
                status,
                content_type,
                body,
            })
        })
        .await
        .map_err(|_| NetFailure)?
    }

    #[cfg(test)]
    fn with_test_root(
        timeouts: &ValidatedTimeouts,
        certificate: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
        address: std::net::SocketAddr,
        overall_timeout: Duration,
    ) -> Result<Self, GatewayError> {
        Ok(Self {
            transport: Transport::new_pinned_https_with_test_root(
                AUTH_HOST,
                timeouts,
                certificate,
                address,
            )?,
            overall_timeout,
        })
    }
}

#[derive(Serialize)]
struct ClientIdRequest {
    client_id: &'static str,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DevicePollPayload {
    device_auth_id: String,
    user_code: String,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

struct AuthResponse {
    status: u16,
    content_type: Option<ResponseContentType>,
    body: Vec<u8>,
}

#[derive(Clone, Copy)]
struct NetFailure;

fn endpoint(segments: &'static [&'static str]) -> Result<Endpoint, NetFailure> {
    Endpoint::new(segments).map_err(|_| NetFailure)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs, future,
        net::SocketAddr,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use rcgen::generate_simple_self_signed;
    use sha2::{Digest, Sha256};
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
    };
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{
            ServerConfig,
            pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        },
    };

    use crate::{
        adapter::codex::auth::{
            CODEX_CLIENT_ID, Credential, LoginError, RefreshExchangeError, complete_device_login,
        },
        config,
    };

    use super::{AUTH_HOST, AUTH_RESPONSE_BYTES, CodexAuthClient};

    static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

    const TEST_REFRESH_TOKEN: &str = "TEST_ONLY_REFRESH_TOKEN_NOT_SECRET_01";
    const TEST_ACCOUNT_ID: &str = "TEST_ONLY_ACCOUNT_ID_NOT_SECRET_01";
    const TEST_ACCESS_TOKEN: &str = "TEST_ONLY_ACCESS_TOKEN_NOT_SECRET_01";
    const TEST_DEVICE_ID: &str = "TEST_ONLY_DEVICE_AUTH_ID_NOT_SECRET_01";
    const TEST_USER_CODE: &str = "TEST-ONLY-CODE-1234";
    const TEST_AUTHORIZATION_CODE: &str = "TEST_ONLY_AUTHORIZATION_CODE_NOT_SECRET_01";
    const TEST_VERIFIER: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn timeouts() -> config::ValidatedTimeouts {
        let id = NEXT_CONFIG.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kanata-codex-auth-net-{}-{id}.toml",
            std::process::id()
        ));
        fs::write(
            &path,
            include_str!("../../../../tests/fixtures/config/example.toml"),
        )
        .unwrap_or_else(|_| panic!("write fixture config"));
        let loaded = config::load(&path).unwrap_or_else(|_| panic!("load fixture config"));
        let _ = fs::remove_file(path);
        loaded.timeouts().clone()
    }

    fn tls_fixture(hostname: &str) -> (TlsAcceptor, CertificateDer<'static>) {
        let certified = generate_simple_self_signed(vec![hostname.to_owned()])
            .unwrap_or_else(|_| panic!("fixture certificate"));
        let certificate = CertificateDer::from(certified.cert.der().to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.signing_key.serialize_der(),
        ));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key)
            .unwrap_or_else(|_| panic!("server config"));
        (TlsAcceptor::from(std::sync::Arc::new(config)), certificate)
    }

    async fn listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|_| panic!("listener"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|_| panic!("listener address"));
        (listener, address)
    }

    fn client(
        certificate: CertificateDer<'static>,
        address: SocketAddr,
        overall_timeout: Duration,
    ) -> CodexAuthClient {
        CodexAuthClient::with_test_root(&timeouts(), certificate, address, overall_timeout)
            .unwrap_or_else(|_| panic!("test client"))
    }

    struct CapturedRequest {
        headers: String,
        body: Vec<u8>,
    }

    async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> CapturedRequest {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 1024];
            let count = stream
                .read(&mut chunk)
                .await
                .unwrap_or_else(|_| panic!("request bytes"));
            assert_ne!(count, 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
            assert!(bytes.len() <= 64 * 1024, "request headers bounded");
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec())
            .unwrap_or_else(|_| panic!("request headers utf8"));
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_else(|| panic!("content length"));
        let total = header_end + content_length;
        while bytes.len() < total {
            let mut chunk = [0_u8; 1024];
            let count = stream
                .read(&mut chunk)
                .await
                .unwrap_or_else(|_| panic!("request body"));
            assert_ne!(count, 0, "request ended before body");
            bytes.extend_from_slice(&chunk[..count]);
        }
        assert_eq!(bytes.len(), total, "one exact request body");
        CapturedRequest {
            headers,
            body: bytes[header_end..].to_vec(),
        }
    }

    fn assert_request(request: &CapturedRequest, path: &str, content_type: &str, body: &str) {
        let mut lines = request.headers.lines();
        assert_eq!(lines.next(), Some(format!("POST {path} HTTP/1.1").as_str()));
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect::<BTreeMap<_, _>>();
        let names = headers.keys().cloned().collect::<BTreeSet<_>>();
        assert_eq!(
            names,
            [
                "accept",
                "accept-encoding",
                "connection",
                "content-length",
                "content-type",
                "host",
                "user-agent",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        );
        assert_eq!(headers.get("host").map(String::as_str), Some(AUTH_HOST));
        assert_eq!(headers.get("connection").map(String::as_str), Some("close"));
        assert_eq!(
            headers.get("accept-encoding").map(String::as_str),
            Some("identity")
        );
        assert_eq!(
            headers.get("accept").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some(content_type)
        );
        assert_eq!(
            headers.get("content-length").map(String::as_str),
            Some(request.body.len().to_string().as_str())
        );
        assert_eq!(request.body, body.as_bytes());
    }

    async fn write_response(
        stream: &mut (impl AsyncWrite + Unpin),
        status: u16,
        content_type: Option<&str>,
        extra_headers: &str,
        body: &[u8],
    ) {
        let reason = match status {
            200 => "OK",
            201 => "Created",
            302 => "Found",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            429 => "Too Many Requests",
            _ => "Internal Server Error",
        };
        let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
        if let Some(content_type) = content_type {
            response.push_str(&format!("content-type: {content_type}\r\n"));
        }
        response.push_str(extra_headers);
        response.push_str(&format!(
            "content-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        ));
        stream
            .write_all(response.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("response headers"));
        stream
            .write_all(body)
            .await
            .unwrap_or_else(|_| panic!("response body"));
    }

    async fn wait_for_peer_close(stream: &mut (impl AsyncRead + Unpin)) {
        let mut byte = [0_u8; 1];
        match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("client left connection open"),
            Err(_) => panic!("client did not close connection"),
        }
    }

    fn jwt_with_account(account_id: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#)
                .as_bytes(),
        );
        let signature = URL_SAFE_NO_PAD.encode(b"TEST_ONLY_SYNTHETIC_SIGNATURE_NOT_SECRET");
        format!("{header}.{payload}.{signature}")
    }

    fn poll_response() -> String {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(TEST_VERIFIER.as_bytes()));
        format!(
            r#"{{"authorization_code":"{TEST_AUTHORIZATION_CODE}","code_challenge":"{challenge}","code_verifier":"{TEST_VERIFIER}"}}"#
        )
    }

    #[test]
    fn production_constructor_is_local_and_does_not_perform_a_request() {
        let _client = CodexAuthClient::new(&timeouts()).unwrap_or_else(|_| panic!("client"));
    }

    #[tokio::test]
    async fn device_login_uses_only_exact_pinned_json_and_preencoded_form_requests() {
        let (listener, address) = listener().await;
        let (acceptor, certificate) = tls_fixture(AUTH_HOST);
        let client = client(certificate, address, Duration::from_secs(5));
        let device_body = format!(
            r#"{{"device_auth_id":"{TEST_DEVICE_ID}","user_code":"{TEST_USER_CODE}","interval":"1"}}"#
        );
        let poll_body = poll_response();
        let id_token = jwt_with_account(TEST_ACCOUNT_ID);
        let token_body = format!(
            r#"{{"id_token":"{id_token}","access_token":"{TEST_ACCESS_TOKEN}","refresh_token":"{TEST_REFRESH_TOKEN}"}}"#
        );
        let expected_form = format!(
            "grant_type=authorization_code&client_id={CODEX_CLIENT_ID}&code={TEST_AUTHORIZATION_CODE}&redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback&code_verifier={TEST_VERIFIER}"
        );
        let server = tokio::spawn(async move {
            let responses = [
                (
                    "/api/accounts/deviceauth/usercode",
                    "application/json",
                    format!(r#"{{"client_id":"{CODEX_CLIENT_ID}"}}"#),
                    200,
                    "application/json",
                    device_body.into_bytes(),
                ),
                (
                    "/api/accounts/deviceauth/token",
                    "application/json",
                    format!(
                        r#"{{"device_auth_id":"{TEST_DEVICE_ID}","user_code":"{TEST_USER_CODE}"}}"#
                    ),
                    403,
                    "text/plain",
                    b"TEST_ONLY_PENDING_BODY_NOT_SECRET_01".to_vec(),
                ),
                (
                    "/api/accounts/deviceauth/token",
                    "application/json",
                    format!(
                        r#"{{"device_auth_id":"{TEST_DEVICE_ID}","user_code":"{TEST_USER_CODE}"}}"#
                    ),
                    404,
                    "text/plain",
                    b"TEST_ONLY_PENDING_BODY_NOT_SECRET_02".to_vec(),
                ),
                (
                    "/api/accounts/deviceauth/token",
                    "application/json",
                    format!(
                        r#"{{"device_auth_id":"{TEST_DEVICE_ID}","user_code":"{TEST_USER_CODE}"}}"#
                    ),
                    200,
                    "application/json",
                    poll_body.into_bytes(),
                ),
                (
                    "/oauth/token",
                    "application/x-www-form-urlencoded",
                    expected_form,
                    200,
                    "application/json",
                    token_body.into_bytes(),
                ),
            ];
            for (
                index,
                (path, content_type, expected_body, status, response_type, response_body),
            ) in responses.into_iter().enumerate()
            {
                let (socket, _) = listener
                    .accept()
                    .await
                    .unwrap_or_else(|_| panic!("accepted connection"));
                let mut stream = acceptor
                    .accept(socket)
                    .await
                    .unwrap_or_else(|_| panic!("verified fixture TLS"));
                let request = read_request(&mut stream).await;
                assert_request(&request, path, content_type, &expected_body);
                if (1..=3).contains(&index) {
                    let poll: serde_json::Value = serde_json::from_slice(&request.body)
                        .unwrap_or_else(|_| panic!("poll JSON"));
                    assert_eq!(poll["device_auth_id"], TEST_DEVICE_ID);
                    assert_eq!(poll["user_code"], TEST_USER_CODE);
                }
                if index == 4 {
                    assert!(!String::from_utf8_lossy(&request.body).contains("%25"));
                }
                write_response(&mut stream, status, Some(response_type), "", &response_body).await;
            }
        });

        let authorization = client
            .request_device_authorization(future::pending::<()>())
            .await
            .unwrap_or_else(|_| panic!("device authorization"));
        assert_eq!(authorization.user_code(), TEST_USER_CODE);
        assert_eq!(
            authorization.verification_url(),
            "https://auth.openai.com/codex/device"
        );
        let credential = complete_device_login(
            authorization,
            |request| client.poll_device_authorization(request),
            |request| client.exchange_authorization_code(request),
            future::pending::<()>(),
        )
        .await
        .unwrap_or_else(|_| panic!("device login"));
        assert_eq!(credential.refresh_token(), TEST_REFRESH_TOKEN);
        assert_eq!(credential.account_id(), TEST_ACCOUNT_ID);
        server
            .await
            .unwrap_or_else(|_| panic!("TLS fixture server"));
    }

    #[tokio::test]
    async fn refresh_sends_exact_sensitive_json_and_uses_stored_account_identity() {
        let (listener, address) = listener().await;
        let (acceptor, certificate) = tls_fixture(AUTH_HOST);
        let client = client(certificate, address, Duration::from_secs(5));
        let expected_body = format!(
            r#"{{"client_id":"{CODEX_CLIENT_ID}","grant_type":"refresh_token","refresh_token":"{TEST_REFRESH_TOKEN}"}}"#
        );
        let response_body = format!(
            r#"{{"access_token":"{TEST_ACCESS_TOKEN}","refresh_token":"TEST_ONLY_ROTATED_REFRESH_TOKEN_NOT_SECRET_02","expires_in":3600}}"#
        );
        let server = tokio::spawn(async move {
            let (socket, _) = listener
                .accept()
                .await
                .unwrap_or_else(|_| panic!("accepted connection"));
            let mut stream = acceptor
                .accept(socket)
                .await
                .unwrap_or_else(|_| panic!("verified fixture TLS"));
            let request = read_request(&mut stream).await;
            assert_request(&request, "/oauth/token", "application/json", &expected_body);
            write_response(
                &mut stream,
                200,
                Some("application/json"),
                "",
                response_body.as_bytes(),
            )
            .await;
        });
        let response = client
            .refresh(
                Credential::new(TEST_REFRESH_TOKEN, TEST_ACCOUNT_ID)
                    .unwrap_or_else(|_| panic!("credential")),
            )
            .await
            .unwrap_or_else(|_| panic!("refresh response"));
        assert!(!format!("{response:?}").contains(TEST_ACCESS_TOKEN));
        server
            .await
            .unwrap_or_else(|_| panic!("TLS fixture server"));
    }

    #[tokio::test]
    async fn wrong_hostname_certificate_is_rejected_for_the_pinned_issuer() {
        let (listener, address) = listener().await;
        let (acceptor, certificate) = tls_fixture("wrong-host.test");
        let client = client(certificate, address, Duration::from_secs(5));
        let server = tokio::spawn(async move {
            let (socket, _) = listener
                .accept()
                .await
                .unwrap_or_else(|_| panic!("accepted connection"));
            assert!(acceptor.accept(socket).await.is_err());
        });
        assert!(matches!(
            client
                .request_device_authorization(future::pending::<()>())
                .await,
            Err(LoginError::InvalidDeviceResponse)
        ));
        server
            .await
            .unwrap_or_else(|_| panic!("TLS fixture server"));
    }

    #[tokio::test]
    async fn status_errors_are_redacted_and_never_follow_or_replay() {
        for status in [302, 401, 429, 500] {
            let (listener, address) = listener().await;
            let (acceptor, certificate) = tls_fixture(AUTH_HOST);
            let client = client(certificate, address, Duration::from_secs(5));
            let expected_body = format!(
                r#"{{"client_id":"{CODEX_CLIENT_ID}","grant_type":"refresh_token","refresh_token":"{TEST_REFRESH_TOKEN}"}}"#
            );
            let server = tokio::spawn(async move {
                let (socket, _) = listener
                    .accept()
                    .await
                    .unwrap_or_else(|_| panic!("accepted connection"));
                let mut stream = acceptor
                    .accept(socket)
                    .await
                    .unwrap_or_else(|_| panic!("verified fixture TLS"));
                let request = read_request(&mut stream).await;
                assert_request(&request, "/oauth/token", "application/json", &expected_body);
                write_response(
                    &mut stream,
                    status,
                    Some("text/plain"),
                    if status == 302 {
                        "location: https://auth.openai.com/oauth/other\r\n"
                    } else {
                        ""
                    },
                    b"TEST_ONLY_ERROR_BODY_NOT_SECRET_03",
                )
                .await;
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_err()
                );
            });
            let error = client
                .refresh(
                    Credential::new(TEST_REFRESH_TOKEN, TEST_ACCOUNT_ID)
                        .unwrap_or_else(|_| panic!("credential")),
                )
                .await
                .expect_err("non-200 must fail without replay");
            let diagnostic = format!("{error} {error:?}");
            assert!(!diagnostic.contains(TEST_REFRESH_TOKEN));
            assert!(!diagnostic.contains("TEST_ONLY_ERROR_BODY_NOT_SECRET_03"));
            server
                .await
                .unwrap_or_else(|_| panic!("TLS fixture server"));
        }
    }

    #[tokio::test]
    async fn oversized_and_truncated_responses_fail_and_close_the_body_stream() {
        for truncated in [false, true] {
            let (listener, address) = listener().await;
            let (acceptor, certificate) = tls_fixture(AUTH_HOST);
            let client = client(certificate, address, Duration::from_secs(5));
            let server = tokio::spawn(async move {
                let (socket, _) = listener
                    .accept()
                    .await
                    .unwrap_or_else(|_| panic!("accepted connection"));
                let mut stream = acceptor
                    .accept(socket)
                    .await
                    .unwrap_or_else(|_| panic!("verified fixture TLS"));
                let _request = read_request(&mut stream).await;
                if truncated {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 32\r\nconnection: close\r\n\r\n{}",
                        )
                        .await
                        .unwrap_or_else(|_| panic!("truncated response"));
                } else {
                    let body = vec![b'x'; AUTH_RESPONSE_BYTES + 1];
                    write_response(&mut stream, 200, Some("application/json"), "", &body).await;
                    wait_for_peer_close(&mut stream).await;
                }
            });
            assert!(matches!(
                client
                    .refresh(
                        Credential::new(TEST_REFRESH_TOKEN, TEST_ACCOUNT_ID)
                            .unwrap_or_else(|_| panic!("credential")),
                    )
                    .await,
                Err(RefreshExchangeError)
            ));
            server
                .await
                .unwrap_or_else(|_| panic!("TLS fixture server"));
        }
    }

    #[tokio::test]
    async fn overall_timeout_cancels_and_closes_an_unfinished_request() {
        let (listener, address) = listener().await;
        let (acceptor, certificate) = tls_fixture(AUTH_HOST);
        let client = client(certificate, address, Duration::from_millis(50));
        let server = tokio::spawn(async move {
            let (socket, _) = listener
                .accept()
                .await
                .unwrap_or_else(|_| panic!("accepted connection"));
            let mut stream = acceptor
                .accept(socket)
                .await
                .unwrap_or_else(|_| panic!("verified fixture TLS"));
            let _request = read_request(&mut stream).await;
            wait_for_peer_close(&mut stream).await;
        });
        assert!(matches!(
            client
                .request_device_authorization(future::pending::<()>())
                .await,
            Err(LoginError::InvalidDeviceResponse)
        ));
        server
            .await
            .unwrap_or_else(|_| panic!("TLS fixture server"));
    }

    #[tokio::test]
    async fn explicit_cancellation_closes_an_unfinished_request() {
        let (listener, address) = listener().await;
        let (acceptor, certificate) = tls_fixture(AUTH_HOST);
        let client = client(certificate, address, Duration::from_secs(5));
        let (received_tx, received_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener
                .accept()
                .await
                .unwrap_or_else(|_| panic!("accepted connection"));
            let mut stream = acceptor
                .accept(socket)
                .await
                .unwrap_or_else(|_| panic!("verified fixture TLS"));
            let _request = read_request(&mut stream).await;
            received_tx
                .send(())
                .unwrap_or_else(|_| panic!("request signal"));
            wait_for_peer_close(&mut stream).await;
        });
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let login = tokio::spawn(async move {
            client
                .request_device_authorization(async {
                    let _ = cancel_rx.await;
                })
                .await
        });
        received_rx
            .await
            .unwrap_or_else(|_| panic!("request observed"));
        cancel_tx
            .send(())
            .unwrap_or_else(|_| panic!("cancel signal"));
        assert!(matches!(
            login.await.unwrap_or_else(|_| panic!("login task")),
            Err(LoginError::Cancelled)
        ));
        server
            .await
            .unwrap_or_else(|_| panic!("TLS fixture server"));
    }
}
