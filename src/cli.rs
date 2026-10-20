use std::path::PathBuf;

use crate::{
    adapter::{Adapter, ollama::OllamaAdapter, openrouter::OpenRouterAdapter, vllm::VllmAdapter},
    auth::EnvironmentSecretResolver,
    config::{self, ProviderKind, ValidatedConfig},
    keys::cli::{Exposure, RouteChoice},
};

const USAGE: &str = "usage: kanata config {compact,expand,plan} ... | kanata portal --config <path> [--port <port>] | kanata doctor --config <path> [--plane all|private|public] [--probe-backends] [--probe-models] [--probe-inference <alias> --operation <operation>] | kanata health --config <path> | kanata check --config <path> [--plane all|private|public] | kanata serve --config <path> [--plane all|private|public] | kanata auth chatgpt {login,status,logout,models} --config <path> [--profile <name>] | kanata key {new,list,show,edit,rm,rotate} ... (see `kanata key`) | kanata routes --config <path> [--json] | kanata routes explain --config <path> --model <alias> --operation <operation> --key-id <id> [--plane all|private|public] [--json]";

pub fn run(arguments: impl IntoIterator<Item = String>) -> Result<Option<String>, String> {
    let arguments: Vec<_> = arguments.into_iter().collect();
    if arguments.is_empty() {
        return Ok(None);
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "check")
    {
        let (path, plane) = parse_config_plane(&arguments[1..]).ok_or_else(|| USAGE.to_owned())?;
        let (_, config) = config::load_plane(path, plane).map_err(|error| error.to_string())?;
        for warning in config.key_warnings(crate::keys::time::now()) {
            eprintln!("warning: {warning}");
        }
        return Ok(Some("configuration valid".into()));
    }
    match arguments[0].as_str() {
        "config" => config::commands::run(&arguments[1..]).map(Some),
        "key" => crate::keys::cli::run(&arguments[1..], route_choices).map(Some),
        "routes" => run_routes(&arguments[1..]).map(Some),
        _ => Err(USAGE.into()),
    }
}

/// `--config <path> [--plane all|private|public]`.
fn parse_config_plane(arguments: &[String]) -> Option<(PathBuf, config::Plane)> {
    match arguments {
        [flag, path] if flag == "--config" => Some((PathBuf::from(path), config::Plane::All)),
        [flag, path, plane_flag, plane] if flag == "--config" && plane_flag == "--plane" => {
            Some((PathBuf::from(path), config::Plane::parse(plane)?))
        }
        _ => None,
    }
}

/// Private-only kinds (ChatGPT, CLIProxyAPI, Anthropic) are never public; others are public when
/// listed in `publication.public_routes`.
fn route_exposure(config: &ValidatedConfig, route: &config::ValidatedRoute) -> Exposure {
    let selector = &route.identity().selector;
    if route_kind(config, route).is_some_and(ProviderKind::is_private_only) {
        Exposure::Never
    } else if config.publication().public_routes().contains(selector) {
        Exposure::Public
    } else {
        Exposure::Private
    }
}

fn route_kind(config: &ValidatedConfig, route: &config::ValidatedRoute) -> Option<ProviderKind> {
    config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == route.adapter_id())
        .map(|adapter| adapter.kind())
}

fn route_choices(config: &ValidatedConfig) -> Vec<RouteChoice> {
    config
        .routes()
        .iter()
        .map(|route| RouteChoice {
            selector: route.identity().selector.clone(),
            exposure: route_exposure(config, route),
        })
        .collect()
}

