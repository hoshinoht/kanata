use super::*;

pub(in crate::config) fn validate_listeners(
    listeners: &RawListeners,
    tailnet_addresses: &[IpAddr],
) -> Result<ValidatedListeners, ConfigError> {
    let admin = listeners
        .admin
        .bind
        .parse::<IpAddr>()
        .map_err(|_| ConfigError::new("listeners.admin.bind", "invalid_ip"))?;
    if !admin.is_loopback() {
        return Err(ConfigError::new("listeners.admin.bind", "not_loopback"));
    }
    if listeners.admin.port == 0 {
        return Err(ConfigError::new("listeners.admin.port", "zero"));
    }
    if listeners.client.port == 0 {
        return Err(ConfigError::new("listeners.client.port", "zero"));
    }
    if listeners.client.port == listeners.admin.port {
        return Err(ConfigError::new("listeners", "client_admin_port_collision"));
    }
    let client = listeners
        .client
        .bind
        .parse::<IpAddr>()
        .map_err(|_| ConfigError::new("listeners.client.bind", "invalid_ip"))?;
    if !client.is_unspecified()
        && !client.is_loopback()
        && !is_private_public_bind(client)
        && !tailnet_addresses.contains(&client)
    {
        return Err(ConfigError::new(
            "listeners.client.bind",
            "not_internal_address",
        ));
    }
    let public = if let Some(public) = &listeners.public {
        if public.port == 0 {
            return Err(ConfigError::new("listeners.public.port", "zero"));
        }
        let bind = public
            .bind
            .parse::<IpAddr>()
            .map_err(|_| ConfigError::new("listeners.public.bind", "invalid_ip"))?;
        if bind.is_loopback() || bind.is_unspecified() {
            return Err(ConfigError::new("listeners.public.bind", "not_concrete"));
        }
        if !is_private_public_bind(bind) {
            return Err(ConfigError::new(
                "listeners.public.bind",
                "not_internal_address",
            ));
        }
        if shares_configured_tailnet_prefix(bind, tailnet_addresses) {
            return Err(ConfigError::new(
                "listeners.public.bind",
                "tailnet_address_forbidden",
            ));
        }
        if listener_bindings_conflict(client, listeners.client.port, bind, public.port) {
            return Err(ConfigError::new(
                "listeners.public",
                "client_port_collision",
            ));
        }
        if bind == client {
            return Err(ConfigError::new(
                "listeners.public",
                "client_bind_collision",
            ));
        }
        Some(ValidatedListener {
            bind,
            port: public.port,
        })
    } else {
        None
    };
    Ok(ValidatedListeners {
        client: ValidatedListener {
            bind: client,
            port: listeners.client.port,
        },
        admin: ValidatedListener {
            bind: admin,
            port: listeners.admin.port,
        },
        public,
    })
}

fn is_private_public_bind(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private(),
        IpAddr::V6(address) => address.is_unique_local(),
    }
}

fn shares_configured_tailnet_prefix(address: IpAddr, tailnet_addresses: &[IpAddr]) -> bool {
    match address {
        IpAddr::V4(_) => false,
        IpAddr::V6(address) => tailnet_addresses.iter().any(|tailnet| match tailnet {
            IpAddr::V4(_) => false,
            IpAddr::V6(tailnet) => address.octets()[..6] == tailnet.octets()[..6],
        }),
    }
}

fn listener_bindings_conflict(
    first_bind: IpAddr,
    first_port: u16,
    second_bind: IpAddr,
    second_port: u16,
) -> bool {
    first_port == second_port
        && (first_bind == second_bind
            || first_bind.is_unspecified()
            || second_bind.is_unspecified())
}
