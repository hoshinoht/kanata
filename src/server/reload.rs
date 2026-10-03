use std::sync::{Arc, Mutex, PoisonError, RwLock};

use axum::{Router, body::Body, http::Request, response::Response};
use serde::Serialize;
use tower::{ServiceExt as _, util::BoxCloneSyncService};

use super::{
    ClientState, PublicClientService, ServerBuildError, client_router, public_client_router,
};
use crate::adapter::Adapter;
use crate::auth::{ApplicationAuth, KeyHandle, SecretResolver};
use crate::config::ValidatedConfig;
use crate::routing::Registry;

#[derive(Clone)]
pub(crate) struct ConfigurationHandle {
    current: Arc<RwLock<Arc<Generation>>>,
    status: Arc<Mutex<ReloadStatus>>,
}

struct Generation {
    state: ClientState,
    client: Router,
    public: Option<PublicClientService>,
}

#[derive(Clone, Default, Serialize)]
pub(crate) struct ReloadStatus {
    enabled: bool,
    healthy: bool,
    generation: u64,
    attempts: u64,
    failures: u64,
    last_attempt: u64,
    last_success: u64,
    last_error: Option<String>,
}

impl ConfigurationHandle {
    pub(super) fn new(
        state: ClientState,
        client: Router,
        public: Option<PublicClientService>,
    ) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(Generation {
                state,
                client,
                public,
            }))),
            status: Arc::new(Mutex::new(ReloadStatus {
                generation: 1,
                healthy: true,
                ..ReloadStatus::default()
            })),
        }
    }

    fn snapshot(&self) -> Arc<Generation> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn keys(&self) -> KeyHandle {
        self.snapshot().state.keys.clone()
    }
    pub(crate) fn status(&self) -> ReloadStatus {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn enable(&self) {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .enabled = true;
    }

    pub(crate) fn record(&self, result: Result<(), String>) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        status.last_attempt = crate::keys::time::now();
        status.attempts = status.attempts.saturating_add(1);
        status.healthy = result.is_ok();
        match result {
            Ok(()) => {
                status.generation = status.generation.saturating_add(1);
                status.last_success = status.last_attempt;
                status.last_error = None;
            }
            Err(error) => {
                status.failures = status.failures.saturating_add(1);
                status.last_error = Some(error);
            }
        }
    }

    pub(crate) fn apply(
        &self,
        config: &ValidatedConfig,
        resolver: &impl SecretResolver,
        adapters: Vec<Arc<dyn Adapter>>,
    ) -> Result<(), ServerBuildError> {
        let previous = self.snapshot();
        let auth = ApplicationAuth::rebuild(config, resolver, &previous.state.keys.current())
            .map_err(ServerBuildError::Auth)?;
        let adapters = super::bind_adapters(config, adapters)?;
        let registry = Arc::new(Registry::from_validated(config));
        let mut state = previous.state.clone();
        state.keys = previous.state.keys.new_generation(auth);
        state.adapters = Arc::new(adapters);
        state.registry = registry.clone();
        let client = client_router(Router::new(), state.clone());
        let public = config.listeners().public().map(|_| {
            let mut public_state = state.clone();
            public_state.listener = crate::telemetry::Listener::Public;
            public_state.public_routes =
                Some(Arc::new(config.publication().public_routes().to_vec()));
            public_client_router(public_state)
        });
        let next = Arc::new(Generation {
            state,
            client,
            public,
        });
        previous
            .state
            .admission
            .extend(&registry, config)
            .map_err(|_| ServerBuildError::ReloadRequiresRestart)?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = next;
        Ok(())
    }

    pub(super) fn client_router(&self) -> Router {
        let handle = self.clone();
        Router::new().fallback(move |request: Request<Body>| {
            let handle = handle.clone();
            async move { handle.dispatch(request, false).await }
        })
    }

    pub(super) fn public_service(&self) -> PublicClientService {
        let handle = self.clone();
        BoxCloneSyncService::new(tower::service_fn(move |request| {
            let handle = handle.clone();
            async move { Ok::<_, std::convert::Infallible>(handle.dispatch(request, true).await) }
        }))
    }

    async fn dispatch(&self, request: Request<Body>, public: bool) -> Response {
        let generation = self.snapshot();
        let response = if public {
            match generation.public.clone() {
                Some(service) => service.oneshot(request).await,
                None => {
                    return super::error_response(
                        axum::http::StatusCode::FORBIDDEN,
                        "Permission denied",
                        "permission_error",
                        "permission_denied",
                    );
                }
            }
        } else {
            generation.client.clone().oneshot(request).await
        };
        match response {
            Ok(response) => response,
            Err(error) => match error {},
        }
    }
}
