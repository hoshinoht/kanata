pub(crate) mod admission;
pub(crate) mod breaker;
pub(crate) mod explain;
pub(crate) mod probe;
pub(crate) mod uploads;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{ProviderKind, ValidatedAdapter, ValidatedConfig, ValidatedRoute};
use crate::core::{
    Capabilities, ExtensionKey, Operation, ReasoningEffort, RouteIdentity, RouteSelector, TrustZone,
};
use url::Url;

#[derive(Clone, Debug)]
pub struct RouteEntry {
    pub identity: RouteIdentity,
    pub adapter_id: String,
    pub provider_kind: ProviderKind,
    pub base_url: Url,
    pub trust_zone: TrustZone,
    pub capabilities: Capabilities,
    pub speech: Option<crate::core::SpeechPolicy>,
    pub extension_allowlist: BTreeSet<ExtensionKey>,
    pub adapter_extension_allowlist: BTreeSet<ExtensionKey>,
    pub requires_streaming_chat: bool,
    pub requires_function_tools: bool,
    pub context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub pinned_reasoning_effort: Option<&'static str>,
}

impl RouteEntry {
    /// Published model name for a pinned reasoning route.
    pub fn model_family_alias(&self) -> &str {
        let alias = self.identity.selector.model_alias.0.as_str();
        alias
            .rsplit_once(':')
            .filter(|(_, effort)| Some(*effort) == self.pinned_reasoning_effort)
            .map(|(model, _)| model)
            .unwrap_or(alias)
    }
}

pub enum ChatRouteError {
    Forbidden,
    Missing,
    ReasoningEffort,
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

    pub fn resolve_chat(
        &self,
        selector: &RouteSelector,
        effort: Option<ReasoningEffort>,
        authorized: impl Fn(&RouteSelector) -> bool,
    ) -> Result<&RouteEntry, ChatRouteError> {
        let exact = self.resolve(selector);
        let alias = selector.model_alias.0.as_str();
        if let Some(route) = exact.filter(|route| {
            route.capabilities.reasoning_control && route.model_family_alias() != alias
        }) {
            if !authorized(selector) {
                return Err(ChatRouteError::Forbidden);
            }
            if effort.is_some_and(|effort| Some(effort.as_str()) != route.pinned_reasoning_effort) {
                return Err(ChatRouteError::ReasoningEffort);
            }
            return Ok(route);
        }
        let family: Vec<_> = self
            .routes()
            .filter(|route| {
                route.identity.selector.operation == Operation::Chat
                    && route.pinned_reasoning_effort.is_some()
                    && route.capabilities.reasoning_control
                    && route.model_family_alias() == alias
            })
            .collect();
        if family.is_empty() {
            if !authorized(selector) {
                return Err(ChatRouteError::Forbidden);
            }
            return exact.ok_or(ChatRouteError::Missing);
        }
        if !family
            .iter()
            .any(|route| authorized(&route.identity.selector))
        {
            return Err(ChatRouteError::Forbidden);
        }
        let Some(effort) = effort else {
            return exact
                .filter(|_| authorized(selector))
                .ok_or(ChatRouteError::ReasoningEffort);
        };
        if !family[0].provider_kind.accepts_reasoning_effort(effort) {
            return Err(ChatRouteError::ReasoningEffort);
        }
        if !family
            .iter()
            .any(|route| route.pinned_reasoning_effort == Some(effort.as_str()))
        {
            return Err(ChatRouteError::ReasoningEffort);
        }
        if let Some(route) = exact.filter(|route| {
            authorized(selector) && route.pinned_reasoning_effort == Some(effort.as_str())
        }) {
            return Ok(route);
        }
        family
            .into_iter()
            .find(|route| {
                route.pinned_reasoning_effort == Some(effort.as_str())
                    && authorized(&route.identity.selector)
            })
            .ok_or(ChatRouteError::Forbidden)
    }
}

fn entry(route: &ValidatedRoute, adapter: &ValidatedAdapter) -> RouteEntry {
    let mut capabilities = adapter.capabilities().clone();
    capabilities.input_audio &= route.allows_input_audio();
    capabilities.input_images &= route.allows_input_images();
    capabilities.audio_streaming_chat &= route.allows_audio_streaming_chat();
    capabilities.audio_function_tools &= route.allows_audio_function_tools();

    RouteEntry {
        identity: route.identity().clone(),
        adapter_id: route.adapter_id().into(),
        provider_kind: adapter.kind(),
        base_url: adapter.base_url().clone(),
        trust_zone: adapter.trust_zone(),
        capabilities,
        speech: route.speech().cloned(),
        extension_allowlist: route.extension_allowlist().clone(),
        adapter_extension_allowlist: adapter.extension_allowlist().clone(),
        requires_streaming_chat: route.requires_streaming_chat(),
        requires_function_tools: route.requires_function_tools(),
        context_tokens: route.context_tokens(),
        max_output_tokens: route.max_output_tokens(),
        pinned_reasoning_effort: route.pinned_reasoning_effort(),
    }
}
