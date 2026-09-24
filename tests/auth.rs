use std::fs;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
};
use kanata::adapter::{Adapter, AdapterFuture};
use kanata::auth::{
    ApplicationAuth, AuthBuildError, EnvironmentSecretResolver, SecretResolutionError,
    SecretResolver,
};
use kanata::config::{self, SecretReference};
use kanata::core::{
    Capabilities, ErrorKind, GatewayError, ModelAlias, Operation, RouteSelector, RoutedRequest,
};
use kanata::routing::Registry;
use kanata::server::{Readiness, TwoPlaneServer};
use serde_json::Value;

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct MemoryResolver(Vec<u8>);

impl SecretResolver for MemoryResolver {
    fn resolve(&self, _: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        Ok(self.0.clone())
    }
}

struct DistinctScopeResolver;

impl SecretResolver for DistinctScopeResolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        match reference.env_name() {
            Some("KANATA_CLIENT_KEY") => Ok(b"OWNER_SYNTHETIC_KEY_001".to_vec()),
            Some("KANATA_EXTERNAL_CLIENT_KEY") => Ok(b"EXTERNAL_SYNTHETIC_KEY_001".to_vec()),
            _ => Err(SecretResolutionError),
        }
    }
}

struct DispatchCounter {
    adapter_id: &'static str,
    calls: Arc<AtomicUsize>,
    capabilities: Capabilities,
}

impl Adapter for DispatchCounter {
    fn id(&self) -> &str {
        self.adapter_id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, _: RoutedRequest) -> AdapterFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(GatewayError {
                kind: ErrorKind::InvalidRequest,
            })
        })
    }
}

fn config() -> config::ValidatedConfig {
    config::load("tests/fixtures/config/example.toml").expect("example config validates")
}

/// The personal template's former inline keys, kept here now that the template uses `[keys]`.
const PERSONAL_INLINE_KEYS: &str = r#"[[application_keys]]
id = "personal-client"
secret_ref = "env:KANATA_CLIENT_KEY"
owner = true
permissions = [
  { model_alias = "local-chat", operation = "chat" },
  { model_alias = "ollama-cloud", operation = "chat" },
  { model_alias = "private-chat-a", operation = "chat" },
  { model_alias = "private-chat-b", operation = "chat" },
  { model_alias = "private-audio-chat", operation = "chat" },
  { model_alias = "private-audio-transcribe", operation = "transcription" },
  { model_alias = "private-native-asr", operation = "transcription" },
  { model_alias = "codex-chat", operation = "chat" },
  { model_alias = "gpt-6-sol", operation = "chat" },
  { model_alias = "gpt-6-luna:low", operation = "chat" },
]

[[application_keys]]
id = "external-client"
secret_ref = "env:KANATA_EXTERNAL_CLIENT_KEY"
permissions = [
  { model_alias = "private-chat-a", operation = "chat" },
]
"#;

fn personal_inline_config(edit: impl FnOnce(String) -> String) -> config::ValidatedConfig {
    let template =
        fs::read_to_string("config/personal.example.toml").expect("personal example reads");
    let keys_start = template.find("[keys]\n").expect("template keys table");
    let keys_end = keys_start + template[keys_start..].find("\n\n").expect("keys table end");
    let contents = edit(format!(
        "{}{PERSONAL_INLINE_KEYS}{}",
        &template[..keys_start],
        &template[keys_end + 1..]
    ));
    let path = std::env::temp_dir().join(format!(
        "kanata-auth-public-{}-{}.toml",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let result = config::load(&path);
    fs::remove_file(path).expect("fixture removes");
    result.expect("personal config validates")
}

fn personal_public_config(public_routes: &str) -> config::ValidatedConfig {
    personal_inline_config(|contents| {
        contents
            .replace(
                "[listeners.admin]",
                "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
            )
            .replace(
                "tailnet_addresses = [\"100.64.0.10\"]",
                &format!("tailnet_addresses = [\"100.64.0.10\"]\npublic_routes = {public_routes}"),
            )
    })
}

fn file_key_config(secret_path: &std::path::Path) -> config::ValidatedConfig {
    let contents = fs::read_to_string("tests/fixtures/config/example.toml").expect("example reads");
    let contents = contents.replace(
        "env:KANATA_CLIENT_KEY",
        &format!("file:{}", secret_path.display()),
    );
    let path = std::env::temp_dir().join(format!(
        "kanata-auth-config-{}-{}.toml",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let config = config::load(&path).expect("file-key config validates");
    fs::remove_file(path).expect("fixture removes");
    config
}

fn secret_file(contents: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "kanata-auth-secret-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("secret writes");
    path
}

fn selector(alias: &str, operation: Operation) -> RouteSelector {
    RouteSelector {
        model_alias: ModelAlias(alias.into()),
        operation,
    }
}

fn headers(value: Option<HeaderValue>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(value) = value {
        headers.insert(header::AUTHORIZATION, value);
    }
    headers
}

fn bearer(value: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {value}")).expect("bearer header")
}

fn models_request_for(key: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header(header::AUTHORIZATION, bearer(key))
        .body(Body::empty())
        .expect("models request")
}

fn chat_request_for(model: &str, key: &str) -> Request<Body> {
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "scoped request" }],
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, bearer(key))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("chat request")
}

fn transcription_request_for(model: &str, key: &str) -> Request<Body> {
    const BOUNDARY: &str = "scope-boundary";
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"sample.wav\"\r\nContent-Type: audio/wav\r\n\r\nsample audio\r\n--{BOUNDARY}--\r\n"
    );
    Request::builder()
        .method("POST")
        .uri("/v1/audio/transcriptions")
        .header(header::AUTHORIZATION, bearer(key))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("transcription request")
}

