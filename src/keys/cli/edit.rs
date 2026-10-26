use super::*;

pub(super) fn edit(
    arguments: &[String],
    catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &[
            "--config",
            "--keys",
            "--add-chat",
            "--remove-chat",
            "--add-transcription",
            "--remove-transcription",
            "--add-embeddings",
            "--remove-embeddings",
            "--add-speech",
            "--remove-speech",
            "--expires",
            "--max-in-flight",
            "--rate-limit",
            "--daily-requests",
            "--daily-tokens",
            "--reservation-tokens",
        ],
        &["--clear-limits", "--clear-quota", "--force"],
        1,
    )?;
    let id = parse_id(
        args.positional
            .first()
            .ok_or_else(|| KEY_USAGE.to_owned())?,
    )?;
    let days = args.one("--expires")?.map(parse_expires).transpose()?;
    let max_in_flight = args
        .one("--max-in-flight")?
        .map(|value| parse_positive("--max-in-flight", value))
        .transpose()?;
    let rate_limit = args
        .one("--rate-limit")?
        .map(parse_rate_limit)
        .transpose()?;
    let clear_limits = args.has("--clear-limits");
    if clear_limits && (max_in_flight.is_some() || rate_limit.is_some()) {
        return Err(
            "--clear-limits cannot be combined with --max-in-flight or --rate-limit".into(),
        );
    }
    let (config, keys_path) = mutating_location(&args, "edit")?;
    let routes = catalog(&config);
    let mut added = scope_flags(&args, &routes, add_flag)?;
    let mut removed = Vec::new();
    for (flag, operation) in [
        ("--remove-chat", Operation::Chat),
        ("--remove-transcription", Operation::Transcription),
        ("--remove-embeddings", Operation::Embeddings),
        ("--remove-speech", Operation::Speech),
    ] {
        for alias in args.all(flag) {
            let scope = selector(alias, operation);
            if removed.contains(&scope) || added.contains(&scope) {
                return Err(format!("{} is given twice", scope_label(&scope)));
            }
            removed.push(scope);
        }
    }
    let quota_given = args.has("--clear-quota")
        || ["--daily-requests", "--daily-tokens", "--reservation-tokens"]
            .iter()
            .any(|flag| !args.all(flag).is_empty());
    let limits_given = max_in_flight.is_some() || rate_limit.is_some() || clear_limits;
    if added.is_empty() && removed.is_empty() && days.is_none() && !limits_given && !quota_given {
        let current = file::load(&keys_path)?;
        let record = current
            .records()
            .iter()
            .find(|record| record.id() == id)
            .ok_or_else(|| format!("no key {id:?}"))?;
        let selection = run_picker(
            &routes,
            record.permissions(),
            "nothing to change; pass --add-chat/--remove-chat/--add-transcription/--remove-transcription/--add-embeddings/--remove-embeddings/--add-speech/--remove-speech, --expires or limit flags",
        )?;
        added = selection
            .iter()
            .filter(|scope| !record.permissions().contains(scope))
            .cloned()
            .collect();
        removed = record
            .permissions()
            .iter()
            .filter(|scope| !selection.contains(scope))
            .cloned()
            .collect();
        if added.is_empty() && removed.is_empty() {
            return Ok(format!("key {id} unchanged"));
        }
    }

    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let mut keys = locked.read()?;
    let record = keys.get_mut(&id).ok_or_else(|| format!("no key {id:?}"))?;
    if record.is_revoked() {
        return Err(format!("key {id:?} is revoked and cannot be edited"));
    }
    let before = record.permissions().to_vec();
    for scope in &added {
        if before.contains(scope) {
            return Err(format!("key {id:?} already has {}", scope_label(scope)));
        }
    }
    for scope in &removed {
        if !before.contains(scope) {
            return Err(format!("key {id:?} has no scope {}", scope_label(scope)));
        }
    }
    let after: Vec<RouteSelector> = before
        .iter()
        .filter(|scope| !removed.contains(scope))
        .chain(added.iter())
        .cloned()
        .collect();
    if after.is_empty() {
        return Err(format!(
            "key {id:?} must keep at least one scope; use `kanata key rm {id}` to remove its access"
        ));
    }
    let exposure = |scope: &RouteSelector| {
        routes
            .iter()
            .find(|route| route.selector == *scope)
            .map(|route| route.exposure)
    };
    let publicly_usable = publicly_usable(record.is_owner(), &before, &routes);
    let mut notes = Vec::new();
    for scope in &added {
        match exposure(scope) {
            Some(Exposure::Never) if publicly_usable && !args.has("--force") => {
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

    let quota = quota_flags(&args, record.daily_quota(), &config)?;
    record.set_daily_quota(quota);
    record.set_permissions(after.clone());
    let mut changes = serde_json::Map::new();
    if quota_given {
        changes.insert("daily_quota".into(), json!(quota));
    }
    changes.insert("added".into(), scope_json(&added));
    changes.insert("removed".into(), scope_json(&removed));
    let mut lines = vec![
        format!("updated key {id}"),
        format!("scopes: {} -> {}", scope_list(&before), scope_list(&after)),
    ];
    if quota_given {
        lines.push(format!("daily quota: {}", format_daily_quota(quota)));
    }
    if let Some(days) = days {
        let expires_at = expires_at(time::now(), days);
        record.set_expires_at(expires_at);
        changes.insert("expires_at".into(), json!(expires_at.map(time::format)));
        lines.push(format!("expires: {}", format_optional_time(expires_at)));
    }
    if limits_given {
        let max_in_flight = max_in_flight.or(if clear_limits {
            None
        } else {
            record.max_in_flight()
        });
        let rate_limit = rate_limit.or(if clear_limits {
            None
        } else {
            record.rate_limit()
        });
        record.set_limits(max_in_flight, rate_limit);
        changes.insert("max_in_flight".into(), json!(max_in_flight));
        changes.insert("rate_limit".into(), rate_limit_json(rate_limit));
        lines.push(format!(
            "limits: max_in_flight {}, rate_limit {}",
            max_in_flight.map_or("-".into(), |value| value.to_string()),
            format_rate_limit(rate_limit)
        ));
    }
    let owner = record.is_owner();
    write_keys(&locked, &keys, &config)?;
    locked.audit(&[AuditEvent {
        action: "edit",
        key_id: id.clone(),
        owner,
        changes: Some(Value::Object(changes)),
    }])?;
    drop(locked);
    if let Some(days) = days {
        long_lived_warning(&id, days);
    }
    for note in notes {
        eprintln!("{note}");
    }
    eprintln!("{APPLY_NOTE}");
    Ok(lines.join("\n"))
}
