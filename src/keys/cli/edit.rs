use super::*;

/// Scopes to grant and revoke.
struct ScopeChanges {
    added: Vec<RouteSelector>,
    removed: Vec<RouteSelector>,
}

/// Audit `changes` and stdout lines of one edit.
struct Report {
    changes: serde_json::Map<String, Value>,
    lines: Vec<String>,
}

pub(super) fn edit(
    args: EditArgs,
    catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
) -> Result<String, String> {
    let id = args.id.as_str();
    let (config, keys_path) = mutating_location(&args.location)?;
    let routes = catalog(&config);
    let Some(scopes) = requested_scopes(&args, &routes, &keys_path)? else {
        return Ok(format!("key {id} unchanged"));
    };

    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let mut keys = locked.read()?;
    let record = keys.get_mut(id).ok_or_else(|| format!("no key {id:?}"))?;
    if record.is_revoked() {
        return Err(format!("key {id:?} is revoked and cannot be edited"));
    }
    let before = record.permissions().to_vec();
    let after = scopes_after(id, &before, &scopes)?;
    let notes = exposure_notes(
        id,
        publicly_usable(record.is_owner(), &before, &routes),
        &scopes.added,
        &routes,
        &config,
        args.force,
    )?;
    let mut report = Report {
        changes: serde_json::Map::new(),
        lines: vec![
            format!("updated key {id}"),
            format!("scopes: {} -> {}", scope_list(&before), scope_list(&after)),
        ],
    };
    apply_quota(&args, record, &config, &mut report)?;
    record.set_permissions(after);
    report
        .changes
        .insert("added".into(), scope_json(&scopes.added));
    report
        .changes
        .insert("removed".into(), scope_json(&scopes.removed));
    if let Some(Expiry(days)) = args.expires {
        apply_expiry(days, record, &mut report);
    }
    if args.limits_given() {
        apply_limits(&args, record, &mut report);
    }
    let owner = record.is_owner();
    write_keys(&locked, &keys, &config)?;
    locked.audit(&[AuditEvent {
        action: "edit",
        key_id: id.to_owned(),
        owner,
        changes: Some(Value::Object(report.changes)),
    }])?;
    drop(locked);
    if let Some(Expiry(days)) = args.expires {
        long_lived_warning(id, days);
    }
    for note in notes {
        eprintln!("{note}");
    }
    eprintln!("{APPLY_NOTE}");
    Ok(report.lines.join("\n"))
}

/// Scope flags, or the route picker when no change flag is given; `None` when nothing changes.
fn requested_scopes(
    args: &EditArgs,
    routes: &[RouteChoice],
    keys_path: &Path,
) -> Result<Option<ScopeChanges>, String> {
    let added = scope_flags(args.added(), routes, add_flag)?;
    let mut removed = Vec::new();
    for (operation, aliases) in args.removed() {
        for alias in aliases {
            let scope = selector(alias, operation);
            if removed.contains(&scope) || added.contains(&scope) {
                return Err(format!("{} is given twice", scope_label(&scope)));
            }
            removed.push(scope);
        }
    }
    if !added.is_empty()
        || !removed.is_empty()
        || args.expires.is_some()
        || args.limits_given()
        || args.quota_given()
    {
        return Ok(Some(ScopeChanges { added, removed }));
    }
    let current = file::load(keys_path)?;
    let record = current
        .records()
        .iter()
        .find(|record| record.id() == args.id)
        .ok_or_else(|| format!("no key {:?}", args.id))?;
    let selection = run_picker(
        routes,
        record.permissions(),
        "nothing to change; pass --add-chat/--remove-chat/--add-transcription/--remove-transcription/--add-embeddings/--remove-embeddings/--add-speech/--remove-speech, --expires or limit flags",
    )?;
    let added: Vec<_> = selection
        .iter()
        .filter(|scope| !record.permissions().contains(scope))
        .cloned()
        .collect();
    let removed: Vec<_> = record
        .permissions()
        .iter()
        .filter(|scope| !selection.contains(scope))
        .cloned()
        .collect();
    Ok((!added.is_empty() || !removed.is_empty()).then_some(ScopeChanges { added, removed }))
}

