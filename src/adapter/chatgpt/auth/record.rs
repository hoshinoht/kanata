use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{AuthError, ISSUER};

pub(super) const MAX_RECORD_BYTES: usize = 1024 * 1024;
pub(super) const MAX_PROFILES: usize = 16;
pub(super) const MAX_TOKEN_BYTES: usize = 16 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    pub version: u8,
    pub host_id: String,
    pub revision: u64,
    pub active_profile: Option<String>,
    pub profiles: BTreeMap<String, Registration>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registration {
    pub client_id: String,
    pub identity: Option<Identity>,
    pub session: Option<Session>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Session {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: String,
    pub scopes: Vec<String>,
    pub saved_at: u64,
    pub expires_at: u64,
    pub earliest_refresh_at: Option<u64>,
}

impl State {
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            version: 1,
            host_id: host_id()?,
            revision: 0,
            active_profile: None,
            profiles: BTreeMap::new(),
        })
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, AuthError> {
        let state: Self = serde_json::from_slice(bytes).map_err(|_| AuthError::Storage)?;
        state.validate()?;
        Ok(state)
    }
    pub fn encode(&self) -> Result<Vec<u8>, AuthError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| AuthError::Storage)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(AuthError::Storage);
        }
        Ok(bytes)
    }
    pub fn changed(&mut self) -> Result<(), AuthError> {
        self.revision = self.revision.checked_add(1).ok_or(AuthError::Storage)?;
        Ok(())
    }
    fn validate(&self) -> Result<(), AuthError> {
        if self.version != 1 || !valid_host_id(&self.host_id) || self.profiles.len() > MAX_PROFILES
        {
            return Err(AuthError::Storage);
        }
        for (name, registration) in &self.profiles {
            validate_profile(name).map_err(|_| AuthError::Storage)?;
            if !valid_client_id(&registration.client_id) {
                return Err(AuthError::Storage);
            }
            if let Some(identity) = &registration.identity
                && (identity.issuer != ISSUER
                    || !valid_value(&identity.subject, 512)
                    || identity
                        .email
                        .as_ref()
                        .is_some_and(|email| !valid_value(email, 512)))
            {
                return Err(AuthError::Storage);
            }
            if let Some(session) = &registration.session
                && (registration.identity.is_none() || session.validate().is_err())
            {
                return Err(AuthError::Storage);
            }
        }
        if self.active_profile.as_ref().is_some_and(|name| {
            self.profiles
                .get(name)
                .is_none_or(|p| p.identity.is_none() || p.session.is_none())
        }) {
            return Err(AuthError::Storage);
        }
        Ok(())
    }
}

impl Session {
    pub fn plan_enabled(&self) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope == "chatgpt.tokens.use.direct")
            && self.scopes.iter().any(|scope| scope == "resource.invoke")
    }
    pub fn validate(&self) -> Result<(), AuthError> {
        if !valid_value(&self.access_token, MAX_TOKEN_BYTES)
            || !valid_value(&self.id_token, MAX_TOKEN_BYTES)
            || self
                .refresh_token
                .as_ref()
                .is_some_and(|token| !valid_value(token, MAX_TOKEN_BYTES))
            || self.expires_at <= self.saved_at
            || self.expires_at - self.saved_at > 86_400
            || self.scopes.len() > 64
            || self
                .scopes
                .iter()
                .any(|s| !valid_value(s, 256) || s.contains(' '))
            || self
                .earliest_refresh_at
                .is_some_and(|value| value > self.expires_at)
        {
            return Err(AuthError::InvalidResponse);
        }
        Ok(())
    }
}

pub(super) fn validate_profile(profile: &str) -> Result<(), AuthError> {
    if profile.is_empty()
        || profile.len() > 64
        || !profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(AuthError::InvalidProfile);
    }
    Ok(())
}
pub(super) fn valid_client_id(value: &str) -> bool {
    value != "dynamic_agent_client"
        && valid_value(value, 256)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
pub(super) fn valid_value(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn host_id() -> Result<String, AuthError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| AuthError::Network)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
fn valid_host_id(value: &str) -> bool {
    let Some(uuid) = value.strip_prefix("urn:uuid:") else {
        return false;
    };
    let bytes = uuid.as_bytes();
    bytes.len() == 36
        && bytes[14] == b'4'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        && bytes.iter().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
            }
        })
}
