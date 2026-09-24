use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::Serialize;
use serde::de::{self, Deserializer};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::time::{Instant, sleep_until, timeout_at};
use url::form_urlencoded;

use super::credential::Credential;

pub const CODEX_ISSUER: &str = "https://auth.openai.com";
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_DEVICE_USERCODE_ENDPOINT: &str =
    "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const CODEX_DEVICE_TOKEN_ENDPOINT: &str =
    "https://auth.openai.com/api/accounts/deviceauth/token";
pub const CODEX_DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
pub const CODEX_DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

pub const MAX_LOGIN_DURATION: Duration = Duration::from_secs(15 * 60);
pub const MAX_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_OPAQUE_VALUE_BYTES: usize = 4 * 1024;
const MAX_USER_CODE_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginError {
    InvalidPolicy,
    DeviceLoginExpired,
    InvalidDeviceResponse,
    DevicePollFailed,
    TokenExchangeFailed,
    InvalidTokenResponse,
    Cancelled,
}

impl fmt::Display for LoginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidPolicy => "invalid Codex login policy",
            Self::DeviceLoginExpired => "Codex device login attempt expired",
            Self::InvalidDeviceResponse => "invalid Codex device login response",
            Self::DevicePollFailed => "Codex device authorization polling failed",
            Self::TokenExchangeFailed => "Codex authorization-code exchange failed",
            Self::InvalidTokenResponse => "invalid Codex login token response",
            Self::Cancelled => "Codex login was cancelled",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for LoginError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevicePollFailure;

impl fmt::Display for DevicePollFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Codex device authorization polling failed")
    }
}

impl std::error::Error for DevicePollFailure {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoginExchangeFailure;

impl fmt::Display for LoginExchangeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Codex authorization-code exchange failed")
    }
}

impl std::error::Error for LoginExchangeFailure {}

/// The fixed first-stage beta device authorization request.
pub struct DeviceCodeRequest {
    deadline: Instant,
}

impl fmt::Debug for DeviceCodeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeviceCodeRequest")
    }
}

impl DeviceCodeRequest {
    pub fn new() -> Result<Self, LoginError> {
        let deadline = Instant::now()
            .checked_add(MAX_LOGIN_DURATION)
            .ok_or(LoginError::InvalidPolicy)?;
        Ok(Self { deadline })
    }

    pub fn endpoint(&self) -> &'static str {
        CODEX_DEVICE_USERCODE_ENDPOINT
    }

    pub fn content_type(&self) -> &'static str {
        "application/json"
    }

    pub fn body(&self) -> &'static [u8] {
        br#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#
    }

    pub fn accept_response(self, body: &[u8]) -> Result<DeviceAuthorization, LoginError> {
        if Instant::now() >= self.deadline {
            return Err(LoginError::DeviceLoginExpired);
        }
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(LoginError::InvalidDeviceResponse);
        }
        let response: DeviceCodeWireResponse =
            serde_json::from_slice(body).map_err(|_| LoginError::InvalidDeviceResponse)?;
        if !valid_nonblank(&response.device_auth_id, MAX_OPAQUE_VALUE_BYTES)
            || !valid_nonblank(&response.user_code, MAX_USER_CODE_BYTES)
            || response.interval == 0
            || Duration::from_secs(response.interval) > MAX_DEVICE_POLL_INTERVAL
        {
            return Err(LoginError::InvalidDeviceResponse);
        }
        let poll_body = serde_json::to_vec(&DevicePollWireRequest {
            device_auth_id: &response.device_auth_id,
            user_code: &response.user_code,
        })
        .map_err(|_| LoginError::InvalidDeviceResponse)?;

        Ok(DeviceAuthorization {
            user_code: response.user_code,
            poll_interval: Duration::from_secs(response.interval),
            poll_body,
            deadline: self.deadline,
        })
    }
}

#[derive(Deserialize)]
struct DeviceCodeWireResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(deserialize_with = "deserialize_interval")]
    interval: u64,
}

fn deserialize_interval<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    value
        .trim()
        .parse()
        .map_err(|_| de::Error::custom("invalid interval"))
}

struct DevicePollWireRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

