mod reload;

use std::{future::Future, path::PathBuf, sync::Arc};

use crate::{
    adapter::Adapter,
    auth::EnvironmentSecretResolver,
    config::{self, Plane, ValidatedConfig},
    keys::usage::{FLUSH_INTERVAL, UsageHandle},
    server::{DEFAULT_SHUTDOWN_GRACE, Readiness, TwoPlaneServer},
};

pub(crate) async fn run<B>(
    config_path: PathBuf,
    plane: Plane,
    build_adapters: B,
) -> Result<(), String>
where
    B: Fn(&ValidatedConfig, &EnvironmentSecretResolver) -> Result<Vec<Arc<dyn Adapter>>, ()>
        + Send
        + Sync
        + 'static,
{
    let full_config = config::load(&config_path).map_err(|error| error.to_string())?;
    let config = full_config
        .for_plane(plane)
        .map_err(|error| error.to_string())?;
    crate::telemetry::logging::install(config.logging());
    for warning in config.key_warnings(crate::keys::time::now()) {
        tracing::warn!(target: "kanata::keys", "{warning}");
    }
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
    let reloader = reload::Reloader::new(
        config_path,
        full_config,
        plane,
        server.configuration_handle(),
        build_adapters,
    );
    let reload_signal = reload::signal().map_err(str::to_owned)?;
    let usage = server.open_usage(plane);
    let signal = shutdown_signal().map_err(str::to_owned)?;
    let (stop_reload, reload_stopped) = tokio::sync::oneshot::channel::<()>();
    let (stop_flush, flush_stopped) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        signal.await;
        drop(stop_reload);
        drop(stop_flush);
    };
    let bound = server
        .bind()
        .await
        .map_err(|_| "listener binding failed".to_owned())?;
    let reload_task = tokio::spawn(reloader.run(reload_signal, reload_stopped));
    let flush_task = usage
        .clone()
        .map(|usage| tokio::spawn(flush_usage(usage, flush_stopped)));
    let public_routes: Vec<String> = config
        .publication()
        .public_routes()
        .iter()
        .map(|selector| {
            let operation = selector.operation.as_str();
            format!("{}:{operation}", selector.model_alias.0)
        })
        .collect();
    tracing::info!(
        target: "kanata::lifecycle",
        version = crate::VERSION,
        pid = std::process::id(),
        plane = plane.as_str(),
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
    reload_task.abort();
    if let Some(task) = flush_task {
        let _ = task.await;
    }
    // After drain, so requests finished during shutdown are included.
    if let Some(usage) = usage {
        let _ = tokio::task::spawn_blocking(move || usage.flush_now()).await;
    }
    match &result {
        Ok(()) => tracing::info!(target: "kanata::lifecycle", "kanata stopped"),
        Err(_) => {
            tracing::error!(target: "kanata::lifecycle", "kanata stopped after runtime failure")
        }
    }
    result
}

/// Writes usage state every [`FLUSH_INTERVAL`] until shutdown begins.
async fn flush_usage(usage: UsageHandle, mut stop: tokio::sync::oneshot::Receiver<()>) {
    let mut interval = tokio::time::interval(FLUSH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = interval.tick() => {}
        }
        let usage = usage.clone();
        if tokio::task::spawn_blocking(move || usage.flush_now())
            .await
            .is_err()
        {
            return;
        }
    }
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