/// `kanata routes`: grantable aliases only; never upstream ids, URLs or secrets.
fn run_routes(arguments: &[String]) -> Result<String, String> {
    if arguments
        .first()
        .is_some_and(|argument| argument == "explain")
    {
        return crate::routing::explain::run(&arguments[1..]);
    }
    let (path, json) = match arguments {
        [flag, path] if flag == "--config" => (path, false),
        [flag, path, json] if flag == "--config" && json == "--json" => (path, true),
        _ => return Err("usage: kanata routes --config <path> [--json]".into()),
    };
    let config = config::load(path).map_err(|error| error.to_string())?;
    let rows: Vec<[String; 5]> = config
        .routes()
        .iter()
        .map(|route| {
            let selector = &route.identity().selector;
            [
                selector.model_alias.0.clone(),
                selector.operation.as_str().into(),
                route_kind(&config, route)
                    .map_or("-", ProviderKind::label)
                    .into(),
                match route_exposure(&config, route) {
                    Exposure::Public => "yes",
                    Exposure::Private => "no",
                    Exposure::Never => "never",
                }
                .into(),
                if route.allows_input_audio() {
                    "audio input"
                } else {
                    ""
                }
                .into(),
            ]
        })
        .collect();
    if json {
        let array: Vec<_> = rows
            .iter()
            .map(|[alias, operation, backend, public, notes]| {
                serde_json::json!({
                    "alias": alias,
                    "operation": operation,
                    "backend": backend,
                    "public": public,
                    "notes": notes,
                })
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&array).expect("json serializes"));
    }
    let header = ["ALIAS", "OPERATION", "BACKEND", "PUBLIC", "NOTES"].map(str::to_owned);
    let mut widths = [0; 5];
    for row in std::iter::once(&header).chain(&rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    Ok(std::iter::once(&header)
        .chain(&rows)
        .map(|row| {
            row.iter()
                .zip(widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

pub async fn run_async(
    arguments: impl IntoIterator<Item = String>,
) -> Result<Option<String>, String> {
    run_async_with_output(arguments, |message| println!("{message}")).await
}

async fn run_async_with_output<F>(
    arguments: impl IntoIterator<Item = String>,
    mut output: F,
) -> Result<Option<String>, String>
where
    F: FnMut(String),
{
    let arguments: Vec<_> = arguments.into_iter().collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == "doctor" || argument == "health")
    {
        return crate::diagnostics::run(&arguments, |config, id| {
            build_adapter(config, id, &EnvironmentSecretResolver)
        })
        .await
        .map(Some);
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "portal")
    {
        return crate::portal::run(&arguments[1..], route_choices, &mut output)
            .await
            .map(Some);
    }
    if arguments.first().is_some_and(|argument| argument == "auth")
        && arguments
            .get(1)
            .is_some_and(|argument| argument == "chatgpt")
    {
        return run_chatgpt_auth(&arguments[2..], &mut output)
            .await
            .map(Some);
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "serve")
    {
        let (path, plane) = parse_config_plane(&arguments[1..]).ok_or_else(|| USAGE.to_owned())?;
        crate::serve::run(path, plane, build_serve_adapters).await?;
        return Ok(Some("server stopped".into()));
    }
    run(arguments)
}

fn build_serve_adapters(
    config: &ValidatedConfig,
    resolver: &EnvironmentSecretResolver,
) -> Result<Vec<std::sync::Arc<dyn Adapter>>, ()> {
    let mut adapters: Vec<std::sync::Arc<dyn Adapter>> = Vec::new();
    for configured in config.adapters() {
        if !config
            .routes()
            .iter()
            .any(|route| route.adapter_id() == configured.id())
        {
            continue;
        }
        adapters.push(build_adapter(config, configured.id(), resolver)?);
    }
    Ok(adapters)
}

fn build_adapter(
    config: &ValidatedConfig,
    id: &str,
    resolver: &EnvironmentSecretResolver,
) -> Result<std::sync::Arc<dyn Adapter>, ()> {
    let configured = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == id)
        .ok_or(())?;
    let route = config
        .routes()
        .iter()
        .find(|route| route.adapter_id() == id)
        .ok_or(())?;
    let adapter: std::sync::Arc<dyn Adapter> = match configured.kind() {
        ProviderKind::Ollama | ProviderKind::AppleFm => std::sync::Arc::new(
            OllamaAdapter::new(configured, config.timeouts(), config.limits()).map_err(|_| ())?,
        ),
        // CLIProxyAPI speaks the same OpenAI chat wire as vLLM.
        ProviderKind::Vllm | ProviderKind::Cliproxy => std::sync::Arc::new(
            VllmAdapter::from_config_with_secrets(
                config,
                configured.id(),
                &route.identity().route_id,
                resolver,
            )
            .map_err(|_| ())?,
        ),
        ProviderKind::Openrouter => std::sync::Arc::new(
            OpenRouterAdapter::from_config(
                config,
                configured.id(),
                &route.identity().route_id,
                resolver,
            )
            .map_err(|_| ())?,
        ),
        ProviderKind::Speech => std::sync::Arc::new(
            crate::adapter::speech::SpeechAdapter::from_config(config, configured.id(), resolver)
                .map_err(|_| ())?,
        ),
        ProviderKind::Chatgpt => std::sync::Arc::new(
            crate::adapter::chatgpt::ChatgptAdapter::from_config(config, configured.id())
                .map_err(|_| ())?,
        ),
        ProviderKind::Anthropic => std::sync::Arc::new(
            crate::adapter::anthropic::AnthropicAdapter::from_config(
                config,
                configured.id(),
                resolver,
            )
            .map_err(|_| ())?,
        ),
    };
    Ok(adapter)
}
async fn run_chatgpt_auth<F: FnMut(String)>(
    arguments: &[String],
    output: &mut F,
) -> Result<String, String> {
    let usage = "usage: kanata auth chatgpt {login,status,logout,models} --config <path> [--profile <name>]";
    let (action, path, profile) = match arguments {
        [action, flag, path] if flag == "--config" => (action.as_str(), path, None),
        [action, flag, path, profile_flag, profile]
            if flag == "--config"
                && profile_flag == "--profile"
                && matches!(action.as_str(), "login" | "logout") =>
        {
            (action.as_str(), path, Some(profile.as_str()))
        }
        _ => return Err(usage.into()),
    };
    if !matches!(action, "login" | "status" | "logout" | "models") {
        return Err(usage.into());
    }
    let config = config::load(path).map_err(|error| error.to_string())?;
    if action == "models" {
        return serde_json::to_string_pretty(&crate::adapter::chatgpt::models(&config).await?)
            .map_err(|_| "model catalog could not be formatted".into());
    }
    let settings = config
        .chatgpt_auth()
        .ok_or("ChatGPT authentication is not configured")?;
    let manager = crate::adapter::chatgpt::auth::AuthManager::new(settings, config.timeouts())
        .map_err(|error| error.to_string())?;
    match action {
        "login" => {
            let flow = manager
                .begin_login(profile.unwrap_or("default"))
                .await
                .map_err(|error| error.to_string())?;
            output(format!(
                "Continue with ChatGPT: open this URL in a browser on this host.\n{}",
                flow.authorization_url()
            ));
            let status = flow
                .finish(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_string_pretty(&status)
                .map_err(|_| "sign-in result could not be formatted".into())
        }
        "status" => serde_json::to_string_pretty(
            &manager.status().await.map_err(|error| error.to_string())?,
        )
        .map_err(|_| "sign-in status could not be formatted".into()),
        "logout" => serde_json::to_string_pretty(
            &manager
                .logout(profile)
                .await
                .map_err(|error| error.to_string())?,
        )
        .map_err(|_| "sign-out result could not be formatted".into()),
        _ => Err(usage.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::run;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn synchronous_check_validates_config() {
        assert_eq!(
            run(args(&[
                "check",
                "--config",
                "tests/fixtures/config/example.toml"
            ])),
            Ok(Some("configuration valid".into()))
        );
    }

    #[test]
    fn check_and_serve_accept_only_known_planes() {
        let config = "tests/fixtures/config/example.toml";
        assert_eq!(
            run(args(&["check", "--config", config, "--plane", "private"])),
            Ok(Some("configuration valid".into()))
        );
        assert_eq!(
            run(args(&["check", "--config", config, "--plane", "public"])),
            Err("config error at listeners.public: required_for_public_plane".into())
        );
        for bad in [
            &["check", "--config", config, "--plane", "bogus"][..],
            &["check", "--config", config, "--plane"][..],
            &["check", "--plane", "public", "--config", config][..],
        ] {
            assert!(run(args(bad)).unwrap_err().starts_with("usage: kanata"));
        }
        assert!(
            super::parse_config_plane(&args(&["--config", config, "--plane", "bogus"])).is_none()
        );
    }
}
