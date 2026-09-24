use bytes::Bytes;
use futures_core::Stream;
use http::{HeaderMap, header};
use std::pin::Pin;

use crate::core::{ErrorKind, GatewayError};

pub(super) const MAX_HEADERS: usize = 64;
pub(super) const MAX_HEADER_BYTES: usize = 64 * 1024;
pub(super) const MAX_CHUNK_BYTES: usize = 16 * 1024;

pub(crate) const COMPLETE_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const STREAM_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;
pub(crate) const AUTH_RESPONSE_BYTES: usize = 16 * 1024;

pub(crate) type Connector = hyper_rustls::HttpsConnector<
    hyper_util::client::legacy::connect::HttpConnector<super::resolver::Resolver>,
>;
pub(crate) type ResponseBody = Pin<Box<dyn Stream<Item = Result<Bytes, GatewayError>> + Send>>;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ResponseContentType {
    Json,
    Form,
    EventStream,
}

pub(crate) struct TransportResponse {
    pub(crate) status: u16,
    pub(crate) content_type: Option<ResponseContentType>,
    pub(crate) content_type_present: bool,
    /// Sanitized media type for diagnostics only.
    pub(crate) media_type: Option<String>,
    pub(crate) body: ResponseBody,
}

/// Lowercase media type without parameters, if it is short and plain.
pub(super) fn diagnostic_media_type(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    let media_type = value.split(';').next()?.trim().to_ascii_lowercase();
    (!media_type.is_empty()
        && media_type.len() <= 64
        && media_type
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/.+-_".contains(&byte)))
    .then_some(media_type)
}

pub(super) fn headers_within_limit(headers: &HeaderMap) -> bool {
    headers.len() <= MAX_HEADERS
        && headers
            .iter()
            .try_fold(0usize, |total, (name, value)| {
                total
                    .checked_add(name.as_str().len())?
                    .checked_add(value.as_bytes().len())
            })
            .is_some_and(|bytes| bytes <= MAX_HEADER_BYTES)
}

pub(super) fn non_identity(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| {
            value.as_bytes().split(|byte| *byte == b',').any(|token| {
                match std::str::from_utf8(token) {
                    Ok(token) => !token.trim().eq_ignore_ascii_case("identity"),
                    Err(_) => true,
                }
            })
        })
}

pub(super) fn parse_response_content_type(
    headers: &HeaderMap,
) -> Result<Option<ResponseContentType>, GatewayError> {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(upstream_failure());
    }
    let value = value.to_str().map_err(|_| upstream_failure())?;
    if value.contains(',') {
        return Err(upstream_failure());
    }
    let mut parts = value.split(';');
    let media_type = parts.next().map(str::trim).ok_or_else(upstream_failure)?;
    if !valid_media_type(media_type) {
        return Err(upstream_failure());
    }
    for parameter in parts {
        if !valid_media_parameter(parameter.trim()) {
            return Err(upstream_failure());
        }
    }
    Ok(match media_type.to_ascii_lowercase().as_str() {
        "application/json" => Some(ResponseContentType::Json),
        "application/x-www-form-urlencoded" => Some(ResponseContentType::Form),
        "text/event-stream" => Some(ResponseContentType::EventStream),
        _ => None,
    })
}

fn valid_media_type(value: &str) -> bool {
    let Some((kind, subtype)) = value.split_once('/') else {
        return false;
    };
    !kind.is_empty()
        && !subtype.is_empty()
        && kind.bytes().all(is_media_token)
        && subtype.bytes().all(is_media_token)
}

fn valid_media_parameter(value: &str) -> bool {
    let Some((name, value)) = value.split_once('=') else {
        return false;
    };
    if name.is_empty() || !name.bytes().all(is_media_token) || value.is_empty() {
        return false;
    }
    if value.starts_with('"') {
        if !value.ends_with('"') || value.len() < 3 {
            return false;
        }
        let mut escaped = false;
        for byte in value[1..value.len() - 1].bytes() {
            if escaped {
                if byte < 0x20 || byte == 0x7f || byte == b',' {
                    return false;
                }
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte < 0x20 || byte == 0x7f || byte == b',' || byte == b'"' {
                return false;
            }
        }
        !escaped
    } else {
        value.bytes().all(is_media_token)
    }
}

fn is_media_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

pub(super) fn timeout(phase: crate::core::TimeoutPhase) -> GatewayError {
    GatewayError {
        kind: ErrorKind::Timeout { phase },
    }
}

pub(super) fn unavailable() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamUnavailable,
    }
}

pub(super) fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}

pub(super) fn internal_error() -> GatewayError {
    GatewayError {
        kind: ErrorKind::Internal,
    }
}