impl Serialize for DevicePollWireRequest<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("DevicePollWireRequest", 2)?;
        state.serialize_field("device_auth_id", self.device_auth_id)?;
        state.serialize_field("user_code", self.user_code)?;
        state.end()
    }
}

/// A validated device authorization; only its verification URL and user code are intended for display.
pub struct DeviceAuthorization {
    user_code: String,
    poll_interval: Duration,
    poll_body: Vec<u8>,
    deadline: Instant,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeviceAuthorization([REDACTED])")
    }
}

impl DeviceAuthorization {
    pub fn verification_url(&self) -> &'static str {
        CODEX_DEVICE_VERIFICATION_URL
    }

    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    fn poll_request(&self) -> DevicePollRequest {
        DevicePollRequest {
            body: DevicePollBody(self.poll_body.clone()),
        }
    }
}

pub struct DevicePollBody(Vec<u8>);

impl fmt::Debug for DevicePollBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DevicePollBody([REDACTED])")
    }
}

impl DevicePollBody {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

pub struct DevicePollRequest {
    body: DevicePollBody,
}

impl fmt::Debug for DevicePollRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DevicePollRequest([REDACTED])")
    }
}

impl DevicePollRequest {
    pub fn endpoint(&self) -> &'static str {
        CODEX_DEVICE_TOKEN_ENDPOINT
    }

    pub fn content_type(&self) -> &'static str {
        "application/json"
    }

    pub fn body(&self) -> &DevicePollBody {
        &self.body
    }
}

pub struct DevicePollResponse {
    status: u16,
    body: Vec<u8>,
}

impl fmt::Debug for DevicePollResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DevicePollResponse")
            .field("status", &self.status)
            .field("body", &"[REDACTED]")
            .finish()
    }
}

