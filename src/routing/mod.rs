pub(crate) mod admission;
pub(crate) mod breaker;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{ProviderKind, ValidatedAdapter, ValidatedConfig, ValidatedRoute};
use crate::core::{Capabilities, ExtensionKey, RouteIdentity, RouteSelector, TrustZone};
use url::Url;

#[derive(Clone, Debug)]
pub struct RouteEntry {
    pub identity: RouteIdentity,
    pub adapter_id: String,
    pub provider_kind: ProviderKind,
    pub base_url: Url,
    pub trust_zone: TrustZone,
    pub capabilities: Capabilities,
    pub extension_allowlist: BTreeSet<ExtensionKey>,
    pub adapter_extension_allowlist: BTreeSet<ExtensionKey>,
    pub requires_streaming_chat: bool,
    pub requires_function_tools: bool,
    pub context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub pinned_reasoning_effort: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct Registry {
    routes: BTreeMap<(String, crate::core::Operation), RouteEntry>,
}

impl Registry {
    pub fn from_validated(config: &ValidatedConfig) -> Self {
        let adapters: BTreeMap<_, _> = config
            .adapters()
            .iter()
            .map(|adapter| (adapter.id(), adapter))
            .collect();
        let routes = config
            .routes()
            .iter()
            .map(|route| {
                let adapter = &adapters[route.adapter_id()];
                let key = (
                    route.identity().selector.model_alias.0.clone(),
                    route.identity().selector.operation,
                );
                (key, entry(route, adapter))
            })
            .collect();
        Self { routes }
    }

    pub fn resolve(&self, selector: &RouteSelector) -> Option<&RouteEntry> {
        self.routes
            .get(&(selector.model_alias.0.clone(), selector.operation))
    }

    pub fn routes(&self) -> impl Iterator<Item = &RouteEntry> {
        self.routes.values()
    }
}

fn entry(route: &ValidatedRoute, adapter: &ValidatedAdapter) -> RouteEntry {
    let mut capabilities = adapter.capabilities().clone();
    capabilities.input_audio &= route.allows_input_audio();
    capabilities.audio_streaming_chat &= route.allows_audio_streaming_chat();
    capabilities.audio_function_tools &= route.allows_audio_function_tools();

    RouteEntry {
        identity: route.identity().clone(),
        adapter_id: route.adapter_id().into(),
        provider_kind: adapter.kind(),
        base_url: adapter.base_url().clone(),
        trust_zone: adapter.trust_zone(),
        capabilities,
        extension_allowlist: route.extension_allowlist().clone(),
        adapter_extension_allowlist: adapter.extension_allowlist().clone(),
        requires_streaming_chat: route.requires_streaming_chat(),
        requires_function_tools: route.requires_function_tools(),
        context_tokens: route.context_tokens(),
        max_output_tokens: route.max_output_tokens(),
        pinned_reasoning_effort: route.pinned_reasoning_effort(),
    }
}
