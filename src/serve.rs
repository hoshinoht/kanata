use std::{future::Future, path::PathBuf, sync::Arc};

use crate::{
    adapter::Adapter,
    auth::EnvironmentSecretResolver,
    config::{self, ValidatedConfig},
    core::Operation,
    server::{DEFAULT_SHUTDOWN_GRACE, Readiness, TwoPlaneServer},
};

pub(crate) async fn run<B>(config_path: PathBuf, build_adapters: B) -> Result<(), String>
where
    B: FnOnce(&ValidatedConfig, &EnvironmentSecretResolver) -> Result<Vec<Arc<dyn Adapter>>, ()>,
{
    let config = config::load(config_path).map_err(|error| error.to_string())?;
    crate::telemetry::logging::install(config.logging());
    let resolver = EnvironmentSecretResolver;
    let adapters = build_adapters(&config, &resolver)
        .map_err(|_| "server initialization failed".to_owned())?;
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &resolver,
        Readiness::new(true),
        adapters,
    )
    .map_err(|_| "server initialization failed".to_owned())?;
    let shutdown = shutdown_signal().map_err(str::to_owned)?;
    let bound = server
        .bind()
        .await
        .map_err(|_| "listener binding failed".to_owned())?;
    let public_routes: Vec<String> = config
        .publication()
        .public_routes()
        .iter()
        .map(|selector| {
            let operation = match selector.operation {
                Operation::Chat => "chat",
                Operation::Transcription => "transcription",
            };
            format!("{}:{operation}", selector.model_alias.0)
        })
        .collect();
    tracing::info!(
        target: "kanata::lifecycle",
        version = crate::VERSION,
        private = %bound.client_addr(),
        public = %bound.public_addr().map_or_else(|| "-".to_owned(), |address| address.to_string()),
        admin = %bound.admin_addr(),
        routes = config.routes().len(),
        public_routes = %public_routes.join(","),
        "kanata started",
    );
    let result = bound
        .serve_until(shutdown, DEFAULT_SHUTDOWN_GRACE)
        .await
        .map_err(|_| "server runtime failed".to_owned());
    match &result {
        Ok(()) => tracing::info!(target: "kanata::lifecycle", "kanata stopped"),
        Err(_) => {
            tracing::error!(target: "kanata::lifecycle", "kanata stopped after runtime failure")
        }
    }
    result
}

fn shutdown_signal() -> Result<impl Future<Output = ()> + Send + 'static, &'static str> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut interrupt = signal(SignalKind::interrupt())
            .map_err(|_| "shutdown handlers could not be installed")?;
        let mut terminate = signal(SignalKind::terminate())
            .map_err(|_| "shutdown handlers could not be installed")?;
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        })
    }
    #[cfg(not(unix))]
    {
        Ok(async {
            let _ = tokio::signal::ctrl_c().await;
        })
    }
}
