use super::*;

pub(super) fn new(
    arguments: &[String],
    catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &[
            "--config",
            "--keys",
            "--id",
            "--chat",
            "--transcription",
            "--embeddings",
            "--speech",
            "--expires",
            "--max-in-flight",
            "--rate-limit",
            "--daily-requests",
            "--daily-tokens",
            "--reservation-tokens",
            "--key-out",
        ],
        &["--owner"],
        0,
    )?;
    let id = parse_id(args.one("--id")?.ok_or_else(|| KEY_USAGE.to_owned())?)?;
    let days = parse_expires(
        args.one("--expires")?
            .ok_or("--expires <1|3|7|13|30|60|unlimited> is required")?,
    )?;
    let max_in_flight = args
        .one("--max-in-flight")?
        .map(|value| parse_positive("--max-in-flight", value))
        .transpose()?;
    let rate_limit = args
        .one("--rate-limit")?
        .map(parse_rate_limit)
        .transpose()?;
    let key_out = args.one("--key-out")?.map(PathBuf::from);
    let owner = args.has("--owner");
    let (config, keys_path) = mutating_location(&args, "new")?;
    let routes = catalog(&config);
    let mut scopes = scope_flags(&args, &routes, new_flag)?;
    if scopes.is_empty() {
        scopes = run_picker(
            &routes,
            &[],
            "pass --chat <alias>, --transcription <alias> --embeddings <alias> or --speech <alias>",
        )?;
    }

    let (key, digest) = generate_key()?;
    let pending = key_out
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
        max_in_flight,
        rate_limit,
        now,
        expires_at,
    );
    record.set_daily_quota(quota_flags(&args, None, &config)?);
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
    Ok(reveal(&id, &key, expires_at, key_out.as_deref(), "created"))
}
