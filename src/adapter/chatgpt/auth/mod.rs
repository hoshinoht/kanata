mod error;
mod file;
mod jwt;
mod login;
mod net;
mod record;
mod store;
#[cfg(test)]
mod tests;

use std::{
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;

use crate::config::{ValidatedChatgptAuth, ValidatedTimeouts};

pub use error::AuthError;
pub use login::LoginFlow;
use record::{Registration, State};

const ISSUER: &str = "https://auth.openai.com";

#[derive(Clone)]
pub struct AuthManager {
    store: store::Store,
    client: Arc<net::Client>,
}
#[derive(Debug, Serialize)]
pub struct AuthStatus {
    pub active_profile: Option<String>,
    pub profiles: Vec<ProfileStatus>,
}
#[derive(Debug, Serialize)]
pub struct ProfileStatus {
    pub profile: String,
    pub client_id: String,
    pub email: Option<String>,
    pub signed_in: bool,
    pub plan_enabled: bool,
    pub access_expires_at: Option<u64>,
}
#[derive(Debug, Serialize)]
pub struct LogoutOutcome {
    pub profile: String,
    pub remote_revocation_confirmed: bool,
}
pub struct AccessToken(String);
impl AccessToken {
    pub fn bearer(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken([REDACTED])")
    }
}

impl AuthManager {
    pub fn new(
        settings: &ValidatedChatgptAuth,
        timeouts: &ValidatedTimeouts,
    ) -> Result<Self, AuthError> {
        Ok(Self {
            store: store::Store::new(settings.state_dir()),
            client: Arc::new(net::Client::new(timeouts)?),
        })
    }
    pub async fn status(&self) -> Result<AuthStatus, AuthError> {
        let locked = self.store.lock().await?;
        let Some(state) = locked.load()? else {
            return Ok(AuthStatus {
                active_profile: None,
                profiles: Vec::new(),
            });
        };
        Ok(AuthStatus {
            active_profile: state.active_profile,
            profiles: state
                .profiles
                .iter()
                .map(|(name, registration)| profile_status(name, registration))
                .collect(),
        })
    }
    pub async fn begin_login(&self, profile: &str) -> Result<LoginFlow, AuthError> {
        LoginFlow::begin(self.clone(), profile).await
    }
    pub async fn access_token(&self) -> Result<AccessToken, AuthError> {
        let locked = self.store.lock().await?;
        let manager = self.clone();
        tokio::spawn(async move { manager.access_token_inner(locked).await })
            .await
            .map_err(|_| AuthError::Network)?
    }
    async fn access_token_inner(&self, locked: store::Locked) -> Result<AccessToken, AuthError> {
        let mut state = locked.load()?.ok_or(AuthError::NotSignedIn)?;
        let name = state.active_profile.clone().ok_or(AuthError::NotSignedIn)?;
        let registration = state.profiles.get(&name).ok_or(AuthError::NotSignedIn)?;
        let session = registration
            .session
            .as_ref()
            .ok_or(AuthError::NotSignedIn)?;
        let now = unix_now()?;
        if now < session.saved_at {
            return Err(AuthError::Clock);
        }
        if !session.plan_enabled() {
            return Err(AuthError::PermissionDenied);
        }
        if session.expires_at > now.saturating_add(60)
            || (session.expires_at > now
                && session
                    .earliest_refresh_at
                    .is_some_and(|earliest| now < earliest))
        {
            return Ok(AccessToken(session.access_token.clone()));
        }
        if session
            .earliest_refresh_at
            .is_some_and(|earliest| now < earliest)
        {
            return Err(AuthError::RefreshTooEarly);
        }
        let refresh = session
            .refresh_token
            .as_deref()
            .ok_or(AuthError::NotSignedIn)?;
        let metadata = self.client.discovery().await?;
        let jwks = self.client.jwks(&metadata).await?;
        let response = self
            .client
            .token(&[
                ("grant_type", "refresh_token"),
                ("client_id", &registration.client_id),
                ("refresh_token", refresh),
                ("resource", net::RESOURCE),
            ])
            .await;
        let response = match response {
            Err(AuthError::InvalidGrant) => {
                state
                    .profiles
                    .get_mut(&name)
                    .ok_or(AuthError::NotSignedIn)?
                    .session = None;
                state.active_profile = None;
                state.changed()?;
                locked.save(&state)?;
                return Err(AuthError::InvalidGrant);
            }
            other => other?,
        };
        let identity = registration
            .identity
            .as_ref()
            .ok_or(AuthError::NotSignedIn)?;
        if let Some(id_token) = &response.id_token {
            let verified = jwt::verify(
                id_token,
                &jwks,
                &registration.client_id,
                None,
                &response.access_token,
                unix_now()?,
            )?;
            if verified.issuer != identity.issuer || verified.subject != identity.subject {
                return Err(AuthError::InvalidIdentity);
            }
        }
        let next = response.session(unix_now()?, Some(&session.id_token), true)?;
        let permitted = next.plan_enabled();
        let access = AccessToken(next.access_token.clone());
        state
            .profiles
            .get_mut(&name)
            .ok_or(AuthError::NotSignedIn)?
            .session = Some(next);
        state.changed()?;
        locked.save(&state)?;
        if !permitted {
            return Err(AuthError::PermissionDenied);
        }
        Ok(access)
    }
    pub async fn logout(&self, profile: Option<&str>) -> Result<LogoutOutcome, AuthError> {
        if let Some(name) = profile {
            record::validate_profile(name)?;
        }
        let locked = self.store.lock().await?;
        let manager = self.clone();
        let profile = profile.map(str::to_owned);
        tokio::spawn(async move { manager.logout_inner(locked, profile).await })
            .await
            .map_err(|_| AuthError::Network)?
    }
    async fn logout_inner(
        &self,
        locked: store::Locked,
        profile: Option<String>,
    ) -> Result<LogoutOutcome, AuthError> {
        let mut state = locked.load()?.ok_or(AuthError::NotSignedIn)?;
        let name = profile
            .or_else(|| state.active_profile.clone())
            .ok_or(AuthError::NotSignedIn)?;
        let registration = state.profiles.get(&name).ok_or(AuthError::NotSignedIn)?;
        let refresh = registration
            .session
            .as_ref()
            .and_then(|session| session.refresh_token.clone());
        let client_id = registration.client_id.clone();
        if state.active_profile.as_deref() == Some(&name) {
            state.active_profile = None;
        }
        state.changed()?;
        locked.save(&state)?;
        let remote_revocation_confirmed = if let Some(refresh) = refresh {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.client.revoke(&client_id, &refresh),
            )
            .await
            .unwrap_or(false)
        } else {
            false
        };
        state
            .profiles
            .get_mut(&name)
            .ok_or(AuthError::NotSignedIn)?
            .session = None;
        state.changed()?;
        locked.save(&state)?;
        Ok(LogoutOutcome {
            profile: name,
            remote_revocation_confirmed,
        })
    }
}
fn profile_status(name: &str, registration: &Registration) -> ProfileStatus {
    ProfileStatus {
        profile: name.into(),
        client_id: registration.client_id.clone(),
        email: registration
            .identity
            .as_ref()
            .and_then(|identity| identity.email.clone()),
        signed_in: registration.session.is_some(),
        plan_enabled: registration
            .session
            .as_ref()
            .is_some_and(|s| s.plan_enabled()),
        access_expires_at: registration
            .session
            .as_ref()
            .map(|session| session.expires_at),
    }
}
fn random_secret() -> Result<String, AuthError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| AuthError::Network)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn unix_now() -> Result<u64, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| AuthError::Clock)
}