async fn model_ids(response: axum::response::Response) -> Vec<String> {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    let value: Value = serde_json::from_slice(&bytes).expect("model list JSON");
    value["data"]
        .as_array()
        .expect("model list")
        .iter()
        .map(|model| model["id"].as_str().expect("model id").to_owned())
        .collect()
}

#[test]
fn bearer_authentication_is_strict_and_non_enumerating() {
    let auth = ApplicationAuth::from_validated(&config(), &MemoryResolver(b"test-key".to_vec()))
        .expect("auth builds");
    let cases = [
        headers(None),
        headers(Some(HeaderValue::from_static("Basic test-key"))),
        headers(Some(HeaderValue::from_static("Bearer "))),
        headers(Some(HeaderValue::from_static("Bearer wrong-key"))),
        headers(Some(
            HeaderValue::from_bytes(b"Bearer \xff").expect("header"),
        )),
        headers(Some(
            HeaderValue::from_str(&format!("Bearer {}", "x".repeat(4097))).expect("header"),
        )),
        headers(Some(HeaderValue::from_static("Bearer comma,value"))),
        headers(Some(HeaderValue::from_static("Bearer colon:value"))),
        headers(Some(HeaderValue::from_static("Bearer middle=padding"))),
        headers(Some(HeaderValue::from_static("Bearer whitespace token"))),
    ];
    let mut duplicate = headers(Some(HeaderValue::from_static("Bearer test-key")));
    duplicate.append(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer test-key"),
    );

    let errors: Vec<_> = cases
        .iter()
        .chain(std::iter::once(&duplicate))
        .map(|headers| match auth.authenticate_headers(headers) {
            Ok(_) => panic!("must reject"),
            Err(error) => error.to_string(),
        })
        .collect();
    assert!(errors.iter().all(|error| error == "authentication failed"));
    assert_eq!(
        auth.authenticate_headers(&headers(Some(HeaderValue::from_static("Bearer test-key"))))
            .expect("valid key")
            .key_identity(),
        "personal-client"
    );
}

#[test]
fn permissions_are_exact_and_model_filtering_is_stable() {
    let config = config();
    let registry = Registry::from_validated(&config);
    let context = ApplicationAuth::from_validated(&config, &MemoryResolver(b"test-key".to_vec()))
        .expect("auth builds")
        .authenticate_headers(&headers(Some(HeaderValue::from_static("Bearer test-key"))))
        .expect("authenticates");

    assert!(
        context
            .authorize(&selector("local-chat", Operation::Chat))
            .is_ok()
    );
    assert!(
        context
            .authorize(&selector("local-chat", Operation::Transcription))
            .is_err()
    );
    assert!(
        context
            .authorize(&selector("local-chat-extra", Operation::Chat))
            .is_err()
    );
    assert!(
        context
            .authorize(&selector("local-*", Operation::Chat))
            .is_err()
    );
    assert_eq!(
        context
            .permitted_models(&registry)
            .into_iter()
            .map(|alias| alias.0)
            .collect::<Vec<_>>(),
        vec![
            "codex-chat",
            "local-chat",
            "private-chat",
            "private-transcribe",
            "remote-chat",
        ]
    );
}

