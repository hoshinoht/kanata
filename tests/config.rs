use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

fn example() -> String {
    fs::read_to_string("tests/fixtures/config/example.toml").expect("example exists")
}

fn personal_example() -> String {
    fs::read_to_string("config/personal.example.toml").expect("personal example exists")
}

fn check(contents: String) -> Result<kanata::config::ValidatedConfig, String> {
    let path = std::env::temp_dir().join(format!(
        "kanata-config-{}-{}.toml",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents).expect("fixture writes");
    let result = kanata::config::load(&path).map_err(|error| error.to_string());
    fs::remove_file(path).expect("fixture removes");
    result
}

#[test]
fn example_passes_offline_cli_check_without_resolving_secrets() {
    let output = Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["check", "--config", "tests/fixtures/config/example.toml"])
        .output()
        .expect("binary runs");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf8"),
        "configuration valid\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn personal_template_keeps_multiple_vllm_endpoints_constructible_offline() {
    let output = Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["check", "--config", "config/personal.example.toml"])
        .output()
        .expect("binary runs");
    assert!(output.status.success());
    let config = kanata::config::load("config/personal.example.toml").expect("template validates");
    assert!(config.application_keys()[0].is_owner());
    assert!(!config.application_keys()[1].is_owner());
    for adapter in config
        .adapters()
        .iter()
        .filter(|adapter| adapter.kind() == kanata::config::ProviderKind::Vllm)
    {
        let route = config
            .routes()
            .iter()
            .find(|route| route.adapter_id() == adapter.id())
            .expect("each vllm adapter has a route");
        let bound = kanata::adapter::vllm::VllmAdapter::from_config(
            &config,
            adapter.id(),
            &route.identity().route_id,
        )
        .expect("each declared vllm adapter is constructible");
        assert_eq!(bound.configured_id(), adapter.id());
    }
    let audio_chat = config
        .adapters()
        .iter()
        .find(|adapter| adapter.id() == "vllm-audio-chat")
        .expect("audio chat endpoint");
    assert!(audio_chat.capabilities().input_audio);
    assert!(
        config
            .routes()
            .iter()
            .any(|route| { route.adapter_id() == audio_chat.id() && route.allows_input_audio() })
    );
    assert_eq!(
        config.codex_auth().expect("Codex auth config").store(),
        kanata::config::CodexAuthStore::Keyring
    );
}

#[test]
fn codex_scopes_allow_any_key_with_one_owner() {
    let personal = personal_example();
    let renamed_owner =
        check(personal.replace("id = \"personal-client\"", "id = \"renamed-owner\""))
            .expect("owner status is selected by its flag, not key ID");
    assert!(renamed_owner.application_keys()[0].is_owner());

    let non_owner_codex = personal.replace(
        "id = \"external-client\"\nsecret_ref = \"env:KANATA_EXTERNAL_CLIENT_KEY\"\npermissions = [\n  { model_alias = \"private-chat-a\", operation = \"chat\" },\n]",
        "id = \"external-client\"\nsecret_ref = \"file:/run/secrets/EXTERNAL_SECRET_MARKER\"\npermissions = [\n  { model_alias = \"gpt-6-luna:low\", operation = \"chat\" },\n]",
    );
    assert_ne!(non_owner_codex, personal);
    check(non_owner_codex).expect("non-owner keys may hold Codex scopes");

    let duplicate_owners = personal.replace(
        "id = \"external-client\"\n",
        "id = \"external-client\"\nowner = true\n",
    );
    let error = check(duplicate_owners).unwrap_err();
    assert_eq!(
        error,
        "config error at application_keys[1].owner: multiple_owners"
    );
}

#[test]
fn container_template_uses_explicit_file_store_without_runtime_io() {
    let output = Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["check", "--config", "config/container.example.toml"])
        .output()
        .expect("binary runs");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());

    let config = kanata::config::load("config/container.example.toml").expect("template validates");
    let auth = config.codex_auth().expect("Codex auth config");
    assert_eq!(auth.store(), kanata::config::CodexAuthStore::File);
    assert_eq!(
        auth.state_dir(),
        std::path::Path::new("/var/lib/kanata/codex")
    );
    assert!(config.adapters()[0].secret_ref().is_none());
}

#[test]
fn validated_config_retains_later_composition_state() {
    let config = check(example()).expect("example validates");
    assert_eq!(config.listeners().client().port(), 8080);
    assert_eq!(config.listeners().admin().bind().to_string(), "127.0.0.1");
    assert_eq!(
        config.publication().tailnet_addresses()[0].to_string(),
        "100.64.0.10"
    );
    assert_eq!(
        config.adapters()[2]
            .secret_ref()
            .expect("reference")
            .env_name(),
        Some("KANATA_OPENROUTER_TOKEN")
    );
    assert!(config.adapters()[3].secret_ref().is_none());
    let codex_auth = config.codex_auth().expect("Codex auth config");
    assert_eq!(codex_auth.store(), kanata::config::CodexAuthStore::Keyring);
    assert_eq!(
        codex_auth.state_dir().to_str(),
        Some("/replace/with/owner-writable/absolute/path/kanata-codex")
    );
    assert_eq!(
        config.application_keys()[0].permissions()[2].operation,
        kanata::core::Operation::Transcription
    );
    assert_eq!(config.limits().max_in_flight(), 8);
    assert_eq!(config.limits().max_audio_bytes(), 25 * 1024 * 1024);
    assert_eq!(
        config.limits().max_audio_chat_body_bytes(),
        config.limits().max_body_bytes() + 4 * config.limits().max_audio_bytes().div_ceil(3)
    );
    assert_eq!(
        config.adapters()[1].transcription_mode(),
        Some(kanata::config::VllmTranscriptionMode::NativeAsr)
    );
    assert!(!config.adapters()[0].capabilities().input_audio);
    assert!(config.routes().iter().all(|route| {
        !route.allows_input_audio()
            && !route.allows_audio_streaming_chat()
            && !route.allows_audio_function_tools()
    }));
    assert_eq!(config.timeouts().overall_ms(), 60_000);
}

