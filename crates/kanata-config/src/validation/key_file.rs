use super::*;

/// Key permission rules for the keys file. `routes = None`
/// (revoked records) checks syntax only.
pub(crate) fn validate_permissions(
    raw: &[RawPermission],
    path: &str,
    routes: Option<&[ValidatedRoute]>,
) -> Result<Vec<RouteSelector>, ConfigError> {
    if raw.is_empty() {
        return Err(ConfigError::new(
            format!("{path}.permissions"),
            ConfigErrorClass::Empty,
        ));
    }
    let mut seen = BTreeSet::new();
    for (index, permission) in raw.iter().enumerate() {
        let permission_path = || format!("{path}.permissions[{index}]");
        if !seen.insert((permission.model_alias.as_str(), permission.operation)) {
            return Err(ConfigError::new(
                permission_path(),
                ConfigErrorClass::Duplicate,
            ));
        }
        match routes {
            Some(routes)
                if !routes.iter().any(|route| {
                    route.identity.selector.model_alias.0 == permission.model_alias
                        && route.identity.selector.operation == permission.operation
                }) =>
            {
                return Err(ConfigError::new(
                    permission_path(),
                    ConfigErrorClass::UnknownRouteSelector,
                ));
            }
            None if parse_model_alias(&permission.model_alias).is_none() => {
                return Err(ConfigError::new(
                    permission_path(),
                    ConfigErrorClass::InvalidExactAlias,
                ));
            }
            _ => {}
        }
    }
    Ok(raw
        .iter()
        .map(|permission| RouteSelector {
            model_alias: ModelAlias(permission.model_alias.clone()),
            operation: permission.operation,
        })
        .collect())
}

pub(crate) fn validate_key_limits(
    max_in_flight: Option<u64>,
    rate_limit: Option<RawRateLimit>,
    path: &str,
) -> Result<(Option<u64>, Option<KeyRateLimit>), ConfigError> {
    let max_in_flight = validate_optional_limit(max_in_flight, path)?;
    let rate_limit = rate_limit
        .map(|limit| validate_rate_limit(limit, path))
        .transpose()?;
    Ok((max_in_flight, rate_limit))
}

pub(crate) fn load_key_file(
    raw: &RawKeys,
    config_dir: &Path,
    routes: &[ValidatedRoute],
    read_file: bool,
) -> Result<(Vec<ValidatedApplicationKey>, KeySource), ConfigError> {
    let path = config_dir.join(validate_key_path(&raw.file, "keys.file")?);
    let usage_dir = raw
        .usage_dir
        .as_deref()
        .map(|dir| validate_key_path(dir, "keys.usage_dir").map(|dir| config_dir.join(dir)))
        .transpose()?;
    if !read_file {
        return Ok((
            Vec::new(),
            KeySource {
                path,
                usage_dir,
                missing: false,
                sha256: None,
            },
        ));
    }
    let Some(bytes) = keys::file::read(&path)? else {
        return Ok((
            Vec::new(),
            KeySource {
                path,
                usage_dir,
                missing: true,
                sha256: None,
            },
        ));
    };
    let file = keys::file::parse(&bytes, routes)?;
    if usage_dir.is_none() && file.active().any(|key| key.daily_quota().is_some()) {
        return Err(ConfigError::new(
            "keys.usage_dir",
            ConfigErrorClass::RequiredForDailyQuota,
        ));
    }
    if usage_dir.is_none() && file.active().any(|key| key.monthly_quota().is_some()) {
        return Err(ConfigError::new(
            "keys.usage_dir",
            ConfigErrorClass::RequiredForMonthlyQuota,
        ));
    }
    Ok((
        keys_from_file(&file, routes)?,
        KeySource {
            path,
            usage_dir,
            missing: false,
            sha256: Some(file.sha256()),
        },
    ))
}

/// Active records only; re-checks scopes so a file parsed elsewhere cannot
/// grant a selector this config does not route.
pub(crate) fn keys_from_file(
    file: &KeysFile,
    routes: &[ValidatedRoute],
) -> Result<Vec<ValidatedApplicationKey>, ConfigError> {
    let mut keys = Vec::new();
    for (index, record) in file.records().iter().enumerate() {
        if record.is_revoked() {
            continue;
        }
        for (permission_index, selector) in record.permissions().iter().enumerate() {
            if !routes
                .iter()
                .any(|route| route.identity.selector == *selector)
            {
                return Err(ConfigError::new(
                    format!("keys_file.keys[{index}].permissions[{permission_index}]"),
                    ConfigErrorClass::UnknownRouteSelector,
                ));
            }
        }
        keys.push(ValidatedApplicationKey {
            id: record.id().to_owned(),
            owner: record.is_owner(),
            digest: *record.digest(),
            permissions: record.permissions().to_vec(),
            max_in_flight: record.max_in_flight(),
            rate_limit: record.rate_limit(),
            expires_at: record.expires_at(),
            daily_quota: record.daily_quota(),
            monthly_quota: record.monthly_quota(),
        });
    }
    Ok(keys)
}

fn validate_rate_limit(raw: RawRateLimit, path: &str) -> Result<KeyRateLimit, ConfigError> {
    if raw.requests == 0 {
        return Err(ConfigError::new(
            format!("{path}.rate_limit.requests"),
            ConfigErrorClass::Zero,
        ));
    }
    if raw.requests > MAX_CONCURRENCY_LIMIT {
        return Err(ConfigError::new(
            format!("{path}.rate_limit.requests"),
            ConfigErrorClass::LimitTooLarge,
        ));
    }
    if raw.per_ms == 0 {
        return Err(ConfigError::new(
            format!("{path}.rate_limit.per_ms"),
            ConfigErrorClass::Zero,
        ));
    }
    if raw.per_ms > MAX_TIMEOUT_MS {
        return Err(ConfigError::new(
            format!("{path}.rate_limit.per_ms"),
            ConfigErrorClass::TimeoutTooLarge,
        ));
    }
    Ok(KeyRateLimit {
        requests: raw.requests,
        per_ms: raw.per_ms,
    })
}
