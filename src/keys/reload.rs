//! Keys-file hot reload for a running server. Never touches routes or adapters.

use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::auth::{ApplicationAuth, KeyHandle, SecretResolutionError, SecretResolver};
use crate::config::{ConfigError, KeySource, Plane, SecretReference, ValidatedConfig};
use crate::keys::{file, time};

pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReloadOutcome {
    /// Same bytes as the applied key set.
    Unchanged,
    /// A new key set is live.
    Applied,
    /// Missing, unreadable or invalid file; the current key set stays live.
    Rejected,
}

/// Re-reads `[keys] file` and swaps the live key set when its content changes.
pub struct KeyReloader {
    /// Un-narrowed, so scopes validate against every route before plane filtering.
    config: ValidatedConfig,
    plane: Plane,
    handle: KeyHandle,
    path: PathBuf,
    applied: Option<[u8; 32]>,
    last_warning: Option<(Option<[u8; 32]>, String)>,
}

impl KeyReloader {
    /// `None` for inline keys, which never reload.
    pub fn new(config: ValidatedConfig, plane: Plane, handle: KeyHandle) -> Option<Self> {
        let KeySource::File {
            path,
            missing,
            sha256,
            ..
        } = config.key_source()
        else {
            return None;
        };
        let path = path.clone();
        let applied = *sha256;
        // Startup already warned about a missing file.
        let last_warning = missing.then(|| (None, not_found().to_string()));
        Some(Self {
            config,
            plane,
            handle,
            path,
            applied,
            last_warning,
        })
    }

    pub fn poll_once(&mut self) -> ReloadOutcome {
        let bytes = match file::read(&self.path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return self.reject(None, not_found()),
            Err(error) => return self.reject(None, error),
        };
        let sha256: [u8; 32] = Sha256::digest(&bytes).into();
        if self.applied == Some(sha256) {
            self.last_warning = None;
            return ReloadOutcome::Unchanged;
        }
        match self.apply(&bytes) {
            Ok(()) => {
                self.applied = Some(sha256);
                self.last_warning = None;
                ReloadOutcome::Applied
            }
            Err(error) => self.reject(Some(sha256), error),
        }
    }

    fn apply(&self, bytes: &[u8]) -> Result<(), ConfigError> {
        let keys = file::parse(bytes, self.config.routes())?;
        let config = self
            .config
            .with_application_keys(&keys)?
            .for_plane(self.plane)?;
        let auth = ApplicationAuth::rebuild(&config, &DigestOnly, &self.handle.current())
            .map_err(|_| ConfigError::new("keys_file", "build_error"))?;
        self.handle.replace(auth);
        let ids: Vec<&str> = config
            .application_keys()
            .iter()
            .map(|key| key.id())
            .collect();
        tracing::info!(
            target: "kanata::keys",
            keys = ids.len(),
            ids = %ids.join(","),
            "keys file reloaded",
        );
        for warning in config.key_warnings(time::now()) {
            tracing::warn!(target: "kanata::keys", "{warning}");
        }
        Ok(())
    }

    fn reject(&mut self, sha256: Option<[u8; 32]>, error: ConfigError) -> ReloadOutcome {
        let warning = (sha256, error.to_string());
        if self.last_warning.as_ref() != Some(&warning) {
            tracing::warn!(
                target: "kanata::keys",
                "keys file rejected; current keys stay active: {}",
                warning.1,
            );
            self.last_warning = Some(warning);
        }
        ReloadOutcome::Rejected
    }
}

fn not_found() -> ConfigError {
    ConfigError::new("keys.file", "not_found")
}

/// Keys-file records are digests; nothing is resolved.
struct DigestOnly;

impl SecretResolver for DigestOnly {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Err(SecretResolutionError)
    }
}