#[test]
fn codex_auth_is_required_only_for_codex_and_defaults_to_keyring() {
    let without_store = check(example().replace("store = \"keyring\"\n", ""))
        .expect("keyring is the default store");
    assert_eq!(
        without_store
            .codex_auth()
            .expect("Codex auth config")
            .store(),
        kanata::config::CodexAuthStore::Keyring
    );

    let missing = check(example().replace(
        "[codex_auth]\nstore = \"keyring\"\nstate_dir = \"/replace/with/owner-writable/absolute/path/kanata-codex\"\n\n",
        "",
    ));
    assert_eq!(missing.unwrap_err(), "config error at codex_auth: required");

    let orphan = example().replace(
        "kind = \"codex\"\nbase_url = \"https://chatgpt.invalid/backend-api/codex\"\ntrust_zone = \"external\"",
        "kind = \"vllm\"\nbase_url = \"http://vllm.invalid:8000\"\ntrust_zone = \"private_network\"",
    );
    assert_eq!(
        check(orphan.clone()).unwrap_err(),
        "config error at codex_auth: without_codex_adapter"
    );
    let no_codex_section = orphan.replace(
        "[codex_auth]\nstore = \"keyring\"\nstate_dir = \"/replace/with/owner-writable/absolute/path/kanata-codex\"\n\n",
        "",
    );
    assert!(check(no_codex_section).is_ok());
}

#[test]
fn codex_auth_state_dir_and_store_errors_are_field_oriented() {
    let missing = check(example().replace(
        "state_dir = \"/replace/with/owner-writable/absolute/path/kanata-codex\"\n",
        "",
    ));
    assert_eq!(
        missing.unwrap_err(),
        "config error at codex_auth.state_dir: required"
    );

    let invalid_store = check(example().replace("store = \"keyring\"", "store = \"STORE_MARKER\""));
    let error = invalid_store.unwrap_err();
    assert_eq!(error, "config error at codex_auth.store: schema_error");
    assert!(!error.contains("STORE_MARKER"));

    for (value, class) in [
        ("", "empty"),
        ("relative/codex", "absolute_path_required"),
        ("/var/lib/kanata/../outside", "parent_traversal_forbidden"),
        ("/", "root_not_allowed"),
    ] {
        let contents = example().replace(
            "state_dir = \"/replace/with/owner-writable/absolute/path/kanata-codex\"",
            &format!("state_dir = \"{value}\""),
        );
        assert_eq!(
            check(contents).unwrap_err(),
            format!("config error at codex_auth.state_dir: {class}")
        );
    }
}

#[test]
fn legacy_codex_secret_references_are_rejected_without_echoing_values() {
    let adapter = "base_url = \"https://chatgpt.invalid/backend-api/codex\"\ntrust_zone = \"external\"\n[adapters.capabilities]";
    for reference in [
        "env:CODEX_TOKEN_MARKER",
        "file:/run/secrets/CODEX_TOKEN_MARKER",
    ] {
        let contents = example().replace(
            adapter,
            &format!(
                "base_url = \"https://chatgpt.invalid/backend-api/codex\"\ntrust_zone = \"external\"\nsecret_ref = \"{reference}\"\n[adapters.capabilities]"
            ),
        );
        let error = check(contents).unwrap_err();
        assert_eq!(
            error,
            "config error at adapters[3].secret_ref: codex_secret_ref_unsupported"
        );
        assert!(!error.contains("CODEX_TOKEN_MARKER"));
    }
}

#[test]
fn legacy_example_defaults_new_audio_flags_off() {
    let config = check(example()).expect("legacy example remains valid");
    assert!(
        config
            .adapters()
            .iter()
            .all(|adapter| !adapter.capabilities().input_audio)
    );
    assert!(
        config
            .routes()
            .iter()
            .all(|route| !route.allows_input_audio())
    );
    assert_eq!(config.adapters()[0].transcription_mode(), None);
}

#[test]
fn audio_bounds_and_encoded_envelope_are_checked_without_changing_text_limit() {
    let over_limit =
        check(example().replace("max_audio_bytes = 26214400", "max_audio_bytes = 26214401"));
    assert_eq!(
        over_limit.unwrap_err(),
        "config error at limits.max_audio_bytes: audio_limit_too_large"
    );

    let overflow = check(example().replace(
        "max_body_bytes = 1048576",
        "max_body_bytes = 18446744073709551615",
    ));
    assert_eq!(
        overflow.unwrap_err(),
        "config error at limits: audio_envelope_overflow"
    );
    let config = check(example()).expect("example validates");
    assert_eq!(config.limits().max_body_bytes(), 1_048_576);
}

