use std::sync::Arc;

use futures_util::StreamExt;
use http::Method;
use serde::Serialize;

use super::{
    Adapter, AdapterFuture, AdapterOutput,
    diagnostics::UpstreamLabel,
    transport::{CredentialHeader, Endpoint, Transport, TransportRequest},
};
use crate::{
    auth::{SecretResolver, canonical_bearer_token},
    config::{ProviderKind, ValidatedConfig},
    core::{
        Capabilities, ErrorKind, GatewayError, MAX_SPEECH_RESPONSE_BYTES, Operation, Request,
        Response, RouteIdentity, RoutedRequest, SpeechPolicy, SpeechResponse, TrustZone,
    },
};

pub struct SpeechAdapter {
    id: String,
    capabilities: Capabilities,
    trust_zone: TrustZone,
    routes: Vec<(RouteIdentity, SpeechPolicy)>,
    transport: Arc<Transport>,
    bearer_token: Option<String>,
    max_body_bytes: usize,
}

impl SpeechAdapter {
    pub fn from_config(
        config: &ValidatedConfig,
        adapter_id: &str,
        resolver: &impl SecretResolver,
    ) -> Result<Self, GatewayError> {
        let adapter = config
            .adapters()
            .iter()
            .find(|adapter| adapter.id() == adapter_id)
            .ok_or_else(internal)?;
        if adapter.kind() != ProviderKind::Speech
            || adapter.capabilities().operations != [Operation::Speech].into_iter().collect()
        {
            return Err(internal());
        }
        let routes = config
            .routes()
            .iter()
            .filter(|route| route.adapter_id() == adapter_id)
            .map(|route| {
                Ok((
                    route.identity().clone(),
                    route.speech().cloned().ok_or_else(internal)?,
                ))
            })
            .collect::<Result<Vec<_>, GatewayError>>()?;
        if routes.is_empty() {
            return Err(internal());
        }
        let bearer_token = adapter
            .secret_ref()
            .map(|reference| {
                let secret = resolver.resolve(reference).map_err(|_| internal())?;
                canonical_bearer_token(reference, secret).map_err(|_| internal())
            })
            .transpose()?;
        Ok(Self {
            id: adapter.id().to_owned(),
            capabilities: adapter.capabilities().clone(),
            trust_zone: adapter.trust_zone(),
            routes,
            transport: Arc::new(Transport::new(adapter, config.timeouts())?),
            bearer_token,
            max_body_bytes: usize::try_from(config.limits().max_body_bytes())
                .map_err(|_| internal())?,
        })
    }
}

#[derive(Serialize)]
struct Payload<'a> {
    model: &'a str,
    input: &'a str,
    voice: &'a str,
    response_format: crate::core::SpeechFormat,
    speed: f64,
    stream: bool,
    allow_voice_tags: bool,
}

impl Adapter for SpeechAdapter {
    fn id(&self) -> &str {
        &self.id
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
        let (context, request) = routed.into_parts();
        let Request::Speech(request) = request else {
            return Box::pin(async {
                Err(GatewayError {
                    kind: ErrorKind::UnsupportedOperation,
                })
            });
        };
        let binding = self
            .routes
            .iter()
            .find(|(identity, _)| *identity == context.route);
        if context.trust_zone != self.trust_zone
            || context.extensions.iter().next().is_some()
            || request.validate().is_err()
            || binding.is_none_or(|(_, policy)| request.check_policy(policy).is_err())
        {
            return Box::pin(async {
                Err(GatewayError {
                    kind: ErrorKind::InvalidRequest,
                })
            });
        }
        let transport = self.transport.clone();
        let credential = self
            .bearer_token
            .as_ref()
            .map(|token| CredentialHeader::authorization(format!("Bearer {token}")));
        let max_body_bytes = self.max_body_bytes;
        let label = UpstreamLabel::new(&self.id, "speech");
        Box::pin(async move {
            let payload = Payload {
                model: &context.route.upstream_id,
                input: &request.input,
                voice: &request.voice,
                response_format: request.response_format,
                speed: request.speed.get(),
                stream: false,
                allow_voice_tags: false,
            };
            let upstream = TransportRequest::json(
                Method::POST,
                Endpoint::new(&["audio", "speech"])?,
                &payload,
                credential,
                None,
                max_body_bytes,
                MAX_SPEECH_RESPONSE_BYTES,
            )?;
            let mut response = transport.execute(upstream).await?;
            if response.status != 200 {
                label.status(response.status, &mut response.body).await;
                return Err(status_error(response.status));
            }
            let accepted = match request.response_format {
                crate::core::SpeechFormat::Mp3 => {
                    response.media_type.as_deref() == Some("audio/mpeg")
                }
                crate::core::SpeechFormat::Wav => matches!(
                    response.media_type.as_deref(),
                    Some("audio/wav" | "audio/x-wav")
                ),
            };
            if !accepted {
                return Err(upstream_failure());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.body.next().await {
                let chunk = chunk?;
                if chunk.len() > MAX_SPEECH_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                    return Err(upstream_failure());
                }
                bytes.extend_from_slice(&chunk);
            }
            let response = SpeechResponse {
                model: request.model.clone(),
                format: request.response_format,
                bytes,
            };
            if !response.valid_for(&request) {
                return Err(upstream_failure());
            }
            Ok(AdapterOutput::Complete(Response::Speech(response)))
        })
    }
}

fn internal() -> GatewayError {
    GatewayError {
        kind: ErrorKind::Internal,
    }
}
fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
fn status_error(status: u16) -> GatewayError {
    GatewayError {
        kind: match status {
            400 | 422 => ErrorKind::InvalidRequest,
            404 => ErrorKind::NotFound,
            401 | 403 | 408 | 503 | 504 => ErrorKind::UpstreamUnavailable,
            429 => ErrorKind::RateLimited,
            _ => ErrorKind::UpstreamFailure,
        },
    }
}