impl DevicePollResponse {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

#[derive(Deserialize)]
struct DevicePollWireResponse {
    authorization_code: String,
    code_challenge: String,
    code_verifier: String,
}

enum DevicePollResult {
    Pending,
    Authorized(AuthorizationCodeRequest),
}

fn parse_device_poll_response(
    response: DevicePollResponse,
    deadline: Instant,
) -> Result<DevicePollResult, LoginError> {
    if response.body.len() > MAX_RESPONSE_BYTES {
        return Err(LoginError::DevicePollFailed);
    }
    if response.status == 403 || response.status == 404 {
        return Ok(DevicePollResult::Pending);
    }
    if !(200..300).contains(&response.status) {
        return Err(LoginError::DevicePollFailed);
    }
    let response: DevicePollWireResponse =
        serde_json::from_slice(&response.body).map_err(|_| LoginError::InvalidDeviceResponse)?;
    if !valid_nonblank(&response.authorization_code, MAX_OPAQUE_VALUE_BYTES)
        || !valid_pkce_verifier(&response.code_verifier)
        || !valid_pkce_challenge(&response.code_challenge)
        || !constant_time_equal(
            &response.code_challenge,
            &s256_challenge(&response.code_verifier),
        )
    {
        return Err(LoginError::InvalidDeviceResponse);
    }
    Ok(DevicePollResult::Authorized(AuthorizationCodeRequest {
        code: response.authorization_code,
        verifier: response.code_verifier,
        redirect_uri: CODEX_DEVICE_REDIRECT_URI.to_owned(),
        deadline,
        expired_error: LoginError::DeviceLoginExpired,
    }))
}

/// Poll the private beta device endpoint and exchange its authorization code through an injected transport.
pub async fn complete_device_login<P, PFut, E, EFut, C>(
    authorization: DeviceAuthorization,
    mut poll: P,
    exchange: E,
    cancellation: C,
) -> Result<Credential, LoginError>
where
    P: FnMut(DevicePollRequest) -> PFut,
    PFut: Future<Output = Result<DevicePollResponse, DevicePollFailure>>,
    E: FnOnce(AuthorizationCodeRequest) -> EFut,
    EFut: Future<Output = Result<LoginTokenResponse, LoginExchangeFailure>>,
    C: Future<Output = ()>,
{
    let mut cancellation = Box::pin(cancellation);
    loop {
        if Instant::now() >= authorization.deadline {
            return Err(LoginError::DeviceLoginExpired);
        }
        let response = tokio::select! {
            biased;
            _ = cancellation.as_mut() => return Err(LoginError::Cancelled),
            response = timeout_at(authorization.deadline, poll(authorization.poll_request())) => {
                response.map_err(|_| LoginError::DeviceLoginExpired)?
                    .map_err(|_| LoginError::DevicePollFailed)?
            }
        };
        if Instant::now() >= authorization.deadline {
            return Err(LoginError::DeviceLoginExpired);
        }
        match parse_device_poll_response(response, authorization.deadline)? {
            DevicePollResult::Pending => {
                let wake_at = Instant::now()
                    .checked_add(authorization.poll_interval)
                    .unwrap_or(authorization.deadline)
                    .min(authorization.deadline);
                tokio::select! {
                    biased;
                    _ = cancellation.as_mut() => return Err(LoginError::Cancelled),
                    _ = sleep_until(wake_at) => {}
                }
            }
            DevicePollResult::Authorized(request) => {
                return exchange_with_cancellation(request, exchange, &mut cancellation).await;
            }
        }
    }
}

async fn exchange_with_cancellation<E, EFut, C>(
    request: AuthorizationCodeRequest,
    exchange: E,
    cancellation: &mut Pin<Box<C>>,
) -> Result<Credential, LoginError>
where
    E: FnOnce(AuthorizationCodeRequest) -> EFut,
    EFut: Future<Output = Result<LoginTokenResponse, LoginExchangeFailure>>,
    C: Future<Output = ()>,
{
    if Instant::now() >= request.deadline {
        return Err(request.expired_error);
    }
    let deadline = request.deadline;
    let expired_error = request.expired_error;
    let response = tokio::select! {
        biased;
        _ = cancellation.as_mut() => return Err(LoginError::Cancelled),
        response = timeout_at(deadline, exchange(request)) => {
            response.map_err(|_| expired_error)?
                .map_err(|_| LoginError::TokenExchangeFailed)?
        }
    };
    if Instant::now() >= deadline {
        return Err(expired_error);
    }
    Ok(response.into_credential())
}

/// A validated refresh-token/account pair ready for a later credential-store handoff.
pub struct LoginTokenResponse {
    credential: Credential,
}

impl fmt::Debug for LoginTokenResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LoginTokenResponse([REDACTED])")
    }
}

impl LoginTokenResponse {
    pub fn new(
        refresh_token: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Result<Self, LoginError> {
        let credential = Credential::new(refresh_token, account_id)
            .map_err(|_| LoginError::InvalidTokenResponse)?;
        Ok(Self { credential })
    }

    fn into_credential(self) -> Credential {
        self.credential
    }
}

pub struct AuthorizationCodeRequest {
    code: String,
    verifier: String,
    redirect_uri: String,
    deadline: Instant,
    expired_error: LoginError,
}

impl fmt::Debug for AuthorizationCodeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizationCodeRequest([REDACTED])")
    }
}

impl AuthorizationCodeRequest {
    pub fn endpoint(&self) -> &'static str {
        CODEX_TOKEN_ENDPOINT
    }

    pub fn method(&self) -> &'static str {
        "POST"
    }

    pub fn content_type(&self) -> &'static str {
        "application/x-www-form-urlencoded"
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub fn form_body(&self) -> AuthorizationCodeForm {
        let mut form = form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "authorization_code")
            .append_pair("client_id", CODEX_CLIENT_ID)
            .append_pair("code", &self.code)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("code_verifier", &self.verifier);
        AuthorizationCodeForm(form.finish())
    }
}

pub struct AuthorizationCodeForm(String);

impl fmt::Debug for AuthorizationCodeForm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizationCodeForm([REDACTED])")
    }
}

impl AuthorizationCodeForm {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_nonblank(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn valid_pkce_verifier(value: &str) -> bool {
    (43..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn valid_pkce_challenge(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn s256_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    left.len() == right.len() && left.as_bytes().ct_eq(right.as_bytes()).unwrap_u8() == 1
}
