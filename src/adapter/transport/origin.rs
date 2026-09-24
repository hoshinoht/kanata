use crate::{
    config::ValidatedAdapter,
    core::{GatewayError, TrustZone},
};

use super::{request::Endpoint, types};

#[derive(Clone)]
pub(super) struct Origin {
    scheme: &'static str,
    authority: String,
    path_prefix: String,
    permits_credentials: bool,
}

impl Origin {
    pub(super) fn pinned_https(hostname: &'static str) -> Result<Self, GatewayError> {
        if !valid_dns_name(hostname) {
            return Err(types::internal_error());
        }
        Ok(Self {
            scheme: "https",
            authority: hostname.to_owned(),
            path_prefix: String::new(),
            permits_credentials: true,
        })
    }

    #[cfg(test)]
    pub(super) fn test_http(authority: String) -> Self {
        Self::test_with_scheme("http", authority, true)
    }

    #[cfg(test)]
    pub(super) fn test_https(authority: String) -> Self {
        Self::test_with_scheme("https", authority, true)
    }

    #[cfg(test)]
    fn test_with_scheme(
        scheme: &'static str,
        authority: String,
        permits_credentials: bool,
    ) -> Self {
        Self {
            scheme,
            authority,
            path_prefix: "/provider".into(),
            permits_credentials,
        }
    }

    pub(super) fn from_adapter(adapter: &ValidatedAdapter) -> Result<Self, GatewayError> {
        let url = adapter.base_url();
        let scheme = match url.scheme() {
            "http" => "http",
            "https" => "https",
            _ => return Err(types::internal_error()),
        };
        if url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || (adapter.trust_zone() == TrustZone::External && scheme != "https")
        {
            return Err(types::internal_error());
        }
        let host = url.host_str().ok_or_else(types::internal_error)?;
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let authority = host
            + &url
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default();
        Ok(Self {
            scheme,
            authority,
            path_prefix: url.path().trim_end_matches('/').to_owned(),
            permits_credentials: scheme == "https",
        })
    }

    pub(super) fn uri(&self, endpoint: &Endpoint) -> Result<http::Uri, GatewayError> {
        let endpoint = endpoint.path();
        let path = if self.path_prefix.is_empty() {
            format!("/{endpoint}")
        } else {
            format!("{}/{endpoint}", self.path_prefix)
        };
        format!("{}://{}{}", self.scheme, self.authority, path)
            .parse()
            .map_err(|_| types::internal_error())
    }

    pub(super) fn authority(&self) -> &str {
        &self.authority
    }

    pub(super) fn permits_credentials(&self) -> bool {
        self.permits_credentials
    }
}

fn valid_dns_name(hostname: &str) -> bool {
    hostname.len() <= 253
        && !hostname.is_empty()
        && hostname.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}
