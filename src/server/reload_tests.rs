use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::{Readiness, TwoPlaneServer};
use crate::adapter::{Adapter, AdapterFuture, AdapterOutput};
use crate::auth::{SecretResolutionError, SecretResolver};
use crate::config::{self, KeyRateLimit, SecretReference, ValidatedConfig};
use crate::core::{
    Capabilities, FinishReason, ModelAlias, NormalizedEvent, Operation, RouteSelector,
    RoutedRequest,
};
use crate::keys::file::{KeysFile, StoredKey};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kanata-config-generation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("keys")).unwrap();
        std::fs::create_dir_all(path.join("state")).unwrap();
        Self(path)
    }
    fn load(
        &self,
        alias: &str,
        primary: bool,
        public: bool,
        key_limit: Option<u64>,
        rate: Option<KeyRateLimit>,
    ) -> ValidatedConfig {
        let mut raw: toml::Table =
            toml::from_str(include_str!("../../config/embeddings.example.toml")).unwrap();
        raw["adapters"][0]["kind"] = "vllm".into();
        raw["adapters"][0]
            .as_table_mut()
            .unwrap()
            .insert("max_in_flight".into(), 1.into());
        raw["adapters"][0]["capabilities"]["operations"] = vec![toml::Value::from("chat")].into();
        raw["adapters"][0]["capabilities"]["streaming_chat"] = true.into();
        let adapter = raw["adapters"][0]["id"].as_str().unwrap().to_owned();
        let route = |id: &str, name: &str| {
            toml::Value::try_from(json!({"id":id,"model_alias":name,"operation":"chat","adapter_id":adapter,"upstream_id":format!("{name}-upstream"),"requires_streaming_chat":true,"requires_function_tools":false})).unwrap()
        };
        let mut routes = vec![route("secondary-route", "secondary")];
        if primary {
            routes.push(route("primary-route", alias));
        }
        raw["routes"] = routes.into();
        raw["listeners"].as_table_mut().unwrap().insert(
            "public".into(),
            toml::Value::try_from(json!({"bind":"172.30.0.3","port":8081})).unwrap(),
        );
        raw["publication"].as_table_mut().unwrap().insert(
            "public_routes".into(),
            if primary && public {
                vec![
                    toml::Value::try_from(json!({"model_alias":alias,"operation":"chat"})).unwrap(),
                ]
            } else {
                vec![]
            }
            .into(),
        );
        raw["limits"]["max_in_flight"] = 1.into();
        raw["limits"]["max_queue"] = 1.into();
        raw["timeouts"]["queue_ms"] = 20.into();
        let mut scopes = vec![RouteSelector {
            model_alias: ModelAlias("secondary".into()),
            operation: Operation::Chat,
        }];
        if primary {
            scopes.push(RouteSelector {
                model_alias: ModelAlias(alias.into()),
                operation: Operation::Chat,
            });
        }
        let mut keys = KeysFile::default();
        keys.push(StoredKey::new(
            "client".into(),
            Sha256::digest(b"test-key").into(),
            false,
            scopes,
            key_limit,
            rate,
            crate::keys::time::now(),
            None,
        ));
        std::fs::write(self.0.join("keys/keys.toml"), keys.render()).unwrap();
        std::fs::write(self.0.join("config.toml"), toml::to_string(&raw).unwrap()).unwrap();
        config::load(self.0.join("config.toml")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Err(SecretResolutionError)
    }
}
struct StreamAdapter {
    id: String,
    capabilities: Capabilities,
}
impl Adapter for StreamAdapter {
    fn id(&self) -> &str {
        &self.id
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    fn execute(&self, request: RoutedRequest) -> AdapterFuture {
        let model = request.request().model_alias().clone();
        let text = request.context().route.upstream_id.clone();
        Box::pin(async move {
            Ok(AdapterOutput::Events(Box::pin(futures_util::stream::iter(
                [
                    Ok(NormalizedEvent::ChatStarted { model }),
                    Ok(NormalizedEvent::ChatTextDelta { text }),
                    Ok(NormalizedEvent::ChatCompleted {
                        finish_reason: FinishReason::Stop,
                        usage: None,
                    }),
                ],
            ))))
        })
    }
}
fn adapters(config: &ValidatedConfig) -> Vec<Arc<dyn Adapter>> {
    config
        .adapters()
        .iter()
        .map(|adapter| {
            Arc::new(StreamAdapter {
                id: adapter.id().into(),
                capabilities: adapter.capabilities().clone(),
            }) as Arc<dyn Adapter>
        })
        .collect()
}
fn server(config: &ValidatedConfig) -> TwoPlaneServer {
    TwoPlaneServer::from_validated_with_adapters(
        config,
        &Resolver,
        Readiness::new(true),
        adapters(config),
    )
    .unwrap()
}
fn request(model: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model":model,"messages":[{"role":"user","content":"hello"}],"stream":true})
                .to_string(),
        ))
        .unwrap()
}
async fn text(response: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}
async fn models(server: &TwoPlaneServer, public: bool) -> Value {
    let request = Request::builder()
        .uri("/v1/models")
        .header("authorization", "Bearer test-key")
        .body(Body::empty())
        .unwrap();
    let response = if public {
        server.public_oneshot(request).await.unwrap()
    } else {
        server.client_oneshot(request).await.unwrap()
    };
    serde_json::from_str(&text(response).await).unwrap()
}

