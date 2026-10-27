use std::collections::BTreeSet;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use axum::http::{HeaderMap, header};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::ValidatedConfig;
use crate::core::{ModelAlias, RouteSelector};
use crate::routing::{Registry, RouteEntry, admission::KeyLimits};

use crate::config::secret::valid_b64token;
pub use crate::config::secret::{
    AuthBuildError, EnvironmentSecretResolver, MAX_BEARER_TOKEN_BYTES, SecretResolutionError,
    SecretResolver, canonical_bearer_token,
};

#[derive(Clone)]
struct KeyRecord {
    identity: String,
    digest: [u8; 32],
    permissions: Vec<RouteSelector>,
    expires_at: Option<u64>,
    limits: Option<Arc<KeyLimits>>,
    quotas: crate::keys::quota::KeyQuotas,
}

/// One immutable key set: auth records with their per-key admission limits.
#[derive(Clone)]
pub struct ApplicationAuth {
    keys: Vec<KeyRecord>,
}

impl ApplicationAuth {
    pub(crate) fn key_ids(&self) -> std::collections::BTreeSet<String> {
        self.keys.iter().map(|key| key.identity.clone()).collect()
    }
    pub fn from_validated(config: &ValidatedConfig) -> Result<Self, AuthBuildError> {
        Self::build(config, None)
    }

    /// Rebuilds the key set, keeping a key's limiter when its limits are unchanged
    /// so in-flight counts and token buckets survive a reload.
    pub(crate) fn rebuild(
        config: &ValidatedConfig,
        previous: &Self,
    ) -> Result<Self, AuthBuildError> {
        Self::build(config, Some(previous))
    }

    fn build(config: &ValidatedConfig, previous: Option<&Self>) -> Result<Self, AuthBuildError> {
        let mut digests = BTreeSet::new();
        let mut keys = Vec::with_capacity(config.keys().len());
        for key in config.keys() {
            let digest = *key.digest();
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
                quotas: crate::keys::quota::KeyQuotas {
                    daily: key.daily_quota(),
                    monthly: key.monthly_quota(),
                },
            });
        }
        Ok(Self { keys })
    }

    /// The secret comes from `Authorization: Bearer`, `x-api-key`, or both when each
    /// appears once and both carry the same secret; anything else is `Invalid`.
    pub fn authenticate_headers(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        let bearer = single_header(headers, header::AUTHORIZATION.as_str())?
            .map(|value| value.strip_prefix("Bearer ").ok_or(AuthError::Invalid))
            .transpose()?;
        let api_key = single_header(headers, API_KEY_HEADER)?;
        let token = match (bearer, api_key) {
            (Some(token), None) | (None, Some(token)) => token,
            (Some(bearer), Some(api_key)) => {
                if hash(bearer.as_bytes())
                    .ct_eq(&hash(api_key.as_bytes()))
                    .unwrap_u8()
                    != 1
                {
                    return Err(AuthError::Invalid);
                }
                bearer
            }
            (None, None) => return Err(AuthError::Invalid),
        };
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
            quotas: key.quotas,
        })
    }
}

/// Alternative carrier of the same secret as `Authorization: Bearer`.
const API_KEY_HEADER: &str = "x-api-key";

/// `None` when absent; a repeated or non-visible-ASCII header is `Invalid`.
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, AuthError> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(AuthError::Invalid);
    }
    value.to_str().map(Some).map_err(|_| AuthError::Invalid)
}

/// Shared, atomically replaceable key set. The lock only guards an `Arc` swap
/// and is never held across `.await`.
#[derive(Clone)]
pub struct KeyHandle {
    reload: Arc<RwLock<ReloadStatus>>,
    current: Arc<RwLock<Arc<ApplicationAuth>>>,
}

#[derive(Clone, Default, serde::Serialize)]
pub(crate) struct ReloadStatus {
    pub(crate) enabled: bool,
    pub(crate) healthy: bool,
    pub(crate) failures: u64,
    pub(crate) last_attempt: u64,
    pub(crate) last_success: u64,
}

impl KeyHandle {
    pub(crate) fn reload_status(&self) -> ReloadStatus {
        self.reload
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn record_reload(&self, healthy: bool) {
        let mut status = self.reload.write().unwrap_or_else(PoisonError::into_inner);
        status.enabled = true;
        status.healthy = healthy;
        status.last_attempt = crate::keys::time::now();
        if healthy {
            status.last_success = status.last_attempt;
        } else {
            status.failures = status.failures.saturating_add(1);
        }
    }

    pub(crate) fn new(auth: ApplicationAuth) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(auth))),
            reload: Arc::new(RwLock::new(ReloadStatus::default())),
        }
    }

    pub(crate) fn new_generation(&self, auth: ApplicationAuth) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(auth))),
            reload: self.reload.clone(),
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

fn hash(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

#[derive(Clone)]
pub struct AuthContext {
    key_identity: String,
    permissions: Vec<RouteSelector>,
    limits: Option<Arc<KeyLimits>>,
    quotas: crate::keys::quota::KeyQuotas,
}

impl AuthContext {
    getters! {
        pub(crate) quotas: crate::keys::quota::KeyQuotas,
        pub key_identity: &str,
        /// Limits from the same key set that authenticated this request.
        pub(crate) key_limits(limits): Option<&KeyLimits> = as_deref,
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
