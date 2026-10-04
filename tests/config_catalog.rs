#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use kanata::config::{self, KeySource, Plane};
use toml::Value;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new(contents: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "kanata-catalog-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("keys")).unwrap();
        fs::create_dir_all(path.join("state")).unwrap();
        fs::write(path.join("keys/keys.toml"), "version = 1\nkeys = []\n").unwrap();
        fs::set_permissions(
            path.join("keys/keys.toml"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::write(path.join("config.toml"), contents).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("config.toml")
    }
    fn load(&self) -> Result<config::ValidatedConfig, String> {
        config::load(self.path()).map_err(|e| e.to_string())
    }
    fn write(&self, name: &str, value: &Value) {
        fs::write(self.0.join(name), toml::to_string(value).unwrap()).unwrap();
    }
    fn command(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kanata"))
            .current_dir(&self.0)
            .args(arguments)
            .output()
            .unwrap()
    }
    fn export(&self, action: &str, output: &str) {
        let result = self.command(&[
            "config",
            action,
            "--config",
            "config.toml",
            "--output",
            output,
        ]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn parse(contents: &str) -> Value {
    toml::from_str(contents).unwrap()
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn catalog(contents: &str) -> Value {
    let mut root = parse(contents);
    root.as_table_mut().unwrap().remove("routes");
    root
}

#[test]
fn templates_roundtrip_with_stable_ids_policies_and_no_provider_io() {
    for template in [
        "personal",
        "container",
        "container.public",
        "chatgpt",
        "compact",
        "vision",
        "speech",
        "embeddings",
    ] {
        let contents = fs::read_to_string(format!("config/{template}.example.toml")).unwrap();
        let fixture = Fixture::new(&contents);
        fixture.export("compact", "compact.toml");
        fixture.export("expand", "expanded.toml");
        for output in ["compact.toml", "expanded.toml"] {
            let plan = success(fixture.command(&[
                "config",
                "plan",
                "--config",
                output,
                "--against",
                "config.toml",
            ]));
            assert_eq!(plan, "no configuration changes\n", "{template}: {output}");
            assert_eq!(
                fs::metadata(fixture.0.join(output))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(fs::read_to_string(fixture.path()).unwrap(), contents);
        let result = fixture.command(&[
            "config",
            "compact",
            "--config",
            "config.toml",
            "--output",
            "compact.toml",
        ]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("already_exists"));
    }
}

#[test]
fn families_expand_only_explicit_efforts_and_preserve_custom_ids_and_exact_grants() {
    let mut root = catalog(include_str!("../config/chatgpt.example.toml"));
    let added = parse(
        r#"
        [route_profiles.account]
        adapter_id = "chatgpt-private"
        requires_streaming_chat = true
        requires_function_tools = true
        default_effort = "medium"
        efforts = ["low", "high"]
        reasoning_summary = "auto"

        [models.chat.assistant]
        profile = "account"
        upstream_id = "fixture-account-model"
        id = "stable-base"
        route_ids = { low = "old-low-id" }
    "#,
    );
    root.as_table_mut()
        .unwrap()
        .extend(added.as_table().unwrap().clone());
    let fixture = Fixture::new(&toml::to_string(&root).unwrap());
    fs::write(
        fixture.0.join("keys/keys.toml"),
        format!(
            r#"
        version = 1
        [[keys]]
        id = "base-only"
        digest = "sha256:{}"
        permissions = [{{ model_alias = "assistant", operation = "chat" }}]
        created_at = "2026-01-01T00:00:00Z"
    "#,
            "11".repeat(32)
        ),
    )
    .unwrap();
    let config = fixture.load().unwrap();
    let routes: Vec<_> = config
        .routes()
        .iter()
        .map(|r| {
            (
                r.identity().route_id.as_str(),
                r.identity().selector.model_alias.0.as_str(),
                r.pinned_reasoning_effort(),
            )
        })
        .collect();
    assert_eq!(
        routes,
        vec![
            ("stable-base", "assistant", Some("medium")),
            ("old-low-id", "assistant:low", Some("low")),
            ("stable-base-high", "assistant:high", Some("high"))
        ]
    );
    assert!(
        config
            .routes()
            .iter()
            .all(|r| r.reasoning_summary() == Some(config::ReasoningSummary::Auto))
    );
    assert_eq!(config.application_keys()[0].permissions().len(), 1);
    assert_eq!(
        config.application_keys()[0].permissions()[0].model_alias.0,
        "assistant"
    );
    fixture.export("compact", "compact.toml");
    assert_eq!(
        success(fixture.command(&[
            "config",
            "plan",
            "--config",
            "compact.toml",
            "--against",
            "config.toml"
        ])),
        "no configuration changes\n"
    );
}

#[test]
fn overrides_replace_booleans_lists_and_optional_values() {
    let mut root = catalog(include_str!("../config/vision.example.toml"));
    root["adapters"][0].as_table_mut().unwrap().insert(
        "extension_allowlist".into(),
        Value::Array(vec!["test.first".into(), "test.second".into()]),
    );
    let added = parse(
        r#"
        [route_profiles.vision]
        adapter_id = "ollama-vision"
        requires_streaming_chat = true
        allows_input_images = true
        context_tokens = 8192
        extension_allowlist = ["test.first", "test.second"]
        [models.chat.first]
        profile = "vision"
        upstream_id = "fixture-vision"
        [models.chat.second]
        profile = "vision"
        upstream_id = "fixture-vision"
        [models.chat.third]
        profile = "vision"
        upstream_id = "fixture-text"
        requires_streaming_chat = false
        allows_input_images = false
        extension_allowlist = []
        unset = ["context_tokens"]
        [models.chat.fourth]
        profile = "vision"
        upstream_id = "fixture-vision"
        extension_allowlist = ["test.second"]
    "#,
    );
    root.as_table_mut()
        .unwrap()
        .extend(added.as_table().unwrap().clone());
    let fixture = Fixture::new(&toml::to_string(&root).unwrap());
    let config = fixture.load().unwrap();
    let third = config
        .routes()
        .iter()
        .find(|r| r.identity().selector.model_alias.0 == "third")
        .unwrap();
    assert!(!third.requires_streaming_chat() && !third.allows_input_images());
    assert!(third.extension_allowlist().is_empty());
    assert_eq!(third.context_tokens(), None);
    let fourth = config
        .routes()
        .iter()
        .find(|r| r.identity().selector.model_alias.0 == "fourth")
        .unwrap();
    assert_eq!(fourth.extension_allowlist().len(), 1);
    fixture.export("compact", "compact.toml");
    assert_eq!(
        success(fixture.command(&[
            "config",
            "plan",
            "--config",
            "compact.toml",
            "--against",
            "config.toml"
        ])),
        "no configuration changes\n"
    );
}

#[test]
fn fragment_definitions_compose_with_root_paths_and_legacy_routes() {
    let mut root = parse(include_str!("../config/personal.example.toml"));
    let mut adapters = toml::Table::new();
    adapters.insert(
        "adapters".into(),
        root.as_table_mut().unwrap().remove("adapters").unwrap(),
    );
    let fixture = Fixture::new(&toml::to_string(&root).unwrap());
    fs::create_dir(fixture.0.join("catalogs")).unwrap();
    fixture.write("catalogs/adapters.toml", &adapters.into());
    root.as_table_mut().unwrap().insert(
        "include".into(),
        Value::Array(vec![
            "catalogs/adapters.toml".into(),
            "catalogs/models.toml".into(),
        ]),
    );
    fixture.write(
        "catalogs/models.toml",
        &parse(
            r#"
        [route_profiles.local]
        adapter_id = "ollama-local"
        requires_streaming_chat = true
        [models.chat.extra]
        profile = "local"
        upstream_id = "fixture-local"
    "#,
        ),
    );
    fixture.write("config.toml", &root);
    let config = fixture.load().unwrap();
    assert_eq!(config.routes().len(), 11);
    match config.key_source() {
        KeySource::File {
            path, usage_dir, ..
        } => {
            assert_eq!(path, &fixture.0.join("keys/keys.toml"));
            assert_eq!(usage_dir.as_ref().unwrap(), &fixture.0.join("state"));
        }
        _ => panic!("file keys"),
    }
    root.as_table_mut().unwrap().insert(
        "include".into(),
        Value::Array(vec![
            "catalogs/models.toml".into(),
            "catalogs/adapters.toml".into(),
        ]),
    );
    fixture.write("reordered.toml", &root);
    assert_eq!(
        success(fixture.command(&[
            "config",
            "plan",
            "--config",
            "reordered.toml",
            "--against",
            "config.toml"
        ])),
        "no configuration changes\n"
    );
    fs::create_dir(fixture.0.join("output")).unwrap();
    fixture.export("expand", "output/expanded.toml");
    fixture.export("compact", "output/compact.toml");
    let exported = parse(&fs::read_to_string(fixture.0.join("output/compact.toml")).unwrap());
    assert_eq!(
        Path::new(exported["keys"]["file"].as_str().unwrap()),
        fs::canonicalize(fixture.0.join("keys/keys.toml")).unwrap()
    );
    assert!(exported.get("include").is_none());
}

#[test]
fn compact_sorts_families_without_duplicating_variants_declared_before_the_base() {
    let mut root = parse(include_str!("../config/chatgpt.example.toml"));
    root["routes"].as_array_mut().unwrap().reverse();
    let fixture = Fixture::new(&toml::to_string(&root).unwrap());
    fixture.export("compact", "compact.toml");
    let compact = parse(&fs::read_to_string(fixture.0.join("compact.toml")).unwrap());
    assert_eq!(compact["models"]["chat"].as_table().unwrap().len(), 1);
    assert_eq!(
        config::load(fixture.0.join("compact.toml"))
            .unwrap()
            .routes()
            .len(),
        2
    );
}

#[test]
fn compact_preserves_explicit_thinking_false_and_output_caps() {
    let mut root = parse(include_str!("../config/personal.example.toml"));
    let adapter = root["adapters"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|adapter| adapter["id"].as_str() == Some("vllm-text-a"))
        .unwrap();
    adapter["capabilities"]
        .as_table_mut()
        .unwrap()
        .insert("sampling_controls".into(), true.into());
    let routes = root["routes"].as_array_mut().unwrap();
    let route = routes
        .iter_mut()
        .find(|route| route["adapter_id"].as_str() == Some("vllm-text-a"))
        .unwrap();
    route.as_table_mut().unwrap().extend([
        ("enable_thinking".into(), false.into()),
        ("context_tokens".into(), 8192.into()),
        ("max_output_tokens".into(), 1024.into()),
    ]);
    let mut second = route.clone();
    second["id"] = "second-text-route".into();
    second["model_alias"] = "second-text".into();
    routes.push(second);
    let fixture = Fixture::new(&toml::to_string(&root).unwrap());
    fixture.export("compact", "compact.toml");
    let compact = config::load(fixture.0.join("compact.toml")).unwrap();
    let route = compact
        .routes()
        .iter()
        .find(|route| route.identity().selector.model_alias.0 == "second-text")
        .unwrap();
    assert_eq!(route.enable_thinking(), Some(false));
    assert_eq!(route.context_tokens(), Some(8192));
    assert_eq!(route.max_output_tokens(), Some(1024));
}

#[test]
fn catalog_errors_are_strict_and_source_oriented_without_echoing_secrets() {
    let base = catalog(include_str!("../config/chatgpt.example.toml"));
    let good = parse(
        r#"
        [route_profiles.account]
        adapter_id = "chatgpt-private"
        default_effort = "medium"
        efforts = ["low", "high"]
        [models.chat.assistant]
        profile = "account"
        upstream_id = "fixture-account-model"
    "#,
    );
    let mut valid = base;
    valid
        .as_table_mut()
        .unwrap()
        .extend(good.as_table().unwrap().clone());
    for (path, value, class) in [
        (
            vec!["models", "chat", "assistant", "profile"],
            "missing".into(),
            "missing_profile",
        ),
        (
            vec!["models", "chat", "assistant", "mystery"],
            "secret-value".into(),
            "unknown_field",
        ),
        (
            vec!["models", "chat", "assistant", "unset"],
            Value::Array(vec!["context_tokens".into(), "context_tokens".into()]),
            "duplicate",
        ),
        (
            vec!["models", "chat", "assistant", "efforts"],
            Value::Array(vec!["low".into(), "low".into()]),
            "duplicate",
        ),
        (
            vec!["models", "chat", "assistant", "efforts"],
            Value::Array(vec![]),
            "empty",
        ),
        (
            vec!["models", "chat", "assistant", "route_ids"],
            parse("wrong = 'old-id'"),
            "unknown_variant",
        ),
        (
            vec!["models", "chat", "assistant", "context_tokens"],
            1.into(),
            "out_of_range",
        ),
        (
            vec!["models", "chat", "assistant", "reasoning_effort"],
            "low".into(),
            "conflicting_effort_options",
        ),
        (
            vec!["route_profiles", "account", "profile"],
            "account".into(),
            "model_only_field",
        ),
    ] {
        let mut root = valid.clone();
        let mut target = &mut root;
        for part in &path[..path.len() - 1] {
            target = &mut target[*part];
        }
        target
            .as_table_mut()
            .unwrap()
            .insert(path[path.len() - 1].into(), value);
        let fixture = Fixture::new(&toml::to_string(&root).unwrap());
        let error = fixture.load().unwrap_err();
        assert!(error.ends_with(class), "{error}");
        assert!(!error.contains("secret-value"));
        if class == "out_of_range" {
            assert!(error.contains("models.chat.assistant.context_tokens"));
        }
    }
    let mut conflict = valid.clone();
    conflict["models"]["chat"]["assistant"]
        .as_table_mut()
        .unwrap()
        .insert("unset".into(), Value::Array(vec!["context_tokens".into()]));
    conflict["models"]["chat"]["assistant"]
        .as_table_mut()
        .unwrap()
        .insert("context_tokens".into(), 8192.into());
    assert!(
        Fixture::new(&toml::to_string(&conflict).unwrap())
            .load()
            .unwrap_err()
            .ends_with("conflicting_override")
    );
    let mut collision = valid;
    collision.as_table_mut().unwrap().insert(
        "routes".into(),
        parse(include_str!("../config/chatgpt.example.toml"))["routes"].clone(),
    );
    collision["models"]["chat"]["assistant"]
        .as_table_mut()
        .unwrap()
        .insert("id".into(), "chatgpt-chat".into());
    assert!(
        Fixture::new(&toml::to_string(&collision).unwrap())
            .load()
            .unwrap_err()
            .ends_with("invalid_or_duplicate_id")
    );
}

#[test]
fn fragments_reject_overrides_recursion_duplicates_and_path_escape() {
    for (fragment, includes, expected) in [
        (
            "[listeners.client]\nbind='127.0.0.1'\nport=8081",
            vec!["fragment.toml"],
            "unknown_field",
        ),
        (
            "include=['fragment.toml']",
            vec!["fragment.toml"],
            "unknown_field",
        ),
        (
            "",
            vec!["fragment.toml", "fragment.toml"],
            "duplicate_fragment",
        ),
        ("", vec!["../fragment.toml"], "relative_fragment_required"),
        ("", vec!["config.toml"], "duplicate_fragment"),
    ] {
        let mut root = parse(include_str!("../config/embeddings.example.toml"));
        root.as_table_mut().unwrap().insert(
            "include".into(),
            Value::Array(includes.into_iter().map(Value::from).collect()),
        );
        let fixture = Fixture::new(&toml::to_string(&root).unwrap());
        fs::write(fixture.0.join("fragment.toml"), fragment).unwrap();
        let error = fixture.load().unwrap_err();
        assert!(error.ends_with(expected), "{error}");
        assert!(error.contains("include["));
    }
    let mut root = parse(include_str!("../config/embeddings.example.toml"));
    root.as_table_mut()
        .unwrap()
        .insert("include".into(), Value::Array(vec!["fragment.toml".into()]));
    let fixture = Fixture::new(&toml::to_string(&root).unwrap());
    symlink("/etc/hosts", fixture.0.join("fragment.toml")).unwrap();
    assert!(
        fixture
            .load()
            .unwrap_err()
            .ends_with("outside_config_directory")
    );
}

#[test]
fn public_allowlist_remains_exact_and_account_families_stay_private() {
    let mut root = parse(include_str!("../config/container.public.example.toml"));
    let fixture = Fixture::new(&toml::to_string(&root).unwrap());
    fixture.export("compact", "compact.toml");
    let compact = config::load(fixture.0.join("compact.toml")).unwrap();
    assert!(
        compact
            .for_plane(Plane::Public)
            .unwrap()
            .routes()
            .is_empty()
    );
    root["publication"].as_table_mut().unwrap().insert(
        "public_routes".into(),
        Value::Array(vec![parse("model_alias='gpt-5.5'\noperation='chat'")]),
    );
    fixture.write("config.toml", &root);
    let error = fixture.load().unwrap_err();
    assert!(
        error.ends_with("codex_not_public") || error.ends_with("unknown_route_selector"),
        "{error}"
    );
    let mut root = parse(&fs::read_to_string(fixture.0.join("compact.toml")).unwrap());
    let private = root["models"]["chat"]
        .as_table()
        .unwrap()
        .keys()
        .find(|name| name.starts_with("gpt-"))
        .unwrap()
        .clone();
    root["publication"].as_table_mut().unwrap().insert(
        "public_routes".into(),
        Value::Array(vec![parse(&format!(
            "model_alias='{private}'\noperation='chat'"
        ))]),
    );
    fixture.write("config.toml", &root);
    assert!(fixture.load().unwrap_err().ends_with("codex_not_public"));
}

#[test]
fn plan_reports_field_changes_publication_restart_and_dangling_grants_without_credentials() {
    let mut root = parse(include_str!("../config/container.public.example.toml"));
    let fixture = Fixture::new(&toml::to_string(&root).unwrap());
    let alias = root["routes"][0]["model_alias"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(
        fixture.0.join("keys/keys.toml"),
        format!(
            r#"
        version = 1
        [[keys]]
        id = "local-client"
        digest = "sha256:{}"
        permissions = [{{ model_alias = "{alias}", operation = "chat" }}]
        created_at = "2026-01-01T00:00:00Z"
    "#,
            "22".repeat(32)
        ),
    )
    .unwrap();
    root["routes"][0]
        .as_table_mut()
        .unwrap()
        .insert("context_tokens".into(), 8192.into());
    root["limits"]
        .as_table_mut()
        .unwrap()
        .insert("max_queue".into(), 12.into());
    root["timeouts"]
        .as_table_mut()
        .unwrap()
        .insert("queue_ms".into(), 1200.into());
    root["publication"].as_table_mut().unwrap().insert(
        "public_routes".into(),
        Value::Array(vec![parse(&format!(
            "model_alias='{alias}'\noperation='chat'"
        ))]),
    );
    root["adapters"][0].as_table_mut().unwrap().insert(
        "base_url".into(),
        "http://fixture-secret-host.invalid:11434".into(),
    );
    fixture.write("candidate.toml", &root);
    let plan = success(fixture.command(&[
        "config",
        "plan",
        "--config",
        "candidate.toml",
        "--against",
        "config.toml",
    ]));
    assert!(plan.contains("context_tokens") && plan.contains("+ public chat"));
    assert!(
        plan.contains("limits: restart required") && plan.contains("timeouts: restart required")
    );
    assert!(plan.contains("base_url"));
    assert!(!plan.contains("fixture-secret-host") && !plan.contains(&"22".repeat(32)));
    root["publication"]
        .as_table_mut()
        .unwrap()
        .insert("public_routes".into(), Value::Array(vec![]));
    root["routes"].as_array_mut().unwrap().remove(0);
    fixture.write("candidate.toml", &root);
    let plan = success(fixture.command(&[
        "config",
        "plan",
        "--config",
        "candidate.toml",
        "--against",
        "config.toml",
    ]));
    assert!(plan.contains("- route chat"));
    assert!(plan.contains("! key local-client: grant targets missing route"));
    assert!(config::load(fixture.0.join("candidate.toml")).is_err());
}
