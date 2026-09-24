use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest, Sha256};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

const EXAMPLE_INLINE_KEY: &str = "[[application_keys]]
id = \"personal-client\"
secret_ref = \"env:KANATA_CLIENT_KEY\"
owner = true
permissions = [
  { model_alias = \"local-chat\", operation = \"chat\" },
  { model_alias = \"private-chat\", operation = \"chat\" },
  { model_alias = \"private-transcribe\", operation = \"transcription\" },
  { model_alias = \"remote-chat\", operation = \"chat\" },
  { model_alias = \"codex-chat\", operation = \"chat\" },
]
";

/// Temp dir with a fixture-based `config.toml`; keys live in `keys/` (created by the CLI).
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        Self::with(|config| {
            config.replace(
                EXAMPLE_INLINE_KEY,
                "[keys]\nfile = \"keys/keys.toml\"\nusage_dir = \"state\"\n",
            )
        })
    }

    /// Adds a public listener with `local-chat` as the only public route.
    fn public() -> Self {
        Self::with(|config| {
            config
                .replace(
                    EXAMPLE_INLINE_KEY,
                    "[keys]\nfile = \"keys/keys.toml\"\nusage_dir = \"state\"\n",
                )
                .replace(
                    "[listeners.admin]",
                    "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
                )
                .replace(
                    "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]",
                    "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]\npublic_routes = [{ model_alias = \"local-chat\", operation = \"chat\" }]",
                )
        })
    }

    fn with(edit: impl FnOnce(&str) -> String) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kanata-key-cli-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).expect("scratch dir");
        let example =
            fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
        let config = edit(&example);
        assert_ne!(config, example);
        fs::write(dir.join("config.toml"), config).expect("config writes");
        Self(dir)
    }

    fn config(&self) -> String {
        self.0
            .join("config.toml")
            .to_str()
            .expect("utf8")
            .to_owned()
    }

    fn keys_dir(&self) -> PathBuf {
        self.0.join("keys")
    }

    fn keys_path(&self) -> PathBuf {
        self.keys_dir().join("keys.toml")
    }

    fn keys_text(&self) -> String {
        fs::read_to_string(self.keys_path()).expect("keys file")
    }

    fn audit(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.keys_dir().join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("audit line is JSON"))
            .collect()
    }

    fn record(&self, id: &str) -> toml::Table {
        let parsed: toml::Table = toml::from_str(&self.keys_text()).expect("keys TOML");
        parsed["keys"]
            .as_array()
            .expect("keys array")
            .iter()
            .find(|key| key["id"].as_str() == Some(id))
            .expect("record")
            .as_table()
            .expect("table")
            .clone()
    }

    /// `kanata key <args> --config <config>`.
    fn key(&self, args: &[&str]) -> Output {
        kanata(&[&["key"], args, &["--config", &self.config()]].concat())
    }

    /// Creates a key and returns it.
    fn new_key(&self, id: &str, extra: &[&str]) -> String {
        let output = self.key(&[&["new", "--id", id, "--expires", "7"], extra].concat());
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output).trim_end().to_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn kanata(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(args)
        .output()
        .expect("binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf8")
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn digest_ref(key: &str) -> String {
    let hex: String = Sha256::digest(key.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

fn timestamp(record: &toml::Table, field: &str) -> u64 {
    kanata::keys::time::parse(record[field].as_str().expect("timestamp")).expect("valid")
}

fn assert_failed(output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(2), "{}", stderr(output));
    assert!(output.stdout.is_empty(), "{}", stdout(output));
    assert!(
        stderr(output).contains(message),
        "expected {message:?} in {}",
        stderr(output)
    );
}

#[test]
fn new_stores_only_the_digest_in_private_files_and_the_server_loads_it() {
    let scratch = Scratch::new();
    let output = scratch.key(&["new", "--id", "t", "--chat", "local-chat", "--expires", "7"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let key = stdout(&output).trim_end().to_owned();
    assert!(key.starts_with("kanata_sk_") && key.len() == 53);
    let notice = stderr(&output);
    assert!(notice.contains("only time"));
    assert!(notice.contains("kanata key rotate t"));
    assert!(notice.contains("~2 s"));
    assert!(!notice.contains("long-lived"));

    let record = scratch.record("t");
    assert_eq!(record["digest"].as_str(), Some(digest_ref(&key).as_str()));
    assert_eq!(
        timestamp(&record, "expires_at") - timestamp(&record, "created_at"),
        7 * 86_400
    );
    assert_eq!(mode(&scratch.keys_dir()), 0o700);
    for file in ["keys.toml", "keys.lock", "audit.jsonl"] {
        assert_eq!(mode(&scratch.keys_dir().join(file)), 0o600, "{file}");
    }
    let audit = fs::read_to_string(scratch.keys_dir().join("audit.jsonl")).expect("audit");
    assert!(!scratch.keys_text().contains(&key) && !audit.contains(&key));
    assert!(!audit.contains("sha256:"));
    let audit = scratch.audit();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["action"], "new");
    assert_eq!(audit[0]["key_id"], "t");
    assert_eq!(audit[0]["owner"], false);

    let config = kanata::config::load(scratch.config()).expect("config loads");
    let loaded = &config.application_keys()[0];
    assert_eq!(loaded.id(), "t");
    assert_eq!(loaded.permissions().len(), 1);
}

#[test]
fn key_out_is_private_create_new_and_long_expiry_warns() {
    let scratch = Scratch::new();
    let out = scratch.0.join("t.key");
    let out_arg = out.to_str().expect("utf8");
    let output = scratch.key(&[
        "new",
        "--id",
        "t",
        "--chat",
        "local-chat",
        "--expires",
        "unlimited",
        "--key-out",
        out_arg,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let key_file = fs::read_to_string(&out).expect("key file");
    let key = key_file.trim_end();
    assert_eq!(mode(&out), 0o600);
    assert!(!stdout(&output).contains(key));
    assert!(stderr(&output).contains(out_arg));
    assert!(stderr(&output).contains("warning: long-lived key"));
    assert_eq!(
        scratch.record("t")["digest"].as_str(),
        Some(digest_ref(key).as_str())
    );
    assert!(scratch.record("t").get("expires_at").is_none());

    let again = scratch.key(&[
        "new",
        "--id",
        "u",
        "--chat",
        "local-chat",
        "--expires",
        "60",
        "--key-out",
        out_arg,
    ]);
    assert_failed(&again, "already exists");
    assert_eq!(fs::read_to_string(&out).expect("unchanged"), key_file);
    assert!(!scratch.keys_text().contains("\"u\""));
}

#[test]
fn new_refuses_bad_requests_before_writing() {
    let scratch = Scratch::new();
    scratch.new_key("owner", &["--owner", "--chat", "local-chat"]);
    scratch.new_key("gone", &["--chat", "local-chat"]);
    assert!(scratch.key(&["rm", "gone"]).status.success());
    let before = scratch.keys_text();

    let base = ["new", "--expires", "7"];
    for (args, message) in [
        (
            &["--id", "gone", "--chat", "local-chat"][..],
            "already exists",
        ),
        (
            &["--id", "second", "--owner", "--chat", "local-chat"][..],
            "owner key already exists",
        ),
        (
            &["--id", "x", "--chat", "local-chta"][..],
            "did you mean \"local-chat\"?",
        ),
        (
            &["--id", "x", "--chat", "private-transcribe"][..],
            "use --transcription private-transcribe",
        ),
        (&["--id", "x"][..], "kanata routes"),
        (&["--id", "bad id", "--chat", "local-chat"][..], "key id"),
    ] {
        assert_failed(&scratch.key(&[&base[..], args].concat()), message);
    }
    assert_failed(
        &scratch.key(&["new", "--id", "x", "--chat", "local-chat", "--expires", "5"]),
        "--expires must be",
    );
    assert_failed(
        &kanata(&[
            "key",
            "new",
            "--id",
            "x",
            "--chat",
            "local-chat",
            "--expires",
            "7",
        ]),
        "requires --config",
    );
    assert_eq!(scratch.keys_text(), before);
}

#[test]
fn list_hides_revoked_marks_expired_merges_usage_and_never_prints_digests() {
    let scratch = Scratch::new();
    fs::create_dir(scratch.keys_dir()).expect("keys dir");
    let digest = |seed: &str| digest_ref(&format!("kanata_sk_SYNTHETIC_{seed}"));
    let keys = format!(
        "version = 1\n\n\
         [[keys]]\nid = \"live\"\ndigest = \"{}\"\npermissions = [{{ model_alias = \"local-chat\", operation = \"chat\" }}]\ncreated_at = \"2026-01-01T00:00:00Z\"\n\n\
         [[keys]]\nid = \"old\"\ndigest = \"{}\"\npermissions = [{{ model_alias = \"local-chat\", operation = \"chat\" }}]\ncreated_at = \"2020-01-01T00:00:00Z\"\nexpires_at = \"2020-01-02T00:00:00Z\"\n\n\
         [[keys]]\nid = \"dead\"\ndigest = \"{}\"\npermissions = [{{ model_alias = \"removed-route\", operation = \"chat\" }}]\ncreated_at = \"2026-01-01T00:00:00Z\"\nrevoked_at = \"2026-02-01T00:00:00Z\"\n",
        digest("live"),
        digest("old"),
        digest("dead")
    );
    fs::write(scratch.keys_path(), keys).expect("keys write");
    fs::set_permissions(scratch.keys_path(), fs::Permissions::from_mode(0o600)).expect("chmod");
    let state = scratch.0.join("state");
    fs::create_dir_all(state.join("public")).expect("state dirs");
    let usage = |requests: u64, at: &str| {
        format!(
            "{{\"version\":1,\"plane\":\"all\",\"keys\":{{\"live\":{{\"requests\":{requests},\"last_used_at\":\"{at}\"}}}}}}"
        )
    };
    fs::write(
        state.join("usage-all.json"),
        usage(2, "2026-03-01T00:00:00Z"),
    )
    .expect("usage");
    fs::write(
        state.join("public/usage-public.json"),
        usage(3, "2026-04-01T00:00:00Z"),
    )
    .expect("usage");

    let list = scratch.key(&["list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    let text = stdout(&list);
    assert!(text.starts_with("ID"), "{text}");
    let live = text
        .lines()
        .find(|line| line.starts_with("live"))
        .expect("live row");
    assert!(
        live.contains("2026-04-01") && live.trim_end().ends_with('5'),
        "{live}"
    );
    let old = text
        .lines()
        .find(|line| line.starts_with("old"))
        .expect("old row");
    assert!(old.contains("2020-01-02 expired"), "{old}");
    assert!(!text.contains("dead"));
    assert!(!text.contains("sha256"));

    let all = scratch.key(&["list", "--all", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&all.stdout).expect("json");
    let rows = json.as_array().expect("array");
    assert_eq!(rows.len(), 3);
    let dead = rows.iter().find(|row| row["id"] == "dead").expect("dead");
    assert_eq!(dead["revoked_at"], "2026-02-01T00:00:00Z");
    for field in [
        "owner",
        "scopes",
        "created_at",
        "expires_at",
        "last_used_at",
        "requests",
    ] {
        assert!(dead.get(field).is_some(), "{field}");
    }
    assert!(!stdout(&all).contains("sha256"));

    let show = scratch.key(&["show", "dead"]);
    assert!(show.status.success(), "{}", stderr(&show));
    assert!(stdout(&show).contains("chat:removed-route (route no longer in config)"));
    assert!(!stdout(&show).contains("sha256"));

    let by_keys = kanata(&[
        "key",
        "list",
        "--keys",
        scratch.keys_path().to_str().expect("utf8"),
    ]);
    assert!(by_keys.status.success(), "{}", stderr(&by_keys));
    assert!(
        stdout(&by_keys)
            .lines()
            .any(|line| line.starts_with("live") && line.trim_end().ends_with('-'))
    );
}

#[test]
fn rm_revokes_once_and_guards_the_owner() {
    let scratch = Scratch::new();
    scratch.new_key("owner", &["--owner", "--chat", "local-chat"]);
    scratch.new_key("t", &["--chat", "local-chat"]);

    assert_failed(&scratch.key(&["rm", "owner"]), "--force");
    assert_failed(&scratch.key(&["rm", "nobody"]), "no key");
    let output = scratch.key(&["rm", "t"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(scratch.record("t").contains_key("revoked_at"));
    let audit_lines = scratch.audit().len();

    let again = scratch.key(&["rm", "t"]);
    assert!(again.status.success());
    assert!(stdout(&again).contains("already revoked"));
    assert_eq!(scratch.audit().len(), audit_lines);
    assert!(scratch.key(&["rm", "owner", "--force"]).status.success());
    assert_eq!(scratch.audit().last().expect("line")["action"], "rm");
}

#[test]
fn rotate_replaces_only_the_secret() {
    let scratch = Scratch::new();
    let old = scratch.new_key(
        "owner",
        &[
            "--owner",
            "--chat",
            "local-chat",
            "--transcription",
            "private-transcribe",
            "--max-in-flight",
            "2",
            "--rate-limit",
            "10/1000",
        ],
    );
    let before = scratch.record("owner");
    let out = scratch.0.join("owner.key");
    fs::write(&out, "stale\n").expect("stale key file");

    let output = scratch.key(&[
        "rotate",
        "--owner",
        "--expires",
        "30",
        "--key-out",
        out.to_str().expect("utf8"),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let new_key = fs::read_to_string(&out).expect("key file");
    let after = scratch.record("owner");
    assert_ne!(after["digest"], before["digest"]);
    assert_eq!(
        after["digest"].as_str(),
        Some(digest_ref(new_key.trim_end()).as_str())
    );
    assert!(!scratch.keys_text().contains(&digest_ref(&old)));
    for field in [
        "id",
        "owner",
        "permissions",
        "max_in_flight",
        "rate_limit",
        "created_at",
    ] {
        assert_eq!(after[field], before[field], "{field}");
    }
    assert!(after.contains_key("rotated_at"));
    assert_eq!(mode(&out), 0o600);
    assert_eq!(scratch.audit().last().expect("line")["action"], "rotate");

    scratch.new_key("gone", &["--chat", "local-chat"]);
    assert!(scratch.key(&["rm", "gone"]).status.success());
    assert_failed(
        &scratch.key(&["rotate", "gone", "--expires", "7"]),
        "revoked",
    );
}

#[test]
fn edit_changes_metadata_and_guards_the_public_plane() {
    let scratch = Scratch::public();
    scratch.new_key("t", &["--chat", "local-chat"]);
    let digest = scratch.record("t")["digest"].clone();

    let output = scratch.key(&["edit", "t", "--add-chat", "private-chat"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("chat:local-chat -> chat:local-chat,chat:private-chat"));
    assert!(stderr(&output).contains("private-only"));
    assert_eq!(scratch.record("t")["digest"], digest);
    let line = scratch.audit().last().expect("line").clone();
    assert_eq!(line["action"], "edit");
    assert_eq!(line["changes"]["added"][0]["model_alias"], "private-chat");

    assert_failed(
        &scratch.key(&["edit", "t", "--add-chat", "codex-chat"]),
        "--force",
    );
    assert!(
        scratch
            .key(&["edit", "t", "--add-chat", "codex-chat", "--force"])
            .status
            .success()
    );
    assert_failed(
        &scratch.key(&[
            "edit",
            "t",
            "--remove-chat",
            "local-chat",
            "--remove-chat",
            "private-chat",
            "--remove-chat",
            "codex-chat",
        ]),
        "at least one scope",
    );

    let before = scratch.record("t");
    let output = scratch.key(&["edit", "t", "--expires", "30", "--max-in-flight", "3"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let after = scratch.record("t");
    let expires = timestamp(&after, "expires_at");
    let now = kanata::keys::time::now();
    assert!(expires > now + 29 * 86_400 && expires <= now + 30 * 86_400);
    assert_ne!(after["expires_at"], before["expires_at"]);
    assert_eq!(after["max_in_flight"].as_integer(), Some(3));
    assert_eq!(after["digest"], digest);

    assert_failed(&scratch.key(&["edit", "t"]), "kanata routes");
    assert!(scratch.key(&["rm", "t"]).status.success());
    assert_failed(
        &scratch.key(&["edit", "t", "--add-chat", "remote-chat"]),
        "revoked",
    );
}

#[test]
fn routes_lists_aliases_with_public_classification_only() {
    let scratch = Scratch::public();
    let output = kanata(&["routes", "--config", &scratch.config()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let row = |alias: &str| -> Vec<String> {
        text.lines()
            .find(|line| line.starts_with(&format!("{alias} ")))
            .expect("row")
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(row("local-chat"), ["local-chat", "chat", "ollama", "yes"]);
    assert_eq!(row("private-chat"), ["private-chat", "chat", "vllm", "no"]);
    assert_eq!(
        row("private-transcribe"),
        ["private-transcribe", "transcription", "vllm", "no"]
    );
    assert_eq!(row("codex-chat"), ["codex-chat", "chat", "codex", "never"]);
    for secret in ["llama3.2", "whisper-1", "gpt-5-codex", ".invalid", "http"] {
        assert!(!text.contains(secret), "{secret}");
    }
}

#[test]
fn migrate_moves_digest_keys_and_refuses_references() {
    let refs = Scratch::with(|config| config.to_owned() + "\n");
    let config_bytes = fs::read(refs.config()).expect("config");
    assert_failed(
        &kanata(&["key", "migrate", "--config", &refs.config()]),
        "personal-client",
    );
    assert_eq!(fs::read(refs.config()).expect("config"), config_bytes);

    let owner = digest_ref("kanata_sk_SYNTHETIC_owner");
    let friend = digest_ref("kanata_sk_SYNTHETIC_friend");
    let inline = format!(
        "[[application_keys]]\nid = \"owner\"\nsecret_ref = \"{owner}\"\nowner = true\nmax_in_flight = 2\nrate_limit = {{ requests = 5, per_ms = 1000 }}\npermissions = [{{ model_alias = \"codex-chat\", operation = \"chat\" }}]\n\n\
         [[application_keys]]\nid = \"friend\"\nsecret_ref = \"{friend}\"\npermissions = [{{ model_alias = \"local-chat\", operation = \"chat\" }}]\n"
    );
    let scratch = Scratch::with(|config| config.replace(EXAMPLE_INLINE_KEY, &inline));
    let config_bytes = fs::read(scratch.config()).expect("config");
    let before = kanata::config::load(scratch.config()).expect("inline config");

    let output = kanata(&["key", "migrate", "--config", &scratch.config()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("file = \"keys/keys.toml\""));
    assert_eq!(fs::read(scratch.config()).expect("config"), config_bytes);
    assert_eq!(scratch.audit().len(), 2);

    fs::write(
        scratch.config(),
        String::from_utf8(config_bytes.clone())
            .expect("utf8")
            .replace(&inline, "[keys]\nfile = \"keys/keys.toml\"\n"),
    )
    .expect("config writes");
    let after = kanata::config::load(scratch.config()).expect("migrated config");
    assert_eq!(
        after.application_keys().len(),
        before.application_keys().len()
    );
    for (old, new) in before
        .application_keys()
        .iter()
        .zip(after.application_keys())
    {
        assert_eq!(old.id(), new.id());
        assert_eq!(old.is_owner(), new.is_owner());
        assert_eq!(old.secret_ref(), new.secret_ref());
        assert_eq!(old.permissions(), new.permissions());
        assert_eq!(old.max_in_flight(), new.max_in_flight());
        assert_eq!(old.rate_limit(), new.rate_limit());
        assert_eq!(new.expires_at(), None);
    }

    fs::write(scratch.config(), &config_bytes).expect("restore inline config");
    assert_failed(
        &kanata(&["key", "migrate", "--config", &scratch.config()]),
        "already has keys",
    );
}

#[test]
fn migrate_resolves_a_relative_keys_path_for_the_snippet() {
    let friend = digest_ref("kanata_sk_SYNTHETIC_friend");
    let inline = format!(
        "[[application_keys]]\nid = \"friend\"\nsecret_ref = \"{friend}\"\npermissions = [{{ model_alias = \"local-chat\", operation = \"chat\" }}]\n"
    );
    let scratch = Scratch::with(|config| config.replace(EXAMPLE_INLINE_KEY, &inline));
    let inline_config = fs::read_to_string(scratch.config()).expect("config");
    fs::create_dir(scratch.0.join("conf")).expect("conf dir");
    fs::write(scratch.0.join("conf/config.toml"), &inline_config).expect("config writes");

    // cwd is the scratch root, not the config dir.
    for (keys_arg, expected) in [
        ("conf/k/keys.toml", "k/keys.toml".to_owned()),
        (
            "outside/keys.toml",
            fs::canonicalize(&scratch.0)
                .expect("canonical scratch")
                .join("outside/keys.toml")
                .display()
                .to_string(),
        ),
    ] {
        fs::write(scratch.0.join("conf/config.toml"), &inline_config).expect("reset config");
        let output = Command::new(env!("CARGO_BIN_EXE_kanata"))
            .current_dir(&scratch.0)
            .args([
                "key",
                "migrate",
                "--config",
                "conf/config.toml",
                "--keys",
                keys_arg,
            ])
            .output()
            .expect("binary runs");
        assert!(output.status.success(), "{}", stderr(&output));
        let snippet = format!("file = \"{expected}\"");
        assert!(stdout(&output).contains(&snippet), "{}", stdout(&output));
        fs::write(
            scratch.0.join("conf/config.toml"),
            inline_config.replace(&inline, &format!("[keys]\n{snippet}\n")),
        )
        .expect("config writes");
        let loaded = kanata::config::load(scratch.0.join("conf/config.toml")).expect("loads");
        assert_eq!(loaded.application_keys()[0].id(), "friend");
    }
}

/// Removes the `[[routes]]` block with `id`.
fn remove_route(scratch: &Scratch, id: &str) {
    let config = fs::read_to_string(scratch.config()).expect("config");
    let start = config
        .find(&format!("[[routes]]\nid = \"{id}\""))
        .expect("route block");
    let end = start + config[start..].find("\n\n").expect("block end") + 2;
    fs::write(
        scratch.config(),
        format!("{}{}", &config[..start], &config[end..]),
    )
    .expect("config writes");
}

#[test]
fn route_drift_allows_only_repairs_and_names_the_fix() {
    let scratch = Scratch::new();
    scratch.new_key("a", &["--chat", "local-chat", "--chat", "private-chat"]);
    scratch.new_key("b", &["--chat", "remote-chat"]);
    remove_route(&scratch, "vllm-chat");
    let one_route_removed = fs::read_to_string(scratch.config()).expect("config");
    remove_route(&scratch, "openrouter-chat");
    assert!(kanata::config::load(scratch.config()).is_err());

    // Any write that leaves a dangling scope is refused with the fixes spelled out.
    let before = scratch.keys_text();
    let output = scratch.key(&["new", "--id", "d", "--chat", "local-chat", "--expires", "7"]);
    assert_failed(
        &output,
        "key \"a\" references routes missing from the config: chat:private-chat; re-add the route",
    );
    assert!(stderr(&output).contains("`kanata key edit a --remove-chat private-chat`"));
    assert!(stderr(&output).contains("`kanata key rm b`"));
    assert_failed(
        &scratch.key(&["edit", "a", "--remove-chat", "private-chat"]),
        "key \"b\"",
    );
    assert_eq!(scratch.keys_text(), before);

    let list = scratch.key(&["list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(stdout(&list).contains("chat:private-chat(no-route)"));

    // One offender at a time: edit removing its dangling scope, then rm.
    fs::write(scratch.config(), one_route_removed).expect("config writes");
    let output = scratch.key(&["edit", "a", "--remove-chat", "private-chat"]);
    assert!(output.status.success(), "{}", stderr(&output));
    remove_route(&scratch, "openrouter-chat");
    let output = scratch.key(&["rm", "b"]);
    assert!(output.status.success(), "{}", stderr(&output));
    kanata::config::load(scratch.config()).expect("repaired keys file loads");
}

#[test]
fn concurrent_new_commands_serialize_on_the_lock() {
    let scratch = Scratch::new();
    let config = scratch.config();
    let children: Vec<_> = (0..8)
        .map(|index| {
            Command::new(env!("CARGO_BIN_EXE_kanata"))
                .args([
                    "key",
                    "new",
                    "--config",
                    &config,
                    "--id",
                    &format!("k{index}"),
                    "--chat",
                    "local-chat",
                    "--expires",
                    "7",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn")
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().expect("wait");
        assert!(output.status.success(), "{}", stderr(&output));
    }
    let bytes = fs::read(scratch.keys_path()).expect("keys");
    let keys = kanata::keys::file::parse_without_routes(&bytes).expect("valid keys file");
    assert_eq!(keys.records().len(), 8);
    let mut entries: Vec<_> = fs::read_dir(scratch.keys_dir())
        .expect("dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf8")
        })
        .collect();
    entries.sort();
    assert_eq!(entries, ["audit.jsonl", "keys.lock", "keys.toml"]);
    assert_eq!(scratch.audit().len(), 8);
}
