use serde::{Deserialize, de::IgnoredAny};

use crate::core::{ErrorKind, GatewayError};

use super::response::UsagePayload;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StreamChunk {
    pub(super) id: String,
    pub(super) object: String,
    #[serde(rename = "created")]
    pub(super) _created: u64,
    pub(super) model: String,
    #[serde(default)]
    pub(super) provider: Option<String>,
    #[serde(default)]
    pub(super) system_fingerprint: Option<String>,
    #[serde(default)]
    pub(super) service_tier: Option<String>,
    #[serde(default, rename = "openrouter_metadata")]
    pub(super) _openrouter_metadata: Option<IgnoredAny>,
    #[serde(default)]
    pub(super) choices: Vec<StreamChoice>,
    #[serde(default)]
    pub(super) usage: Option<UsagePayload>,
    #[serde(default)]
    pub(super) error: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StreamChoice {
    pub(super) index: u64,
    pub(super) delta: StreamDelta,
    #[serde(default)]
    pub(super) finish_reason: Option<String>,
    #[serde(default)]
    pub(super) native_finish_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StreamDelta {
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) content: Option<String>,
}

pub(super) fn parse(data: &str) -> Result<StreamChunk, GatewayError> {
    serde_json::from_str(data).map_err(|_| upstream_failure())
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
