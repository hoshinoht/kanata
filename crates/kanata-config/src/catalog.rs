use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};

use super::*;

const MAX_FRAGMENTS: usize = 64;
const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const META_FIELDS: [&str; 7] = [
    "profile",
    "id",
    "route_ids",
    "default_effort",
    "efforts",
    "unset",
    "upstream_id",
];
const CLEARABLE: [&str; 8] = [
    "reasoning_effort",
    "reasoning_summary",
    "context_tokens",
    "max_output_tokens",
    "enable_thinking",
    "default_effort",
    "efforts",
    "adapter_id",
];

pub(super) struct Document {
    pub(super) raw: RawConfig,
    sources: Vec<String>,
    profiles: BTreeMap<String, String>,
}

impl Document {
    pub(super) fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = read(path, "config")?;
        let mut raw: RawConfig = parse_toml(&contents, "config")?;
        if raw.include.len() > MAX_FRAGMENTS {
            return Err(ConfigError::new(
                "include",
                ConfigErrorClass::TooManyFragments,
            ));
        }
        let mut sources: Vec<_> = (0..raw.routes.len())
            .map(|i| format!("routes[{i}]"))
            .collect();
        let mut model_sources = BTreeMap::new();
        let mut profiles = BTreeMap::new();
        let total = contents.len();
        let mut snapshots = vec![(path.to_owned(), contents)];
        if !raw.include.is_empty() {
            include_fragments(
                path,
                &mut raw,
                &mut sources,
                &mut model_sources,
                &mut snapshots,
                total,
            )?;
        }
        validate_profiles(&raw.route_profiles)?;
        for (operation, models) in raw.models.entries() {
            for (index, (alias, model)) in models.iter().enumerate() {
                let mut source = format!("models.{}[{index}]", operation.as_str());
                if parse_model_alias(alias).is_none() {
                    return Err(ConfigError::new(
                        source,
                        ConfigErrorClass::InvalidExactAlias,
                    ));
                }
                source = format!("models.{}.{}", operation.as_str(), alias);
                if let Some(fragment) = model_sources.get(&(operation, alias.clone())) {
                    source = format!("{fragment}.{source}");
                }
                let routes = expand(model, alias, operation, &raw, &source)?;
                if let Some(profile) = &model.profile {
                    for route in &routes {
                        profiles.insert(route.id.clone(), profile.clone());
                    }
                }
                sources.extend(std::iter::repeat_n(source, routes.len()));
                raw.routes.extend(routes);
            }
        }
        raw.include.clear();
        raw.route_profiles.clear();
        raw.models = RawModels::default();
        if snapshots.len() > 1 {
            for (file, contents) in snapshots {
                if read(&file, "include")? != contents {
                    return Err(ConfigError::new(
                        "include",
                        ConfigErrorClass::ChangedDuringRead,
                    ));
                }
            }
        }
        Ok(Self {
            raw,
            sources,
            profiles,
        })
    }

    pub(super) fn validate(
        &self,
        path: &Path,
        read_files: bool,
    ) -> Result<ValidatedConfig, ConfigError> {
        let config = self.validate_deferring_ca(path, read_files)?;
        if read_files {
            config.load_ca_bundles()
        } else {
            Ok(config)
        }
    }

    /// Validates with adapter `ca_file`s path-checked only; `read_keys` reads a `[keys] file`.
    pub(super) fn validate_deferring_ca(
        &self,
        path: &Path,
        read_keys: bool,
    ) -> Result<ValidatedConfig, ConfigError> {
        let mut config = validate(self.raw.clone(), directory(path), read_keys)
            .map_err(|error| self.remap(error))?;
        config.route_sources = self
            .raw
            .routes
            .iter()
            .zip(&self.sources)
            .map(|(route, source)| {
                let source = self.profiles.get(&route.id).map_or_else(
                    || source.clone(),
                    |profile| format!("{source}; profile {profile}"),
                );
                (route.id.clone(), source)
            })
            .collect();
        Ok(config)
    }

    fn remap(&self, mut error: ConfigError) -> ConfigError {
        if let Some(tail) = error.path.strip_prefix("routes[")
            && let Some((index, suffix)) = tail.split_once(']')
            && let Ok(index) = index.parse::<usize>()
            && let Some(source) = self.sources.get(index)
        {
            error.path = format!("{source}{suffix}");
        }
        error
    }
}

pub(super) fn directory(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn read(path: &Path, source: &str) -> Result<String, ConfigError> {
    use std::io::Read as _;
    let file =
        fs::File::open(path).map_err(|_| ConfigError::new(source, ConfigErrorClass::ReadError))?;
    let mut bytes = Vec::new();
    file.take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ConfigError::new(source, ConfigErrorClass::ReadError))?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(ConfigError::new(source, ConfigErrorClass::DocumentTooLarge));
    }
    String::from_utf8(bytes).map_err(|_| ConfigError::new(source, ConfigErrorClass::ReadError))
}