/// `before` with the changes applied; refuses no-op changes and an empty result.
fn scopes_after(
    id: &str,
    before: &[RouteSelector],
    scopes: &ScopeChanges,
) -> Result<Vec<RouteSelector>, String> {
    if let Some(scope) = scopes.added.iter().find(|scope| before.contains(scope)) {
        return Err(format!("key {id:?} already has {}", scope_label(scope)));
    }
    if let Some(scope) = scopes.removed.iter().find(|scope| !before.contains(scope)) {
        return Err(format!("key {id:?} has no scope {}", scope_label(scope)));
    }
    let after: Vec<RouteSelector> = before
        .iter()
        .filter(|scope| !scopes.removed.contains(scope))
        .chain(&scopes.added)
        .cloned()
        .collect();
    if after.is_empty() {
        return Err(format!(
            "key {id:?} must keep at least one scope; use `kanata key rm {id}` to remove its access"
        ));
    }
    Ok(after)
}

/// Notes for private-only grants; refuses a never-public grant on a public key without `force`.
fn exposure_notes(
    id: &str,
    publicly_usable: bool,
    added: &[RouteSelector],
    routes: &[RouteChoice],
    config: &ValidatedConfig,
    force: bool,
) -> Result<Vec<String>, String> {
    let exposure = |scope: &RouteSelector| {
        routes
            .iter()
            .find(|route| route.selector == *scope)
            .map(|route| route.exposure)
    };
    let mut notes = Vec::new();
    for scope in added {
        match exposure(scope) {
            Some(Exposure::Never) if publicly_usable && !force => {
                return Err(format!(
                    "adding {} removes key {id:?} from the public listener entirely (that route is never public); pass --force to proceed",
                    scope_label(scope)
                ));
            }
            Some(Exposure::Private) if config.listeners().public().is_some() => {
                notes.push(format!(
                    "note: {} is private-only; it works on the private listener only",
                    scope_label(scope)
                ))
            }
            _ => {}
        }
    }
    Ok(notes)
}

fn apply_quota(
    args: &EditArgs,
    record: &mut StoredKey,
    config: &ValidatedConfig,
    report: &mut Report,
) -> Result<(), String> {
    let quota = quota_flags(&args.quota, args.clear_quota, record.daily_quota(), config)?;
    record.set_daily_quota(quota);
    if args.quota_given() {
        report.changes.insert("daily_quota".into(), json!(quota));
        report
            .lines
            .push(format!("daily quota: {}", format_daily_quota(quota)));
    }
    Ok(())
}

fn apply_expiry(days: Option<u64>, record: &mut StoredKey, report: &mut Report) {
    let expires_at = expires_at(time::now(), days);
    record.set_expires_at(expires_at);
    report
        .changes
        .insert("expires_at".into(), json!(expires_at.map(time::format)));
    report
        .lines
        .push(format!("expires: {}", format_optional_time(expires_at)));
}

/// Given limits replace the current ones; `--clear-limits` removes both.
fn apply_limits(args: &EditArgs, record: &mut StoredKey, report: &mut Report) {
    let (max_in_flight, rate_limit) = if args.clear_limits {
        (None, None)
    } else {
        (
            args.limits.max_in_flight.or(record.max_in_flight()),
            args.limits.rate_limit.or(record.rate_limit()),
        )
    };
    record.set_limits(max_in_flight, rate_limit);
    report
        .changes
        .insert("max_in_flight".into(), json!(max_in_flight));
    report
        .changes
        .insert("rate_limit".into(), rate_limit_json(rate_limit));
    report.lines.push(format!(
        "limits: max_in_flight {}, rate_limit {}",
        max_in_flight.map_or("-".into(), |value| value.to_string()),
        format_rate_limit(rate_limit)
    ));
}
