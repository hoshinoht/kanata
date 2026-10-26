use super::*;

pub(crate) fn parse_sha256_hex(value: &str) -> Option<[u8; 32]> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut digest = [0; 32];
    for (slot, [high, low]) in digest.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *slot = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Some(digest)
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn parse_model_alias(value: &str) -> Option<Option<ReasoningEffort>> {
    let Some((model, effort)) = value.split_once(':') else {
        return valid_identifier(value).then_some(None);
    };
    if !valid_identifier(model) || effort.contains(':') {
        return None;
    }
    let effort = ReasoningEffort::parse(effort)?;
    Some(Some(effort))
}

pub(in crate::config) fn valid_env_name(value: &&str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_uppercase() || (index > 0 && byte.is_ascii_digit())
        })
}

pub(in crate::config) fn valid_path_token(value: &&str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
pub(in crate::config) fn valid_diagnostic_path(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'[' | b']'))
}

pub(in crate::config) fn validate_key_path<'a>(
    value: &'a str,
    path: &str,
) -> Result<&'a Path, ConfigError> {
    if value.is_empty() {
        return Err(ConfigError::new(path, "empty"));
    }
    if value.chars().any(char::is_control) {
        return Err(ConfigError::new(path, "invalid_path"));
    }
    Ok(Path::new(value))
}

pub(in crate::config) fn validate_extension_allowlist(
    values: Vec<String>,
    path: &str,
) -> Result<BTreeSet<ExtensionKey>, ConfigError> {
    let mut validated = BTreeSet::new();
    for (index, value) in values.into_iter().enumerate() {
        let entry_path = format!("{path}[{index}]");
        let key = ExtensionKey::parse(value)
            .map_err(|_| ConfigError::new(entry_path.clone(), "invalid_extension_key"))?;
        if !validated.insert(key) {
            return Err(ConfigError::new(entry_path, "duplicate"));
        }
    }
    Ok(validated)
}

/// Optional concurrency cap: unset means uncapped, zero is rejected.
pub(in crate::config) fn validate_optional_limit(
    value: Option<u64>,
    path: &str,
) -> Result<Option<u64>, ConfigError> {
    match value {
        Some(0) => Err(ConfigError::new(format!("{path}.max_in_flight"), "zero")),
        Some(value) if value > MAX_CONCURRENCY_LIMIT => Err(ConfigError::new(
            format!("{path}.max_in_flight"),
            "limit_too_large",
        )),
        value => Ok(value),
    }
}
