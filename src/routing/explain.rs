use serde_json::{Value, json};

use crate::{
    config::{self, Plane, ValidatedConfig},
    core::{ModelAlias, Operation, RouteSelector},
};

const USAGE: &str = "usage: kanata routes explain --config <path> --model <alias> --operation chat|transcription|embeddings|speech --key-id <id> [--plane all|private|public] [--json]";

pub(crate) fn run(arguments: &[String]) -> Result<String, String> {
    let mut path = None;
    let mut model = None;
    let mut operation = None;
    let mut key = None;
    let mut plane = None;
    let mut json_output = false;
    let mut flags = arguments.iter();
    while let Some(flag) = flags.next() {
        match flag.as_str() {
            "--config" if path.is_none() => path = Some(flags.next().ok_or(USAGE)?),
            "--model" if model.is_none() => model = Some(flags.next().ok_or(USAGE)?),
            "--operation" if operation.is_none() => {
                operation = Some(Operation::parse(flags.next().ok_or(USAGE)?).ok_or(USAGE)?)
            }
            "--key-id" if key.is_none() => key = Some(flags.next().ok_or(USAGE)?),
            "--plane" if plane.is_none() => {
                plane = Some(Plane::parse(flags.next().ok_or(USAGE)?).ok_or(USAGE)?)
            }
            "--json" if !json_output => json_output = true,
            _ => return Err(USAGE.into()),
        }
    }
    let model = model.ok_or(USAGE)?;
    let key = key.ok_or(USAGE)?;
    if config::parse_model_alias(model).is_none() || !config::valid_identifier(key) {
        return Err("invalid model alias or key id".into());
    }
    let selector = RouteSelector {
        model_alias: ModelAlias(model.clone()),
        operation: operation.ok_or(USAGE)?,
    };
    let full = config::load(path.ok_or(USAGE)?).map_err(|error| error.to_string())?;
    let plane = plane.unwrap_or(Plane::All);
    let effective = full.for_plane(plane).map_err(|error| error.to_string())?;
    let registry = super::Registry::from_validated(&effective);
    let route = registry.resolve(&selector);
    let configured = full
        .routes()
        .iter()
        .any(|route| route.identity().selector == selector);
    let private = decision(&effective, &selector, key, configured, false);
    let public = decision(&effective, &selector, key, configured, true);
    let report = json!({
        "source": "configuration_snapshot",
        "plane": plane.as_str(),
        "selector": selector,
        "key_id": key,
        "configured": configured,
        "loaded": route.is_some(),
        "route": route.map(|route| json!({
            "id": route.identity.route_id,
            "adapter_id": route.adapter_id,
            "provider": route.provider_kind.label(),
            "trust_zone": route.trust_zone,
            "declared_capabilities": route.capabilities,
            "max_output_tokens": route.max_output_tokens,
        })),
        "private": private,
        "public": public,
        "backend_verification": "not_probed",
    });
    if json_output {
        return serde_json::to_string_pretty(&report)
            .map_err(|_| "could not render route report".into());
    }
    Ok(format!(
        "configuration snapshot (plane {}): {} / {}\nkey: {}\nroute: {}\nprivate: {}\npublic: {}\nbackend: not probed; runtime reload state and admission are not evaluated",
        plane.as_str(),
        model,
        selector.operation.as_str(),
        key,
        route.map_or("not loaded", |route| route.identity.route_id.as_str()),
        private["reason"].as_str().unwrap_or("unknown"),
        public["reason"].as_str().unwrap_or("unknown"),
    ))
}

fn decision(
    config: &ValidatedConfig,
    selector: &RouteSelector,
    key_id: &str,
    configured: bool,
    public: bool,
) -> Value {
    let key = config
        .application_keys()
        .iter()
        .find(|key| key.id() == key_id);
    let loaded = config
        .routes()
        .iter()
        .any(|route| &route.identity().selector == selector);
    let reason = if public && config.listeners().public().is_none() {
        "listener_disabled"
    } else if !configured {
        "route_not_configured"
    } else if !loaded {
        "route_not_loaded_in_plane"
    } else if let Some(key) = key {
        if key.is_expired(crate::keys::time::now()) {
            "key_expired"
        } else if !key.permissions().contains(selector) {
            "scope_missing"
        } else if public && !config.publication().public_routes().contains(selector) {
            "route_not_public"
        } else {
            "allowed"
        }
    } else {
        "key_not_loaded_or_revoked"
    };
    json!({"allowed":reason == "allowed", "reason":reason})
}
