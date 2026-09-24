use std::fmt;
use std::future::Future;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

use super::credential::valid_value;
use super::{Credential, CredentialStore, StoreError};

pub const MAX_REFRESH_SKEW: Duration = Duration::from_secs(5 * 60);
pub const MAX_REFRESH_LOCK_WAIT: Duration = Duration::from_secs(30);

pub struct AccessToken {
    value: String,
    account_id: String,
}

impl AccessToken {
    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }
}

impl Clone for AccessToken {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            account_id: self.account_id.clone(),
        }
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken([REDACTED])")
    }
}

impl fmt::Display for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Codex access token [REDACTED]")
    }
}

pub struct RefreshResponse {
    access_token: AccessToken,
    expires_at: SystemTime,
    account_id: String,
    rotated_refresh_token: Option<String>,
}

impl RefreshResponse {
    pub fn new(
        access_token: impl Into<String>,
        expires_at: SystemTime,
        account_id: impl Into<String>,
        rotated_refresh_token: Option<String>,
    ) -> Result<Self, RefreshError> {
        let access_token = access_token.into();
        let account_id = account_id.into();
        if !valid_value(&access_token)
            || !valid_value(&account_id)
            || rotated_refresh_token
                .as_deref()
                .is_some_and(|token| !valid_value(token))
        {
            return Err(RefreshError::InvalidResponse);
        }
        Ok(Self {
            access_token: AccessToken {
                value: access_token,
                account_id: account_id.clone(),
            },
            expires_at,
            account_id,
            rotated_refresh_token,
        })
    }
}

impl fmt::Debug for RefreshResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RefreshResponse([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefreshExchangeError;

impl fmt::Display for RefreshExchangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Codex token refresh exchange failed")
    }
}

impl std::error::Error for RefreshExchangeError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshError {
    InvalidPolicy,
    MissingCredential,
    ExchangeFailed,
    InvalidResponse,
    AccountMismatch,
    Storage(StoreError),
}

impl fmt::Display for RefreshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidPolicy => "invalid Codex refresh policy",
            Self::MissingCredential => "Codex credential is not configured",
            Self::ExchangeFailed => "Codex token refresh exchange failed",
            Self::InvalidResponse => "invalid Codex token refresh response",
            Self::AccountMismatch => "Codex token refresh account mismatch",
            Self::Storage(error) => return error.fmt(formatter),
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RefreshError {}

impl From<StoreError> for RefreshError {
    fn from(error: StoreError) -> Self {
        Self::Storage(error)
    }
}

struct CachedToken {
    access_token: AccessToken,
    expires_at: SystemTime,
}

impl CachedToken {
    fn is_usable_at(&self, now: SystemTime, skew: Duration) -> bool {
        self.expires_at
            .duration_since(now)
            .is_ok_and(|remaining| remaining > skew)
    }
}

pub struct RefreshCoordinator {
    store: CredentialStore,
    refresh_skew: Duration,
    lock_timeout: Duration,
    cached_token: Mutex<Option<CachedToken>>,
}

impl RefreshCoordinator {
    pub fn new(
        store: CredentialStore,
        refresh_skew: Duration,
        lock_timeout: Duration,
    ) -> Result<Self, RefreshError> {
        if refresh_skew > MAX_REFRESH_SKEW || lock_timeout > MAX_REFRESH_LOCK_WAIT {
            return Err(RefreshError::InvalidPolicy);
        }
        Ok(Self {
            store,
            refresh_skew,
            lock_timeout,
            cached_token: Mutex::new(None),
        })
    }

    pub async fn access_token<F, Fut>(&self, exchange: F) -> Result<AccessToken, RefreshError>
    where
        F: FnOnce(Credential) -> Fut + Send,
        Fut: Future<Output = Result<RefreshResponse, RefreshExchangeError>> + Send,
    {
        let mut cached_token = self.cached_token.lock().await;
        if let Some(token) = cached_token
            .as_ref()
            .filter(|token| token.is_usable_at(SystemTime::now(), self.refresh_skew))
        {
            return Ok(token.access_token.clone());
        }
        *cached_token = None;

        self.refresh_locked(&mut cached_token, None, exchange).await
    }

    /// Refresh after a definitive pre-output 401; callers must invoke this at most once per request.
    /// This method does not replay requests.
    pub async fn refresh_after_unauthorized<F, Fut>(
        &self,
        rejected_token: &AccessToken,
        exchange: F,
    ) -> Result<AccessToken, RefreshError>
    where
        F: FnOnce(Credential) -> Fut + Send,
        Fut: Future<Output = Result<RefreshResponse, RefreshExchangeError>> + Send,
    {
        let mut cached_token = self.cached_token.lock().await;
        if let Some(token) = cached_token
            .as_ref()
            .filter(|token| token.is_usable_at(SystemTime::now(), self.refresh_skew))
            && !token.access_token.same_value(rejected_token)
        {
            if token.access_token.account_id() != rejected_token.account_id() {
                return Err(RefreshError::AccountMismatch);
            }
            return Ok(token.access_token.clone());
        }
        *cached_token = None;

        self.refresh_locked(
            &mut cached_token,
            Some(rejected_token.account_id()),
            exchange,
        )
        .await
    }

    /// Remove the local credential and cached token without remote revocation.
    pub async fn logout(&self) -> Result<(), RefreshError> {
        let mut cached_token = self.cached_token.lock().await;
        *cached_token = None;
        let locked = self.store.lock(self.lock_timeout).await?;
        locked.logout()?;
        Ok(())
    }

    async fn refresh_locked<F, Fut>(
        &self,
        cached_token: &mut Option<CachedToken>,
        expected_account_id: Option<&str>,
        exchange: F,
    ) -> Result<AccessToken, RefreshError>
    where
        F: FnOnce(Credential) -> Fut + Send,
        Fut: Future<Output = Result<RefreshResponse, RefreshExchangeError>> + Send,
    {
        let locked = self.store.lock(self.lock_timeout).await?;
        let credential = locked.load()?.ok_or(RefreshError::MissingCredential)?;
        if expected_account_id.is_some_and(|expected| expected != credential.account_id()) {
            return Err(RefreshError::AccountMismatch);
        }
        let original_refresh_token = credential.refresh_token().to_owned();
        let account_id = credential.account_id().to_owned();
        let response = exchange(credential)
            .await
            .map_err(|_| RefreshError::ExchangeFailed)?;

        if response.account_id != account_id {
            return Err(RefreshError::AccountMismatch);
        }
        if !response
            .expires_at
            .duration_since(SystemTime::now())
            .is_ok_and(|remaining| remaining > self.refresh_skew)
        {
            return Err(RefreshError::InvalidResponse);
        }

        let mut rotated = false;
        if let Some(refresh_token) = response.rotated_refresh_token
            && refresh_token != original_refresh_token
        {
            let credential = Credential::new(refresh_token, account_id)?;
            locked.save(&credential)?;
            rotated = true;
        }

        let expires_at = response.expires_at;
        let access_token = response.access_token;
        drop(locked);

        let cached = CachedToken {
            access_token,
            expires_at,
        };
        if !cached.is_usable_at(SystemTime::now(), self.refresh_skew) {
            return Err(RefreshError::InvalidResponse);
        }
        let result = cached.access_token.clone();
        *cached_token = Some(cached);
        tracing::info!(
            target: "kanata::upstream",
            provider = "codex",
            rotated,
            "upstream access token refreshed",
        );
        Ok(result)
    }
}

impl AccessToken {
    fn same_value(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
