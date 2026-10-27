//! Listeners, authentication, routing, admission, handlers and telemetry.

#![forbid(unsafe_code)]
#![deny(clippy::await_holding_lock, clippy::await_holding_refcell_ref)]

#[macro_use]
extern crate kanata_core;

use kanata_config as config;
use kanata_core as core;
use kanata_core::adapter;
use kanata_keys as keys;

mod api;
pub mod auth;
pub mod key_reload;
pub mod routing;
pub mod server;
pub mod telemetry;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod api_test_surface {
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
        routing::get,
    };

    use crate::core::{ModelAlias, Operation, RouteSelector};
    use crate::server::{Authenticated, ClientState, ForbiddenResponse, Readiness, TwoPlaneServer};

    fn routes() -> Router<ClientState> {
        Router::new()
            .route("/test/denied", get(denied))
            .route("/test/allowed", get(allowed))
            .route("/live", get(extension_live))
            .fallback(extension_fallback)
    }

    async fn denied(authenticated: Authenticated) -> Result<&'static str, ForbiddenResponse> {
        authenticated.authorize(&RouteSelector {
            model_alias: ModelAlias("local-chat".into()),
            operation: Operation::Transcription,
        })?;
        Ok("allowed\n")
    }

    async fn allowed(authenticated: Authenticated) -> Result<&'static str, ForbiddenResponse> {
        authenticated.authorize(&RouteSelector {
            model_alias: ModelAlias("local-chat".into()),
            operation: Operation::Chat,
        })?;
        Ok("allowed\n")
    }

    async fn extension_live() -> &'static str {
        "extension-live\n"
    }

    async fn extension_fallback() -> &'static str {
        "extension-fallback\n"
    }

    #[tokio::test]
    async fn sibling_routes_receive_authenticated_state_and_exact_403() {
        let config = crate::config::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/config/example.toml"
        ))
        .expect("config");
        let server = TwoPlaneServer::from_validated_with_client_routes(
            &config,
            Readiness::new(false),
            routes(),
        )
        .expect("server");
        let denied = server
            .client_oneshot(
                Request::builder()
                    .uri("/v1/test/denied")
                    .header("authorization", "Bearer test-key")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            to_bytes(denied.into_body(), usize::MAX)
                .await
                .expect("body"),
            "{\"error\":{\"code\":\"permission_denied\",\"message\":\"Permission denied\",\"param\":null,\"type\":\"permission_error\"}}"
        );
        let allowed = server
            .client_oneshot(
                Request::builder()
                    .uri("/v1/test/allowed")
                    .header("authorization", "Bearer test-key")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(allowed.status(), StatusCode::OK);
        let extension_live = server
            .client_oneshot(
                Request::builder()
                    .uri("/v1/live")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(extension_live.status(), StatusCode::OK);
        let extension_fallback = server
            .client_oneshot(
                Request::builder()
                    .uri("/v1/extension-miss")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(extension_fallback.status(), StatusCode::OK);
        for authorization in [None, Some("Bearer test-key")] {
            let mut request = Request::builder().uri("/live");
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            let response = server
                .client_oneshot(request.body(Body::empty()).expect("request"))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let admin_live = server
            .admin_oneshot(
                Request::builder()
                    .uri("/live")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(admin_live.status(), StatusCode::OK);
    }
}
