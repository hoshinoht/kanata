use super::*;

pub(super) fn rm(args: RmArgs) -> Result<String, String> {
    let id = args.id;
    let (config, keys_path) = mutating_location(&args.location)?;
    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let mut keys = locked.read()?;
    let record = keys.get_mut(&id).ok_or_else(|| format!("no key {id:?}"))?;
    if record.is_revoked() {
        return Ok(format!("key {id} is already revoked"));
    }
    if record.is_owner() && !args.force {
        return Err(format!(
            "key {id:?} is the owner key; pass --force to revoke it"
        ));
    }
    record.revoke(time::now());
    let owner = record.is_owner();
    write_keys(&locked, &keys, &config)?;
    locked.audit(&[AuditEvent {
        action: "rm",
        key_id: id.clone(),
        owner,
        changes: None,
    }])?;
    eprintln!("{APPLY_NOTE}");
    Ok(format!("revoked key {id}"))
}