pub(super) fn table<T: Serialize>(value: &T) -> Result<toml::Table, ConfigError> {
    toml::Table::try_from(value)
        .map_err(|_| ConfigError::new("config", ConfigErrorClass::SerializationError))
}

fn expand(
    model: &RawModel,
    alias: &str,
    operation: Operation,
    raw: &RawConfig,
    source: &str,
) -> Result<Vec<RawRoute>, ConfigError> {
    let (mut settings, effective) = resolve_settings(model, raw, source)?;
    let upstream = effective.upstream_id.as_ref().ok_or_else(|| {
        ConfigError::new(format!("{source}.upstream_id"), ConfigErrorClass::Required)
    })?;
    let adapter = effective
        .adapter_id
        .as_ref()
        .and_then(|id| raw.adapters.iter().find(|adapter| &adapter.id == id))
        .ok_or_else(|| {
            ConfigError::new(
                format!("{source}.adapter_id"),
                ConfigErrorClass::MissingAdapter,
            )
        })?;
    let id = effective
        .id
        .clone()
        .unwrap_or_else(|| default_id(alias, operation));
    let family = effective.efforts.is_some() || effective.default_effort.is_some();
    let variants = effort_variants(&effective, adapter, alias, operation, family, source)?;
    if effective
        .route_ids
        .keys()
        .any(|key| !variants.iter().any(|(variant, _, _)| variant == key))
    {
        return Err(ConfigError::new(
            format!("{source}.route_ids"),
            ConfigErrorClass::UnknownVariant,
        ));
    }
    for field in META_FIELDS {
        settings.remove(field);
    }
    settings.insert("operation".into(), operation.as_str().into());
    settings.insert("upstream_id".into(), upstream.clone().into());
    settings
        .entry("requires_streaming_chat")
        .or_insert(false.into());
    settings
        .entry("requires_function_tools")
        .or_insert(false.into());
    variants
        .into_iter()
        .map(|(variant, alias, effort)| {
            let mut route = settings.clone();
            let route_id = effective
                .route_ids
                .get(&variant)
                .cloned()
                .unwrap_or_else(|| {
                    if variant == "base" {
                        id.clone()
                    } else {
                        format!("{id}-{variant}")
                    }
                });
            route.insert("id".into(), route_id.into());
            route.insert("model_alias".into(), alias.into());
            if family && let Some(effort) = effort {
                route.insert("reasoning_effort".into(), effort.as_str().into());
            }
            parse_toml(
                &toml::to_string(&route)
                    .map_err(|_| ConfigError::new(source, ConfigErrorClass::SerializationError))?,
                source,
            )
        })
        .collect()
}

/// Profile settings minus `unset` fields, with the model's overrides applied.
fn resolve_settings(
    model: &RawModel,
    raw: &RawConfig,
    source: &str,
) -> Result<(toml::Table, RawModel), ConfigError> {
    let mut settings = match &model.profile {
        Some(name) => table(raw.route_profiles.get(name).ok_or_else(|| {
            ConfigError::new(
                format!("{source}.profile"),
                ConfigErrorClass::MissingProfile,
            )
        })?)?,
        None => toml::Table::new(),
    };
    let mut cleared = BTreeSet::new();
    for field in &model.unset {
        if !CLEARABLE.contains(&field.as_str()) {
            return Err(ConfigError::new(
                format!("{source}.unset"),
                ConfigErrorClass::InvalidClearField,
            ));
        }
        if !cleared.insert(field) {
            return Err(ConfigError::new(
                format!("{source}.unset"),
                ConfigErrorClass::Duplicate,
            ));
        }
        settings.remove(field);
    }
    let overrides = table(model)?;
    if cleared.iter().any(|field| overrides.contains_key(*field)) {
        return Err(ConfigError::new(
            format!("{source}.unset"),
            ConfigErrorClass::ConflictingOverride,
        ));
    }
    settings.extend(overrides);
    let effective: RawModel = parse_toml(
        &toml::to_string(&settings)
            .map_err(|_| ConfigError::new(source, ConfigErrorClass::SerializationError))?,
        source,
    )?;
    Ok((settings, effective))
}