#[test]
fn resolved_key_failures_do_not_expose_secret_material() {
    let config = config();
    let empty = match ApplicationAuth::from_validated(&config, &MemoryResolver(Vec::new())) {
        Ok(_) => panic!("empty key must fail"),
        Err(error) => error,
    };
    assert_eq!(empty, AuthBuildError::InvalidSecret);

    let contents = fs::read_to_string("tests/fixtures/config/example.toml").expect("example reads");
    let duplicate = contents.replace(
        "[limits]",
        "[[application_keys]]\nid = \"second-client\"\nsecret_ref = \"env:KANATA_SECOND_CLIENT_KEY\"\npermissions = [{ model_alias = \"local-chat\", operation = \"chat\" }]\n\n[limits]",
    );
    let path = std::env::temp_dir().join(format!(
        "kanata-auth-{}-{}.toml",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, duplicate).expect("fixture writes");
    let duplicate_config = config::load(&path).expect("duplicate config validates");
    fs::remove_file(path).expect("fixture removes");
    let error = match ApplicationAuth::from_validated(
        &duplicate_config,
        &MemoryResolver(b"DUPLICATE_MARKER".to_vec()),
    ) {
        Ok(_) => panic!("duplicate key must fail"),
        Err(error) => error.to_string(),
    };
    assert_eq!(error, "application keys contain duplicate material");
    assert!(!error.contains("DUPLICATE_MARKER"));
}

#[test]
fn configured_keys_use_the_same_b64token_grammar_as_bearer_headers() {
    for invalid in [
        b"comma,value".as_slice(),
        b"colon:value",
        b"middle=padding",
        b"embedded whitespace",
        b"control\x01",
        b"\xff",
    ] {
        let error =
            match ApplicationAuth::from_validated(&config(), &MemoryResolver(invalid.to_vec())) {
                Ok(_) => panic!("invalid key must fail"),
                Err(error) => error,
            };
        assert_eq!(error, AuthBuildError::InvalidSecret);
    }
    ApplicationAuth::from_validated(&config(), &MemoryResolver(b"AZaz09-._~+/==".to_vec()))
        .expect("RFC 6750 b64token builds");
}

#[test]
fn production_file_keys_normalize_one_terminal_newline_and_stay_bounded() {
    for contents in [b"file-token\n".as_slice(), b"file-token\r\n"] {
        let path = secret_file(contents);
        let config = file_key_config(&path);
        let auth = ApplicationAuth::from_validated(&config, &EnvironmentSecretResolver)
            .expect("terminal newline normalizes");
        fs::remove_file(path).expect("secret removes");
        assert!(
            auth.authenticate_headers(&headers(Some(HeaderValue::from_static(
                "Bearer file-token"
            ))))
            .is_ok()
        );
    }

    for contents in [b"file-token\n\n".as_slice(), b"file token", b"\xff"] {
        let path = secret_file(contents);
        let config = file_key_config(&path);
        let result = ApplicationAuth::from_validated(&config, &EnvironmentSecretResolver);
        fs::remove_file(path).expect("secret removes");
        assert_eq!(result.err(), Some(AuthBuildError::InvalidSecret));
    }

    let path = secret_file(&vec![b'a'; 4099]);
    let config = file_key_config(&path);
    let result = ApplicationAuth::from_validated(&config, &EnvironmentSecretResolver);
    fs::remove_file(path).expect("secret removes");
    assert_eq!(result.err(), Some(AuthBuildError::SecretResolution));
}

#[tokio::test]
async fn personal_codex_keys_filter_models_and_block_forbidden_dispatch() {
    const OWNER_KEY: &str = "OWNER_SYNTHETIC_KEY_001";
    const EXTERNAL_KEY: &str = "EXTERNAL_SYNTHETIC_KEY_001";
    let config = personal_inline_config(|contents| contents);
    let auth = ApplicationAuth::from_validated(&config, &DistinctScopeResolver)
        .expect("distinct scoped keys build");
    let owner = auth
        .authenticate_headers(&headers(Some(bearer(OWNER_KEY))))
        .expect("owner key authenticates");
    let external = auth
        .authenticate_headers(&headers(Some(bearer(EXTERNAL_KEY))))
        .expect("external key authenticates");
    let registry = Registry::from_validated(&config);

    assert!(
        owner
            .authorize(&selector("gpt-6-sol", Operation::Chat))
            .is_ok()
    );
    assert!(
        owner
            .authorize(&selector("private-chat-a", Operation::Chat))
            .is_ok()
    );
    assert!(
        owner
            .authorize(&selector("private-native-asr", Operation::Transcription))
            .is_ok()
    );
    assert!(
        external
            .authorize(&selector("private-chat-a", Operation::Chat))
            .is_ok()
    );
    assert!(
        external
            .authorize(&selector("gpt-6-sol", Operation::Chat))
            .is_err()
    );
    assert!(
        external
            .authorize(&selector("private-chat-a", Operation::Transcription))
            .is_err()
    );
    assert!(
        external
            .authorize(&selector("private-chat-b", Operation::Chat))
            .is_err()
    );
    assert!(
        owner
            .permitted_models(&registry)
            .iter()
            .any(|model| model.0 == "gpt-6-sol")
    );
    assert_eq!(
        external
            .permitted_models(&registry)
            .into_iter()
            .map(|model| model.0)
            .collect::<Vec<_>>(),
        ["private-chat-a"]
    );

    let codex = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "codex-private")
        .expect("configured Codex adapter");
    let vllm = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "vllm-text-a")
        .expect("configured non-Codex adapter");
    let codex_dispatches = Arc::new(AtomicUsize::new(0));
    let vllm_dispatches = Arc::new(AtomicUsize::new(0));
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &DistinctScopeResolver,
        Readiness::new(true),
        vec![
            Arc::new(DispatchCounter {
                adapter_id: "codex-private",
                calls: codex_dispatches.clone(),
                capabilities: codex.capabilities().clone(),
            }),
            Arc::new(DispatchCounter {
                adapter_id: "vllm-text-a",
                calls: vllm_dispatches.clone(),
                capabilities: vllm.capabilities().clone(),
            }),
        ],
    )
    .expect("fixture server with distinct keys");

    let owner_models = server
        .client_oneshot(models_request_for(OWNER_KEY))
        .await
        .expect("owner models response");
    assert_eq!(owner_models.status(), StatusCode::OK);
    assert_eq!(
        model_ids(owner_models).await,
        [
            "codex-chat",
            "gpt-6-luna:low",
            "gpt-6-sol",
            "private-chat-a"
        ]
    );
    let external_models = server
        .client_oneshot(models_request_for(EXTERNAL_KEY))
        .await
        .expect("external models response");
    assert_eq!(external_models.status(), StatusCode::OK);
    assert_eq!(model_ids(external_models).await, ["private-chat-a"]);

    server
        .client_oneshot(chat_request_for("gpt-6-sol", OWNER_KEY))
        .await
        .expect("owner chat response");
    server
        .client_oneshot(chat_request_for("private-chat-a", OWNER_KEY))
        .await
        .expect("owner non-Codex chat response");
    server
        .client_oneshot(chat_request_for("private-chat-a", EXTERNAL_KEY))
        .await
        .expect("external non-Codex chat response");
    assert_eq!(codex_dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(vllm_dispatches.load(Ordering::SeqCst), 2);

    let denied_chat = server
        .client_oneshot(chat_request_for("gpt-6-sol", EXTERNAL_KEY))
        .await
        .expect("denied chat response");
    assert_eq!(denied_chat.status(), StatusCode::FORBIDDEN);
    let denied_non_codex = server
        .client_oneshot(chat_request_for("private-chat-b", EXTERNAL_KEY))
        .await
        .expect("unscoped non-Codex route response");
    assert_eq!(denied_non_codex.status(), StatusCode::FORBIDDEN);
    let denied_transcription = server
        .client_oneshot(transcription_request_for("private-chat-a", EXTERNAL_KEY))
        .await
        .expect("denied transcription response");
    assert_eq!(denied_transcription.status(), StatusCode::FORBIDDEN);
    assert_eq!(codex_dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(vllm_dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn public_scopes_intersect_keys_and_exact_routes_without_public_codex() {
    const OWNER_KEY: &str = "OWNER_SYNTHETIC_KEY_001";
    const EXTERNAL_KEY: &str = "EXTERNAL_SYNTHETIC_KEY_001";
    let config = personal_public_config(
        r#"[
          { model_alias = "private-chat-a", operation = "chat" },
          { model_alias = "private-native-asr", operation = "transcription" },
        ]"#,
    );
    let codex = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "codex-private")
        .expect("Codex adapter");
    let chat = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "vllm-text-a")
        .expect("chat adapter");
    let transcription = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "vllm-native-asr")
        .expect("transcription adapter");
    let codex_dispatches = Arc::new(AtomicUsize::new(0));
    let chat_dispatches = Arc::new(AtomicUsize::new(0));
    let transcription_dispatches = Arc::new(AtomicUsize::new(0));
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &DistinctScopeResolver,
        Readiness::new(true),
        vec![
            Arc::new(DispatchCounter {
                adapter_id: "codex-private",
                calls: codex_dispatches.clone(),
                capabilities: codex.capabilities().clone(),
            }),
            Arc::new(DispatchCounter {
                adapter_id: "vllm-text-a",
                calls: chat_dispatches.clone(),
                capabilities: chat.capabilities().clone(),
            }),
            Arc::new(DispatchCounter {
                adapter_id: "vllm-native-asr",
                calls: transcription_dispatches.clone(),
                capabilities: transcription.capabilities().clone(),
            }),
        ],
    )
    .expect("public and private routers share validated adapters");

    let owner_models = server
        .public_oneshot(models_request_for(OWNER_KEY))
        .await
        .expect("configured public router");
    assert_eq!(owner_models.status(), StatusCode::OK);
    assert_eq!(
        model_ids(owner_models).await,
        ["private-chat-a", "private-native-asr"]
    );
    let external_models = server
        .public_oneshot(models_request_for(EXTERNAL_KEY))
        .await
        .expect("configured public router");
    assert_eq!(external_models.status(), StatusCode::OK);
    assert_eq!(model_ids(external_models).await, ["private-chat-a"]);

    let public_codex = server
        .public_oneshot(chat_request_for("gpt-6-sol", OWNER_KEY))
        .await
        .expect("configured public router");
    assert_eq!(public_codex.status(), StatusCode::FORBIDDEN);
    assert_eq!(codex_dispatches.load(Ordering::SeqCst), 0);

    let external_chat = server
        .public_oneshot(chat_request_for("private-chat-a", EXTERNAL_KEY))
        .await
        .expect("configured public router");
    assert_eq!(external_chat.status(), StatusCode::BAD_REQUEST);
    assert_eq!(chat_dispatches.load(Ordering::SeqCst), 1);

    let external_asr = server
        .public_oneshot(transcription_request_for(
            "private-native-asr",
            EXTERNAL_KEY,
        ))
        .await
        .expect("configured public router");
    assert_eq!(external_asr.status(), StatusCode::FORBIDDEN);
    assert_eq!(transcription_dispatches.load(Ordering::SeqCst), 0);

    let owner_asr = server
        .public_oneshot(transcription_request_for("private-native-asr", OWNER_KEY))
        .await
        .expect("configured public router");
    assert_eq!(owner_asr.status(), StatusCode::BAD_REQUEST);
    assert_eq!(transcription_dispatches.load(Ordering::SeqCst), 1);

    let private_codex = server
        .client_oneshot(chat_request_for("gpt-6-sol", OWNER_KEY))
        .await
        .expect("private owner route");
    assert_eq!(private_codex.status(), StatusCode::BAD_REQUEST);
    assert_eq!(codex_dispatches.load(Ordering::SeqCst), 1);
}

