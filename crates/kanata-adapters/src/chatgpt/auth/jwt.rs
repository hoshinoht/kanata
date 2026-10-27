use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::{
    AuthError, ISSUER,
    record::{Identity, MAX_TOKEN_BYTES, valid_value},
};

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: String,
    crit: Option<Vec<String>>,
}
#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Key>,
}
#[derive(Deserialize)]
struct Key {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    key_ops: Option<Vec<String>>,
    n: Option<String>,
    e: Option<String>,
}
#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: Audience,
    azp: Option<String>,
    exp: u64,
    iat: u64,
    nbf: Option<u64>,
    nonce: Option<String>,
    email: Option<String>,
    at_hash: Option<String>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Several(Vec<String>),
}

pub(super) fn verify(
    token: &str,
    jwks: &[u8],
    client_id: &str,
    nonce: Option<&str>,
    access_token: &str,
    now: u64,
) -> Result<Identity, AuthError> {
    if token.len() > MAX_TOKEN_BYTES || jwks.len() > 128 * 1024 {
        return Err(AuthError::InvalidIdentity);
    }
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(AuthError::InvalidIdentity);
    }
    let header: Header =
        serde_json::from_slice(&decode(parts[0])?).map_err(|_| AuthError::InvalidIdentity)?;
    if header.alg != "RS256"
        || !valid_value(&header.kid, 256)
        || header.crit.is_some_and(|c| !c.is_empty())
    {
        return Err(AuthError::InvalidIdentity);
    }
    let keys: Jwks = serde_json::from_slice(jwks).map_err(|_| AuthError::InvalidIdentity)?;
    if keys.keys.is_empty() || keys.keys.len() > 64 {
        return Err(AuthError::InvalidIdentity);
    }
    let matching: Vec<_> = keys
        .keys
        .iter()
        .filter(|k| k.kid.as_ref() == Some(&header.kid))
        .collect();
    if matching.len() != 1 {
        return Err(AuthError::InvalidIdentity);
    }
    let key = matching[0];
    if key.kty != "RSA"
        || key.alg.as_deref().is_some_and(|a| a != "RS256")
        || key.usage.as_deref().is_some_and(|u| u != "sig")
        || key
            .key_ops
            .as_ref()
            .is_some_and(|ops| !ops.iter().any(|op| op == "verify"))
    {
        return Err(AuthError::InvalidIdentity);
    }
    let modulus = decode(key.n.as_deref().ok_or(AuthError::InvalidIdentity)?)?;
    let exponent = decode(key.e.as_deref().ok_or(AuthError::InvalidIdentity)?)?;
    if modulus.len() < 256 || modulus.len() > 1024 || exponent.len() > 8 {
        return Err(AuthError::InvalidIdentity);
    }
    let message = format!("{}.{}", parts[0], parts[1]);
    RsaPublicKeyComponents {
        n: &modulus,
        e: &exponent,
    }
    .verify(
        &RSA_PKCS1_2048_8192_SHA256,
        message.as_bytes(),
        &decode(parts[2])?,
    )
    .map_err(|_| AuthError::InvalidIdentity)?;
    let claims: Claims =
        serde_json::from_slice(&decode(parts[1])?).map_err(|_| AuthError::InvalidIdentity)?;
    let (audience_matches, multiple) = match &claims.aud {
        Audience::One(value) => (value == client_id, false),
        Audience::Several(values) => (
            values.iter().any(|value| value == client_id),
            values.len() > 1,
        ),
    };
    if claims.iss != ISSUER
        || !valid_value(&claims.sub, 512)
        || !audience_matches
        || claims
            .azp
            .as_deref()
            .is_some_and(|value| value != client_id)
        || (multiple && claims.azp.as_deref() != Some(client_id))
        || claims.exp <= now
        || claims.iat > now.saturating_add(60)
        || claims.exp <= claims.iat
        || claims
            .nbf
            .is_some_and(|value| value > now.saturating_add(60))
        || nonce.is_some_and(|expected| {
            claims
                .nonce
                .as_deref()
                .is_none_or(|value| !bool::from(value.as_bytes().ct_eq(expected.as_bytes())))
        })
        || claims
            .email
            .as_ref()
            .is_some_and(|value| !valid_value(value, 512))
    {
        return Err(AuthError::InvalidIdentity);
    }
    if let Some(hash) = claims.at_hash {
        let digest = Sha256::digest(access_token.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(&digest[..16]);
        if !bool::from(expected.as_bytes().ct_eq(hash.as_bytes())) {
            return Err(AuthError::InvalidIdentity);
        }
    }
    Ok(Identity {
        issuer: claims.iss,
        subject: claims.sub,
        email: claims.email,
    })
}
fn decode(value: &str) -> Result<Vec<u8>, AuthError> {
    if value.is_empty() || value.len() > MAX_TOKEN_BYTES {
        return Err(AuthError::InvalidIdentity);
    }
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| AuthError::InvalidIdentity)
}
