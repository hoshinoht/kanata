use http::{Method, Request, Uri, header};
use serde::Serialize;

use crate::core::GatewayError;

use super::{encoded::EncodedBody, multipart::MultipartRequest, origin::Origin, types};

/// Identifies Kanata to upstreams; some edges reject requests without one.
const USER_AGENT: &str = concat!("kanata/", env!("CARGO_PKG_VERSION"));

pub(crate) struct Endpoint(&'static [&'static str]);

impl Endpoint {
    pub(crate) fn new(segments: &'static [&'static str]) -> Result<Self, GatewayError> {
        if segments.is_empty() || segments.iter().any(|segment| !valid_segment(segment)) {
            return Err(types::internal_error());
        }
        Ok(Self(segments))
    }

    pub(super) fn path(&self) -> String {
        self.0.join("/")
    }
}

pub(crate) struct TransportRequest {
    pub(super) method: Method,
    pub(super) endpoint: Endpoint,
    pub(super) body: EncodedBody,
    pub(super) credential: Option<CredentialHeader>,
    pub(super) accept: Option<Accept>,
    pub(super) response_budget: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum Accept {
    Json,
    EventStream,
}

pub(crate) struct CredentialHeader {
    pub(super) value: String,
    chatgpt_account_id: Option<http::HeaderValue>,
}

impl TransportRequest {
    pub(crate) fn json<T: Serialize>(
        method: Method,
        endpoint: Endpoint,
        value: &T,
        credential: Option<CredentialHeader>,
        accept: Option<Accept>,
        request_budget: usize,
        response_budget: usize,
    ) -> Result<Self, GatewayError> {
        let body = EncodedBody::json(value, request_budget)?;
        Self::from_body(
            method,
            endpoint,
            body,
            credential,
            accept,
            request_budget,
            response_budget,
            true,
        )
    }

    pub(crate) fn sensitive_json<T: Serialize>(
        method: Method,
        endpoint: Endpoint,
        value: &T,
        request_budget: usize,
        response_budget: usize,
    ) -> Result<Self, GatewayError> {
        let body = EncodedBody::sensitive_json(value, request_budget)?;
        Self::from_body(
            method,
            endpoint,
            body,
            None,
            Some(Accept::Json),
            request_budget,
            response_budget,
            true,
        )
    }

    pub(crate) fn form_encoded(
        method: Method,
        endpoint: Endpoint,
        value: &str,
        request_budget: usize,
        response_budget: usize,
    ) -> Result<Self, GatewayError> {
        let body = EncodedBody::form_encoded(value, request_budget)?;
        Self::from_body(
            method,
            endpoint,
            body,
            None,
            Some(Accept::Json),
            request_budget,
            response_budget,
            true,
        )
    }

    pub(crate) fn form(
        method: Method,
        endpoint: Endpoint,
        fields: Vec<(String, String)>,
        credential: Option<CredentialHeader>,
        accept: Option<Accept>,
        request_budget: usize,
        response_budget: usize,
    ) -> Result<Self, GatewayError> {
        let body = EncodedBody::form(&fields, request_budget)?;
        Self::from_body(
            method,
            endpoint,
            body,
            credential,
            accept,
            request_budget,
            response_budget,
            true,
        )
    }

    pub(crate) fn multipart(
        method: Method,
        endpoint: Endpoint,
        request: MultipartRequest,
        credential: Option<CredentialHeader>,
        accept: Option<Accept>,
        request_budget: usize,
        response_budget: usize,
    ) -> Result<Self, GatewayError> {
        let body = EncodedBody::multipart(request, request_budget)?;
        Self::from_body(
            method,
            endpoint,
            body,
            credential,
            accept,
            request_budget,
            response_budget,
            true,
        )
    }

    #[cfg(test)]
    pub(crate) fn json_for_test<T: Serialize>(
        method: Method,
        endpoint: Endpoint,
        value: &T,
        credential: Option<CredentialHeader>,
        accept: Option<Accept>,
        request_budget: usize,
        response_budget: usize,
    ) -> Result<Self, GatewayError> {
        let body = EncodedBody::json(value, request_budget)?;
        Self::from_body(
            method,
            endpoint,
            body,
            credential,
            accept,
            request_budget,
            response_budget,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_body(
        method: Method,
        endpoint: Endpoint,
        body: EncodedBody,
        credential: Option<CredentialHeader>,
        accept: Option<Accept>,
        request_budget: usize,
        response_budget: usize,
        validate_response_budget: bool,
    ) -> Result<Self, GatewayError> {
        if request_budget == 0
            || body.len() > request_budget
            || response_budget == 0
            || (validate_response_budget
                && !matches!(
                    response_budget,
                    types::COMPLETE_RESPONSE_BYTES
                        | types::STREAM_RESPONSE_BYTES
                        | types::OAUTH_RESPONSE_BYTES
                        | types::AUTH_RESPONSE_BYTES
                ))
        {
            return Err(types::internal_error());
        }
        Ok(Self {
            method,
            endpoint,
            body,
            credential,
            accept,
            response_budget,
        })
    }
}

impl CredentialHeader {
    pub(crate) fn authorization(value: String) -> Self {
        Self {
            value,
            chatgpt_account_id: None,
        }
    }

    pub(crate) fn authorization_with_chatgpt_account_id(
        value: String,
        account_id: &str,
    ) -> Result<Self, GatewayError> {
        if account_id.trim().is_empty()
            || account_id.len() > 4 * 1024
            || !account_id.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        {
            return Err(types::internal_error());
        }
        let mut account_id =
            http::HeaderValue::from_str(account_id).map_err(|_| types::internal_error())?;
        account_id.set_sensitive(true);
        Ok(Self {
            value,
            chatgpt_account_id: Some(account_id),
        })
    }
}

pub(super) fn build_request(
    origin: &Origin,
    uri: Uri,
    request: TransportRequest,
) -> Result<Request<EncodedBody>, GatewayError> {
    let mut builder = Request::builder()
        .method(request.method)
        .uri(
            uri.path_and_query()
                .ok_or_else(types::internal_error)?
                .clone(),
        )
        .header(header::HOST, origin.host_header())
        .header(header::USER_AGENT, USER_AGENT)
        .header(header::CONNECTION, "close")
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::CONTENT_LENGTH, request.body.len().to_string())
        .header(header::CONTENT_TYPE, request.body.content_type());
    if let Some(accept) = request.accept {
        builder = builder.header(header::ACCEPT, accept_value(accept));
    }
    if let Some(credential) = request.credential {
        let mut value =
            http::HeaderValue::from_str(&credential.value).map_err(|_| types::internal_error())?;
        value.set_sensitive(true);
        builder = builder.header(header::AUTHORIZATION, value);
        if let Some(account_id) = credential.chatgpt_account_id {
            builder = builder.header("chatgpt-account-id", account_id);
        }
    }
    builder
        .body(request.body)
        .map_err(|_| types::internal_error())
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn accept_value(value: Accept) -> &'static str {
    match value {
        Accept::Json => "application/json",
        Accept::EventStream => "text/event-stream",
    }
}
