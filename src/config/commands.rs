use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::catalog::{Document, default_id, directory, table};
use super::*;

const USAGE: &str = "usage: kanata config {compact,expand} --config <path> --output <new-path> | kanata config plan --config <candidate> --against <current>";

pub(crate) fn run(arguments: &[String]) -> Result<String, String> {
    match arguments {
        [action, flag, path, output_flag, output]
            if matches!(action.as_str(), "compact" | "expand")
                && flag == "--config"
                && output_flag == "--output" =>
        {
            export(Path::new(path), Path::new(output), action == "compact")
                .map_err(|e| e.to_string())
        }
        [action, flag, path, against_flag, against]
            if action == "plan" && flag == "--config" && against_flag == "--against" =>
        {
            plan(Path::new(path), Path::new(against)).map_err(|e| e.to_string())
        }
        _ => Err(USAGE.into()),
    }
}

fn absolute(path: &Path) -> Result<PathBuf, ConfigError> {
    fs::canonicalize(path).map_err(|_| ConfigError::new("config", "read_error"))
}

fn export(path: &Path, output: &Path, compact: bool) -> Result<String, ConfigError> {
    let path = absolute(path)?;
    let document = Document::load(&path)?;
    let before = document.validate(&path, true)?;
    let output_dir = absolute(directory(output))?;
    let output = output_dir.join(
        output
            .file_name()
            .ok_or_else(|| ConfigError::new("output", "invalid_path"))?,
    );
    if output.symlink_metadata().is_ok() {
        return Err(ConfigError::new("output", "already_exists"));
    }
    let mut raw = document.raw;
    if compact {
        compact_routes(&mut raw, &before)?;
    }
    if output_dir != directory(&path)
        && let Some(keys) = &mut raw.keys
    {
        keys.file = directory(&path)
            .join(&keys.file)
            .to_str()
            .ok_or_else(|| ConfigError::new("keys.file", "invalid_path"))?
            .to_owned();
        if let Some(usage) = &mut keys.usage_dir {
            *usage = directory(&path)
                .join(&*usage)
                .to_str()
                .ok_or_else(|| ConfigError::new("keys.usage_dir", "invalid_path"))?
                .to_owned();
        }
    }
    let contents = render(&raw, compact)?;
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).map_err(|_| ConfigError::new("output", "random_error"))?;
    let name: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let temporary = Temporary(output_dir.join(format!(".kanata-config-{name}.tmp")));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary.0)
        .map_err(|_| ConfigError::new("output", "write_error"))?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| ConfigError::new("output", "write_error"))?;
    let after = Document::load(&temporary.0)?.validate(&temporary.0, true)?;
    if canonical(before) != canonical(after) {
        return Err(ConfigError::new("output", "not_equivalent"));
    }
    fs::hard_link(&temporary.0, &output).map_err(|error| {
        ConfigError::new(
            "output",
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                "already_exists"
            } else {
                "write_error"
            },
        )
    })?;
    Ok(format!(
        "configuration written; exact routes, IDs, policies and key grants verified ({})",
        output.display()
    ))
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn canonical(mut config: ValidatedConfig) -> ValidatedConfig {
    config.route_sources.clear();
    config.adapters.sort_by(|a, b| a.id.cmp(&b.id));
    config
        .routes
        .sort_by(|a, b| a.identity.route_id.cmp(&b.identity.route_id));
    config.application_keys.sort_by(|a, b| a.id.cmp(&b.id));
    let selector = |a: &RouteSelector, b: &RouteSelector| {
        (&a.model_alias, a.operation).cmp(&(&b.model_alias, b.operation))
    };
    config.publication.public_routes.sort_by(selector);
    for key in &mut config.application_keys {
        key.permissions.sort_by(selector);
    }
    config
}

fn model_from_route(route: &RawRoute) -> Result<toml::Table, ConfigError> {
    let mut model = table(route)?;
    model.remove("model_alias");
    model.remove("operation");
    if route.id == default_id(&route.model_alias, route.operation) {
        model.remove("id");
    }
    Ok(model)
}

fn family_policy(route: &RawRoute) -> Result<toml::Table, ConfigError> {
    let mut policy = table(route)?;
    for field in [
        "id",
        "model_alias",
        "reasoning_effort",
        "codex_reasoning_effort",
    ] {
        policy.remove(field);
    }
    Ok(policy)
}

