use std::fmt;

use super::StoreError;

const MAX_VALUE_BYTES: usize = 4 * 1024;

pub struct Credential {
    refresh_token: String,
    account_id: String,
}

impl Credential {
    pub fn new(
        refresh_token: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Result<Self, StoreError> {
        let refresh_token = refresh_token.into();
        let account_id = account_id.into();
        if !valid_value(&refresh_token) || !valid_value(&account_id) {
            return Err(StoreError::InvalidCredential);
        }
        Ok(Self {
            refresh_token,
            account_id,
        })
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credential([REDACTED])")
    }
}

impl fmt::Display for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Codex credential [REDACTED]")
    }
}

pub(super) fn valid_value(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_VALUE_BYTES
        && !value.chars().any(char::is_control)
}