#[test]
fn vllm_transcription_mode_is_explicit_and_operation_scoped() {
    let missing = check(example().replace("transcription_mode = \"native_asr\"\n", ""));
    assert_eq!(
        missing.unwrap_err(),
        "config error at adapters[1].transcription_mode: required_for_transcription"
    );

    let audio_chat = check(example().replace("native_asr", "audio_chat"));
    assert_eq!(
        audio_chat.unwrap_err(),
        "config error at adapters[1].transcription_mode: requires_input_audio_capability"
    );

    let no_transcription = check(example().replace(
        "operations = [\"chat\", \"transcription\"]",
        "operations = [\"chat\"]",
    ));
    assert_eq!(
        no_transcription.unwrap_err(),
        "config error at adapters[1].transcription_mode: without_transcription_operation"
    );

    let invalid_mode = check(example().replace("native_asr", "unknown_mode"));
    assert_eq!(
        invalid_mode.unwrap_err(),
        "config error at adapters[1].transcription_mode: schema_error"
    );
}

#[test]
fn audio_capability_flags_require_their_text_and_audio_prerequisites() {
    let streaming = check(example().replace(
        "function_tools = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
        "function_tools = true\naudio_streaming_chat = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
    ));
    assert_eq!(
        streaming.unwrap_err(),
        "config error at adapters[1].capabilities.audio_streaming_chat: requires_audio_and_streaming_chat"
    );

    let tools = check(example().replace(
        "function_tools = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
        "function_tools = true\naudio_function_tools = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
    ));
    assert_eq!(
        tools.unwrap_err(),
        "config error at adapters[1].capabilities.audio_function_tools: requires_audio_and_function_tools"
    );
}

#[test]
fn chat_option_capabilities_are_limited_to_what_each_adapter_kind_encodes() {
    let codex_anchor = "function_tools = true\n\n[[routes]]";
    let codex = check(example().replacen(
        codex_anchor,
        "function_tools = true\nstructured_output = true\n\n[[routes]]",
        1,
    ));
    assert_eq!(
        codex.unwrap_err(),
        "config error at adapters[3].capabilities.structured_output: unsupported_by_adapter_kind"
    );
    let codex_sampling = check(example().replacen(
        codex_anchor,
        "function_tools = true\nsampling_controls = true\n\n[[routes]]",
        1,
    ));
    assert_eq!(
        codex_sampling.unwrap_err(),
        "config error at adapters[3].capabilities.sampling_controls: unsupported_by_adapter_kind"
    );
    check(example().replacen(
        codex_anchor,
        "function_tools = true\nreasoning_control = true\n\n[[routes]]",
        1,
    ))
    .expect("codex may declare reasoning control");

    let vllm = check(example().replace(
        "function_tools = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
        "function_tools = true\nreasoning_control = true\n\n[[adapters]]\nid = \"openrouter-remote\"",
    ));
    assert_eq!(
        vllm.unwrap_err(),
        "config error at adapters[1].capabilities.reasoning_control: unsupported_by_adapter_kind"
    );
}

#[test]
fn schema_and_listener_errors_are_field_oriented() {
    let unknown = check(example().replace("port = 8080", "port = 8080\nunknown = true"));
    assert_eq!(
        unknown.unwrap_err(),
        "config error at listeners.client.unknown: unknown_field"
    );
    let type_error = check(example().replace("port = 8080", "port = \"not-a-port\""));
    assert_eq!(
        type_error.unwrap_err(),
        "config error at listeners.client.port: type_error"
    );
    let admin = check(example().replace("bind = \"127.0.0.1\"", "bind = \"0.0.0.0\""));
    assert_eq!(
        admin.unwrap_err(),
        "config error at listeners.admin.bind: not_loopback"
    );
    let collision = check(example().replace("port = 9090", "port = 8080"));
    assert_eq!(
        collision.unwrap_err(),
        "config error at listeners: client_admin_port_collision"
    );
}

#[test]
fn rejects_invalid_publication_and_secret_references_without_echoing_them() {
    let address = check(example().replace("100.64.0.10", "192.0.2.1"));
    assert_eq!(
        address.unwrap_err(),
        "config error at publication.tailnet_addresses[0]: not_tailnet_address"
    );
    let duplicate = check(example().replace("fd7a:115c:a1e0::10", "100.64.0.10"));
    assert_eq!(
        duplicate.unwrap_err(),
        "config error at publication.tailnet_addresses[1]: duplicate"
    );
    let secret = check(example().replace("env:KANATA_CLIENT_KEY", "plaintext-should-not-appear"));
    let error = secret.unwrap_err();
    assert_eq!(
        error,
        "config error at application_keys[0].secret_ref: invalid_secret_reference"
    );
    assert!(!error.contains("plaintext-should-not-appear"));
}

#[test]
fn accepts_only_exact_tailscale_ipv4_range() {
    assert!(check(example().replace("100.64.0.10", "100.64.0.0")).is_ok());
    assert!(check(example().replace("100.64.0.10", "100.127.255.255")).is_ok());
    for address in ["100.63.255.255", "100.128.0.0", "101.0.0.1", "127.0.0.1"] {
        let error = check(example().replace("100.64.0.10", address)).unwrap_err();
        assert_eq!(
            error,
            "config error at publication.tailnet_addresses[0]: not_tailnet_address"
        );
    }
}

