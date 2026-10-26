use super::*;

pub(super) fn rotate(arguments: &[String]) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &["--config", "--keys", "--expires", "--key-out"],
        &["--owner"],
        1,
    )?;
    let target = match (args.positional.first(), args.has("--owner")) {
        (Some(id), false) => Some(parse_id(id)?),
        (None, true) => None,
        _ => return Err(KEY_USAGE.into()),
    };
    let days = parse_expires(
        args.one("--expires")?
            .ok_or("--expires <1|3|7|13|30|60|unlimited> is required")?,
    )?;
    let key_out = args.one("--key-out")?.map(PathBuf::from);
    let (config, keys_path) = mutating_location(&args, "rotate")?;

    let (key, digest) = generate_key()?;
    let pending = key_out
        .as_deref()
        .map(|path| SecretFile::prepare(path, &key, true))
        .transpose()?;
    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let now = time::now();
    let expires_at = expires_at(now, days);
    let mut keys = locked.read()?;
    let record = match &target {
        Some(id) => keys.get_mut(id).ok_or_else(|| format!("no key {id:?}"))?,
        None => keys
            .records_mut()
            .iter_mut()
            .find(|record| record.is_owner() && !record.is_revoked())
            .ok_or("no active owner key; create one with `kanata key new --owner`")?,
    };
    if record.is_revoked() {
        return Err(format!(
            "key {:?} is revoked; create a new key instead",
            record.id()
        ));
    }
    record.rotate(digest, now, expires_at);
    let id = record.id().to_owned();
    let owner = record.is_owner();
    let event = AuditEvent {
        action: "rotate",
        key_id: id.clone(),
        owner,
        changes: None,
    };
    commit_secret(&locked, &keys, &config, event, &key, pending)?;
    drop(locked);
    long_lived_warning(&id, days);
    Ok(reveal(&id, &key, expires_at, key_out.as_deref(), "rotated"))
}