fn sha256_hex(value: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn config_from(contents: String) -> Result<config::ValidatedConfig, config::ConfigError> {
    let path = std::env::temp_dir().join(format!(
        "kanata-auth-digest-{}-{}.toml",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let result = config::load(&path);
    fs::remove_file(path).expect("fixture removes");
    result
}

#[test]
fn digest_keys_authenticate_without_resolution_and_collide_with_resolved_keys() {
    let token = "kanata_sk_DIGEST_SYNTHETIC_KEY_001";
    let example = fs::read_to_string("tests/fixtures/config/example.toml").expect("example reads");
    let digest_ref = format!("sha256:{}", sha256_hex(token.as_bytes()));
    let digest_config = config_from(example.replace("env:KANATA_CLIENT_KEY", &digest_ref))
        .expect("digest config validates");

    // Resolving anything would fail, so success proves the digest is used directly.
    let auth = ApplicationAuth::from_validated(&digest_config, &DistinctScopeResolver)
        .expect("digest auth builds");
    assert_eq!(
        auth.authenticate_headers(&headers(Some(bearer(token))))
            .expect("digest key authenticates")
            .key_identity(),
        "personal-client"
    );
    for wrong in [
        &token[..token.len() - 1],
        "kanata_sk_DIGEST_SYNTHETIC_KEY_002",
    ] {
        assert!(
            auth.authenticate_headers(&headers(Some(bearer(wrong))))
                .is_err()
        );
    }

    let secret_path = secret_file(format!("{token}\n").as_bytes());
    let duplicate = config_from(format!(
        "{}\n[[application_keys]]\nid = \"friend\"\nsecret_ref = \"file:{}\"\npermissions = [\n  {{ model_alias = \"local-chat\", operation = \"chat\" }},\n]\n",
        example.replace("env:KANATA_CLIENT_KEY", &digest_ref),
        secret_path.display()
    ))
    .expect("duplicate-material config validates offline");
    let result = ApplicationAuth::from_validated(&duplicate, &EnvironmentSecretResolver);
    fs::remove_file(secret_path).expect("secret removes");
    assert!(matches!(result, Err(AuthBuildError::DuplicateSecret)));
}
