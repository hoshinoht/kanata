use super::*;

pub(super) fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serializes")
}

// ---- list / show ----

pub(super) fn format_date(at: u64) -> String {
    time::format(at)[..10].to_owned()
}

pub(super) fn format_optional_time(at: Option<u64>) -> String {
    at.map_or_else(|| "never".into(), time::format)
}

pub(super) fn format_rate_limit(limit: Option<KeyRateLimit>) -> String {
    limit.map_or_else(
        || "-".into(),
        |limit| format!("{}/{}ms", limit.requests, limit.per_ms),
    )
}

pub(super) fn format_daily_quota(quota: Option<crate::keys::quota::DailyQuota>) -> String {
    quota.map_or_else(
        || "unlimited".into(),
        |quota| {
            let value = |limit: Option<u64>| {
                limit.map_or_else(|| "unset".into(), |limit| limit.to_string())
            };
            format!(
                "requests {}, tokens {}, reservation {}",
                value(quota.requests),
                value(quota.tokens),
                value(quota.reservation_tokens)
            )
        },
    )
}

pub(super) fn rate_limit_json(limit: Option<KeyRateLimit>) -> Value {
    limit.map_or(
        Value::Null,
        |limit| json!({"requests": limit.requests, "per_ms": limit.per_ms}),
    )
}

pub(super) fn scope_list(scopes: &[RouteSelector]) -> String {
    scopes.iter().map(scope_label).collect::<Vec<_>>().join(",")
}

pub(super) fn usage_for(usage_dir: Option<&Path>) -> Option<BTreeMap<String, KeyUsage>> {
    usage_dir.map(usage::read_merged)
}

pub(super) fn key_json(
    record: &StoredKey,
    usage: Option<&BTreeMap<String, KeyUsage>>,
    now: u64,
) -> serde_json::Map<String, Value> {
    let used = usage.and_then(|usage| usage.get(record.id()));
    let Value::Object(object) = json!({
        "id": record.id(),
        "owner": record.is_owner(),
        "scopes": record.permissions().iter().map(|scope| json!({
            "model_alias": scope.model_alias.0,
            "operation": operation_name(scope.operation),
        })).collect::<Vec<_>>(),
        "created_at": time::format(record.created_at()),
        "expires_at": record.expires_at().map(time::format),
        "expired": record.is_expired(now),
        "rotated_at": record.rotated_at().map(time::format),
        "revoked_at": record.revoked_at().map(time::format),
        "last_used_at": used.filter(|used| used.last_used_at > 0).map(|used| time::format(used.last_used_at)),
        "requests": usage.map(|_| used.map_or(0, |used| used.requests)),
        "tokens": used.map(|used| used.tokens),
        "daily_quota": record.daily_quota(),
    }) else {
        unreachable!("object literal")
    };
    object
}

/// Scopes whose route is not in `config`.
pub(super) fn missing_routes_json(record: &StoredKey, config: &ValidatedConfig) -> Value {
    record
        .permissions()
        .iter()
        .filter(|scope| !route_exists(config, scope))
        .map(|scope| {
            json!({
                "model_alias": scope.model_alias.0,
                "operation": operation_name(scope.operation),
            })
        })
        .collect()
}

