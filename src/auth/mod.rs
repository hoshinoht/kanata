use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::sync::{Arc, PoisonError, RwLock};

use axum::http::{HeaderMap, header};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::{SecretReference, ValidatedConfig};
use crate::core::{ModelAlias, RouteSelector};
use crate::routing::{Registry, RouteEntry, admission::KeyLimits};

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
    expires_at: Option<u64>,
    limits: Option<Arc<KeyLimits>>,
}

/// One immutable key set: auth records with their per-key admission limits.
#[derive(Clone)]
pub struct ApplicationAuth {
    keys: Vec<KeyRecord>,
}

impl ApplicationAuth {
    pub fn from_validated(
        config: &ValidatedConfig,
        resolver: &impl SecretResolver,
    ) -> Result<Self, AuthBuildError> {
        Self::build(config, resolver, None)
    }

    /// Rebuilds the key set, keeping a key's limiter when its limits are unchanged
    /// so in-flight counts and token buckets survive a reload.
    pub(crate) fn rebuild(
        config: &ValidatedConfig,
        resolver: &impl SecretResolver,
        previous: &Self,
    ) -> Result<Self, AuthBuildError> {
        Self::build(config, resolver, Some(previous))
    }

    fn build(
        config: &ValidatedConfig,
        resolver: &impl SecretResolver,
        previous: Option<&Self>,
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
            let reused = previous
                .and_then(|previous| {
                    previous
                        .keys
                        .iter()
                        .find(|record| record.identity == key.id())
                })
                .and_then(|record| record.limits.clone())
                .filter(|limits| limits.same_limits(key.max_in_flight(), key.rate_limit()));
            keys.push(KeyRecord {
                identity: key.id().into(),
                digest,
                permissions: key.permissions().to_vec(),
                expires_at: key.expires_at(),
                limits: reused.or_else(|| KeyLimits::build(key.max_in_flight(), key.rate_limit())),
            });
        }
        Ok(Self { keys })
    }

    pub fn authenticate_headers(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        let values: Vec<_> = headers.get_all(header::AUTHORIZATION).iter().collect();
        if values.len() != 1 {
            return Err(AuthError::Invalid);
        }
        let value = values[0].to_str().map_err(|_| AuthError::Invalid)?;
        let token = value.strip_prefix("Bearer ").ok_or(AuthError::Invalid)?;
        if !valid_b64token(token.as_bytes()) {
            return Err(AuthError::Invalid);
        }
        self.authenticate_token(token.as_bytes(), crate::keys::time::now())
    }

    fn authenticate_token(&self, token: &[u8], now: u64) -> Result<AuthContext, AuthError> {
        let digest = hash(token);
        let mut selected = None;
        for (index, key) in self.keys.iter().enumerate() {
            if key.digest.ct_eq(&digest).unwrap_u8() == 1 {
                selected = Some(index);
            }
        }
        let key = &self.keys[selected.ok_or(AuthError::Invalid)?];
        // Only the holder of the exact secret reaches this point.
        if key.expires_at.is_some_and(|at| now >= at) {
            return Err(AuthError::Expired {
                key_id: key.identity.clone(),
            });
        }
        Ok(AuthContext {
            key_identity: key.identity.clone(),
            permissions: key.permissions.clone(),
            limits: key.limits.clone(),
        })
    }
}

/// Shared, atomically replaceable key set. The lock only guards an `Arc` swap
/// and is never held across `.await`.
#[derive(Clone)]
pub struct KeyHandle {
    current: Arc<RwLock<Arc<ApplicationAuth>>>,
}

impl KeyHandle {
    pub(crate) fn new(auth: ApplicationAuth) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(auth))),
        }
    }

    /// The key set in effect now.
    pub fn current(&self) -> Arc<ApplicationAuth> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn replace(&self, auth: ApplicationAuth) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(auth);
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
    limits: Option<Arc<KeyLimits>>,
}

impl AuthContext {
    pub fn key_identity(&self) -> &str {
        &self.key_identity
    }

    /// Limits from the same key set that authenticated this request.
    pub(crate) fn key_limits(&self) -> Option<&KeyLimits> {
        self.limits.as_deref()
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

/// Unknown and revoked keys are both `Invalid`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthError {
    Invalid,
    Expired { key_id: String },
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "authentication failed",
            Self::Expired { .. } => "key expired",
        })
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
