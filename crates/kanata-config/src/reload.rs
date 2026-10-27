use super::{ConfigError, ConfigErrorClass, ValidatedConfig};

impl ValidatedConfig {
    pub fn check_reload_compatible(&self, next: &Self) -> Result<(), ConfigError> {
        if let Some(field) = self.restart_fields(next).first() {
            return Err(ConfigError::new(
                format!("reload.{field}"),
                ConfigErrorClass::RestartRequired,
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
            (self.chatgpt_auth == next.chatgpt_auth, "chatgpt_auth"),
        ] {
            if !same {
                fields.push(field);
            }
        }
        if self.key_source.path != next.key_source.path
            || self.key_source.usage_dir != next.key_source.usage_dir
        {
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
