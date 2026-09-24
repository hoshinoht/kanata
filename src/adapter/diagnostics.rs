//! Operator-facing upstream failure logs. Never logs tokens, headers, or request bodies;
//! provider messages are logged only at DEBUG.

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use crate::adapter::transport::ResponseBody;
use crate::telemetry::sanitize;

const TARGET: &str = "kanata::upstream";
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const ERROR_BODY_WAIT: Duration = Duration::from_millis(500);
const UNSET: &str = "-";

#[derive(Clone)]
pub(crate) struct UpstreamLabel {
    adapter: String,
    provider: &'static str,
}

impl UpstreamLabel {
    pub(crate) fn new(adapter: &str, provider: &'static str) -> Self {
        Self {
            adapter: adapter.to_owned(),
            provider,
        }
    }

    /// Logs a non-success upstream status, reading at most 8 KiB of its body.
    pub(crate) async fn status(&self, status: u16, body: &mut ResponseBody) {
        if !tracing::enabled!(target: TARGET, tracing::Level::WARN) {
            return;
        }
        let bytes = read_prefix(body).await;
        let error = serde_json::from_slice::<Value>(&bytes)
            .map(|value| ProviderError::from_json(&value))
            .unwrap_or_default();
        self.log(Some(status), None, &error, "upstream error response");
    }

    pub(crate) fn content_type(&self, status: u16, media_type: Option<&str>) {
        tracing::warn!(
            target: TARGET,
            adapter = %self.adapter,
            provider = self.provider,
            status,
            media_type = media_type.unwrap_or("-"),
            "upstream response has unexpected content type",
        );
    }

    /// `reason` must be a secret-free category, such as a unit error variant.
    pub(crate) fn credential(&self, reason: impl std::fmt::Debug) {
        tracing::warn!(
            target: TARGET,
            adapter = %self.adapter,
            provider = self.provider,
            reason = ?reason,
            "upstream credential unavailable",
        );
    }

    /// Logs a failure found while parsing an otherwise successful response stream.
    pub(crate) fn stream(&self, event: &str, error: &ProviderError) {
        self.log(None, Some(event), error, "upstream stream failed");
    }

    fn log(
        &self,
        status: Option<u16>,
        event: Option<&str>,
        error: &ProviderError,
        summary: &'static str,
    ) {
        tracing::warn!(
            target: TARGET,
            adapter = %self.adapter,
            provider = self.provider,
            status,
            event,
            code = error.code.as_deref().unwrap_or(UNSET),
            error_type = error.error_type.as_deref().unwrap_or(UNSET),
            "{summary}",
        );
        if let Some(message) = error.message.as_deref() {
            tracing::debug!(
                target: TARGET,
                adapter = %self.adapter,
                provider = self.provider,
                message,
                "upstream error message",
            );
        }
    }
}

/// `body` must be an error response; success bodies carry tokens.
pub(crate) fn token_refresh_failed(
    provider: &'static str,
    category: &'static str,
    status: Option<u16>,
    body: Option<&[u8]>,
) {
    let error = body
        .map(|body| &body[..body.len().min(MAX_ERROR_BODY_BYTES)])
        .and_then(|body| serde_json::from_slice::<Value>(body).ok())
        .map(|value| ProviderError::from_json(&value))
        .unwrap_or_default();
    tracing::warn!(
        target: TARGET,
        provider,
        category,
        status,
        code = error.code.as_deref().unwrap_or(UNSET),
        error_type = error.error_type.as_deref().unwrap_or(UNSET),
        "upstream token refresh failed",
    );
    if let Some(message) = error.message.as_deref() {
        tracing::debug!(target: TARGET, provider, message, "upstream token refresh error message");
    }
}

/// Sanitized provider error fields.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderError {
    pub(crate) code: Option<String>,
    pub(crate) error_type: Option<String>,
    pub(crate) message: Option<String>,
}

impl ProviderError {
    /// Accepts `{"error":{...}}`, `{"error":"..."}`, and flat `code`/`type`/`message`/`detail` shapes.
    pub(crate) fn from_json(value: &Value) -> Self {
        let mut error = Self::default();
        let scope = match value.get("error") {
            Some(Value::String(text)) => {
                match sanitize::token(text).filter(|token| token == text) {
                    Some(code) => error.code = Some(code),
                    None => error.message = Some(sanitize::message(text)),
                }
                value
            }
            Some(object @ Value::Object(_)) => object,
            _ => value,
        };
        if error.code.is_none() {
            error.code = scope.get("code").and_then(scalar_token);
        }
        error.error_type = scope.get("type").and_then(scalar_token);
        if error.message.is_none() {
            error.message = ["message", "error_description", "detail"]
                .iter()
                .find_map(|field| scope.get(*field).or_else(|| value.get(*field)))
                .and_then(Value::as_str)
                .map(sanitize::message);
        }
        error
    }
}

fn scalar_token(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => sanitize::token(text),
        Value::Number(number) => sanitize::token(&number.to_string()),
        _ => None,
    }
}

async fn read_prefix(body: &mut ResponseBody) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(ERROR_BODY_WAIT, async {
        while bytes.len() < MAX_ERROR_BODY_BYTES {
            let Some(Ok(chunk)) = body.next().await else {
                break;
            };
            let take = chunk.len().min(MAX_ERROR_BODY_BYTES - bytes.len());
            bytes.extend_from_slice(&chunk[..take]);
        }
    })
    .await;
    bytes
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ProviderError;

    #[test]
    fn provider_error_shapes_are_sanitized() {
        let nested = ProviderError::from_json(&json!({
            "error": {"code": "bad\ncode", "type": "invalid_request_error", "message": "m\u{1b}x"}
        }));
        assert_eq!(nested.code.as_deref(), Some("badcode"));
        assert_eq!(nested.error_type.as_deref(), Some("invalid_request_error"));
        assert_eq!(nested.message.as_deref(), Some("m x"));

        let oauth = ProviderError::from_json(
            &json!({"error": "invalid_grant", "error_description": "expired"}),
        );
        assert_eq!(oauth.code.as_deref(), Some("invalid_grant"));
        assert_eq!(oauth.message.as_deref(), Some("expired"));

        let plain = ProviderError::from_json(&json!({"error": "model 'x' not found"}));
        assert_eq!(plain.code, None);
        assert_eq!(plain.message.as_deref(), Some("model 'x' not found"));

        let numeric = ProviderError::from_json(&json!({"error": {"code": 429}}));
        assert_eq!(numeric.code.as_deref(), Some("429"));

        let detail = ProviderError::from_json(&json!({"detail": "Unsupported model"}));
        assert_eq!(detail.message.as_deref(), Some("Unsupported model"));
    }
}
