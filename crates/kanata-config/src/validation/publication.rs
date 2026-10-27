use super::*;

pub(crate) fn validate_publication(
    publication: &RawPublication,
) -> Result<ValidatedPublication, ConfigError> {
    if publication.tailnet_addresses.is_empty() {
        return Err(ConfigError::new(
            "publication.tailnet_addresses",
            ConfigErrorClass::Empty,
        ));
    }
    let mut addresses = BTreeSet::new();
    for (index, value) in publication.tailnet_addresses.iter().enumerate() {
        let address = value.parse::<IpAddr>().map_err(|_| {
            ConfigError::new(
                format!("publication.tailnet_addresses[{index}]"),
                ConfigErrorClass::InvalidIp,
            )
        })?;
        let valid = match address {
            IpAddr::V4(ip) => ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]),
            IpAddr::V6(ip) => ip.octets()[..6] == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0],
        };
        if !valid {
            return Err(ConfigError::new(
                format!("publication.tailnet_addresses[{index}]"),
                ConfigErrorClass::NotTailnetAddress,
            ));
        }
        if !addresses.insert(address) {
            return Err(ConfigError::new(
                format!("publication.tailnet_addresses[{index}]"),
                ConfigErrorClass::Duplicate,
            ));
        }
    }
    Ok(ValidatedPublication {
        tailnet_addresses: addresses.into_iter().collect(),
        public_routes: Vec::new(),
    })
}

pub(crate) fn validate_public_routes(
    selectors: &[RawPermission],
    public_listener_configured: bool,
    routes: &[ValidatedRoute],
    adapters: &[ValidatedAdapter],
) -> Result<Vec<RouteSelector>, ConfigError> {
    let mut seen = BTreeSet::new();
    let mut validated = Vec::with_capacity(selectors.len());
    for (index, selector) in selectors.iter().enumerate() {
        let path = format!("publication.public_routes[{index}]");
        if !public_listener_configured {
            return Err(ConfigError::new(
                path,
                ConfigErrorClass::PublicListenerRequired,
            ));
        }
        if !seen.insert((selector.model_alias.clone(), selector.operation)) {
            return Err(ConfigError::new(path, ConfigErrorClass::Duplicate));
        }
        let Some(route) = routes.iter().find(|route| {
            route.identity.selector.model_alias.0 == selector.model_alias
                && route.identity.selector.operation == selector.operation
        }) else {
            return Err(ConfigError::new(
                path,
                ConfigErrorClass::UnknownRouteSelector,
            ));
        };
        let Some(adapter) = adapters
            .iter()
            .find(|adapter| adapter.id == route.adapter_id)
        else {
            return Err(ConfigError::new(path, ConfigErrorClass::MissingAdapter));
        };
        if !adapter
            .capabilities
            .operations
            .contains(&selector.operation)
        {
            return Err(ConfigError::new(
                path,
                ConfigErrorClass::UnsupportedByAdapter,
            ));
        }
        if adapter.kind.is_private_only() {
            return Err(ConfigError::new(
                path,
                match adapter.kind {
                    ProviderKind::Cliproxy => ConfigErrorClass::CliproxyNotPublic,
                    ProviderKind::Anthropic => ConfigErrorClass::AnthropicNotPublic,
                    _ => ConfigErrorClass::ChatgptNotPublic,
                },
            ));
        }
        validated.push(RouteSelector {
            model_alias: ModelAlias(selector.model_alias.clone()),
            operation: selector.operation,
        });
    }
    Ok(validated)
}
