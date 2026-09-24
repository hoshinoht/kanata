use std::{future, future::Future, path::PathBuf};

use crate::{
    adapter::{
        Adapter,
        codex::CodexAdapter,
        codex::auth::{
            AuthorizationCodeRequest, CodexAuthClient, CredentialStore, DeviceAuthorization,
            DevicePollFailure, DevicePollRequest, DevicePollResponse, LoginError,
            LoginExchangeFailure, LoginTokenResponse, MAX_LOGIN_DURATION, MAX_REFRESH_LOCK_WAIT,
            complete_device_login,
        },
        ollama::OllamaAdapter,
        openrouter::OpenRouterAdapter,
        vllm::VllmAdapter,
    },
    auth::EnvironmentSecretResolver,
    config::{self, ProviderKind, ValidatedConfig},
    core::Operation,
    keys::cli::{Exposure, RouteChoice},
};

const USAGE: &str = "usage: kanata check --config <path> [--plane all|private|public] | kanata serve --config <path> [--plane all|private|public] | kanata auth codex {login,status,logout} --config <path> | kanata key {new,list,show,edit,rm,rotate,migrate} ... (see `kanata key`) | kanata routes --config <path> [--json]";

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
        let config = config::load(path)
            .and_then(|config| config.for_plane(plane))
            .map_err(|error| error.to_string())?;
        for warning in config.key_warnings(crate::keys::time::now()) {
            eprintln!("warning: {warning}");
        }
        return Ok(Some("configuration valid".into()));
    }
    match arguments[0].as_str() {
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

/// Codex routes are never public; others are public when listed in `publication.public_routes`.
fn route_exposure(config: &ValidatedConfig, route: &config::ValidatedRoute) -> Exposure {
    let selector = &route.identity().selector;
    if route_kind(config, route) == Some(ProviderKind::Codex) {
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
                match selector.operation {
                    Operation::Chat => "chat",
                    Operation::Transcription => "transcription",
                }
                .into(),
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
    if arguments.first().is_some_and(|argument| argument == "auth") {
        let (action, path) = parse_auth_command(&arguments)?;
        return run_auth_command(action, path, &mut output).await.map(Some);
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
        let Some(route) = config
            .routes()
            .iter()
            .find(|route| route.adapter_id() == configured.id())
        else {
            continue;
        };
        let adapter: std::sync::Arc<dyn Adapter> = match configured.kind() {
            ProviderKind::Ollama | ProviderKind::AppleFm => std::sync::Arc::new(
                OllamaAdapter::new(configured, config.timeouts(), config.limits())
                    .map_err(|_| ())?,
            ),
            ProviderKind::Vllm => std::sync::Arc::new(
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
            ProviderKind::Codex => std::sync::Arc::new(
                CodexAdapter::from_config(config, configured.id()).map_err(|_| ())?,
            ),
        };
        adapters.push(adapter);
    }
    Ok(adapters)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthAction {
    Login,
    Status,
    Logout,
}

fn parse_auth_command(arguments: &[String]) -> Result<(AuthAction, PathBuf), String> {
    if arguments.len() != 5
        || arguments[0] != "auth"
        || arguments[1] != "codex"
        || arguments[3] != "--config"
    {
        return Err(USAGE.into());
    }

    let action = match arguments[2].as_str() {
        "login" => AuthAction::Login,
        "status" => AuthAction::Status,
        "logout" => AuthAction::Logout,
        _ => return Err(USAGE.into()),
    };
    Ok((action, PathBuf::from(&arguments[4])))
}

async fn run_auth_command<F>(
    action: AuthAction,
    path: PathBuf,
    output: &mut F,
) -> Result<String, String>
where
    F: FnMut(String),
{
    let config = config::load(path).map_err(|error| error.to_string())?;
    if config.codex_auth().is_none() {
        return Err("Codex authentication is not configured".into());
    }

    match action {
        AuthAction::Login => {
            let client = CodexAuthClient::new(config.timeouts())
                .map_err(|_| "Codex authentication transport could not be initialized")?;
            let interrupt = async { tokio::signal::ctrl_c().await.map_err(|_| ()) };
            execute_auth_action(action, &config, Some(&client), output, interrupt).await
        }
        AuthAction::Status | AuthAction::Logout => {
            execute_auth_action::<CodexAuthClient, _, _>(
                action,
                &config,
                None,
                output,
                future::pending(),
            )
            .await
        }
    }
}

trait DeviceLoginTransport {
    async fn request_device_authorization(&self) -> Result<DeviceAuthorization, LoginError>;

    async fn poll_device_authorization(
        &self,
        request: DevicePollRequest,
    ) -> Result<DevicePollResponse, DevicePollFailure>;

    async fn exchange_authorization_code(
        &self,
        request: AuthorizationCodeRequest,
    ) -> Result<LoginTokenResponse, LoginExchangeFailure>;
}

impl DeviceLoginTransport for CodexAuthClient {
    async fn request_device_authorization(&self) -> Result<DeviceAuthorization, LoginError> {
        CodexAuthClient::request_device_authorization(self, future::pending()).await
    }

    async fn poll_device_authorization(
        &self,
        request: DevicePollRequest,
    ) -> Result<DevicePollResponse, DevicePollFailure> {
        CodexAuthClient::poll_device_authorization(self, request).await
    }

    async fn exchange_authorization_code(
        &self,
        request: AuthorizationCodeRequest,
    ) -> Result<LoginTokenResponse, LoginExchangeFailure> {
        CodexAuthClient::exchange_authorization_code(self, request).await
    }
}

async fn execute_auth_action<C, O, I>(
    action: AuthAction,
    config: &ValidatedConfig,
    client: Option<&C>,
    output: &mut O,
    interrupt: I,
) -> Result<String, String>
where
    C: DeviceLoginTransport,
    O: FnMut(String),
    I: Future<Output = Result<(), ()>>,
{
    let auth_config = config
        .codex_auth()
        .ok_or_else(|| "Codex authentication is not configured".to_owned())?;
    let store = CredentialStore::from_config(auth_config);

    match action {
        AuthAction::Login => {
            let client =
                client.ok_or_else(|| "Codex authentication client is unavailable".to_owned())?;
            let login = login_and_store(client, &store, output);
            tokio::select! {
                biased;
                result = tokio::time::timeout(MAX_LOGIN_DURATION, login) => {
                    result.unwrap_or_else(|_| Err(LoginError::DeviceLoginExpired.to_string()))
                }
                signal = interrupt => match signal {
                    Ok(()) => Err(LoginError::Cancelled.to_string()),
                    Err(()) => Err("Codex login could not install an interrupt handler".into()),
                }
            }
        }
        AuthAction::Status => {
            let locked = store
                .lock(MAX_REFRESH_LOCK_WAIT)
                .await
                .map_err(|error| error.to_string())?;
            if locked.load().map_err(|error| error.to_string())?.is_some() {
                Ok("Codex credentials are present in the configured local store.".into())
            } else {
                Ok("No local Codex credentials are configured.".into())
            }
        }
        AuthAction::Logout => {
            let locked = store
                .lock(MAX_REFRESH_LOCK_WAIT)
                .await
                .map_err(|error| error.to_string())?;
            locked.logout().map_err(|error| error.to_string())?;
            Ok("Local Codex credentials removed.".into())
        }
    }
}

async fn login_and_store<C, O>(
    client: &C,
    store: &CredentialStore,
    output: &mut O,
) -> Result<String, String>
where
    C: DeviceLoginTransport,
    O: FnMut(String),
{
    let authorization = client
        .request_device_authorization()
        .await
        .map_err(|error| error.to_string())?;
    output(format!(
        "Verification URL: {}\nOne-time user code: {}",
        authorization.verification_url(),
        authorization.user_code()
    ));

    let credential = complete_device_login(
        authorization,
        |request| client.poll_device_authorization(request),
        |request| client.exchange_authorization_code(request),
        future::pending(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let locked = store
        .lock(MAX_REFRESH_LOCK_WAIT)
        .await
        .map_err(|error| error.to_string())?;
    locked
        .save(&credential)
        .map_err(|error| error.to_string())?;
    Ok("Codex credentials saved locally.".into())
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        rc::Rc,
        sync::{
            Mutex,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
    };

    use crate::adapter::codex::auth::{
        CODEX_DEVICE_TOKEN_ENDPOINT, CODEX_DEVICE_USERCODE_ENDPOINT, CODEX_TOKEN_ENDPOINT,
        DeviceCodeRequest, DevicePollFailure, DevicePollRequest, DevicePollResponse,
        LoginExchangeFailure, LoginTokenResponse,
    };

    use super::{
        AuthAction, CredentialStore, DeviceLoginTransport, execute_auth_action, parse_auth_command,
        run,
    };

    const DEVICE_CODE_RESPONSE: &[u8] =
        include_bytes!("../tests/fixtures/codex_login/device-code-response.json");
    const DEVICE_POLL_SUCCESS: &[u8] =
        include_bytes!("../tests/fixtures/codex_login/device-token-success.json");
    const DEVICE_AUTH_ID: &str = "TEST_ONLY_DEVICE_AUTH_ID_NOT_SECRET";
    const USER_CODE: &str = "TEST-ONLY-7QZX";
    const AUTHORIZATION_CODE: &str = "TEST_ONLY_AUTHORIZATION_CODE_NOT_SECRET";
    const REFRESH_TOKEN: &str = "TEST_ONLY_REFRESH_TOKEN_NOT_SECRET_0001";
    const ACCOUNT_ID: &str = "TEST_ONLY_ACCOUNT_ID_NOT_SECRET_0001";
    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
            let path = root.join(format!(
                "kanata-cli-codex-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create private fixture directory");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .expect("secure fixture directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn file_config(directory: &TestDir) -> crate::config::ValidatedConfig {
        let state_dir = directory.path().join("codex-state");
        fs::create_dir(&state_dir).expect("create owner state directory");
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700))
            .expect("secure owner state directory");
        let template = include_str!("../tests/fixtures/config/example.toml");
        let config_text = template
            .replace("store = \"keyring\"", "store = \"file\"")
            .replace(
                "/replace/with/owner-writable/absolute/path/kanata-codex",
                &state_dir.to_string_lossy(),
            );
        let config_path = directory.path().join("fixture.toml");
        fs::write(&config_path, config_text).expect("write fixture config");
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600))
            .expect("secure fixture config");
        crate::config::load(config_path).expect("validate file-store config")
    }

    #[derive(Default)]
    struct FixtureAuth {
        polls: AtomicUsize,
        exchanges: AtomicUsize,
    }

    impl DeviceLoginTransport for FixtureAuth {
        async fn request_device_authorization(
            &self,
        ) -> Result<super::DeviceAuthorization, super::LoginError> {
            let request = DeviceCodeRequest::new()?;
            assert_eq!(request.endpoint(), CODEX_DEVICE_USERCODE_ENDPOINT);
            request.accept_response(DEVICE_CODE_RESPONSE)
        }

        async fn poll_device_authorization(
            &self,
            request: DevicePollRequest,
        ) -> Result<DevicePollResponse, DevicePollFailure> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(request.endpoint(), CODEX_DEVICE_TOKEN_ENDPOINT);
            Ok(DevicePollResponse::new(200, DEVICE_POLL_SUCCESS.to_vec()))
        }

        async fn exchange_authorization_code(
            &self,
            request: super::AuthorizationCodeRequest,
        ) -> Result<LoginTokenResponse, LoginExchangeFailure> {
            self.exchanges.fetch_add(1, Ordering::Relaxed);
            assert_eq!(request.endpoint(), CODEX_TOKEN_ENDPOINT);
            let form = request.form_body();
            assert!(form.as_str().contains(AUTHORIZATION_CODE));
            LoginTokenResponse::new(REFRESH_TOKEN, ACCOUNT_ID).map_err(|_| LoginExchangeFailure)
        }
    }

    struct PendingAuth {
        poll_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        polls: AtomicUsize,
    }

    impl DeviceLoginTransport for PendingAuth {
        async fn request_device_authorization(
            &self,
        ) -> Result<super::DeviceAuthorization, super::LoginError> {
            let request = DeviceCodeRequest::new()?;
            assert_eq!(request.endpoint(), CODEX_DEVICE_USERCODE_ENDPOINT);
            request.accept_response(DEVICE_CODE_RESPONSE)
        }

        async fn poll_device_authorization(
            &self,
            request: DevicePollRequest,
        ) -> Result<DevicePollResponse, DevicePollFailure> {
            assert_eq!(request.endpoint(), CODEX_DEVICE_TOKEN_ENDPOINT);
            self.polls.fetch_add(1, Ordering::Relaxed);
            if let Some(started) = self.poll_started.lock().expect("poll lock").take() {
                let _ = started.send(());
            }
            std::future::pending().await
        }

        async fn exchange_authorization_code(
            &self,
            _: super::AuthorizationCodeRequest,
        ) -> Result<LoginTokenResponse, LoginExchangeFailure> {
            panic!("cancelled login must not exchange a token")
        }
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn auth_commands_require_the_documented_shape() {
        for (arguments, action) in [
            (
                args(&["auth", "codex", "login", "--config", "kanata.toml"]),
                AuthAction::Login,
            ),
            (
                args(&["auth", "codex", "status", "--config", "kanata.toml"]),
                AuthAction::Status,
            ),
            (
                args(&["auth", "codex", "logout", "--config", "kanata.toml"]),
                AuthAction::Logout,
            ),
        ] {
            let (parsed, path) = parse_auth_command(&arguments).expect("parse command");
            assert_eq!(parsed, action);
            assert_eq!(path, PathBuf::from("kanata.toml"));
        }
        assert!(
            parse_auth_command(&args(&["auth", "codex", "login", "kanata.toml"]))
                .unwrap_err()
                .starts_with("usage: kanata")
        );
    }

    #[test]
    fn synchronous_check_still_validates_config_without_opening_a_store() {
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

    #[tokio::test]
    async fn login_displays_only_the_device_url_and_user_code_then_saves_fixture_credential() {
        let directory = TestDir::new();
        let config = file_config(&directory);
        let auth = FixtureAuth::default();
        let mut displayed = Vec::new();
        let mut output = |message| displayed.push(message);
        let (action, path) = parse_auth_command(&args(&[
            "auth",
            "codex",
            "login",
            "--config",
            "fixture.toml",
        ]))
        .expect("parse login command");
        assert_eq!(path, PathBuf::from("fixture.toml"));

        let result = execute_auth_action(
            action,
            &config,
            Some(&auth),
            &mut output,
            std::future::pending(),
        )
        .await
        .expect("fixture login");

        assert_eq!(
            displayed,
            [format!(
                "Verification URL: {}\nOne-time user code: {USER_CODE}",
                crate::adapter::codex::auth::CODEX_DEVICE_VERIFICATION_URL
            )]
        );
        assert_eq!(result, "Codex credentials saved locally.");
        let user_output = format!("{}\n{result}", displayed.join("\n"));
        for marker in [
            DEVICE_AUTH_ID,
            AUTHORIZATION_CODE,
            REFRESH_TOKEN,
            ACCOUNT_ID,
            "TEST_ONLY_ACCESS_TOKEN_NOT_SECRET",
            "localhost",
            "scope=",
        ] {
            assert!(
                !user_output.contains(marker),
                "unexpected output marker: {marker}"
            );
        }
        assert_eq!(auth.polls.load(Ordering::Relaxed), 1);
        assert_eq!(auth.exchanges.load(Ordering::Relaxed), 1);

        let state_dir = config.codex_auth().expect("Codex config").state_dir();
        assert_eq!(
            fs::metadata(state_dir)
                .expect("state directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let store = CredentialStore::from_config(config.codex_auth().expect("Codex config"));
        let locked = store
            .lock(super::MAX_REFRESH_LOCK_WAIT)
            .await
            .expect("store lock");
        let saved = locked
            .load()
            .expect("stored credential")
            .expect("login saved");
        assert_eq!(saved.refresh_token(), REFRESH_TOKEN);
        assert_eq!(saved.account_id(), ACCOUNT_ID);
        assert!(!format!("{saved:?}").contains(REFRESH_TOKEN));
        assert!(!format!("{saved:?}").contains(ACCOUNT_ID));
        assert_eq!(
            fs::metadata(state_dir.join("credential-v1.json"))
                .expect("credential file")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn status_is_secret_free_and_logout_is_local_idempotent_and_locked() {
        let directory = TestDir::new();
        let config = file_config(&directory);
        let auth = FixtureAuth::default();
        let mut output = |_| {};
        execute_auth_action(
            AuthAction::Login,
            &config,
            Some(&auth),
            &mut output,
            std::future::pending(),
        )
        .await
        .expect("fixture login");

        let status = execute_auth_action::<FixtureAuth, _, _>(
            AuthAction::Status,
            &config,
            None,
            &mut output,
            std::future::pending(),
        )
        .await
        .expect("status");
        assert_eq!(
            status,
            "Codex credentials are present in the configured local store."
        );
        for marker in [REFRESH_TOKEN, ACCOUNT_ID, DEVICE_AUTH_ID] {
            assert!(!status.contains(marker));
        }

        for _ in 0..2 {
            let logout = execute_auth_action::<FixtureAuth, _, _>(
                AuthAction::Logout,
                &config,
                None,
                &mut output,
                std::future::pending(),
            )
            .await
            .expect("local logout");
            assert_eq!(logout, "Local Codex credentials removed.");
        }
        let state_dir = config.codex_auth().expect("Codex config").state_dir();
        assert!(state_dir.join("credential.lock").exists());
        assert!(!state_dir.join("credential-v1.json").exists());
        assert_eq!(auth.polls.load(Ordering::Relaxed), 1);
        assert_eq!(auth.exchanges.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn interrupt_drops_pending_poll_and_does_not_save_late_credentials() {
        let directory = TestDir::new();
        let config = file_config(&directory);
        let (poll_started_tx, poll_started_rx) = tokio::sync::oneshot::channel();
        let auth = PendingAuth {
            poll_started: Mutex::new(Some(poll_started_tx)),
            polls: AtomicUsize::new(0),
        };
        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();
        let displayed = Rc::new(RefCell::new(Vec::new()));
        let output_displayed = Rc::clone(&displayed);
        let mut output = move |message| output_displayed.borrow_mut().push(message);
        let command = execute_auth_action(
            AuthAction::Login,
            &config,
            Some(&auth),
            &mut output,
            async move { interrupt_rx.await.map_err(|_| ()) },
        );
        tokio::pin!(command);
        tokio::select! {
            result = &mut command => panic!("login returned before interrupt: {result:?}"),
            _ = poll_started_rx => {}
        }

        interrupt_tx.send(()).expect("send interrupt");
        assert_eq!(command.await, Err(super::LoginError::Cancelled.to_string()));
        tokio::task::yield_now().await;
        assert_eq!(auth.polls.load(Ordering::Relaxed), 1);
        assert_eq!(
            displayed.borrow().as_slice(),
            &[format!(
                "Verification URL: {}\nOne-time user code: {USER_CODE}",
                crate::adapter::codex::auth::CODEX_DEVICE_VERIFICATION_URL
            )]
        );
        let store = CredentialStore::from_config(config.codex_auth().expect("Codex config"));
        let locked = store
            .lock(super::MAX_REFRESH_LOCK_WAIT)
            .await
            .expect("store lock");
        assert!(locked.load().expect("store read").is_none());
    }
}