fn compact_routes(raw: &mut RawConfig, config: &ValidatedConfig) -> Result<(), ConfigError> {
    let mut consumed = BTreeSet::new();
    let mut entries = BTreeMap::new();
    for route in raw
        .routes
        .iter()
        .filter(|route| !route.model_alias.contains(':'))
        .chain(
            raw.routes
                .iter()
                .filter(|route| route.model_alias.contains(':')),
        )
    {
        if consumed.contains(&route.id) {
            continue;
        }
        let validated = config
            .routes
            .iter()
            .find(|r| r.identity.route_id == route.id)
            .expect("validated route");
        let mut model = model_from_route(route)?;
        if route.operation == Operation::Chat
            && !route.model_alias.contains(':')
            && let Some(default) = validated.pinned_reasoning_effort()
        {
            let policy = family_policy(route)?;
            let variants: Vec<_> = raw
                .routes
                .iter()
                .filter(|other| {
                    other.operation == Operation::Chat
                        && other
                            .model_alias
                            .starts_with(&format!("{}:", route.model_alias))
                })
                .collect();
            if !variants.is_empty()
                && variants
                    .iter()
                    .all(|other| family_policy(other).is_ok_and(|other| other == policy))
            {
                model.remove("reasoning_effort");
                model.remove("codex_reasoning_effort");
                model.insert("default_effort".into(), default.into());
                let mut efforts = Vec::new();
                let mut ids = toml::Table::new();
                for variant in variants {
                    let (_, effort) = variant.model_alias.split_once(':').expect("effort alias");
                    efforts.push(toml::Value::String(effort.into()));
                    if variant.id != format!("{}-{effort}", route.id) {
                        ids.insert(effort.into(), variant.id.clone().into());
                    }
                    consumed.insert(variant.id.clone());
                }
                model.insert("efforts".into(), toml::Value::Array(efforts));
                if !ids.is_empty() {
                    model.insert("route_ids".into(), ids.into());
                }
            }
        }
        consumed.insert(route.id.clone());
        entries.insert((route.operation, route.model_alias.clone()), model);
    }
    let mut groups: BTreeMap<(Operation, String), Vec<(Operation, String)>> = BTreeMap::new();
    for ((operation, alias), model) in &entries {
        let adapter = model["adapter_id"].as_str().expect("validated adapter");
        groups
            .entry((*operation, adapter.into()))
            .or_default()
            .push((*operation, alias.clone()));
    }
    for ((operation, adapter), models) in groups {
        if models.len() < 2 {
            continue;
        }
        let name = format!("{adapter}-{}", operation.as_str());
        let mut profile = toml::Table::new();
        let fields: BTreeSet<_> = models
            .iter()
            .flat_map(|key| entries[key].keys().cloned())
            .collect();
        for field in fields {
            if matches!(
                field.as_str(),
                "id" | "upstream_id" | "route_ids" | "reasoning_effort" | "codex_reasoning_effort"
            ) {
                continue;
            }
            let mut counts: BTreeMap<String, (usize, toml::Value)> = BTreeMap::new();
            let mut absent = 0;
            for key in &models {
                match entries[key].get(&field) {
                    Some(value) => {
                        let entry = counts
                            .entry(value.to_string())
                            .or_insert((0, value.clone()));
                        entry.0 += 1;
                    }
                    None => absent += 1,
                }
            }
            if let Some((count, value)) = counts.into_values().max_by_key(|(count, _)| *count)
                && count >= 2
                && count > absent
                && (field == "enable_thinking" || value != toml::Value::Boolean(false))
                && !value.as_array().is_some_and(Vec::is_empty)
            {
                profile.insert(field, value);
            }
        }
        for key in models {
            let model = entries.get_mut(&key).expect("model entry");
            let mut unset = Vec::new();
            for (field, value) in &profile {
                match model.get(field) {
                    Some(current) if current == value => {
                        model.remove(field);
                    }
                    None => unset.push(toml::Value::String(field.clone())),
                    _ => {}
                }
            }
            model.insert("profile".into(), name.clone().into());
            if !unset.is_empty() {
                model.insert("unset".into(), unset.into());
            }
        }
        raw.route_profiles.insert(name, deserialize(&profile)?);
    }
    for ((operation, alias), mut model) in entries {
        let profile = model
            .get("profile")
            .and_then(toml::Value::as_str)
            .map(|name| table(&raw.route_profiles[name]))
            .transpose()?
            .unwrap_or_default();
        model.retain(|field, value| {
            if field != "enable_thinking" && value == &toml::Value::Boolean(false) {
                return profile.get(field) == Some(&toml::Value::Boolean(true));
            }
            if value.as_array().is_some_and(Vec::is_empty) {
                return profile
                    .get(field)
                    .and_then(toml::Value::as_array)
                    .is_some_and(|values| !values.is_empty());
            }
            true
        });
        raw.models
            .operation_mut(operation)
            .insert(alias, deserialize(&model)?);
    }
    raw.routes.clear();
    Ok(())
}

