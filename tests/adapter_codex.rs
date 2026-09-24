use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use kanata::{
    adapter::{Adapter, codex::CodexAdapter},
    config::{self, ValidatedConfig},
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, Extensions, InputAudioFormat,
        ModelAlias, Operation, Request, RequestContext, RouteIdentity, RoutedRequest, ToolChoice,
        TrustZone, ValidatedAudio, ValidatedFile,
    },
};

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

struct ConfigFixture {
    root: PathBuf,
    state: PathBuf,
}

impl ConfigFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kanata-adapter-codex-{}-{}",
            std::process::id(),
            NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("fixture directory");
        Self {
            state: root.join("state"),
            root,
        }
    }

    fn load(&self, base_url: &str) -> ValidatedConfig {
        let state = self.state.to_string_lossy();
        let contents = include_str!("../config/personal.example.toml")
            .replace("https://chatgpt.invalid/backend-api/codex", base_url)
            .replace(
                "/replace/with/owner-writable/absolute/path/kanata-codex",
                &state,
            );
        let path = self.root.join("fixture.toml");
        fs::write(&path, contents).expect("write fixture config");
        let config = config::load(&path).expect("valid Codex config");
        let _ = fs::remove_file(path);
        config
    }

    fn route(&self, config: &ValidatedConfig, route_id: &str) -> RouteIdentity {
        config
            .routes()
            .iter()
            .find(|route| route.identity().route_id == route_id)
            .expect("configured route")
            .identity()
            .clone()
    }
}

impl Drop for ConfigFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn chat(model: &str, streaming: bool) -> Request {
    Request::Chat(ChatRequest {
        model: ModelAlias(model.into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatContent::Text {
                text: "TEST_ONLY_PROMPT_NOT_SECRET_0001".into(),
            }],
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        stream: streaming,
        extensions: Extensions::default(),
        options: Default::default(),
    })
}

fn routed(route: RouteIdentity, request: Request) -> RoutedRequest {
    RoutedRequest::new(
        RequestContext {
            request_id: "TEST_ONLY_REQUEST_ID_NOT_SECRET_0001".into(),
            route,
            trust_zone: TrustZone::External,
            extensions: Extensions::default(),
        },
        request,
    )
    .expect("valid routed request")
}

fn error_kind(
    result: Result<kanata::adapter::AdapterOutput, kanata::core::GatewayError>,
) -> ErrorKind {
    match result {
        Err(error) => error.kind,
        Ok(_) => panic!("adapter unexpectedly accepted fixture request"),
    }
}

#[tokio::test]
async fn constructor_binds_all_exact_routes_and_declares_only_fixture_proven_features() {
    let fixture = ConfigFixture::new();
    let config = fixture.load("https://chatgpt.com/backend-api/codex");
    let adapter = CodexAdapter::from_config(&config, "codex-private").expect("adapter");

    assert_eq!(adapter.id(), "codex-private");
    assert_eq!(adapter.configured_id(), "codex-private");
    assert_eq!(
        adapter.capabilities().operations,
        [Operation::Chat].into_iter().collect()
    );
    assert!(adapter.capabilities().streaming_chat);
    assert!(adapter.capabilities().function_tools);
    assert!(!adapter.capabilities().input_audio);
    assert!(!adapter.capabilities().audio_streaming_chat);
    assert!(!adapter.capabilities().audio_function_tools);
    assert!(!format!("{adapter:?}").contains("TEST_ONLY"));

    let low = routed(
        fixture.route(&config, "codex-gpt-6-luna-low"),
        chat("gpt-6-luna:low", false),
    );
    assert_eq!(
        error_kind(adapter.execute(low).await),
        ErrorKind::UpstreamUnavailable,
        "the second exact route is bound; it fails only because no fixture credential is stored"
    );
}

#[test]
fn offline_invalid_template_origin_is_rejected_at_runtime_construction() {
    let fixture = ConfigFixture::new();
    let config = fixture.load("https://chatgpt.invalid/backend-api/codex");
    assert_eq!(
        CodexAdapter::from_config(&config, "codex-private")
            .expect_err("offline origin must not be dispatched")
            .kind,
        ErrorKind::Internal
    );
}

#[tokio::test]
async fn unbound_routes_audio_and_transcription_reject_before_credential_access() {
    let fixture = ConfigFixture::new();
    let config = fixture.load("https://chatgpt.com/backend-api/codex");
    let adapter = CodexAdapter::from_config(&config, "codex-private").expect("adapter");

    let mut changed_upstream = fixture.route(&config, "codex-chat");
    changed_upstream.upstream_id = "unconfigured-model".into();
    assert_eq!(
        error_kind(
            adapter
                .execute(routed(changed_upstream, chat("codex-chat", false)))
                .await
        ),
        ErrorKind::InvalidRequest
    );

    let mut audio = chat("codex-chat", false);
    let Request::Chat(ref mut chat) = audio else {
        panic!("chat request")
    };
    chat.messages[0].content = vec![ChatContent::InputAudio {
        audio: ValidatedAudio::new(InputAudioFormat::Wav, vec![1, 2, 3]).expect("audio"),
    }];
    assert_eq!(
        error_kind(
            adapter
                .execute(routed(fixture.route(&config, "codex-chat"), audio))
                .await
        ),
        ErrorKind::UnsupportedOperation
    );

    let transcription = Request::Transcription(kanata::core::TranscriptionRequest {
        model: ModelAlias("codex-chat".into()),
        file: ValidatedFile::new("fixture.wav", "audio/wav", vec![1]).expect("file"),
        language: None,
        prompt: None,
        extensions: Extensions::default(),
    });
    assert_eq!(
        error_kind(
            adapter
                .execute(routed(
                    RouteIdentity::new(
                        "unbound-transcription",
                        "fixture-codex-model",
                        ModelAlias("codex-chat".into()),
                        Operation::Transcription,
                    ),
                    transcription,
                ))
                .await
        ),
        ErrorKind::UnsupportedOperation
    );
}
