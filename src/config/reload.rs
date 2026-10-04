use super::{ConfigError, KeySource, ValidatedConfig};

impl ValidatedConfig {
    pub(crate) fn check_reload_compatible(&self, next: &Self) -> Result<(), ConfigError> {
        if let Some(field) = self.restart_fields(next).first() {
            return Err(ConfigError::new(
                format!("reload.{field}"),
                "restart_required",
            ));
        }
        Ok(())
    }

    pub(crate) fn restart_fields(&self, next: &Self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        for (same, field) in [
            (self.listeners == next.listeners, "listeners"),
            (self.limits == next.limits, "limits"),
            (self.timeouts == next.timeouts, "timeouts"),
            (self.logging == next.logging, "logging"),
            (self.codex_auth == next.codex_auth, "auth"),
            (self.chatgpt_auth == next.chatgpt_auth, "chatgpt_auth"),
        ] {
            if !same {
                fields.push(field);
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
            fields.push("keys");
        }
        for previous in &self.adapters {
            if let Some(next) = next.adapters.iter().find(|next| next.id() == previous.id())
                && (previous.max_in_flight() != next.max_in_flight()
                    || previous.circuit_breaker() != next.circuit_breaker())
            {
                fields.push("adapter_limits");
                break;
            }
        }
        fields
    }
}