fn deserialize<T: serde::de::DeserializeOwned>(table: &toml::Table) -> Result<T, ConfigError> {
    parse_toml(
        &toml::to_string(table).map_err(|_| ConfigError::new("config", "serialization_error"))?,
        "config",
    )
}

fn render(raw: &RawConfig, compact: bool) -> Result<String, ConfigError> {
    let mut root = table(raw)?;
    if compact {
        root.remove("models");
    }
    if raw.routes.is_empty() {
        root.remove("routes");
    }
    if raw.application_keys.is_empty() {
        root.remove("application_keys");
    }
    let mut output = toml::to_string_pretty(&root)
        .map_err(|_| ConfigError::new("config", "serialization_error"))?;
    if compact {
        for (operation, models) in raw.models.entries() {
            if models.is_empty() {
                continue;
            }
            output.push_str(&format!("\n[models.{}]\n", operation.as_str()));
            for (alias, model) in models {
                let fields = table(model)?
                    .into_iter()
                    .map(|(field, value)| format!("{field} = {value}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let alias = toml::Value::String(alias.clone());
                output.push_str(&format!("{alias} = {{ {fields} }}\n"));
            }
        }
    }
    Ok(output)
}

fn selector(route: &ValidatedRoute) -> (String, Operation) {
    (
        route.identity.selector.model_alias.0.clone(),
        route.identity.selector.operation,
    )
}

fn plan(candidate: &Path, current: &Path) -> Result<String, ConfigError> {
    let current = absolute(current)?;
    let candidate = absolute(candidate)?;
    let before = Document::load(&current)?.validate(&current, true)?;
    let after = Document::load(&candidate)?.validate(&candidate, false)?;
    let old: BTreeMap<_, _> = before.routes.iter().map(|r| (selector(r), r)).collect();
    let new: BTreeMap<_, _> = after.routes.iter().map(|r| (selector(r), r)).collect();
    let mut changes = Vec::new();
    for key in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        let (alias, operation) = key;
        let label = format!("{} {alias}", operation.as_str());
        match (old.get(key), new.get(key)) {
            (None, Some(route)) => changes.push(format!(
                "+ route {label} [{}]",
                after.route_sources[&route.identity.route_id]
            )),
            (Some(route), None) => changes.push(format!(
                "- route {label} [{}]",
                before.route_sources[&route.identity.route_id]
            )),
            (Some(previous), Some(next)) if previous != next => {
                changes.push(format!(
                    "~ route {label}: {} [{}]",
                    route_changes(previous, next).join(", "),
                    after.route_sources[&next.identity.route_id]
                ));
            }
            _ => {}
        }
    }
    let old_adapters: BTreeMap<_, _> = before.adapters.iter().map(|a| (&a.id, a)).collect();
    let new_adapters: BTreeMap<_, _> = after.adapters.iter().map(|a| (&a.id, a)).collect();
    for id in old_adapters
        .keys()
        .chain(new_adapters.keys())
        .collect::<BTreeSet<_>>()
    {
        match (old_adapters.get(id), new_adapters.get(id)) {
            (None, Some(_)) => changes.push(format!("+ adapter {id}")),
            (Some(_), None) => changes.push(format!("- adapter {id}")),
            (Some(previous), Some(next)) if previous != next => changes.push(format!(
                "~ adapter {id}: {}",
                adapter_changes(previous, next).join(", ")
            )),
            _ => {}
        }
    }
    let public = |config: &ValidatedConfig| {
        config
            .publication
            .public_routes
            .iter()
            .map(|s| (s.model_alias.0.clone(), s.operation))
            .collect::<BTreeSet<_>>()
    };
    let old_public = public(&before);
    let new_public = public(&after);
    for (alias, operation) in new_public.difference(&old_public) {
        changes.push(format!("+ public {} {alias}", operation.as_str()));
    }
    for (alias, operation) in old_public.difference(&new_public) {
        changes.push(format!("- public {} {alias}", operation.as_str()));
    }
    if before.publication.tailnet_addresses != after.publication.tailnet_addresses {
        changes.push("~ publication.tailnet_addresses".into());
    }
    let previous_keys = key_views(&before)?;
    let next_keys = key_views(&after)?;
    for id in previous_keys
        .keys()
        .chain(next_keys.keys())
        .collect::<BTreeSet<_>>()
    {
        match (previous_keys.get(id), next_keys.get(id)) {
            (None, Some(_)) => changes.push(format!("+ key {id}")),
            (Some(_), None) => changes.push(format!("- key {id}")),
            (Some(previous), Some(next)) if previous != next => {
                changes.push(format!("~ key {id}: grants, limits or credentials changed"))
            }
            _ => {}
        }
    }
    for (id, key) in &next_keys {
        for permission in key["permissions"].as_array().expect("permissions") {
            let alias = permission["model_alias"].as_str().expect("alias");
            let operation = permission["operation"].as_str().expect("operation");
            if !new.contains_key(&(
                alias.to_owned(),
                Operation::parse(operation).expect("operation"),
            )) {
                changes.push(format!(
                    "! key {id}: grant targets missing route {operation} {alias}"
                ));
            }
        }
    }
    let restart = before.restart_fields(&after);
    for field in restart {
        changes.push(format!("! {field}: restart required"));
    }
    if changes.is_empty() {
        return Ok("no configuration changes".into());
    }
    Ok(changes.join("\n"))
}

fn route_changes(a: &ValidatedRoute, b: &ValidatedRoute) -> Vec<&'static str> {
    let mut fields = Vec::new();
    macro_rules! changed {
        ($($field:ident),* $(,)?) => { $(if a.$field != b.$field { fields.push(stringify!($field)); })* };
    }
    if a.identity.route_id != b.identity.route_id {
        fields.push("id");
    }
    if a.identity.upstream_id != b.identity.upstream_id {
        fields.push("upstream_id");
    }
    changed!(
        adapter_id,
        reasoning_effort,
        codex_reasoning_effort,
        reasoning_summary,
        codex_reasoning_summary,
        extension_allowlist,
        requires_streaming_chat,
        requires_function_tools,
        allows_input_audio,
        allows_input_images,
        speech,
        allows_audio_streaming_chat,
        allows_audio_function_tools,
        context_tokens,
        max_output_tokens,
        enable_thinking
    );
    fields
}