#[test]
fn rejects_urls_permissions_and_bounds_that_change_the_security_contract() {
    let url = check(example().replace(
        "https://openrouter.invalid/api/v1",
        "http://user:pass@openrouter.invalid/#fragment",
    ));
    assert_eq!(
        url.unwrap_err(),
        "config error at adapters[2].base_url: userinfo_forbidden"
    );
    let external_http = check(example().replace(
        "https://openrouter.invalid/api/v1",
        "http://openrouter.invalid/api/v1",
    ));
    assert_eq!(
        external_http.unwrap_err(),
        "config error at adapters[2].base_url: https_required"
    );
    let fragment = check(example().replace(
        "https://openrouter.invalid/api/v1",
        "https://openrouter.invalid/api/v1#fragment",
    ));
    assert_eq!(
        fragment.unwrap_err(),
        "config error at adapters[2].base_url: fragment_forbidden"
    );
    let query = check(example().replace(
        "https://openrouter.invalid/api/v1",
        "https://openrouter.invalid/api/v1?token=QUERY_MARKER",
    ));
    let error = query.unwrap_err();
    assert_eq!(
        error,
        "config error at adapters[2].base_url: query_forbidden"
    );
    assert!(!error.contains("QUERY_MARKER"));
    let permission = check(example().replace(
        "{ model_alias = \"local-chat\", operation = \"chat\" }",
        "{ model_alias = \"missing\", operation = \"chat\" }",
    ));
    assert_eq!(
        permission.unwrap_err(),
        "config error at application_keys[0].permissions[0]: unknown_route_selector"
    );
    let bounds = check(example().replace("max_queue = 32", "max_queue = 0"));
    assert_eq!(
        bounds.unwrap_err(),
        "config error at limits.max_queue: zero"
    );
    let timeout = check(example().replace("connect_ms = 5000", "connect_ms = 60001"));
    assert_eq!(
        timeout.unwrap_err(),
        "config error at timeouts.connect_ms: exceeds_overall"
    );
}

#[test]
fn timeout_bounds_are_finite_and_field_specific() {
    const MAX_TIMEOUT_MS: u64 = 604_800_000;
    let fields = [
        ("queue_ms", 1_000),
        ("connect_ms", 5_000),
        ("headers_ms", 10_000),
        ("first_byte_ms", 15_000),
        ("idle_ms", 30_000),
        ("overall_ms", 60_000),
    ];

    for (field, default) in fields {
        assert!(
            check(timeout_fixture(field, default, MAX_TIMEOUT_MS)).is_ok(),
            "{field} accepts the seven-day bound"
        );
        for value in [MAX_TIMEOUT_MS + 1, u64::MAX] {
            assert_eq!(
                check(timeout_fixture(field, default, value)).unwrap_err(),
                format!("config error at timeouts.{field}: timeout_too_large")
            );
        }
    }
}

fn timeout_fixture(field: &str, default: u64, value: u64) -> String {
    const MAX_TIMEOUT_MS: u64 = 604_800_000;
    let mut contents = example();
    if field == "overall_ms" {
        contents = contents.replace("overall_ms = 60000", &format!("overall_ms = {value}"));
    } else {
        contents = contents
            .replace(
                "overall_ms = 60000",
                &format!("overall_ms = {MAX_TIMEOUT_MS}"),
            )
            .replace(
                &format!("{field} = {default}"),
                &format!("{field} = {value}"),
            );
    }
    contents
}

fn with_public_listener(mut contents: String, routes: &str) -> String {
    contents = contents.replace(
        "[listeners.admin]",
        "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
    );
    contents = contents.replace(
        "tailnet_addresses =",
        &format!("public_routes = {routes}\ntailnet_addresses ="),
    );
    contents
}

#[test]
fn public_routes_default_empty_and_tailnet_publication_remains_mandatory() {
    let config = check(example()).expect("private config validates");
    assert!(config.listeners().public().is_none());
    assert!(config.publication().public_routes().is_empty());

    let missing_tailnet = example().replace(
        "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]\n",
        "",
    );
    assert_eq!(
        check(missing_tailnet).unwrap_err(),
        "config error at publication: schema_error"
    );
}

#[test]
fn public_route_allowlist_requires_exact_capable_non_codex_routes_and_listener() {
    let routes = r#"[
  { model_alias = "private-chat", operation = "chat" },
  { model_alias = "private-transcribe", operation = "transcription" },
]"#;
    let config = check(with_public_listener(example(), routes)).expect("public policy validates");
    assert_eq!(
        config
            .listeners()
            .public()
            .expect("public listener")
            .bind()
            .to_string(),
        "172.30.0.3"
    );
    assert_eq!(
        config.listeners().public().expect("public listener").port(),
        8081
    );
    assert_eq!(config.publication().public_routes().len(), 2);
    assert_eq!(
        config.publication().public_routes()[0].model_alias.0,
        "private-chat"
    );
    assert_eq!(
        config.publication().public_routes()[0].operation,
        kanata::core::Operation::Chat
    );
    assert_eq!(
        config.publication().public_routes()[1].operation,
        kanata::core::Operation::Transcription
    );

    let no_listener = example().replace(
        "tailnet_addresses =",
        "public_routes = [{ model_alias = \"private-chat\", operation = \"chat\" }]\ntailnet_addresses =",
    );
    assert_eq!(
        check(no_listener).unwrap_err(),
        "config error at publication.public_routes[0]: public_listener_required"
    );

    let unknown = with_public_listener(
        example(),
        r#"[{ model_alias = "private-chat-extra", operation = "chat" }]"#,
    );
    assert_eq!(
        check(unknown).unwrap_err(),
        "config error at publication.public_routes[0]: unknown_route_selector"
    );

    let duplicate = with_public_listener(
        example(),
        r#"[
          { model_alias = "private-chat", operation = "chat" },
          { model_alias = "private-chat", operation = "chat" },
        ]"#,
    );
    assert_eq!(
        check(duplicate).unwrap_err(),
        "config error at publication.public_routes[1]: duplicate"
    );

    let codex = with_public_listener(
        example(),
        r#"[{ model_alias = "codex-chat", operation = "chat" }]"#,
    );
    assert_eq!(
        check(codex).unwrap_err(),
        "config error at publication.public_routes[0]: codex_not_public"
    );

    let unsupported_selector = with_public_listener(
        example(),
        r#"[{ model_alias = "remote-chat", operation = "transcription" }]"#,
    );
    assert_eq!(
        check(unsupported_selector).unwrap_err(),
        "config error at publication.public_routes[0]: unknown_route_selector"
    );
}

