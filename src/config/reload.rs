use super::{ConfigError, KeySource, ValidatedConfig};

impl ValidatedConfig {
    pub(crate) fn check_reload_compatible(&self, next: &Self) -> Result<(), ConfigError> {
        for (same, field) in [
            (self.listeners == next.listeners, "listeners"),
            (self.limits == next.limits, "limits"),
            (self.timeouts == next.timeouts, "timeouts"),
            (self.logging == next.logging, "logging"),
            (self.codex_auth == next.codex_auth, "auth"),
            (self.chatgpt_auth == next.chatgpt_auth, "chatgpt_auth"),
        ] {
            if !same {
                return Err(ConfigError::new(
                    format!("reload.{field}"),
                    "restart_required",
                ));
            }
        }
        let same_keys = match (&self.key_source, &next.key_source) {
            (KeySource::Inline, KeySource::Inline) => true,
            (
                KeySource::File {
                    path, usage_dir, ..
                },
                KeySource::File {
                    path: next_path,
                    usage_dir: next_usage,
                    ..
                },
            ) => path == next_path && usage_dir == next_usage,
            _ => false,
        };
        if !same_keys {
            return Err(ConfigError::new("reload.keys", "restart_required"));
        }
        for previous in &self.adapters {
            if let Some(next) = next.adapters.iter().find(|next| next.id() == previous.id())
                && (previous.max_in_flight() != next.max_in_flight()
                    || previous.circuit_breaker() != next.circuit_breaker())
            {
                return Err(ConfigError::new(
                    "reload.adapter_limits",
                    "restart_required",
                ));
            }
        }
        Ok(())
    }
}
