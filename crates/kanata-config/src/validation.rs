use super::*;

mod adapters;
mod bounds;
mod common;
mod key_file;
mod listeners;
mod publication;
mod routes;

pub(super) use adapters::validate_adapter;
pub(super) use bounds::validate_bounds;
pub(crate) use common::parse_sha256_hex;
pub use common::{parse_model_alias, valid_identifier};
pub(super) use common::{
    valid_diagnostic_path, valid_env_name, valid_path_token, validate_extension_allowlist,
    validate_key_path, validate_optional_limit,
};
pub(super) use key_file::{keys_from_file, load_key_file};
pub(crate) use key_file::{validate_key_limits, validate_permissions};
pub(super) use listeners::validate_listeners;
pub(super) use publication::{validate_public_routes, validate_publication};
pub(super) use routes::validate_route;

pub(super) fn validate(
    raw: RawConfig,
    config_dir: &Path,
    read_files: bool,
) -> Result<ValidatedConfig, ConfigError> {
    let mut publication = validate_publication(&raw.publication)?;
    let listeners = validate_listeners(&raw.listeners, publication.tailnet_addresses())?;
    let (limits, timeouts) = validate_bounds(&raw.limits, &raw.timeouts)?;

    let mut adapter_ids = BTreeSet::new();
    let mut adapters = Vec::with_capacity(raw.adapters.len());
    for (index, adapter) in raw.adapters.into_iter().enumerate() {
        adapters.push(validate_adapter(index, adapter, &mut adapter_ids)?);
    }
    if adapters.is_empty() {
        return Err(ConfigError::new("adapters", ConfigErrorClass::Empty));
    }
    let chatgpt_auth = validate_chatgpt_auth(&adapters, raw.chatgpt_auth)?;

    let mut route_ids = BTreeSet::new();
    let mut selectors = BTreeSet::new();
    let mut routes = Vec::with_capacity(raw.routes.len());
    for (index, route) in raw.routes.into_iter().enumerate() {
        routes.push(validate_route(
            index,
            route,
            &adapters,
            &mut route_ids,
            &mut selectors,
        )?);
    }
    if routes.is_empty() {
        return Err(ConfigError::new("routes", ConfigErrorClass::Empty));
    }

    validate_reasoning_families(&routes, &adapters)?;

    publication.public_routes = validate_public_routes(
        &raw.publication.public_routes,
        listeners.public.is_some(),
        &routes,
        &adapters,
    )?;
    let keys = raw
        .keys
        .ok_or_else(|| ConfigError::new("keys", ConfigErrorClass::Required))?;
    let (keys, key_source) = load_key_file(&keys, config_dir, &routes, read_files)?;
    Ok(ValidatedConfig {
        listeners,
        publication,
        chatgpt_auth,
        adapters,
        routes,
        keys,
        key_source,
        limits,
        timeouts,
        route_sources: Default::default(),
        logging: ValidatedLogging {
            level: raw.logging.level,
            format: raw.logging.format,
        },
    })
}

/// `[chatgpt_auth]`: required with a ChatGPT adapter and rejected without one.
fn validate_chatgpt_auth(
    adapters: &[ValidatedAdapter],
    auth: Option<RawChatgptAuth>,
) -> Result<Option<ValidatedChatgptAuth>, ConfigError> {
    let has_chatgpt_adapter = adapters
        .iter()
        .any(|adapter| adapter.kind == ProviderKind::Chatgpt);
    Ok(match (has_chatgpt_adapter, auth) {
        (true, Some(auth)) => {
            let state_dir = PathBuf::from(&auth.state_dir);
            if auth.state_dir.chars().any(char::is_control)
                || !state_dir.is_absolute()
                || state_dir
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
                || !state_dir
                    .components()
                    .any(|part| matches!(part, std::path::Component::Normal(_)))
            {
                return Err(ConfigError::new(
                    "chatgpt_auth.state_dir",
                    ConfigErrorClass::InvalidAbsolutePath,
                ));
            }
            Some(ValidatedChatgptAuth { state_dir })
        }
        (true, None) => return Err(ConfigError::new("chatgpt_auth", ConfigErrorClass::Required)),
        (false, Some(_)) => {
            return Err(ConfigError::new(
                "chatgpt_auth",
                ConfigErrorClass::WithoutChatgptAdapter,
            ));
        }
        (false, None) => None,
    })
}

/// Pinned reasoning-control variants of one family share adapter, upstream and route policy.
fn validate_reasoning_families(
    routes: &[ValidatedRoute],
    adapters: &[ValidatedAdapter],
) -> Result<(), ConfigError> {
    for (index, route) in routes.iter().enumerate().filter(|(_, route)| {
        route.pinned_reasoning_effort().is_some()
            && adapters.iter().any(|adapter| {
                adapter.id == route.adapter_id && adapter.capabilities.reasoning_control
            })
    }) {
        for other in routes.iter().filter(|other| {
            other.identity.selector.operation == Operation::Chat
                && other.model_family_alias() == route.model_family_alias()
        }) {
            if other.pinned_reasoning_effort().is_none()
                || route.adapter_id != other.adapter_id
                || route.identity.upstream_id != other.identity.upstream_id
                || route.allows_input_images != other.allows_input_images
                || route.extension_allowlist != other.extension_allowlist
                || route.enable_thinking != other.enable_thinking
                || route.allows_input_audio != other.allows_input_audio
                || route.allows_audio_streaming_chat != other.allows_audio_streaming_chat
                || route.allows_audio_function_tools != other.allows_audio_function_tools
                || route.requires_streaming_chat != other.requires_streaming_chat
                || route.requires_function_tools != other.requires_function_tools
                || route.context_tokens != other.context_tokens
                || route.max_output_tokens != other.max_output_tokens
                || route.reasoning_summary() != other.reasoning_summary()
            {
                return Err(ConfigError::new(
                    format!("routes[{index}]"),
                    ConfigErrorClass::InconsistentReasoningFamily,
                ));
            }
        }
    }
    Ok(())
}
