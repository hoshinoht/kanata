use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::Read;

use axum::http::{HeaderMap, header};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::{SecretReference, ValidatedConfig};
use crate::core::{ModelAlias, RouteSelector};
use crate::routing::{Registry, RouteEntry};

pub const MAX_BEARER_TOKEN_BYTES: usize = 4096;
const MAX_FILE_SECRET_BYTES: usize = MAX_BEARER_TOKEN_BYTES + 2;

pub trait SecretResolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError>;
}

pub struct EnvironmentSecretResolver;

impl SecretResolver for EnvironmentSecretResolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        match reference {
            SecretReference::Env(name) => std::env::var(name)
                .map(String::into_bytes)
                .map_err(|_| SecretResolutionError),
            SecretReference::File(path) => read_bounded_file(path),
            SecretReference::Sha256(_) => Err(SecretResolutionError),
        }
    }
}

fn read_bounded_file(path: &std::path::Path) -> Result<Vec<u8>, SecretResolutionError> {
    let mut file = File::open(path).map_err(|_| SecretResolutionError)?;
    let mut secret = Vec::with_capacity(MAX_FILE_SECRET_BYTES);
    file.by_ref()
        .take((MAX_FILE_SECRET_BYTES + 1) as u64)
        .read_to_end(&mut secret)
        .map_err(|_| SecretResolutionError)?;
    if secret.len() > MAX_FILE_SECRET_BYTES {
        return Err(SecretResolutionError);
    }
    Ok(secret)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretResolutionError;

impl fmt::Display for SecretResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("secret resolution failed")
    }
}

impl std::error::Error for SecretResolutionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthBuildError {
    SecretResolution,
    InvalidSecret,
    DuplicateSecret,
}

impl fmt::Display for AuthBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SecretResolution => "application key resolution failed",
            Self::InvalidSecret => "application key is invalid",
            Self::DuplicateSecret => "application keys contain duplicate material",
        })
    }
}

impl std::error::Error for AuthBuildError {}

#[derive(Clone)]
struct KeyRecord {
    identity: String,
    digest: [u8; 32],
    permissions: Vec<RouteSelector>,
}

#[derive(Clone)]
pub struct ApplicationAuth {
    keys: Vec<KeyRecord>,
}

impl ApplicationAuth {
    pub fn from_validated(
        config: &ValidatedConfig,
        resolver: &impl SecretResolver,
    ) -> Result<Self, AuthBuildError> {
        let mut digests = BTreeSet::new();
        let mut keys = Vec::with_capacity(config.application_keys().len());
        for key in config.application_keys() {
            let digest = if let Some(digest) = key.secret_ref().sha256_digest() {
                *digest
            } else {
                let secret = resolver
                    .resolve(key.secret_ref())
                    .map_err(|_| AuthBuildError::SecretResolution)?;
                hash(canonical_token(key.secret_ref(), secret)?.as_bytes())
            };
            if !digests.insert(digest) {
                return Err(AuthBuildError::DuplicateSecret);
            }
            keys.push(KeyRecord {
                identity: key.id().into(),
                digest,
                permissions: key.permissions().to_vec(),
            });
        }
        Ok(Self { keys })
    }

    pub fn authenticate_headers(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        let values: Vec<_> = headers.get_all(header::AUTHORIZATION).iter().collect();
        if values.len() != 1 {
            return Err(AuthError);
        }
        let value = values[0].to_str().map_err(|_| AuthError)?;
        let token = value.strip_prefix("Bearer ").ok_or(AuthError)?;
        if !valid_b64token(token.as_bytes()) {
            return Err(AuthError);
        }
        self.authenticate_token(token.as_bytes()).ok_or(AuthError)
    }

    fn authenticate_token(&self, token: &[u8]) -> Option<AuthContext> {
        let digest = hash(token);
        let mut selected = None;
        for (index, key) in self.keys.iter().enumerate() {
            if key.digest.ct_eq(&digest).unwrap_u8() == 1 {
                selected = Some(index);
            }
        }
        selected.map(|index| {
            let key = &self.keys[index];
            AuthContext {
                key_identity: key.identity.clone(),
                permissions: key.permissions.clone(),
            }
        })
    }
}

fn canonical_token(
    reference: &SecretReference,
    mut secret: Vec<u8>,
) -> Result<String, AuthBuildError> {
    if matches!(reference, SecretReference::File(_)) {
        strip_one_terminal_newline(&mut secret);
    }
    let token = std::str::from_utf8(&secret).map_err(|_| AuthBuildError::InvalidSecret)?;
    if !valid_b64token(token.as_bytes()) {
        return Err(AuthBuildError::InvalidSecret);
    }
    Ok(token.into())
}

pub(crate) fn canonical_bearer_token(
    reference: &SecretReference,
    secret: Vec<u8>,
) -> Result<String, AuthBuildError> {
    canonical_token(reference, secret)
}

fn strip_one_terminal_newline(secret: &mut Vec<u8>) {
    if secret.last() == Some(&b'\n') {
        secret.pop();
        if secret.last() == Some(&b'\r') {
            secret.pop();
        }
    }
}

fn valid_b64token(token: &[u8]) -> bool {
    if token.is_empty() || token.len() > MAX_BEARER_TOKEN_BYTES {
        return false;
    }
    let mut padding = false;
    for byte in token {
        match byte {
            b'=' if !padding => padding = true,
            b'=' => {}
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'+' | b'/'
                if !padding => {}
            _ => return false,
        }
    }
    !matches!(token[0], b'=')
}

fn hash(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

#[derive(Clone)]
pub struct AuthContext {
    key_identity: String,
    permissions: Vec<RouteSelector>,
}

impl AuthContext {
    pub fn key_identity(&self) -> &str {
        &self.key_identity
    }

    pub fn authorize(&self, selector: &RouteSelector) -> Result<(), ForbiddenError> {
        self.permissions
            .iter()
            .any(|permission| permission == selector)
            .then_some(())
            .ok_or(ForbiddenError)
    }

    pub fn permitted_routes<'a>(
        &'a self,
        registry: &'a Registry,
    ) -> impl Iterator<Item = &'a RouteEntry> + 'a {
        registry
            .routes()
            .filter(|route| self.authorize(&route.identity.selector).is_ok())
    }

    pub fn permitted_models(&self, registry: &Registry) -> Vec<ModelAlias> {
        self.permitted_routes(registry)
            .map(|route| route.identity.selector.model_alias.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthError;

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("authentication failed")
    }
}

impl std::error::Error for AuthError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForbiddenError;

impl fmt::Display for ForbiddenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("permission denied")
    }
}

impl std::error::Error for ForbiddenError {}