#[test]
fn public_listener_requires_a_concrete_separate_bind() {
    let wildcard = with_public_listener(example(), "[]")
        .replace("bind = \"172.30.0.3\"", "bind = \"0.0.0.0\"");
    assert_eq!(
        check(wildcard).unwrap_err(),
        "config error at listeners.public.bind: not_concrete"
    );

    let collision = with_public_listener(example(), "[]").replace("port = 8081", "port = 8080");
    assert_eq!(
        check(collision).unwrap_err(),
        "config error at listeners.public: client_port_collision"
    );
}

#[test]
fn public_listener_bind_is_private_or_ula_and_separate_from_tailnet_prefix() {
    for address in [
        "8.8.8.8",
        "100.64.0.11",
        "172.32.0.1",
        "169.254.1.1",
        "fe80::1",
    ] {
        let invalid = with_public_listener(example(), "[]")
            .replace("bind = \"172.30.0.3\"", &format!("bind = \"{address}\""));
        assert_eq!(
            check(invalid).unwrap_err(),
            "config error at listeners.public.bind: not_internal_address",
            "{address}"
        );
    }

    let tailnet_ula = with_public_listener(example(), "[]")
        .replace("bind = \"172.30.0.3\"", "bind = \"fd7a:115c:a1e0::11\"");
    assert_eq!(
        check(tailnet_ula).unwrap_err(),
        "config error at listeners.public.bind: tailnet_address_forbidden"
    );

    for address in [
        "10.20.30.40",
        "172.31.255.254",
        "192.168.12.34",
        "fc00::1",
        "fd12:3456::1",
    ] {
        let valid = with_public_listener(example(), "[]")
            .replace("bind = \"172.30.0.3\"", &format!("bind = \"{address}\""));
        let config = check(valid).expect("private public bind validates");
        assert_eq!(
            config
                .listeners()
                .public()
                .expect("public listener")
                .bind()
                .to_string(),
            address
        );
    }
}

#[test]
fn private_client_bind_requires_internal_or_configured_tailnet_address() {
    for address in [
        "0.0.0.0",
        "::",
        "127.0.0.1",
        "10.20.30.40",
        "172.31.255.254",
        "192.168.12.34",
        "fc00::1",
        "fd12:3456::1",
        "100.64.0.10",
        "fd7a:115c:a1e0::10",
    ] {
        let valid = example().replace("bind = \"0.0.0.0\"", &format!("bind = \"{address}\""));
        assert!(
            check(valid).is_ok(),
            "client bind {address} is accepted by config validation"
        );
    }

    for address in [
        "203.0.113.15",
        "8.8.8.8",
        "169.254.1.1",
        "fe80::1",
        "100.64.0.11",
    ] {
        let invalid = example().replace("bind = \"0.0.0.0\"", &format!("bind = \"{address}\""));
        assert_eq!(
            check(invalid).unwrap_err(),
            "config error at listeners.client.bind: not_internal_address",
            "{address}"
        );
    }
}

#[test]
fn public_and_private_ingress_require_distinct_addresses() {
    let same_address = with_public_listener(example(), "[]")
        .replace("bind = \"0.0.0.0\"", "bind = \"172.30.0.3\"");
    assert_eq!(
        check(same_address).unwrap_err(),
        "config error at listeners.public: client_bind_collision"
    );

    let same_address_and_port = with_public_listener(example(), "[]")
        .replace("bind = \"0.0.0.0\"", "bind = \"172.30.0.3\"")
        .replace("port = 8081", "port = 8080");
    assert_eq!(
        check(same_address_and_port).unwrap_err(),
        "config error at listeners.public: client_port_collision"
    );
}

#[test]
fn public_container_template_is_offline_only_and_keeps_private_tailnet_input() {
    let output = Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["check", "--config", "config/container.public.example.toml"])
        .output()
        .expect("binary runs");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"configuration valid\n");
    assert!(output.stderr.is_empty());

    let config = kanata::config::load("config/container.public.example.toml")
        .expect("public example validates");
    assert_eq!(config.listeners().client().bind().to_string(), "172.30.0.2");
    assert_eq!(
        config
            .listeners()
            .public()
            .expect("public listener")
            .bind()
            .to_string(),
        "172.29.0.2"
    );
    assert_eq!(config.publication().tailnet_addresses().len(), 1);
    assert!(config.publication().public_routes().is_empty());
    assert_eq!(
        config.codex_auth().expect("Codex auth").store(),
        kanata::config::CodexAuthStore::File
    );
    assert!(
        config
            .application_keys()
            .iter()
            .all(|key| { key.secret_ref().sha256_digest().is_some() })
    );
}

