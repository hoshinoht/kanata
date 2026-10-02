use std::path::PathBuf;
use std::sync::Arc;

use crate::adapter::Adapter;
use crate::auth::EnvironmentSecretResolver;
use crate::config::{self, Plane, ValidatedConfig};
use crate::keys::reload::{KeyReloader, POLL_INTERVAL};
use crate::server::reload::ConfigurationHandle;

pub(super) struct Reloader<B> {
    path: PathBuf,
    config: ValidatedConfig,
    plane: Plane,
    handle: ConfigurationHandle,
    keys: Option<KeyReloader>,
    build: B,
}

impl<B> Reloader<B>
where
    B: Fn(&ValidatedConfig, &EnvironmentSecretResolver) -> Result<Vec<Arc<dyn Adapter>>, ()>
        + Send
        + Sync
        + 'static,
{
    pub(super) fn new(
        path: PathBuf,
        config: ValidatedConfig,
        plane: Plane,
        handle: ConfigurationHandle,
        build: B,
    ) -> Self {
        let keys = KeyReloader::new(config.clone(), plane, handle.keys());
        #[cfg(unix)]
        handle.enable();
        Self {
            path,
            config,
            plane,
            handle,
            keys,
            build,
        }
    }

    fn reload(&mut self) -> Result<(), String> {
        let full = config::load(&self.path).map_err(|error| error.to_string())?;
        self.config
            .check_reload_compatible(&full)
            .map_err(|error| error.to_string())?;
        let narrowed = full
            .for_plane(self.plane)
            .map_err(|error| error.to_string())?;
        let adapters = (self.build)(&narrowed, &EnvironmentSecretResolver)
            .map_err(|_| "configuration reload: adapter initialization failed")?;
        self.handle
            .apply(&narrowed, &EnvironmentSecretResolver, adapters)
            .map_err(|error| format!("configuration reload: {error}"))?;
        self.keys = KeyReloader::new(full.clone(), self.plane, self.handle.keys());
        self.config = full;
        Ok(())
    }

    pub(super) async fn run(
        mut self,
        mut signal: ReloadSignal,
        mut stop: tokio::sync::oneshot::Receiver<()>,
    ) {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            let requested = tokio::select! {
                _ = &mut stop => return,
                _ = signal.recv() => true,
                _ = interval.tick() => false,
            };
            self = match tokio::task::spawn_blocking(move || {
                if requested {
                    let result = self.reload();
                    match &result {
                        Ok(()) => tracing::info!(target: "kanata::lifecycle", "configuration reloaded"),
                        Err(error) => tracing::warn!(target: "kanata::lifecycle", "configuration rejected; previous generation stays active: {error}"),
                    }
                    self.handle.record(result);
                } else if let Some(keys) = self.keys.as_mut() { keys.poll_once(); }
                self
            }).await {
                Ok(reloader) => reloader,
                Err(_) => return,
            };
        }
    }
}

#[cfg(unix)]
pub(super) type ReloadSignal = tokio::signal::unix::Signal;
#[cfg(unix)]
pub(super) fn signal() -> Result<ReloadSignal, &'static str> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|_| "reload handler could not be installed")
}

#[cfg(not(unix))]
pub(super) struct ReloadSignal;
#[cfg(not(unix))]
impl ReloadSignal {
    async fn recv(&mut self) {
        std::future::pending::<()>().await;
    }
}
#[cfg(not(unix))]
pub(super) fn signal() -> Result<ReloadSignal, &'static str> {
    Ok(ReloadSignal)
}
