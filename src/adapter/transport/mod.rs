mod body;
mod driver;
mod encoded;
mod multipart;
mod origin;
mod request;
mod resolver;
pub(crate) mod sse;
mod time;
mod types;

#[cfg(test)]
mod tests;

use hyper::client::conn::http1;
use hyper_rustls::HttpsConnectorBuilder;
use tower::Service;

use crate::{
    config::{ValidatedAdapter, ValidatedTimeouts},
    core::GatewayError,
};

use self::{
    body::bounded_body,
    driver::RequestDriver,
    origin::Origin,
    request::build_request,
    time::{PhaseDeadline, Timeouts},
    types::{Connector, headers_within_limit, non_identity, parse_response_content_type},
};

#[allow(unused_imports)]
pub(crate) use multipart::{MultipartFile, MultipartRequest};
#[allow(unused_imports)]
pub(crate) use request::{Accept, CredentialHeader, Endpoint, TransportRequest};
#[allow(unused_imports)]
pub(crate) use types::{
    AUTH_RESPONSE_BYTES, COMPLETE_RESPONSE_BYTES, OAUTH_RESPONSE_BYTES, ResponseBody,
    ResponseContentType, STREAM_RESPONSE_BYTES, TransportResponse,
};

pub(crate) struct Transport {
    origin: Origin,
    connector: Connector,
    timeouts: Timeouts,
}

impl Transport {
    pub(crate) fn new(
        adapter: &ValidatedAdapter,
        timeouts: &ValidatedTimeouts,
    ) -> Result<Self, GatewayError> {
        let origin = Origin::from_adapter(adapter)?;
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new_with_resolver(
            resolver::production(),
        );
        http.enforce_http(false);
        let connector = HttpsConnectorBuilder::new()
            .try_with_platform_verifier()
            .map_err(|_| types::internal_error())?
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Ok(Self {
            origin,
            connector,
            timeouts: Timeouts::from_validated(timeouts),
        })
    }

    pub(crate) fn new_pinned_https(
        hostname: &'static str,
        timeouts: &ValidatedTimeouts,
    ) -> Result<Self, GatewayError> {
        let origin = Origin::pinned_https(hostname)?;
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new_with_resolver(
            resolver::production(),
        );
        http.enforce_http(false);
        let connector = HttpsConnectorBuilder::new()
            .try_with_platform_verifier()
            .map_err(|_| types::internal_error())?
            .https_only()
            .enable_http1()
            .wrap_connector(http);
        Ok(Self {
            origin,
            connector,
            timeouts: Timeouts::from_validated(timeouts),
        })
    }

    #[cfg(test)]
    fn new_with_connector(
        adapter: &ValidatedAdapter,
        timeouts: &ValidatedTimeouts,
        connector: Connector,
    ) -> Result<Self, GatewayError> {
        Ok(Self {
            origin: Origin::from_adapter(adapter)?,
            connector,
            timeouts: Timeouts::from_validated(timeouts),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_with_test_root(
        adapter: &ValidatedAdapter,
        timeouts: &ValidatedTimeouts,
        certificate: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
        address: std::net::SocketAddr,
    ) -> Result<Self, GatewayError> {
        Self::new_with_connector(adapter, timeouts, fixture_connector(certificate, address))
    }

    #[cfg(test)]
    pub(crate) fn new_pinned_https_with_test_root(
        hostname: &'static str,
        timeouts: &ValidatedTimeouts,
        certificate: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
        address: std::net::SocketAddr,
    ) -> Result<Self, GatewayError> {
        Ok(Self {
            origin: Origin::pinned_https(hostname)?,
            connector: fixture_connector_for_host(Some(hostname), certificate, address),
            timeouts: Timeouts::from_validated(timeouts),
        })
    }

    pub(crate) async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, GatewayError> {
        if (request.credential.is_some() || request.body.is_sensitive())
            && !self.origin.permits_credentials()
        {
            return Err(types::internal_error());
        }
        let uri = self.origin.uri(&request.endpoint)?;
        let budget = request.response_budget;
        let request = build_request(&self.origin, uri.clone(), request)?;

        let connect_deadline =
            PhaseDeadline::from_now(self.timeouts.connect, crate::core::TimeoutPhase::Connect)?;
        let mut connector = self.connector.clone();
        let io = time::run(connect_deadline, move || async move {
            connector.call(uri).await.map_err(|_| types::unavailable())
        })
        .await?;

        let headers_deadline =
            PhaseDeadline::from_now(self.timeouts.headers, crate::core::TimeoutPhase::Headers)?;
        let mut builder = http1::Builder::new();
        builder
            .max_headers(types::MAX_HEADERS)
            .max_buf_size(types::MAX_HEADER_BYTES);
        let (sender, connection) = time::run(headers_deadline, move || async move {
            builder
                .handshake(io)
                .await
                .map_err(|_| types::upstream_failure())
        })
        .await?;

        let driver = RequestDriver::new(sender, connection, request);
        let (response, connection) = time::run(headers_deadline, move || driver.run()).await?;

        if !headers_within_limit(response.headers()) || non_identity(response.headers()) {
            return Err(types::upstream_failure());
        }
        let status = response.status().as_u16();
        let media_type = types::diagnostic_media_type(response.headers());
        let content_type = parse_response_content_type(response.headers())?;
        let content_type_present = response.headers().contains_key(http::header::CONTENT_TYPE);
        let body = bounded_body(response.into_body(), connection, budget, self.timeouts);
        Ok(TransportResponse {
            status,
            content_type,
            content_type_present,
            media_type,
            body,
        })
    }
}

#[cfg(test)]
fn fixture_connector(
    certificate: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
    address: std::net::SocketAddr,
) -> Connector {
    fixture_connector_for_host(None, certificate, address)
}

#[cfg(test)]
fn fixture_connector_for_host(
    hostname: Option<&'static str>,
    certificate: tokio_rustls::rustls::pki_types::CertificateDer<'static>,
    address: std::net::SocketAddr,
) -> Connector {
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};

    let mut roots = RootCertStore::empty();
    roots
        .add(certificate)
        .unwrap_or_else(|_| panic!("fixture root certificate"));
    let tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let resolver = resolver::test_resolver(1, move |host| {
        if hostname.is_some_and(|expected| expected != host) {
            return Err(std::io::Error::other("unexpected fixture host"));
        }
        Ok(vec![address])
    });
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new_with_resolver(resolver);
    http.enforce_http(false);
    HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_only()
        .enable_http1()
        .wrap_connector(http)
}

#[cfg(test)]
fn test_connector() -> Connector {
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new_with_resolver(
        resolver::production(),
    );
    http.enforce_http(false);
    HttpsConnectorBuilder::new()
        .try_with_platform_verifier()
        .unwrap_or_else(|_| panic!("platform verifier"))
        .https_or_http()
        .enable_http1()
        .wrap_connector(http)
}
