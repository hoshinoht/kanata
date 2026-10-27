use super::*;

pub(super) fn new(
    args: NewArgs,
    catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
) -> Result<String, String> {
    let id = args.id.clone();
    let days = args.expires.0;
    let owner = args.owner;
    let (config, keys_path) = mutating_location(&args.location)?;
    let routes = catalog(&config);
    let mut scopes = scope_flags(args.scopes(), &routes, new_flag)?;
    if scopes.is_empty() {
        scopes = run_picker(
            &routes,
            &[],
            "pass --chat <alias>, --transcription <alias> --embeddings <alias> or --speech <alias>",
        )?;
    }

    let (key, digest) = generate_key()?;
    let pending = args
        .key_out
        .as_deref()
        .map(|path| SecretFile::prepare(path, &key, false))
        .transpose()?;
    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let now = time::now();
    let expires_at = expires_at(now, days);
    let mut keys = locked.read()?;
    if keys.records().iter().any(|record| record.id() == id) {
        return Err(format!(
            "key id {id:?} already exists (ids are never reused, even after rm)"
        ));
    }
    if owner && keys.active().any(StoredKey::is_owner) {
        return Err(
            "an owner key already exists; use `kanata key rotate --owner` to replace it".to_owned(),
        );
    }
    let mut record = StoredKey::new(
        id.clone(),
        digest,
        owner,
        scopes,
        args.limits.max_in_flight,
        args.limits.rate_limit,
        now,
        expires_at,
    );
    record.set_daily_quota(quota_flags(
        args.quota.daily(),
        false,
        None,
        &config,
        "daily",
    )?);
    record.set_monthly_quota(quota_flags(
        args.quota.monthly(),
        false,
        None,
        &config,
        "monthly",
    )?);
    keys.push(record);
    let event = AuditEvent {
        action: "new",
        key_id: id.clone(),
        owner,
        changes: None,
    };
    commit_secret(&locked, &keys, &config, event, &key, pending)?;
    drop(locked);
    long_lived_warning(&id, days);
    Ok(reveal(
        &id,
        &key,
        expires_at,
        args.key_out.as_deref(),
        "created",
    ))
}