fn adapter_changes(a: &ValidatedAdapter, b: &ValidatedAdapter) -> Vec<&'static str> {
    let mut fields = Vec::new();
    macro_rules! changed {
        ($($field:ident),* $(,)?) => { $(if a.$field != b.$field { fields.push(stringify!($field)); })* };
    }
    changed!(
        kind,
        base_url,
        trust_zone,
        secret_ref,
        transcription_mode,
        extension_allowlist,
        capabilities,
        max_in_flight,
        circuit_breaker
    );
    fields
}

fn key_views(config: &ValidatedConfig) -> Result<BTreeMap<String, serde_json::Value>, ConfigError> {
    let mut keys = BTreeMap::new();
    if let KeySource::File {
        path, usage_dir, ..
    } = &config.key_source
    {
        if let Some(bytes) = keys::file::read(path)? {
            let file = keys::file::parse_without_routes(&bytes)?;
            for key in file.active() {
                if usage_dir.is_none() && key.daily_quota().is_some() {
                    return Err(ConfigError::new(
                        "keys.usage_dir",
                        "required_for_daily_quota",
                    ));
                }
                keys.insert(key.id().to_owned(), serde_json::json!({
                    "permissions": key.permissions(), "owner": key.is_owner(), "digest": key.digest(),
                    "expires_at": key.expires_at(), "daily_quota": key.daily_quota(),
                    "max_in_flight": key.max_in_flight(),
                    "rate_limit": key.rate_limit().map(|rate| (rate.requests, rate.per_ms)),
                }));
            }
        }
    } else {
        for key in &config.application_keys {
            keys.insert(key.id.clone(), serde_json::json!({
                "permissions": key.permissions, "owner": key.owner,
                "reference": format!("{:?}", key.secret_ref), "max_in_flight": key.max_in_flight,
                "rate_limit": key.rate_limit.map(|rate| (rate.requests, rate.per_ms)),
            }));
        }
    }
    for key in keys.values_mut() {
        key["permissions"]
            .as_array_mut()
            .expect("permissions")
            .sort_by_key(|permission| permission.to_string());
    }
    Ok(keys)
}
