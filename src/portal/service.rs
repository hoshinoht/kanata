use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{self, KeyRateLimit, KeySource, ValidatedConfig, valid_identifier};
use crate::core::{ModelAlias, Operation, RouteSelector};
use crate::keys::cli::{Exposure, RouteChoice};
use crate::keys::file::{self, KeysFile, StoredKey};
use crate::keys::quota::DailyQuota;
use crate::keys::store::{self, AuditEvent};
use crate::keys::{time, usage};

pub(super) struct KeyService {
    pub path: PathBuf,
    pub catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Change {
    New {
        revision: String,
        id: String,
        scopes: Vec<Scope>,
        expires: String,
        #[serde(default)]
        owner: bool,
        limits: Option<Limits>,
        daily_quota: Option<DailyQuota>,
        #[serde(default)]
        confirm_private: bool,
    },
    Edit {
        revision: String,
        id: String,
        scopes: Vec<Scope>,
        expires: Option<String>,
        limits: Option<Limits>,
        daily_quota: Option<DailyQuota>,
        #[serde(default)]
        clear_quota: bool,
        #[serde(default)]
        confirm_private: bool,
    },
    Rotate {
        revision: String,
        id: String,
        expires: String,
        confirm: String,
    },
    Revoke {
        revision: String,
        id: String,
        confirm: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scope {
    model_alias: String,
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Limits {
    max_in_flight: Option<u64>,
    rate_limit: Option<Rate>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rate {
    requests: u64,
    per_ms: u64,
}

impl Change {
    fn identity(&self) -> (&str, &str) {
        match self {
            Self::New { revision, id, .. }
            | Self::Edit { revision, id, .. }
            | Self::Rotate { revision, id, .. }
            | Self::Revoke { revision, id, .. } => (revision, id),
        }
    }
}

pub(super) fn validate_location(path: &Path) -> Result<(), String> {
    location(path).map(|_| ())
}

fn location(path: &Path) -> Result<(ValidatedConfig, PathBuf), String> {
    let config = config::load_deferring_keys(path).map_err(|error| error.to_string())?;
    let KeySource::File { path, .. } = config.key_source() else {
        return Err("Portal requires a separate keys file. Run `kanata key migrate --config <path>` on the host first".into());
    };
    let path = path.clone();
    Ok((config, path))
}

fn revision(keys: &KeysFile) -> String {
    keys.sha256()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl KeyService {
    pub fn snapshot(&self) -> Result<Value, String> {
        let (config, path) = location(&self.path)?;
        let keys = match file::read(&path).map_err(|error| error.to_string())? {
            Some(bytes) => file::parse_without_routes(&bytes).map_err(|error| error.to_string())?,
            None => KeysFile::default(),
        };
        let usage_dir = match config.key_source() {
            KeySource::File { usage_dir, .. } => usage_dir.as_deref(),
            _ => None,
        };
        let usage = usage_dir.map(usage::read_merged);
        let catalog = (self.catalog)(&config);
        let now = time::now();
        let records: Vec<Value> = keys.records().iter().map(|key| {
            let used = usage.as_ref().and_then(|usage| usage.get(key.id()));
            json!({
                "id": key.id(), "owner": key.is_owner(), "scopes": scope_json(key.permissions()),
                "created_at": time::format(key.created_at()), "expires_at": key.expires_at().map(time::format),
                "expired": key.is_expired(now), "revoked_at": key.revoked_at().map(time::format),
                "rotated_at": key.rotated_at().map(time::format),
                "max_in_flight": key.max_in_flight(),
                "rate_limit": key.rate_limit().map(|value| json!({"requests": value.requests, "per_ms": value.per_ms})),
                "requests": usage.as_ref().map(|_| used.map_or(0, |value| value.requests)),
                "tokens": used.map(|value| value.tokens),
                "last_used_at": used.filter(|value| value.last_used_at > 0).map(|value| time::format(value.last_used_at)),
                "public_access": publicly_usable(key, &catalog),
                "daily_quota": key.daily_quota(),
                "missing_routes": key.permissions().iter().filter(|scope| !catalog.iter().any(|route| route.selector == **scope)).map(|scope| json!({"model_alias":scope.model_alias.0,"operation":scope.operation.as_str()})).collect::<Vec<_>>(),
            })
        }).collect();
        Ok(json!({
            "revision": revision(&keys), "keys": records,
            "routes": catalog.iter().map(|route| {
                let configured = config.routes().iter().find(|configured| configured.identity().selector == route.selector);
                let selectable = configured.filter(|configured| config.adapters().iter().any(|adapter| adapter.id() == configured.adapter_id() && adapter.capabilities().reasoning_control));
                json!({
                    "model_alias": route.selector.model_alias.0, "operation": route.selector.operation.as_str(),
                    "display_alias": selectable.map(|configured| configured.model_family_alias()).unwrap_or(&route.selector.model_alias.0),
                    "reasoning_effort": selectable.and_then(|configured| configured.pinned_reasoning_effort()),
                    "exposure": match route.exposure { Exposure::Public => "public", Exposure::Private => "private", Exposure::Never => "never_public" }
                })
            }).collect::<Vec<_>>(),
            "usage_configured": usage_dir.is_some(), "usage_note": "Persisted usage may lag by 30 seconds. Missing provider token usage is not zero.",
            "reload_note": "Saved keys are normally reloaded within 2 seconds. This portal does not verify gateway availability or reload success."
        }))
    }

    pub fn change(&self, change: Change) -> Result<Value, String> {
        let (config, path) = location(&self.path)?;
        let (expected, id) = change.identity();
        if !valid_identifier(id) {
            return Err(
                "Use a key ID containing letters, digits, periods, underscores or hyphens".into(),
            );
        }
        if matches!(&change, Change::New { .. }) && id.len() > 64 {
            return Err("New key IDs in the portal are limited to 64 characters".into());
        }
        let id = id.to_owned();
        let locked = store::lock(&path, store::LOCK_TIMEOUT)?;
        let mut keys = locked.read()?;
        if expected != revision(&keys) {
            return Err("Keys changed since this page was loaded. Refresh before saving".into());
        }
        let catalog = (self.catalog)(&config);
        let now = time::now();
        let mut secret = None;
        let mut changes = None;
        let (action, owner) = match change {
            Change::New {
                scopes,
                expires,
                owner,
                limits,
                daily_quota,
                confirm_private,
                ..
            } => {
                if keys.records().iter().any(|record| record.id() == id) {
                    return Err("This key ID already exists; revoked IDs cannot be reused".into());
                }
                if owner && keys.active().any(StoredKey::is_owner) {
                    return Err("An owner key already exists. Rotate that key instead".into());
                }
                let scopes = parse_scopes(scopes, &catalog, confirm_private)?;
                let expires = expires_at(&expires, now)?;
                let (max, rate) = parse_limits(limits.as_ref());
                let value = format!("kanata_sk_{}", super::random_secret()?);
                let daily_quota = parse_quota(daily_quota, false, None, &config)?;
                let mut key = StoredKey::new(
                    id.clone(),
                    super::digest(&value),
                    owner,
                    scopes,
                    max,
                    rate,
                    now,
                    expires,
                );
                key.set_daily_quota(daily_quota);
                keys.push(key);
                secret = Some(value);
                ("new", owner)
            }
            Change::Edit {
                scopes,
                expires,
                limits,
                daily_quota,
                clear_quota,
                confirm_private,
                ..
            } => {
                let scopes = parse_scopes(scopes, &catalog, confirm_private)?;
                let key = active_key(&mut keys, &id)?;
                let daily_quota =
                    parse_quota(daily_quota, clear_quota, key.daily_quota(), &config)?;
                key.set_daily_quota(daily_quota);
                let before = scope_json(key.permissions());
                key.set_permissions(scopes);
                if let Some(expires) = expires {
                    key.set_expires_at(expires_at(&expires, now)?);
                }
                if let Some(limits) = limits {
                    let (max, rate) = parse_limits(Some(&limits));
                    key.set_limits(max, rate);
                }
                changes = Some(
                    json!({"before_scopes":before,"after_scopes":scope_json(key.permissions()),"expires_at":key.expires_at().map(time::format),"max_in_flight":key.max_in_flight(),"daily_quota":key.daily_quota(),"rate_limit":key.rate_limit().map(|rate| json!({"requests":rate.requests,"per_ms":rate.per_ms}))}),
                );
                ("edit", key.is_owner())
            }
            Change::Rotate {
                expires, confirm, ..
            } => {
                if confirm != id {
                    return Err("Type the key ID to confirm rotation".into());
                }
                let key = active_key(&mut keys, &id)?;
                let value = format!("kanata_sk_{}", super::random_secret()?);
                key.rotate(super::digest(&value), now, expires_at(&expires, now)?);
                secret = Some(value);
                ("rotate", key.is_owner())
            }
            Change::Revoke { confirm, .. } => {
                if confirm != id {
                    return Err("Type the key ID to confirm revocation".into());
                }
                let key = active_key(&mut keys, &id)?;
                key.revoke(now);
                ("rm", key.is_owner())
            }
        };
        locked.write(&keys, config.routes())?;
        let warning = locked
            .audit(&[AuditEvent {
                action,
                key_id: id.clone(),
                owner,
                changes,
            }])
            .err();
        Ok(json!({"ok":true,"id":id,"secret":secret,"warning":warning}))
    }
}

fn active_key<'a>(keys: &'a mut KeysFile, id: &str) -> Result<&'a mut StoredKey, String> {
    let key = keys
        .records_mut()
        .iter_mut()
        .find(|key| key.id() == id)
        .ok_or("Key not found")?;
    if key.is_revoked() {
        return Err("This key is revoked. Create a new key instead".into());
    }
    Ok(key)
}

fn parse_scopes(
    scopes: Vec<Scope>,
    catalog: &[RouteChoice],
    confirm_private: bool,
) -> Result<Vec<RouteSelector>, String> {
    if scopes.is_empty() || scopes.len() > catalog.len() {
        return Err("Select at least one configured route".into());
    }
    let mut result = Vec::new();
    for scope in scopes {
        config::parse_model_alias(&scope.model_alias).ok_or("Invalid model alias")?;
        let selector = RouteSelector {
            model_alias: ModelAlias(scope.model_alias),
            operation: scope.operation,
        };
        let route = catalog
            .iter()
            .find(|route| route.selector == selector)
            .ok_or("A selected route is no longer configured; refresh and try again")?;
        if route.exposure == Exposure::Never && !confirm_private {
            return Err("Confirm that this key will be excluded from the public listener".into());
        }
        if result.contains(&selector) {
            return Err("Duplicate scope".into());
        }
        result.push(selector);
    }
    Ok(result)
}

fn parse_limits(limits: Option<&Limits>) -> (Option<u64>, Option<KeyRateLimit>) {
    (
        limits.and_then(|value| value.max_in_flight),
        limits
            .and_then(|value| value.rate_limit.as_ref())
            .map(|value| KeyRateLimit {
                requests: value.requests,
                per_ms: value.per_ms,
            }),
    )
}

fn expires_at(choice: &str, now: u64) -> Result<Option<u64>, String> {
    match choice {
        "unlimited" => Ok(None),
        "1" | "3" | "7" | "13" | "30" | "60" => Ok(Some(
            now + choice.parse::<u64>().expect("validated days") * 86400,
        )),
        _ => Err("Choose an expiry of 1, 3, 7, 13, 30 or 60 days, or unlimited".into()),
    }
}

fn scope_json(scopes: &[RouteSelector]) -> Value {
    scopes
        .iter()
        .map(
            |scope| json!({"model_alias":scope.model_alias.0,"operation":scope.operation.as_str()}),
        )
        .collect()
}

fn publicly_usable(key: &StoredKey, catalog: &[RouteChoice]) -> bool {
    !key.is_owner()
        && !key.permissions().iter().any(|scope| {
            catalog
                .iter()
                .any(|route| route.selector == *scope && route.exposure == Exposure::Never)
        })
        && key.permissions().iter().any(|scope| {
            catalog
                .iter()
                .any(|route| route.selector == *scope && route.exposure == Exposure::Public)
        })
}

fn parse_quota(
    requested: Option<DailyQuota>,
    clear: bool,
    current: Option<DailyQuota>,
    config: &ValidatedConfig,
) -> Result<Option<DailyQuota>, String> {
    if clear {
        if requested.is_some() {
            return Err("Set a daily quota or clear it, not both".into());
        }
        return Ok(None);
    }
    let quota = requested.or(current);
    if let Some(quota) = quota {
        quota
            .validate()
            .map_err(|class| format!("Invalid daily quota: {class}"))?;
        if !matches!(
            config.key_source(),
            KeySource::File {
                usage_dir: Some(_),
                ..
            }
        ) {
            return Err(
                "Daily quotas require [keys] usage_dir in the gateway configuration".into(),
            );
        }
    }
    Ok(quota)
}