#[tokio::test]
async fn generation_replaces_routes_scopes_and_publication_while_old_stream_finishes() {
    let fixture = Fixture::new();
    let before = fixture.load("first", true, true, Some(1), None);
    let server = server(&before);
    let usage = server.open_usage(config::Plane::All).unwrap();
    let held = server.client_oneshot(request("first")).await.unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    let next = fixture.load("second", true, false, Some(1), None);
    before.check_reload_compatible(&next).unwrap();
    server
        .configuration_handle()
        .apply(&next, &Resolver, adapters(&next))
        .unwrap();
    assert!(
        models(&server, false).await["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "second")
    );
    assert_eq!(models(&server, true).await["data"], json!([]));
    assert_eq!(
        server
            .client_oneshot(request("first"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let busy = server.client_oneshot(request("second")).await.unwrap();
    assert_eq!(busy.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(text(busy).await.contains("key_busy"));
    let old = text(held).await;
    assert!(old.contains("first-upstream"));
    assert!(!old.contains("second-upstream"));
    let new = server.client_oneshot(request("second")).await.unwrap();
    assert_eq!(new.status(), StatusCode::OK);
    assert!(text(new).await.contains("second-upstream"));
    assert_eq!(usage.flush_now(), crate::keys::usage::FlushOutcome::Written);
    let used = crate::keys::usage::read_merged(&fixture.0.join("state"));
    assert!(used["client"].requests >= 4);
    assert_eq!(used["client"].tokens.missing, 2);
}

#[tokio::test]
async fn route_and_adapter_capacity_survive_removal_and_readdition() {
    let fixture = Fixture::new();
    let before = fixture.load("first", true, true, None, None);
    let server = server(&before);
    let held = server.client_oneshot(request("first")).await.unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    let removed = fixture.load("first", false, false, None, None);
    server
        .configuration_handle()
        .apply(&removed, &Resolver, adapters(&removed))
        .unwrap();
    let restored = fixture.load("first", true, true, None, None);
    server
        .configuration_handle()
        .apply(&restored, &Resolver, adapters(&restored))
        .unwrap();
    let busy = server.client_oneshot(request("first")).await.unwrap();
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(text(busy).await.contains("gateway_busy"));
    drop(held);
    let response = server.client_oneshot(request("first")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    server.admission.close();
    assert_eq!(
        server
            .client_oneshot(request("secondary"))
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn key_rate_bucket_is_preserved_and_rejected_preparation_keeps_prior_generation() {
    let fixture = Fixture::new();
    let rate = Some(KeyRateLimit {
        requests: 1,
        per_ms: 60_000,
    });
    let before = fixture.load("first", true, true, None, rate);
    let server = server(&before);
    let first = server.client_oneshot(request("first")).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let _ = text(first).await;
    let next = fixture.load("second", true, true, None, rate);
    let mut invalid = adapters(&next);
    invalid.push(invalid[0].clone());
    assert!(
        server
            .configuration_handle()
            .apply(&next, &Resolver, invalid)
            .is_err()
    );
    assert!(
        models(&server, false).await["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "first")
    );
    server
        .configuration_handle()
        .apply(&next, &Resolver, adapters(&next))
        .unwrap();
    let limited = server.client_oneshot(request("second")).await.unwrap();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(text(limited).await.contains("key_rate_limited"));
    let mut raw: toml::Table =
        toml::from_str(&std::fs::read_to_string(fixture.0.join("config.toml")).unwrap()).unwrap();
    raw["limits"]["max_in_flight"] = 2.into();
    std::fs::write(
        fixture.0.join("config.toml"),
        toml::to_string(&raw).unwrap(),
    )
    .unwrap();
    let changed = config::load(fixture.0.join("config.toml")).unwrap();
    assert!(
        next.check_reload_compatible(&changed)
            .unwrap_err()
            .to_string()
            .contains("restart_required")
    );
}