#[test]
fn rejects_provider_zone_mismatches_and_duplicate_declarations() {
    let remote_local = check(example().replace(
        "base_url = \"https://openrouter.invalid/api/v1\"\ntrust_zone = \"external\"",
        "base_url = \"http://openrouter.invalid/api/v1\"\ntrust_zone = \"local\"",
    ));
    assert_eq!(
        remote_local.unwrap_err(),
        "config error at adapters[2].trust_zone: provider_zone_mismatch"
    );
    let local_external = check(example().replace(
        "base_url = \"http://ollama.invalid:11434\"\ntrust_zone = \"local\"",
        "base_url = \"https://ollama.invalid:11434\"\ntrust_zone = \"external\"",
    ));
    assert_eq!(
        local_external.unwrap_err(),
        "config error at adapters[0].trust_zone: provider_zone_mismatch"
    );
    let operations = check(example().replace(
        "operations = [\"chat\", \"transcription\"]",
        "operations = [\"chat\", \"transcription\", \"chat\"]",
    ));
    assert_eq!(
        operations.unwrap_err(),
        "config error at adapters[1].capabilities.operations[2]: duplicate"
    );
    let permissions = check(example().replace(
        "{ model_alias = \"local-chat\", operation = \"chat\" },\n  { model_alias = \"private-chat\", operation = \"chat\" },",
        "{ model_alias = \"local-chat\", operation = \"chat\" },\n  { model_alias = \"local-chat\", operation = \"chat\" },",
    ));
    assert_eq!(
        permissions.unwrap_err(),
        "config error at application_keys[0].permissions[1]: duplicate"
    );
}

#[test]
fn codex_effort_aliases_are_exact_scoped_and_field_validated() {
    let baseline = check(example()).expect("legacy baseline validates");
    let legacy_route = baseline
        .routes()
        .iter()
        .find(|route| route.identity().route_id == "codex-chat")
        .expect("legacy Codex route");
    assert_eq!(
        legacy_route.codex_reasoning_effort(),
        Some(kanata::config::CodexReasoningEffort::Medium)
    );

    let personal_contents = personal_example();
    let personal = check(personal_contents.clone()).expect("personal template validates");
    let low_route = personal
        .routes()
        .iter()
        .find(|route| route.identity().route_id == "codex-gpt-6-luna-low")
        .expect("configured low alias");
    assert_eq!(
        low_route.identity().selector.model_alias.0,
        "gpt-6-luna:low"
    );
    assert_eq!(low_route.identity().upstream_id, "gpt-6-luna");
    assert_eq!(
        low_route.codex_reasoning_effort(),
        Some(kanata::config::CodexReasoningEffort::Low)
    );

    for (alias, effort, expected) in [
        (
            "gpt-6-luna:medium",
            "medium",
            kanata::config::CodexReasoningEffort::Medium,
        ),
        (
            "gpt-6-luna:high",
            "high",
            kanata::config::CodexReasoningEffort::High,
        ),
    ] {
        let contents = personal_contents
            .replace(
                "model_alias = \"gpt-6-luna:low\"",
                &format!("model_alias = \"{alias}\""),
            )
            .replace(
                "codex_reasoning_effort = \"low\"",
                &format!("codex_reasoning_effort = \"{effort}\""),
            );
        let config = check(contents).expect("supported suffix and field agree");
        let route = config
            .routes()
            .iter()
            .find(|route| route.identity().route_id == "codex-gpt-6-luna-low")
            .expect("configured Codex effort route");
        assert_eq!(route.codex_reasoning_effort(), Some(expected));
    }

    for effort in ["low", "medium", "high"] {
        let contents = personal_contents
            .replace(
                "model_alias = \"gpt-6-luna:low\"",
                "model_alias = \"gpt-6-luna\"",
            )
            .replace(
                "codex_reasoning_effort = \"low\"",
                &format!("codex_reasoning_effort = \"{effort}\""),
            );
        assert_eq!(
            check(contents).unwrap_err(),
            "config error at routes[9].codex_reasoning_effort: effort_requires_alias"
        );
    }

    for alias in ["gpt-6-luna:fast", "gpt-6-luna:", "gpt-6-luna:low:medium"] {
        let contents = personal_contents.replace(
            "model_alias = \"gpt-6-luna:low\"",
            &format!("model_alias = \"{alias}\""),
        );
        assert_eq!(
            check(contents).unwrap_err(),
            "config error at routes[9].model_alias: invalid_exact_alias"
        );
    }

    let missing_effort = check(personal_contents.replace("codex_reasoning_effort = \"low\"\n", ""));
    assert_eq!(
        missing_effort.unwrap_err(),
        "config error at routes[9].codex_reasoning_effort: required_for_effort_alias"
    );
    let mismatched_effort = check(personal_contents.replace(
        "model_alias = \"gpt-6-luna:low\"",
        "model_alias = \"gpt-6-luna:high\"",
    ));
    assert_eq!(
        mismatched_effort.unwrap_err(),
        "config error at routes[9].codex_reasoning_effort: does_not_match_alias"
    );

    let invalid_effort = check(personal_contents.replace(
        "codex_reasoning_effort = \"low\"",
        "codex_reasoning_effort = \"EFFORT_MARKER\"",
    ));
    let invalid_effort_error = invalid_effort.unwrap_err();
    assert_eq!(
        invalid_effort_error,
        "config error at routes[9].codex_reasoning_effort: schema_error"
    );
    assert!(!invalid_effort_error.contains("EFFORT_MARKER"));

    let non_codex_alias = check(personal_contents.replace(
        "model_alias = \"local-chat\"",
        "model_alias = \"local-chat:low\"",
    ));
    assert_eq!(
        non_codex_alias.unwrap_err(),
        "config error at routes[0].model_alias: codex_effort_alias_only"
    );

    let non_codex_effort = check(personal_contents.replace(
        "upstream_id = \"qwen3:0.6b\"\nrequires_streaming_chat",
        "upstream_id = \"qwen3:0.6b\"\ncodex_reasoning_effort = \"low\"\nrequires_streaming_chat",
    ));
    assert_eq!(
        non_codex_effort.unwrap_err(),
        "config error at routes[0].codex_reasoning_effort: codex_only"
    );

    let context = |value: &str| {
        check(personal_contents.replace(
            "upstream_id = \"qwen3:0.6b\"\nrequires_streaming_chat",
            &format!(
                "upstream_id = \"qwen3:0.6b\"\ncontext_tokens = {value}\nrequires_streaming_chat"
            ),
        ))
    };
    assert!(context("16384").is_ok());
    for value in ["255", "16777217"] {
        assert_eq!(
            context(value).unwrap_err(),
            "config error at routes[0].context_tokens: out_of_range"
        );
    }
    let transcription_context = check(personal_contents.replacen(
        "operation = \"transcription\"\n",
        "operation = \"transcription\"\ncontext_tokens = 8192\n",
        1,
    ));
    assert!(
        transcription_context
            .unwrap_err()
            .ends_with(".context_tokens: chat_only")
    );

    let transcription_effort = check(personal_contents.replace(
        "model_alias = \"gpt-6-luna:low\"\noperation = \"chat\"",
        "model_alias = \"gpt-6-luna:low\"\noperation = \"transcription\"",
    ));
    assert_eq!(
        transcription_effort.unwrap_err(),
        "config error at routes[9].codex_reasoning_effort: codex_chat_only"
    );
}

