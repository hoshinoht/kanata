use super::*;

pub(crate) fn validate_bounds(
    limits: &RawLimits,
    timeouts: &RawTimeouts,
) -> Result<(ValidatedLimits, ValidatedTimeouts), ConfigError> {
    for (path, value) in [
        ("limits.max_queue", limits.max_queue),
        ("limits.max_in_flight", limits.max_in_flight),
        ("limits.max_body_bytes", limits.max_body_bytes),
        ("limits.max_audio_bytes", limits.max_audio_bytes),
        ("limits.max_extension_bytes", limits.max_extension_bytes),
    ] {
        if value == 0 {
            return Err(ConfigError::new(path, ConfigErrorClass::Zero));
        }
    }
    if limits.max_audio_bytes > MAX_CONFIG_AUDIO_BYTES {
        return Err(ConfigError::new(
            "limits.max_audio_bytes",
            ConfigErrorClass::AudioLimitTooLarge,
        ));
    }
    let encoded_audio_bytes = limits
        .max_audio_bytes
        .checked_add(2)
        .and_then(|value| value.checked_div(3))
        .and_then(|chunks| chunks.checked_mul(4))
        .ok_or_else(|| ConfigError::new("limits", ConfigErrorClass::AudioEnvelopeOverflow))?;
    let max_audio_chat_body_bytes = limits
        .max_body_bytes
        .checked_add(encoded_audio_bytes)
        .ok_or_else(|| ConfigError::new("limits", ConfigErrorClass::AudioEnvelopeOverflow))?;
    if timeouts.overall_ms == 0 {
        return Err(ConfigError::new(
            "timeouts.overall_ms",
            ConfigErrorClass::Zero,
        ));
    }
    if timeouts.overall_ms > MAX_TIMEOUT_MS {
        return Err(ConfigError::new(
            "timeouts.overall_ms",
            ConfigErrorClass::TimeoutTooLarge,
        ));
    }
    for (path, value) in [
        ("timeouts.queue_ms", timeouts.queue_ms),
        ("timeouts.connect_ms", timeouts.connect_ms),
        ("timeouts.headers_ms", timeouts.headers_ms),
        ("timeouts.first_byte_ms", timeouts.first_byte_ms),
        ("timeouts.idle_ms", timeouts.idle_ms),
    ] {
        if value == 0 {
            return Err(ConfigError::new(path, ConfigErrorClass::Zero));
        }
        if value > MAX_TIMEOUT_MS {
            return Err(ConfigError::new(path, ConfigErrorClass::TimeoutTooLarge));
        }
        if value > timeouts.overall_ms {
            return Err(ConfigError::new(path, ConfigErrorClass::ExceedsOverall));
        }
    }
    let max_uploads = limits.max_uploads.unwrap_or(8);
    let max_buffered_bytes = limits.max_buffered_bytes.unwrap_or(256 * 1024 * 1024);
    if max_uploads == 0 || max_uploads > MAX_CONCURRENCY_LIMIT {
        return Err(ConfigError::new(
            "limits.max_uploads",
            ConfigErrorClass::OutOfRange,
        ));
    }
    if !(1..=4 * 1024 * 1024 * 1024).contains(&max_buffered_bytes) {
        return Err(ConfigError::new(
            "limits.max_buffered_bytes",
            ConfigErrorClass::OutOfRange,
        ));
    }
    let largest = max_audio_chat_body_bytes.max(
        limits
            .max_audio_bytes
            .saturating_add(crate::core::MAX_TRANSCRIPTION_OVERHEAD_BYTES as u64),
    );
    if largest
        .checked_mul(3)
        .is_none_or(|bytes| bytes > max_buffered_bytes)
    {
        return Err(ConfigError::new(
            "limits.max_buffered_bytes",
            ConfigErrorClass::BelowRequestReservation,
        ));
    }
    let upload_ms = timeouts
        .upload_ms
        .unwrap_or(30_000.min(timeouts.overall_ms));
    if upload_ms == 0 || upload_ms > timeouts.overall_ms {
        return Err(ConfigError::new(
            "timeouts.upload_ms",
            ConfigErrorClass::OutOfRange,
        ));
    }
    let sse_keepalive_ms = timeouts
        .sse_keepalive_ms
        .unwrap_or(15_000.min(timeouts.overall_ms));
    if sse_keepalive_ms == 0 {
        return Err(ConfigError::new(
            "timeouts.sse_keepalive_ms",
            ConfigErrorClass::Zero,
        ));
    }
    if sse_keepalive_ms > timeouts.overall_ms {
        return Err(ConfigError::new(
            "timeouts.sse_keepalive_ms",
            ConfigErrorClass::TimeoutTooLarge,
        ));
    }
    Ok((
        ValidatedLimits {
            max_uploads,
            max_buffered_bytes,
            max_queue: limits.max_queue,
            max_in_flight: limits.max_in_flight,
            max_body_bytes: limits.max_body_bytes,
            max_audio_bytes: limits.max_audio_bytes,
            max_audio_chat_body_bytes,
            max_extension_bytes: limits.max_extension_bytes,
        },
        ValidatedTimeouts {
            upload_ms,
            queue_ms: timeouts.queue_ms,
            connect_ms: timeouts.connect_ms,
            headers_ms: timeouts.headers_ms,
            first_byte_ms: timeouts.first_byte_ms,
            idle_ms: timeouts.idle_ms,
            overall_ms: timeouts.overall_ms,
            sse_keepalive_ms,
        },
    ))
}
