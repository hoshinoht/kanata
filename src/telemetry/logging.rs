use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;

use crate::config::{LogFormat, LogLevel, ValidatedLogging};

/// Installs the process-wide stdout subscriber; later calls are no-ops.
pub(crate) fn install(logging: &ValidatedLogging) {
    let level = match logging.level() {
        LogLevel::Trace => LevelFilter::TRACE,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Error => LevelFilter::ERROR,
    };
    let targets = targets(level);
    let builder = tracing_subscriber::fmt()
        .with_max_level(level.max(LevelFilter::WARN))
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .with_target(true);
    let _ = match logging.format() {
        LogFormat::Text => {
            tracing::subscriber::set_global_default(builder.compact().finish().with(targets))
                .is_ok()
        }
        LogFormat::Json => tracing::subscriber::set_global_default(
            builder
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .finish()
                .with(targets),
        )
        .is_ok(),
    };
}

/// Configured level for `kanata*` targets; third-party crates stay at WARN.
fn targets(level: LevelFilter) -> Targets {
    Targets::new()
        .with_target("kanata", level)
        .with_default(LevelFilter::WARN)
}

#[cfg(test)]
mod tests {
    use tracing::Level;
    use tracing::level_filters::LevelFilter;

    #[test]
    fn debug_level_applies_only_to_kanata_targets() {
        let filter = super::targets(LevelFilter::DEBUG);
        assert!(filter.would_enable("kanata::access", &Level::DEBUG));
        assert!(!filter.would_enable("hyper::proto", &Level::DEBUG));
        assert!(filter.would_enable("hyper::proto", &Level::WARN));
    }
}