#[test]
fn digest_references_are_application_key_only_and_strictly_formatted() {
    let digest = "ab".repeat(32);
    let owner = check(example().replace("env:KANATA_CLIENT_KEY", &format!("sha256:{digest}")))
        .expect("owner digest key validates");
    assert!(
        owner.application_keys()[0]
            .secret_ref()
            .sha256_digest()
            .is_some()
    );

    let adapter =
        check(example().replace("env:KANATA_OPENROUTER_TOKEN", &format!("sha256:{digest}")))
            .unwrap_err();
    assert_eq!(
        adapter,
        "config error at adapters[2].secret_ref: digest_reference_unsupported"
    );
    assert!(!adapter.contains(&digest));

    for malformed in [
        "ab".repeat(31),
        "ab".repeat(33),
        "AB".repeat(32),
        format!("{}zz", "ab".repeat(31)),
    ] {
        let error =
            check(example().replace("env:KANATA_CLIENT_KEY", &format!("sha256:{malformed}")))
                .unwrap_err();
        assert_eq!(
            error,
            "config error at application_keys[0].secret_ref: invalid_secret_reference"
        );
        assert!(!error.contains(&malformed));
    }

    let duplicate = check(
        example()
            .replace("env:KANATA_CLIENT_KEY", &format!("sha256:{digest}"))
            .replace(
                "\n[limits]",
                &format!(
                    "\n[[application_keys]]\nid = \"second\"\nsecret_ref = \"sha256:{digest}\"\npermissions = [{{ model_alias = \"local-chat\", operation = \"chat\" }}]\n\n[limits]"
                ),
            ),
    )
    .unwrap_err();
    assert_eq!(
        duplicate,
        "config error at application_keys[1].secret_ref: duplicate_secret"
    );
    assert!(!duplicate.contains(&digest));
}

#[test]
fn logging_section_is_optional_defaulted_and_strict() {
    use kanata::config::{LogFormat, LogLevel};

    let defaults = check(example()).expect("logging defaults");
    assert_eq!(defaults.logging().level(), LogLevel::Info);
    assert_eq!(defaults.logging().format(), LogFormat::Text);
    let configured = check(format!(
        "{}\n[logging]\nlevel = \"debug\"\nformat = \"json\"\n",
        example()
    ))
    .expect("logging configured");
    assert_eq!(configured.logging().level(), LogLevel::Debug);
    assert_eq!(configured.logging().format(), LogFormat::Json);

    for (section, expected) in [
        (
            "level = \"verbose\"",
            "config error at logging.level: schema_error",
        ),
        (
            "format = \"xml\"",
            "config error at logging.format: schema_error",
        ),
        (
            "colour = true",
            "config error at logging.colour: unknown_field",
        ),
    ] {
        let error = check(format!("{}\n[logging]\n{section}\n", example())).unwrap_err();
        assert_eq!(error, expected);
    }
}