pub(super) fn list(arguments: &[String]) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &["--config", "--keys", "--usage-dir"],
        &["--all", "--json"],
        0,
    )?;
    let location = read_location(&args)?;
    let usage = usage_for(location.usage_dir.as_deref());
    let now = time::now();
    let records: Vec<&StoredKey> = location
        .keys
        .records()
        .iter()
        .filter(|record| args.has("--all") || !record.is_revoked())
        .collect();
    if args.has("--json") {
        let array: Vec<Value> = records
            .iter()
            .map(|record| {
                let mut object = key_json(record, usage.as_ref(), now);
                if let Some(config) = &location.config {
                    object.insert("missing_routes".into(), missing_routes_json(record, config));
                }
                Value::Object(object)
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&array).expect("json serializes"));
    }
    let mut rows = vec![
        [
            "ID",
            "OWNER",
            "SCOPES",
            "CREATED",
            "EXPIRES",
            "REVOKED",
            "LAST USED",
            "REQUESTS",
        ]
        .map(str::to_owned),
    ];
    for record in records {
        let used = usage.as_ref().and_then(|usage| usage.get(record.id()));
        let expires = match record.expires_at() {
            None => "never".into(),
            Some(at) if record.is_expired(now) => format!("{} expired", format_date(at)),
            Some(at) => format_date(at),
        };
        rows.push([
            record.id().to_owned(),
            if record.is_owner() { "yes" } else { "no" }.into(),
            record
                .permissions()
                .iter()
                .map(|scope| match &location.config {
                    Some(config) if !route_exists(config, scope) => {
                        format!("{}(no-route)", scope_label(scope))
                    }
                    _ => scope_label(scope),
                })
                .collect::<Vec<_>>()
                .join(","),
            format_date(record.created_at()),
            expires,
            record.revoked_at().map_or("-".into(), format_date),
            used.filter(|used| used.last_used_at > 0)
                .map_or("-".into(), |used| format_date(used.last_used_at)),
            match (&usage, used) {
                (None, _) => "-".into(),
                (Some(_), used) => used.map_or(0, |used| used.requests).to_string(),
            },
        ]);
    }
    Ok(render_table(&rows))
}

pub(super) fn render_table<const N: usize>(rows: &[[String; N]]) -> String {
    let mut widths = [0; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .zip(widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn show(arguments: &[String]) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &["--config", "--keys", "--usage-dir"],
        &["--json"],
        1,
    )?;
    let id = parse_id(
        args.positional
            .first()
            .ok_or_else(|| KEY_USAGE.to_owned())?,
    )?;
    let location = read_location(&args)?;
    let record = location
        .keys
        .records()
        .iter()
        .find(|record| record.id() == id)
        .ok_or_else(|| format!("no key {id:?}"))?;
    let usage = usage_for(location.usage_dir.as_deref());
    let now = time::now();
    let missing = |scope: &RouteSelector| {
        location
            .config
            .as_ref()
            .is_some_and(|config| !route_exists(config, scope))
    };
    if args.has("--json") {
        let mut object = key_json(record, usage.as_ref(), now);
        object.insert("max_in_flight".into(), json!(record.max_in_flight()));
        object.insert("rate_limit".into(), rate_limit_json(record.rate_limit()));
        if let Some(config) = &location.config {
            object.insert("missing_routes".into(), missing_routes_json(record, config));
        }
        return Ok(serde_json::to_string_pretty(&object).expect("json serializes"));
    }
    let used = usage.as_ref().and_then(|usage| usage.get(record.id()));
    let scopes = record
        .permissions()
        .iter()
        .map(|scope| {
            if missing(scope) {
                format!("{} (route no longer in config)", scope_label(scope))
            } else {
                scope_label(scope)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut expires = format_optional_time(record.expires_at());
    if record.is_expired(now) {
        expires.push_str(" (expired)");
    }
    let rows = [
        ("id", record.id().to_owned()),
        ("owner", if record.is_owner() { "yes" } else { "no" }.into()),
        ("scopes", scopes),
        (
            "max_in_flight",
            record
                .max_in_flight()
                .map_or("-".into(), |value| value.to_string()),
        ),
        ("rate_limit", format_rate_limit(record.rate_limit())),
        ("daily_quota", format_daily_quota(record.daily_quota())),
        (
            "tokens (input/output/reasoning)",
            used.map_or("-".into(), |used| {
                format!(
                    "{}/{}/{} ({} reports, {} missing)",
                    used.tokens.input_tokens,
                    used.tokens.output_tokens,
                    used.tokens.reasoning_tokens,
                    used.tokens.reported,
                    used.tokens.missing
                )
            }),
        ),
        ("created", time::format(record.created_at())),
        ("expires", expires),
        (
            "rotated",
            record.rotated_at().map_or("-".into(), time::format),
        ),
        (
            "revoked",
            record.revoked_at().map_or("-".into(), time::format),
        ),
        (
            "last used",
            used.filter(|used| used.last_used_at > 0)
                .map_or("-".into(), |used| time::format(used.last_used_at)),
        ),
        (
            "requests",
            match (&usage, used) {
                (None, _) => "-".into(),
                (Some(_), used) => used.map_or(0, |used| used.requests).to_string(),
            },
        ),
    ]
    .map(|(label, value)| [format!("{label}:"), value]);
    Ok(render_table(&rows))
}
