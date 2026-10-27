use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, error::ErrorKind};

use crate::{
    adapter::{Adapter, ollama::OllamaAdapter, openrouter::OpenRouterAdapter, vllm::VllmAdapter},
    auth::EnvironmentSecretResolver,
    config::{self, ProviderKind, ValidatedConfig},
    core::Operation,
    keys::cli::{Exposure, RouteChoice},
};

/// `kanata` command line.
#[derive(Debug, Parser)]
#[command(name = "kanata", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate a configuration file.
    Check(PlaneArgs),
    /// Run the gateway listeners.
    Serve(PlaneArgs),
    /// Report configuration, key and gateway state, with optional backend probes.
    Doctor(crate::diagnostics::DoctorArgs),
    /// Exit successfully when the running gateway is ready.
    Health(crate::diagnostics::HealthArgs),
    /// Serve the host-only key portal on loopback.
    Portal(crate::portal::PortalArgs),
    /// Manage provider sign-in.
    Auth {
        #[command(subcommand)]
        provider: AuthProvider,
    },
    /// Manage client keys.
    Key {
        #[command(subcommand)]
        command: Box<crate::keys::cli::KeyCommand>,
    },
    /// Rewrite or compare configuration files.
    Config {
        #[command(subcommand)]
        command: config::commands::ConfigCommand,
    },
    /// List grantable routes, or explain one route decision.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Routes(RoutesArgs),
}

/// `--config <path> [--plane all|private|public]`.
#[derive(Debug, Args)]
struct PlaneArgs {
    /// Configuration file.
    #[arg(long, value_name = "path")]
    config: PathBuf,
    /// Listeners to validate or run.
    #[arg(long, value_name = "all|private|public", value_parser = parse_plane, default_value = "all")]
    plane: config::Plane,
}

#[derive(Debug, Subcommand)]
enum AuthProvider {
    /// ChatGPT account sign-in.
    Chatgpt {
        #[command(subcommand)]
        action: ChatgptAction,
    },
}

#[derive(Debug, Subcommand)]
enum ChatgptAction {
    /// Sign in through the browser.
    Login(ProfileArgs),
    /// Show the stored sign-in state.
    Status(ConfigArgs),
    /// Remove stored credentials.
    Logout(ProfileArgs),
    /// Print the account's model catalog.
    Models(ConfigArgs),
}

#[derive(Debug, Args)]
struct ConfigArgs {
    /// Configuration file.
    #[arg(long, value_name = "path")]
    config: PathBuf,
}

#[derive(Debug, Args)]
struct ProfileArgs {
    /// Configuration file.
    #[arg(long, value_name = "path")]
    config: PathBuf,
    /// Credential profile.
    #[arg(long, value_name = "name")]
    profile: Option<String>,
}

#[derive(Debug, Args)]
struct RoutesArgs {
    #[command(subcommand)]
    command: Option<RoutesCommand>,
    /// Configuration file.
    #[arg(long, value_name = "path", required = true)]
    config: Option<PathBuf>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum RoutesCommand {
    /// Explain whether a key may use a route, from configuration only.
    Explain(crate::routing::explain::ExplainArgs),
}

/// Parsed arguments, or text to print without running a command.
enum Parsed {
    Exit(Option<String>),
    Run(Command),
}

/// No arguments prints name and version; help and version text are output, not errors.
fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let arguments: Vec<_> = arguments.into_iter().collect();
    if arguments.is_empty() {
        return Ok(Parsed::Exit(None));
    }
    match Cli::try_parse_from(std::iter::once(crate::NAME.to_owned()).chain(arguments)) {
        Ok(cli) => Ok(Parsed::Run(cli.command)),
        Err(error) => {
            let text = error.render().to_string().trim_end().to_owned();
            match error.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => Ok(Parsed::Exit(Some(text))),
                _ => Err(text),
            }
        }
    }
}

/// `all`, `private` or `public`.
pub(crate) fn parse_plane(value: &str) -> Result<config::Plane, String> {
    config::Plane::parse(value).ok_or_else(|| "expected all, private or public".into())
}

/// `chat`, `transcription`, `embeddings` or `speech`.
pub(crate) fn parse_operation(value: &str) -> Result<Operation, String> {
    Operation::parse(value)
        .ok_or_else(|| "expected chat, transcription, embeddings or speech".into())
}

pub fn run(arguments: impl IntoIterator<Item = String>) -> Result<Option<String>, String> {
    match parse(arguments)? {
        Parsed::Exit(text) => Ok(text),
        Parsed::Run(command) => run_sync(command),
    }
}

