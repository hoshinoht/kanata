//! Per-key request and token allowances.

use serde::{Deserialize, Serialize};

use crate::ConfigErrorClass;

/// One allowance; `daily_quota` and `monthly_quota` share this shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DailyQuota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_tokens: Option<u64>,
}

impl DailyQuota {
    pub fn validate(self) -> Result<(), ConfigErrorClass> {
        if self.requests.is_none() && self.tokens.is_none() {
            return Err(ConfigErrorClass::Empty);
        }
        if [self.requests, self.tokens, self.reservation_tokens].contains(&Some(0)) {
            return Err(ConfigErrorClass::Zero);
        }
        match (self.tokens, self.reservation_tokens) {
            (Some(total), Some(reservation)) if reservation <= total => Ok(()),
            (None, None) => Ok(()),
            _ => Err(ConfigErrorClass::InvalidTokenReservation),
        }
    }
}
