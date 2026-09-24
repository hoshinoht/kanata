use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::de::{self, DeserializeOwned, Deserializer};

use super::credential::valid_value;
use super::login::{LoginError, LoginTokenResponse};
use super::refresh::{MAX_REFRESH_SKEW, RefreshError, RefreshResponse};

const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 4 * 1024;
const MAX_JWT_PAYLOAD_BYTES: usize = 3 * MAX_TOKEN_BYTES / 4;
const MAX_JSON_DEPTH: usize = 16;
// Match the pinned Codex parser's chrono::DateTime range (through 262142-12-31).
const MAX_SOURCE_EXPIRY_SECONDS: u64 = 8_210_266_876_799;

/// Caller must bind the response to the pinned HTTPS issuer; this decodes claims but does not verify JWT signatures.
pub fn normalize_authorization_code_response(
    body: &[u8],
) -> Result<LoginTokenResponse, LoginError> {
    let response: AuthorizationCodeTokenResponse =
        parse_bounded_json(body, MAX_RESPONSE_BYTES).ok_or(LoginError::InvalidTokenResponse)?;
    if !valid_value(&response.id_token)
        || !valid_value(&response.access_token)
        || !valid_value(&response.refresh_token)
    {
        return Err(LoginError::InvalidTokenResponse);
    }
    let account_id =
        account_id_from_id_token(&response.id_token).ok_or(LoginError::InvalidTokenResponse)?;

    LoginTokenResponse::new(response.refresh_token, account_id)
}

/// Normalize a refresh body using the account identity already held by the credential store.
pub fn normalize_refresh_response(
    body: &[u8],
    stored_account_id: &str,
) -> Result<RefreshResponse, RefreshError> {
    let response: RefreshTokenWireResponse =
        parse_bounded_json(body, MAX_RESPONSE_BYTES).ok_or(RefreshError::InvalidResponse)?;
    if !valid_value(&response.access_token)
        || response
            .rotated_refresh_token
            .as_deref()
            .is_some_and(|token| !valid_value(token))
    {
        return Err(RefreshError::InvalidResponse);
    }

    let now = SystemTime::now();
    let expires_at = match response.expires_in {
        Some(seconds) => now.checked_add(Duration::from_secs(seconds)),
        None => access_token_expiration(&response.access_token),
    }
    .filter(|expires_at| expiry_is_usable(now, *expires_at))
    .ok_or(RefreshError::InvalidResponse)?;

    RefreshResponse::new(
        response.access_token,
        expires_at,
        stored_account_id,
        response.rotated_refresh_token,
    )
}

#[derive(Deserialize)]
struct AuthorizationCodeTokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct RefreshTokenWireResponse {
    access_token: String,
    #[serde(default, deserialize_with = "deserialize_refresh_token")]
    #[serde(rename = "refresh_token")]
    rotated_refresh_token: Option<String>,
    #[serde(default, deserialize_with = "deserialize_expires_in")]
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct IdTokenClaims {
    #[serde(rename = "https://api.openai.com/auth")]
    auth: AuthClaims,
}

#[derive(Deserialize)]
struct AuthClaims {
    chatgpt_account_id: String,
}

#[derive(Deserialize)]
struct AccessTokenClaims {
    exp: i64,
}

fn deserialize_refresh_token<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

fn deserialize_expires_in<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let seconds = u64::deserialize(deserializer)?;
    if seconds == 0 {
        return Err(de::Error::custom("expires_in must be positive"));
    }
    Ok(Some(seconds))
}

fn account_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(id_token)?;
    let claims: IdTokenClaims = parse_bounded_json(&payload, MAX_JWT_PAYLOAD_BYTES)?;
    valid_value(&claims.auth.chatgpt_account_id).then_some(claims.auth.chatgpt_account_id)
}

fn access_token_expiration(access_token: &str) -> Option<SystemTime> {
    let payload = decode_jwt_payload(access_token)?;
    let claims: AccessTokenClaims = parse_bounded_json(&payload, MAX_JWT_PAYLOAD_BYTES)?;
    let seconds = u64::try_from(claims.exp).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

fn decode_jwt_payload(token: &str) -> Option<Vec<u8>> {
    if token.len() > MAX_TOKEN_BYTES || !valid_value(token) {
        return None;
    }
    let mut segments = token.split('.');
    let header = segments.next()?;
    let payload = segments.next()?;
    let signature = segments.next()?;
    if segments.next().is_some() || [header, payload, signature].contains(&"") {
        return None;
    }

    let header = URL_SAFE_NO_PAD.decode(header).ok()?;
    let _: serde_json::Map<String, serde_json::Value> =
        parse_bounded_json(&header, MAX_JWT_PAYLOAD_BYTES)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    URL_SAFE_NO_PAD.decode(signature).ok()?;
    (payload.len() <= MAX_JWT_PAYLOAD_BYTES).then_some(payload)
}

fn parse_bounded_json<T>(bytes: &[u8], max_bytes: usize) -> Option<T>
where
    T: DeserializeOwned,
{
    if bytes.len() > max_bytes || !within_json_depth(bytes) {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

fn within_json_depth(bytes: &[u8]) -> bool {
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;

    for byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_JSON_DEPTH {
                    return false;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    true
}

fn expiry_is_usable(now: SystemTime, expires_at: SystemTime) -> bool {
    expires_at
        .duration_since(UNIX_EPOCH)
        .is_ok_and(|timestamp| timestamp.as_secs() <= MAX_SOURCE_EXPIRY_SECONDS)
        && expires_at
            .duration_since(now)
            .is_ok_and(|remaining| remaining > MAX_REFRESH_SKEW)
}