fn run_sync(command: Command) -> Result<Option<String>, String> {
    match command {
        Command::Check(PlaneArgs {
            config: path,
            plane,
        }) => {
            let (_, config) = config::load_plane(path, plane).map_err(|error| error.to_string())?;
            for warning in config.key_warnings(crate::keys::time::now()) {
                eprintln!("warning: {warning}");
            }
            Ok(Some("configuration valid".into()))
        }
        Command::Config { command } => config::commands::run(command).map(Some),
        Command::Key { command } => crate::keys::cli::run(*command, route_choices).map(Some),
        Command::Routes(RoutesArgs {
            command: Some(RoutesCommand::Explain(args)),
            ..
        }) => crate::routing::explain::run(args).map(Some),
        Command::Routes(RoutesArgs { config, json, .. }) => {
            let path = config.ok_or("kanata routes requires --config <path>")?;
            run_routes(&path, json).map(Some)
        }
        Command::Serve(_)
        | Command::Doctor(_)
        | Command::Health(_)
        | Command::Portal(_)
        | Command::Auth { .. } => Err("this command needs the async runtime".into()),
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
fn run_routes(path: &Path, json: bool) -> Result<String, String> {
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
    let command = match parse(arguments)? {
        Parsed::Exit(text) => return Ok(text),
        Parsed::Run(command) => command,
    };
    match command {
        Command::Doctor(args) => crate::diagnostics::doctor(args, |config, id| {
            build_adapter(config, id, &EnvironmentSecretResolver)
        })
        .await
        .map(Some),
        Command::Health(args) => crate::diagnostics::health(args).await.map(Some),
        Command::Portal(args) => crate::portal::run(args, route_choices, &mut output)
            .await
            .map(Some),
        Command::Auth {
            provider: AuthProvider::Chatgpt { action },
        } => run_chatgpt_auth(action, &mut output).await.map(Some),
        Command::Serve(PlaneArgs {
            config: path,
            plane,
        }) => {
            crate::serve::run(path, plane, build_serve_adapters).await?;
            Ok(Some("server stopped".into()))
        }
        command => run_sync(command),
    }
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
    action: ChatgptAction,
    output: &mut F,
) -> Result<String, String> {
    let (path, profile) = match &action {
        ChatgptAction::Login(args) | ChatgptAction::Logout(args) => {
            (&args.config, args.profile.as_deref())
        }
        ChatgptAction::Status(args) | ChatgptAction::Models(args) => (&args.config, None),
    };
    let config = config::load(path).map_err(|error| error.to_string())?;
    let manager = || {
        let settings = config
            .chatgpt_auth()
            .ok_or("ChatGPT authentication is not configured")?;
        crate::adapter::chatgpt::auth::AuthManager::new(settings, config.timeouts())
            .map_err(|error| error.to_string())
    };
    match action {
        ChatgptAction::Models(_) => {
            serde_json::to_string_pretty(&crate::adapter::chatgpt::models(&config).await?)
                .map_err(|_| "model catalog could not be formatted".into())
        }
        ChatgptAction::Login(_) => {
            let flow = manager()?
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
        ChatgptAction::Status(_) => serde_json::to_string_pretty(
            &manager()?
                .status()
                .await
                .map_err(|error| error.to_string())?,
        )
        .map_err(|_| "sign-in status could not be formatted".into()),
        ChatgptAction::Logout(_) => serde_json::to_string_pretty(
            &manager()?
                .logout(profile)
                .await
                .map_err(|error| error.to_string())?,
        )
        .map_err(|_| "sign-out result could not be formatted".into()),
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
        assert_eq!(
            run(args(&["check", "--plane", "public", "--config", config])),
            Err("config error at listeners.public: required_for_public_plane".into())
        );
        for (bad, message) in [
            (
                &["check", "--config", config, "--plane", "bogus"][..],
                "expected all, private or public",
            ),
            (
                &["check", "--config", config, "--plane"][..],
                "a value is required",
            ),
            (
                &["check", "--config", config, "--bogus"][..],
                "unexpected argument",
            ),
            (&["check"][..], "--config <path>"),
        ] {
            let error = run(args(bad)).unwrap_err();
            assert!(
                error.starts_with("error: ") && error.contains(message),
                "{error}"
            );
        }
    }

    #[test]
    fn help_and_version_are_output_not_errors() {
        let help = run(args(&["key", "new", "--help"])).unwrap().unwrap();
        assert!(help.contains("Usage: kanata key new"), "{help}");
        assert_eq!(
            run(args(&["--version"])),
            Ok(Some(format!("{} {}", crate::NAME, crate::VERSION)))
        );
        assert_eq!(run(Vec::new()), Ok(None));
    }
}
