use crate::config::{self, Plane};
use crate::{
    adapter::Adapter,
    auth::EnvironmentSecretResolver,
    core::{ModelAlias, Operation, RouteSelector},
    routing::Registry,
};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, clap::Args)]
pub(crate) struct DoctorArgs {
    /// Configuration file.
    #[arg(long, value_name = "path")]
    config: PathBuf,
    /// Plane to check.
    #[arg(long, value_name = "all|private|public", value_parser = crate::cli::parse_plane, default_value = "all")]
    plane: Plane,
    /// Open a TCP connection to each routed backend.
    #[arg(long)]
    probe_backends: bool,
    /// Check each route's upstream model in the backend catalog.
    #[arg(long)]
    probe_models: bool,
    /// Send one synthetic request to this route alias; needs --operation.
    #[arg(long, value_name = "alias", requires = "operation")]
    probe_inference: Option<String>,
    /// Route operation: chat, transcription, embeddings or speech.
    #[arg(long, value_name = "operation", value_parser = crate::cli::parse_operation, requires = "probe_inference")]
    operation: Option<Operation>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct HealthArgs {
    /// Configuration file.
    #[arg(long, value_name = "path")]
    config: PathBuf,
}

/// `kanata health`: succeeds when the admin listener reports ready.
pub(crate) async fn health(args: HealthArgs) -> Result<String, String> {
    let config = config::load_deferring_keys(&args.config)
        .and_then(|config| config.for_plane(Plane::default()))
        .map_err(|error| error.to_string())?;
    let body = get(admin_address(&config), "/ready").await?;
    if body == "ready\n" {
        Ok("ready".into())
    } else {
        Err("gateway not ready".into())
    }
}

fn admin_address(config: &config::ValidatedConfig) -> SocketAddr {
    let admin = config.listeners().admin();
    SocketAddr::new(admin.bind(), admin.port())
}

pub(crate) async fn doctor(
    args: DoctorArgs,
    build_adapter: impl Fn(&config::ValidatedConfig, &str) -> Result<std::sync::Arc<dyn Adapter>, ()>,
) -> Result<String, String> {
    let DoctorArgs {
        config: path,
        plane,
        probe_backends: probes,
        probe_models: models,
        probe_inference: inference,
        operation,
    } = args;
    let config = config::load_plane(&path, plane)
        .map(|(_, config)| config)
        .map_err(|error| error.to_string())?;
    if models || inference.is_some() {
        crate::telemetry::logging::install(config.logging());
    }
    let registry = Registry::from_validated(&config);
    let inference_route = inference
        .zip(operation)
        .map(|(alias, operation)| {
            registry
                .resolve(&RouteSelector {
                    model_alias: ModelAlias(alias),
                    operation,
                })
                .ok_or("inference probe: exact route is not loaded in this plane")
        })
        .transpose()?;
    let address = admin_address(&config);
    let mut report = vec![format!("config: valid ({} routes)", config.routes().len())];
    report.push(if config.key_source().missing {
        "keys: missing; no keys loaded".into()
    } else {
        format!(
            "keys: readable; permissions checked; {} active records",
            config.keys().len()
        )
    });
    for warning in config.key_warnings(crate::keys::time::now()) {
        report.push(format!("warning: {warning}"));
    }
    match get(address, "/status").await {
        Ok(body) => {
            let value: serde_json::Value =
                serde_json::from_str(&body).map_err(|_| "invalid admin status")?;
            report.push(format!("gateway ready: {}", value["ready"]));
            report.push(format!("key reload: {}", value["key_reload"]));
            if let Some(reload) = value.get("configuration_reload") {
                report.push(format!("configuration reload: {reload}"));
            }
        }
        Err(_) => report.push(
            "gateway: admin status unavailable (check the process and its network namespace)"
                .into(),
        ),
    }
    if probes {
        for adapter in config.adapters().iter().filter(|adapter| {
            config
                .routes()
                .iter()
                .any(|route| route.adapter_id() == adapter.id())
        }) {
            let url = adapter.base_url();
            let host = url.host_str().ok_or("backend host unavailable")?;
            let port = url
                .port_or_known_default()
                .ok_or("backend port unavailable")?;
            let reachable = matches!(
                tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect((host, port))).await,
                Ok(Ok(_))
            );
            report.push(format!(
                "backend {}: TCP {} (TLS, credentials and inference untested)",
                adapter.id(),
                if reachable {
                    "reachable"
                } else {
                    "unreachable"
                }
            ));
        }
    }
    if models {
        for adapter in config.adapters().iter().filter(|adapter| {
            registry
                .routes()
                .any(|route| route.adapter_id == adapter.id())
        }) {
            let catalog = crate::adapter::probe::models(
                adapter,
                config.timeouts(),
                &EnvironmentSecretResolver,
            )
            .await;
            for route in registry
                .routes()
                .filter(|route| route.adapter_id == adapter.id())
            {
                let status = match &catalog {
                    Ok(ids) if ids.contains(&route.identity.upstream_id) => "listed",
                    Ok(_) => "not_listed",
                    Err(status) => status,
                };
                report.push(format!(
                    "model {}/{}: {status} (catalog only; inference untested)",
                    route.identity.selector.model_alias.0,
                    route.identity.selector.operation.as_str()
                ));
            }
        }
    }
    if let Some(route) = inference_route {
        let status = match build_adapter(&config, &route.adapter_id) {
            Ok(adapter) => crate::routing::probe::inference(route, adapter.as_ref()).await,
            Err(()) => Err("adapter_or_credentials_unavailable"),
        };
        report.push(format!(
            "inference {}/{}: {} {}",
            route.identity.selector.model_alias.0,
            route.identity.selector.operation.as_str(),
            if status.is_ok() { "verified" } else { "failed" },
            status.unwrap_or_else(|status| status)
        ));
        report.push("probe: synthetic input; 15-second deadline; bypasses client key admission and usage accounting; tools, images and other capabilities untested".into());
    }
    Ok(report.join("\n"))
}

async fn get(address: SocketAddr, path: &'static str) -> Result<String, String> {
    if !address.ip().is_loopback() {
        return Err("admin address must be loopback".into());
    }
    tokio::time::timeout(TIMEOUT, async {
        let mut socket = tokio::net::TcpStream::connect(address)
            .await
            .map_err(|_| "admin unavailable")?;
        socket
            .write_all(
                format!("GET {path} HTTP/1.0\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .map_err(|_| "admin write failed")?;
        let mut bytes = Vec::new();
        socket
            .take(65_537)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "admin read failed")?;
        if bytes.len() > 65_536 {
            return Err("admin response too large");
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| "invalid admin response")?;
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or("invalid admin response")?;
        if !matches!(
            head.lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1)),
            Some("200")
        ) {
            return Err("gateway not ready");
        }
        Ok(body.to_owned())
    })
    .await
    .map_err(|_| "admin timeout".to_owned())?
    .map_err(str::to_owned)
}
