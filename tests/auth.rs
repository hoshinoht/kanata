#[path = "support/scratch.rs"]
mod scratch;

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
use kanata::auth::{ApplicationAuth, AuthError};
use kanata::config;
use kanata::core::{
    Capabilities, ErrorKind, GatewayError, ModelAlias, Operation, RouteSelector, RoutedRequest,
};
use kanata::routing::Registry;
use kanata::server::{Readiness, TwoPlaneServer};
use serde_json::Value;

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

const OWNER_KEY: &str = "OWNER_SYNTHETIC_KEY_001";
const EXTERNAL_KEY: &str = "EXTERNAL_SYNTHETIC_KEY_001";

/// The personal template with a ChatGPT adapter ([`scratch::personal_with_chatgpt`]) and two
/// keys-file records: the owner key and an external key scoped to one vLLM route.
fn personal_config(edit: impl FnOnce(String) -> String) -> config::ValidatedConfig {
    let template = scratch::personal_with_chatgpt();
    let keys_start = template.find("[keys]\n").expect("template keys table");
    let keys_end = keys_start + template[keys_start..].find("\n\n").expect("keys table end");
    let owner = scratch::key_record(
        "personal-client",
        OWNER_KEY,
        r#"owner = true
permissions = [
  { model_alias = "local-chat", operation = "chat" },
  { model_alias = "ollama-cloud", operation = "chat" },
  { model_alias = "private-chat-a", operation = "chat" },
  { model_alias = "private-chat-b", operation = "chat" },
  { model_alias = "private-audio-chat", operation = "chat" },
  { model_alias = "private-audio-transcribe", operation = "transcription" },
  { model_alias = "private-native-asr", operation = "transcription" },
  { model_alias = "chatgpt-chat", operation = "chat" },
  { model_alias = "gpt-6-sol", operation = "chat" },
  { model_alias = "gpt-6-luna:low", operation = "chat" },
]"#,
    );
    let external = scratch::key_record(
        "external-client",
        EXTERNAL_KEY,
        r#"permissions = [
  { model_alias = "private-chat-a", operation = "chat" },
]"#,
    );
    let contents = edit(format!(
        "{}{}{}",
        &template[..keys_start],
        scratch::KEYS_SECTION,
        &template[keys_end + 1..]
    ));
    scratch::load_text(&format!(
        "{contents}{}{owner}{external}",
        scratch::KEYS_FILE_MARKER
    ))
    .expect("personal config validates")
}