#[test]
fn apple_fm_adapters_are_local_chat_without_tools() {
    let apple = example().replacen("kind = \"ollama\"", "kind = \"apple_fm\"", 1);
    assert_eq!(
        check(apple.clone()).unwrap_err(),
        "config error at adapters[0].capabilities.function_tools: unsupported_by_adapter_kind"
    );
    let without_tools = apple
        .replacen("function_tools = true\n", "function_tools = false\n", 1)
        .replacen(
            "requires_function_tools = true",
            "requires_function_tools = false",
            1,
        );
    assert!(check(without_tools.clone()).is_ok());
    assert_eq!(
        check(
            without_tools
                .replacen("trust_zone = \"local\"", "trust_zone = \"external\"", 1)
                .replace("http://ollama.invalid", "https://ollama.invalid")
        )
        .unwrap_err(),
        "config error at adapters[0].trust_zone: provider_zone_mismatch"
    );
}

#[test]
fn planes_split_listeners_routes_keys_and_codex() {
    use kanata::config::{Plane, ProviderKind};

    let template = fs::read_to_string("config/container.public.example.toml").expect("template");
    let extra_keys = r#"
[[application_keys]]
id = "tester"
secret_ref = "sha256:1111111111111111111111111111111111111111111111111111111111111111"
permissions = [{ model_alias = "qwen3-0.6b", operation = "chat" }]

[[application_keys]]
id = "private-codex"
secret_ref = "sha256:2222222222222222222222222222222222222222222222222222222222222222"
permissions = [
  { model_alias = "qwen3-0.6b", operation = "chat" },
  { model_alias = "gpt-6-sol", operation = "chat" },
]
"#;
    let config = check(
        template.replace(
            "public_routes = []",
            "public_routes = [{ model_alias = \"qwen3-0.6b\", operation = \"chat\" }]",
        ) + extra_keys,
    )
    .expect("public template with a route");

    let private = config.for_plane(Plane::Private).expect("private plane");
    assert!(private.listeners().public().is_none());
    assert!(private.publication().public_routes().is_empty());
    assert_eq!(private.routes().len(), config.routes().len());
    assert!(private.codex_auth().is_some());

    let public = config.for_plane(Plane::Public).expect("public plane");
    assert!(public.listeners().public().is_some());
    assert!(public.listeners().client().bind().is_loopback());
    assert!(public.codex_auth().is_none());
    assert!(
        public
            .adapters()
            .iter()
            .all(|adapter| adapter.kind() != ProviderKind::Codex)
    );
    let aliases: Vec<_> = public
        .routes()
        .iter()
        .map(|route| route.identity().selector.model_alias.0.as_str())
        .collect();
    assert_eq!(aliases, ["qwen3-0.6b"]);
    // Codex-capable keys (owner or not) never enter the public process; others keep only public scopes.
    let ids: Vec<_> = public
        .application_keys()
        .iter()
        .map(|key| key.id())
        .collect();
    assert_eq!(ids, ["tester"]);
    assert!(public.application_keys().iter().all(|key| {
        !key.is_owner()
            && key
                .permissions()
                .iter()
                .all(|selector| config.publication().public_routes().contains(selector))
    }));

    // With nothing public, no key (owner or not) reaches the public process.
    let nothing_public = check(template.clone() + extra_keys).expect("empty allowlist");
    let nothing_public = nothing_public
        .for_plane(Plane::Public)
        .expect("public plane");
    assert!(nothing_public.application_keys().is_empty());
    assert!(nothing_public.routes().is_empty() && nothing_public.adapters().is_empty());

    let without_listener =
        check(fs::read_to_string("config/container.example.toml").expect("template"))
            .expect("private template");
    assert_eq!(
        without_listener
            .for_plane(Plane::Public)
            .unwrap_err()
            .to_string(),
        "config error at listeners.public: required_for_public_plane"
    );
}

#[test]
fn optional_capacity_limits_validate_bounds() {
    let adapter = "transcription_mode = \"native_asr\"\n";
    let key = "owner = true\n";
    let config = check(
        example()
            .replacen(
                adapter,
                &format!("{adapter}max_in_flight = 2\ncircuit_breaker = {{ enabled = false }}\n"),
                1,
            )
            .replacen(
                key,
                &format!(
                    "{key}max_in_flight = 1\nrate_limit = {{ requests = 4, per_ms = 300000 }}\n"
                ),
                1,
            ),
    )
    .expect("limits validate");
    let vllm = &config.adapters()[1];
    assert_eq!(vllm.max_in_flight(), Some(2));
    assert!(!vllm.circuit_breaker().enabled);
    assert_eq!(
        vllm.circuit_breaker().failures,
        5,
        "unset fields keep defaults"
    );
    assert!(
        config.adapters()[0].circuit_breaker().enabled,
        "on by default"
    );
    assert_eq!(config.application_keys()[0].max_in_flight(), Some(1));

    for (find, insert, error) in [
        (
            adapter,
            "max_in_flight = 0\n",
            "adapters[1].max_in_flight: zero",
        ),
        (
            adapter,
            "circuit_breaker = { failures = 0 }\n",
            "adapters[1].circuit_breaker.failures: zero",
        ),
        (
            key,
            "rate_limit = { requests = 1, per_ms = 0 }\n",
            "application_keys[0].rate_limit.per_ms: zero",
        ),
    ] {
        let contents = example().replacen(find, &format!("{find}{insert}"), 1);
        assert_eq!(
            check(contents).unwrap_err(),
            format!("config error at {error}")
        );
    }
}
