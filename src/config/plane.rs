use super::*;

impl ValidatedConfig {
    /// Narrows the config to what one `kanata serve --plane` process needs.
    pub fn for_plane(&self, plane: Plane) -> Result<Self, ConfigError> {
        let mut config = self.clone();
        match plane {
            Plane::All => {}
            Plane::Private => {
                config.listeners.public = None;
                config.publication.public_routes.clear();
            }
            Plane::Public => {
                if config.listeners.public.is_none() {
                    return Err(ConfigError::new(
                        "listeners.public",
                        "required_for_public_plane",
                    ));
                }
                let public = config.publication.public_routes.clone();
                let codex_selectors: Vec<RouteSelector> = config
                    .routes
                    .iter()
                    .filter(|route| {
                        config.adapters.iter().any(|adapter| {
                            adapter.id == route.adapter_id && adapter.kind == ProviderKind::Codex
                        })
                    })
                    .map(|route| route.identity.selector.clone())
                    .collect();
                // Container loopback only: the public process serves no private clients.
                config.listeners.client.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
                config
                    .routes
                    .retain(|route| public.contains(&route.identity.selector));
                let adapter_ids: BTreeSet<String> = config
                    .routes
                    .iter()
                    .map(|route| route.adapter_id.clone())
                    .collect();
                config
                    .adapters
                    .retain(|adapter| adapter_ids.contains(&adapter.id));
                config.codex_auth = None;
                config.application_keys = config
                    .application_keys
                    .into_iter()
                    // Keys that can reach Codex never enter the public process.
                    .filter(|key| {
                        !key.owner
                            && key
                                .permissions
                                .iter()
                                .all(|selector| !codex_selectors.contains(selector))
                    })
                    .filter_map(|mut key| {
                        key.permissions.retain(|selector| public.contains(selector));
                        (!key.permissions.is_empty()).then_some(key)
                    })
                    .collect();
            }
        }
        Ok(config)
    }
}
