use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use url::Url;

use crate::{
    adapter::transport::{
        Endpoint, OAUTH_RESPONSE_BYTES, ResponseContentType, Transport, TransportRequest,
    },
    config::ValidatedTimeouts,
};

use super::{
    AuthError, ISSUER,
    record::{MAX_TOKEN_BYTES, Session, valid_value},
};

const RESPONSE_BYTES: usize = 128 * 1024;
const FORM_BYTES: usize = 64 * 1024;
pub(super) const RESOURCE: &str = "https://api.openai.com/v1";
const AUTH_HOST: &str = "auth.openai.com";

pub(super) struct Client {
    transport: Transport,
    timeout: Duration,
}
#[derive(Deserialize)]
pub(super) struct Discovery {
    pub issuer: String,
    pub jwks_uri: String,
    pub revocation_endpoint: String,
    pub id_token_signing_alg_values_supported: Vec<String>,
}
#[derive(Deserialize)]
pub(super) struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub token_type: String,
    pub expires_in: u64,
    pub scope: String,
    pub earliest_refresh_at: Option<u64>,
}

impl Client {
    pub fn new(timeouts: &ValidatedTimeouts) -> Result<Self, AuthError> {
        Ok(Self {
            transport: Transport::new_pinned_https(AUTH_HOST, timeouts)
                .map_err(|_| AuthError::Network)?,
            timeout: Duration::from_millis(timeouts.overall_ms()).min(Duration::from_secs(30)),
        })
    }
    pub async fn discovery(&self) -> Result<Discovery, AuthError> {
        let response = self
            .get(
                Endpoint::new(&[".well-known", "openid-configuration"])
                    .map_err(|_| AuthError::Discovery)?,
            )
            .await?;
        let metadata: Discovery =
            serde_json::from_slice(&response).map_err(|_| AuthError::Discovery)?;
        if metadata.issuer != ISSUER
            || !metadata
                .id_token_signing_alg_values_supported
                .iter()
                .any(|a| a == "RS256")
        {
            return Err(AuthError::Discovery);
        }
        discovered_endpoint(&metadata.jwks_uri)?;
        discovered_endpoint(&metadata.revocation_endpoint)?;
        Ok(metadata)
    }
    pub async fn jwks(&self, metadata: &Discovery) -> Result<Vec<u8>, AuthError> {
        self.get(discovered_endpoint(&metadata.jwks_uri)?).await
    }
    async fn get(&self, endpoint: Endpoint) -> Result<Vec<u8>, AuthError> {
        let request = TransportRequest::get(endpoint, None, OAUTH_RESPONSE_BYTES)
            .map_err(|_| AuthError::Network)?;
        let (status, kind, body) = self.execute(request).await?;
        if status != 200 || kind != Some(ResponseContentType::Json) {
            return Err(AuthError::Discovery);
        }
        Ok(body)
    }
    pub async fn token(&self, fields: &[(&str, &str)]) -> Result<TokenResponse, AuthError> {
        let endpoint = Endpoint::new(&["api", "accounts", "oauth", "token"])
            .map_err(|_| AuthError::Network)?;
        let (status, kind, body) = self.post(endpoint, fields).await?;
        if status != 200 {
            return Err(token_error(&body));
        }
        if kind != Some(ResponseContentType::Json) {
            return Err(AuthError::InvalidResponse);
        }
        serde_json::from_slice(&body).map_err(|_| AuthError::InvalidResponse)
    }
    pub async fn revoke(&self, client_id: &str, refresh_token: &str) -> bool {
        for attempt in 0..3 {
            let result = async {
                let metadata = self.discovery().await?;
                self.post(
                    discovered_endpoint(&metadata.revocation_endpoint)?,
                    &[
                        ("token", refresh_token),
                        ("token_type_hint", "refresh_token"),
                        ("client_id", client_id),
                    ],
                )
                .await
            }
            .await;
            match result {
                Ok((200, _, body)) if body.is_empty() => return true,
                Ok((status, _, _)) if status < 500 => return false,
                _ if attempt < 2 => {
                    tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await
                }
                _ => return false,
            }
        }
        false
    }
    async fn post(
        &self,
        endpoint: Endpoint,
        fields: &[(&str, &str)],
    ) -> Result<(u16, Option<ResponseContentType>, Vec<u8>), AuthError> {
        let encoded = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.extend_pairs(fields.iter().copied());
            form.finish()
        };
        let request = TransportRequest::form_encoded(
            http::Method::POST,
            endpoint,
            &encoded,
            FORM_BYTES,
            OAUTH_RESPONSE_BYTES,
        )
        .map_err(|_| AuthError::InvalidResponse)?;
        self.execute(request).await
    }
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<(u16, Option<ResponseContentType>, Vec<u8>), AuthError> {
        tokio::time::timeout(self.timeout, async {
            let mut response = self
                .transport
                .execute(request)
                .await
                .map_err(|_| AuthError::Network)?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.body.next().await {
                let chunk = chunk.map_err(|_| AuthError::Network)?;
                if bytes.len().saturating_add(chunk.len()) > RESPONSE_BYTES {
                    return Err(AuthError::InvalidResponse);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok((response.status, response.content_type, bytes))
        })
        .await
        .map_err(|_| AuthError::Network)?
    }
    #[cfg(test)]
    pub(super) fn with_test_root(
        timeouts: &ValidatedTimeouts,
        certificate: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
        address: std::net::SocketAddr,
    ) -> Self {
        Self {
            transport: Transport::new_pinned_https_with_test_root(
                AUTH_HOST,
                timeouts,
                certificate,
                address,
            )
            .expect("fixture transport"),
            timeout: Duration::from_secs(5),
        }
    }
}
impl TokenResponse {
    pub fn session(
        self,
        now: u64,
        previous_id_token: Option<&str>,
        refreshing: bool,
    ) -> Result<Session, AuthError> {
        if !self.token_type.eq_ignore_ascii_case("Bearer")
            || self.expires_in == 0
            || self.expires_in > 86_400
            || !valid_value(&self.access_token, MAX_TOKEN_BYTES)
            || self.scope.len() > 16 * 1024
            || self.scope.chars().any(|c| c.is_control())
            || (refreshing && self.refresh_token.is_none())
        {
            return Err(AuthError::InvalidResponse);
        }
        let scopes = self
            .scope
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if scopes.iter().any(|s| s == "offline_access") && self.refresh_token.is_none() {
            return Err(AuthError::InvalidResponse);
        }
        let session = Session {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            id_token: self
                .id_token
                .or_else(|| previous_id_token.map(str::to_owned))
                .ok_or(AuthError::InvalidResponse)?,
            scopes,
            saved_at: now,
            expires_at: now.checked_add(self.expires_in).ok_or(AuthError::Clock)?,
            earliest_refresh_at: self.earliest_refresh_at,
        };
        session.validate()?;
        Ok(session)
    }
}
fn token_error(body: &[u8]) -> AuthError {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: String,
    }
    let Ok(error) = serde_json::from_slice::<ErrorBody>(body) else {
        return AuthError::Network;
    };
    match error.error.as_str() {
        "invalid_grant"
        | "invalid_refresh_token"
        | "token_expired"
        | "refresh_token_expired"
        | "refresh_token_invalidated"
        | "refresh_token_reused" => AuthError::InvalidGrant,
        "invalid_client" => AuthError::InvalidClient,
        _ => AuthError::Network,
    }
}
pub(super) fn discovered_endpoint(value: &str) -> Result<Endpoint, AuthError> {
    let url = Url::parse(value).map_err(|_| AuthError::Discovery)?;
    if url.scheme() != "https"
        || url.host_str() != Some(AUTH_HOST)
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || value.bytes().any(|b| b.is_ascii_control())
        || value.contains('\\')
    {
        return Err(AuthError::Discovery);
    }
    let raw_path = value
        .strip_prefix("https://auth.openai.com")
        .ok_or(AuthError::Discovery)?;
    let raw_path = raw_path.strip_prefix(":443").unwrap_or(raw_path);
    if raw_path != url.path() {
        return Err(AuthError::Discovery);
    }
    Endpoint::from_path(raw_path).map_err(|_| AuthError::Discovery)
}