fn personal_public_config(public_routes: &str) -> config::ValidatedConfig {
    personal_config(|contents| {
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
    let auth = ApplicationAuth::from_validated(&config()).expect("auth builds");
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
fn api_key_header_alone_or_matching_bearer_authenticates() {
    let auth = ApplicationAuth::from_validated(&config()).expect("auth builds");
    let with = |pairs: &[(&'static str, &'static str)]| {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    };
    let accepted = [
        with(&[("x-api-key", "test-key")]),
        with(&[
            ("authorization", "Bearer test-key"),
            ("x-api-key", "test-key"),
        ]),
    ];
    for headers in &accepted {
        assert_eq!(
            auth.authenticate_headers(headers)
                .expect("valid key")
                .key_identity(),
            "personal-client"
        );
    }
    let rejected = [
        with(&[("x-api-key", "wrong-key")]),
        with(&[("x-api-key", "")]),
        with(&[("x-api-key", "Bearer test-key")]),
        with(&[("x-api-key", "test-key"), ("x-api-key", "test-key")]),
        with(&[
            ("authorization", "Bearer test-key"),
            ("x-api-key", "wrong-key"),
        ]),
        with(&[
            ("authorization", "Bearer wrong-key"),
            ("x-api-key", "test-key"),
        ]),
        with(&[
            ("authorization", "Basic test-key"),
            ("x-api-key", "test-key"),
        ]),
        with(&[
            ("authorization", "Bearer test-key"),
            ("authorization", "Bearer test-key"),
            ("x-api-key", "test-key"),
        ]),
        with(&[
            ("authorization", "Bearer test-key"),
            ("x-api-key", "test-key"),
            ("x-api-key", "test-key"),
        ]),
    ];
    for headers in &rejected {
        assert_eq!(
            auth.authenticate_headers(headers).err(),
            Some(AuthError::Invalid)
        );
    }
}

#[test]
fn permissions_are_exact_and_model_filtering_is_stable() {
    let config = config();
    let registry = Registry::from_validated(&config);
    let context = ApplicationAuth::from_validated(&config)
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
            "chatgpt-chat",
            "local-chat",
            "private-chat",
            "private-transcribe",
            "remote-chat",
        ]
    );
}

#[tokio::test]
async fn personal_chatgpt_keys_filter_models_and_block_forbidden_dispatch() {
    let config = personal_config(|contents| contents);
    let auth = ApplicationAuth::from_validated(&config).expect("distinct scoped keys build");
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

    let chatgpt = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "chatgpt-private")
        .expect("configured ChatGPT adapter");
    let vllm = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "vllm-text-a")
        .expect("configured non-ChatGPT adapter");
    let chatgpt_dispatches = Arc::new(AtomicUsize::new(0));
    let vllm_dispatches = Arc::new(AtomicUsize::new(0));
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        Readiness::new(true),
        vec![
            Arc::new(DispatchCounter {
                adapter_id: "chatgpt-private",
                calls: chatgpt_dispatches.clone(),
                capabilities: chatgpt.capabilities().clone(),
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
    // ChatGPT reasoning routes are discovered under their family name.
    assert_eq!(
        model_ids(owner_models).await,
        ["chatgpt-chat", "gpt-6-luna", "gpt-6-sol", "private-chat-a"]
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
        .expect("owner non-ChatGPT chat response");
    server
        .client_oneshot(chat_request_for("private-chat-a", EXTERNAL_KEY))
        .await
        .expect("external non-ChatGPT chat response");
    assert_eq!(chatgpt_dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(vllm_dispatches.load(Ordering::SeqCst), 2);

    let denied_chat = server
        .client_oneshot(chat_request_for("gpt-6-sol", EXTERNAL_KEY))
        .await
        .expect("denied chat response");
    assert_eq!(denied_chat.status(), StatusCode::FORBIDDEN);
    let denied_non_chatgpt = server
        .client_oneshot(chat_request_for("private-chat-b", EXTERNAL_KEY))
        .await
        .expect("unscoped non-ChatGPT route response");
    assert_eq!(denied_non_chatgpt.status(), StatusCode::FORBIDDEN);
    let denied_transcription = server
        .client_oneshot(transcription_request_for("private-chat-a", EXTERNAL_KEY))
        .await
        .expect("denied transcription response");
    assert_eq!(denied_transcription.status(), StatusCode::FORBIDDEN);
    assert_eq!(chatgpt_dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(vllm_dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn public_scopes_intersect_keys_and_exact_routes_without_public_chatgpt() {
    let config = personal_public_config(
        r#"[
          { model_alias = "private-chat-a", operation = "chat" },
          { model_alias = "private-native-asr", operation = "transcription" },
        ]"#,
    );
    let chatgpt = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "chatgpt-private")
        .expect("ChatGPT adapter");
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
    let chatgpt_dispatches = Arc::new(AtomicUsize::new(0));
    let chat_dispatches = Arc::new(AtomicUsize::new(0));
    let transcription_dispatches = Arc::new(AtomicUsize::new(0));
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        Readiness::new(true),
        vec![
            Arc::new(DispatchCounter {
                adapter_id: "chatgpt-private",
                calls: chatgpt_dispatches.clone(),
                capabilities: chatgpt.capabilities().clone(),
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

    let public_chatgpt = server
        .public_oneshot(chat_request_for("gpt-6-sol", OWNER_KEY))
        .await
        .expect("configured public router");
    assert_eq!(public_chatgpt.status(), StatusCode::FORBIDDEN);
    assert_eq!(chatgpt_dispatches.load(Ordering::SeqCst), 0);

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

    let private_chatgpt = server
        .client_oneshot(chat_request_for("gpt-6-sol", OWNER_KEY))
        .await
        .expect("private owner route");
    assert_eq!(private_chatgpt.status(), StatusCode::BAD_REQUEST);
    assert_eq!(chatgpt_dispatches.load(Ordering::SeqCst), 1);
}

/// The example config with its keys file replaced by `records`.
fn keyed_example(records: &str) -> String {
    let example = fs::read_to_string("tests/fixtures/config/example.toml").expect("example reads");
    let keyed = example.replace(
        "[keys]\nfile = \"example-keys.toml\"\n",
        scratch::KEYS_SECTION,
    );
    assert_ne!(keyed, example);
    format!("{keyed}{}{records}", scratch::KEYS_FILE_MARKER)
}

#[test]
fn key_digests_authenticate_exact_tokens_and_reject_duplicate_material() {
    let token = "kanata_sk_DIGEST_SYNTHETIC_KEY_001";
    let local = "permissions = [{ model_alias = \"local-chat\", operation = \"chat\" }]";
    let digest_config = scratch::load_text(&keyed_example(&scratch::key_record(
        "personal-client",
        token,
        local,
    )))
    .expect("digest config validates");

    let auth = ApplicationAuth::from_validated(&digest_config).expect("digest auth builds");
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

    let duplicate = scratch::load_text(&keyed_example(&format!(
        "{}{}",
        scratch::key_record("personal-client", token, local),
        scratch::key_record("friend", token, local)
    )));
    assert_eq!(
        duplicate.unwrap_err().to_string(),
        "config error at keys_file.keys[1].digest: duplicate_secret"
    );
}