/// `(variant, alias, effort)` for the base route and, in a family, each listed effort.
fn effort_variants(
    effective: &RawModel,
    adapter: &RawAdapter,
    alias: &str,
    operation: Operation,
    family: bool,
    source: &str,
) -> Result<Vec<(String, String, Option<ReasoningEffort>)>, ConfigError> {
    let mut variants = vec![("base".to_owned(), alias.to_owned(), None)];
    if family {
        if operation != Operation::Chat
            || !(adapter.kind.applies_route_effort_pin() || adapter.capabilities.reasoning_control)
            || alias.contains(':')
        {
            return Err(ConfigError::new(
                source,
                ConfigErrorClass::ReasoningChatFamilyRequired,
            ));
        }
        if effective.reasoning_effort.is_some() {
            return Err(ConfigError::new(
                source,
                ConfigErrorClass::ConflictingEffortOptions,
            ));
        }
        let default = effective.default_effort.ok_or_else(|| {
            ConfigError::new(
                format!("{source}.default_effort"),
                ConfigErrorClass::Required,
            )
        })?;
        variants[0].2 = Some(default);
        if let Some(efforts) = &effective.efforts {
            if efforts.is_empty() {
                return Err(ConfigError::new(
                    format!("{source}.efforts"),
                    ConfigErrorClass::Empty,
                ));
            }
            let mut seen = BTreeSet::new();
            for effort in efforts {
                if !adapter.kind.accepts_reasoning_effort(*effort) {
                    return Err(ConfigError::new(
                        format!("{source}.efforts"),
                        ConfigErrorClass::UnsupportedEffort,
                    ));
                }
                if !seen.insert(effort.as_str()) {
                    return Err(ConfigError::new(
                        format!("{source}.efforts"),
                        ConfigErrorClass::Duplicate,
                    ));
                }
                variants.push((
                    effort.as_str().to_owned(),
                    format!("{alias}:{}", effort.as_str()),
                    Some(*effort),
                ));
            }
        }
    }
    Ok(variants)
}

/// Merges every `include` fragment into `raw`, recording route and model sources and the
/// file snapshots that must stay unchanged until the load completes.
fn include_fragments(
    path: &Path,
    raw: &mut RawConfig,
    sources: &mut Vec<String>,
    model_sources: &mut BTreeMap<(Operation, String), String>,
    snapshots: &mut Vec<(std::path::PathBuf, String)>,
    mut total: usize,
) -> Result<(), ConfigError> {
    let root = fs::canonicalize(directory(path))
        .map_err(|_| ConfigError::new("include", ConfigErrorClass::ReadError))?;
    let mut seen = BTreeSet::from([fs::canonicalize(path)
        .map_err(|_| ConfigError::new("config", ConfigErrorClass::ReadError))?]);
    for (index, name) in raw.include.clone().into_iter().enumerate() {
        let source = format!("include[{index}]");
        let fragment = Path::new(&name);
        if name.is_empty()
            || fragment
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(ConfigError::new(
                source,
                ConfigErrorClass::RelativeFragmentRequired,
            ));
        }
        let fragment = fs::canonicalize(root.join(fragment))
            .map_err(|_| ConfigError::new(&source, ConfigErrorClass::ReadError))?;
        if !fragment.starts_with(&root) {
            return Err(ConfigError::new(
                source,
                ConfigErrorClass::OutsideConfigDirectory,
            ));
        }
        if !seen.insert(fragment.clone()) {
            return Err(ConfigError::new(
                source,
                ConfigErrorClass::DuplicateFragment,
            ));
        }
        let contents = read(&fragment, &source)?;
        total += contents.len();
        if total > MAX_DOCUMENT_BYTES {
            return Err(ConfigError::new(
                "include",
                ConfigErrorClass::DocumentTooLarge,
            ));
        }
        let addition: RawFragment = parse_toml(&contents, &source)?;
        sources.extend((0..addition.routes.len()).map(|i| format!("{source}.routes[{i}]")));
        raw.adapters.extend(addition.adapters);
        raw.routes.extend(addition.routes);
        for (name, profile) in addition.route_profiles {
            if raw.route_profiles.insert(name, profile).is_some() {
                return Err(ConfigError::new(
                    format!("{source}.route_profiles"),
                    ConfigErrorClass::Duplicate,
                ));
            }
        }
        for (operation, models) in addition.models.entries() {
            for (alias, model) in models {
                if raw
                    .models
                    .operation_mut(operation)
                    .insert(alias.clone(), model.clone())
                    .is_some()
                {
                    return Err(ConfigError::new(
                        format!("{source}.models.{}", operation.as_str()),
                        ConfigErrorClass::DuplicateSelector,
                    ));
                }
                model_sources.insert((operation, alias.clone()), source.clone());
            }
        }
        snapshots.push((fragment, contents));
    }
    Ok(())
}

/// Profiles carry shared policy only, never per-model fields.
fn validate_profiles(profiles: &BTreeMap<String, RawModel>) -> Result<(), ConfigError> {
    for (index, (name, profile)) in profiles.iter().enumerate() {
        let source = format!("route_profiles[{index}]");
        if !valid_identifier(name) {
            return Err(ConfigError::new(source, ConfigErrorClass::InvalidProfile));
        }
        if profile.profile.is_some()
            || profile.id.is_some()
            || !profile.route_ids.is_empty()
            || profile.upstream_id.is_some()
            || !profile.unset.is_empty()
        {
            return Err(ConfigError::new(source, ConfigErrorClass::ModelOnlyField));
        }
    }
    Ok(())
}

pub(super) fn default_id(alias: &str, operation: Operation) -> String {
    let alias = alias.replace(':', "-");
    if operation == Operation::Chat {
        alias
    } else {
        format!("{alias}-{}", operation.as_str())
    }
}
