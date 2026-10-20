#![allow(dead_code)]
//! Unique temporary paths, directories and config files for integration tests.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use kanata::config::{ConfigError, ValidatedConfig, load};
use sha2::{Digest as _, Sha256};

/// Splits test config text from the `keys.toml` that [`load_text`] writes beside it.
pub const KEYS_FILE_MARKER: &str = "\n# --- keys.toml ---\n";
/// The `[keys]` section naming that file.
pub const KEYS_SECTION: &str = "[keys]\nfile = \"keys.toml\"\n";
/// Bearer secret of the fixture keys.
pub const TEST_KEY: &str = "test-key";

/// One `[[keys]]` record whose secret is `secret`; `fields` are its remaining TOML lines.
pub fn key_record(id: &str, secret: &str, fields: &str) -> String {
    let hex: String = Sha256::digest(secret.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "[[keys]]\nid = \"{id}\"\ndigest = \"sha256:{hex}\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n{fields}\n"
    )
}

/// A private-only ChatGPT adapter with `reasoning_control` and pinned chat routes
/// `chatgpt-chat` (medium), `gpt-6-sol` (medium) and `gpt-6-luna:low`.
pub const CHATGPT_SECTION: &str = r#"[chatgpt_auth]
state_dir = "/replace/with/private/absolute/path/kanata-chatgpt"

[[adapters]]
id = "chatgpt-private"
kind = "chatgpt"
base_url = "https://api.openai.com/v1"
trust_zone = "external"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = true
function_tools = true
reasoning_control = true

[[routes]]
id = "chatgpt-chat"
model_alias = "chatgpt-chat"
operation = "chat"
adapter_id = "chatgpt-private"
upstream_id = "gpt-5"
reasoning_effort = "medium"
requires_streaming_chat = true
requires_function_tools = true

[[routes]]
id = "chatgpt-gpt-6-sol"
model_alias = "gpt-6-sol"
operation = "chat"
adapter_id = "chatgpt-private"
upstream_id = "gpt-6-sol"
reasoning_effort = "medium"
requires_streaming_chat = true
requires_function_tools = true

[[routes]]
id = "chatgpt-gpt-6-luna-low"
model_alias = "gpt-6-luna:low"
operation = "chat"
adapter_id = "chatgpt-private"
upstream_id = "gpt-6-luna"
reasoning_effort = "low"
requires_streaming_chat = true
requires_function_tools = true

"#;

/// Template text with [`CHATGPT_SECTION`] inserted after its routes, at the
/// `# Application keys live` comment of the native and container templates.
pub fn with_chatgpt(template: &str) -> String {
    let routes_end = template
        .find("# Application keys live")
        .expect("template keys comment");
    format!(
        "{}{CHATGPT_SECTION}{}",
        &template[..routes_end],
        &template[routes_end..]
    )
}

/// `config/personal.example.toml` with [`with_chatgpt`], which makes the ChatGPT routes
/// `routes[7]` to `routes[9]`.
pub fn personal_with_chatgpt() -> String {
    with_chatgpt(
        &fs::read_to_string("config/personal.example.toml").expect("personal example reads"),
    )
}

/// `tests/fixtures/config/example.toml` with the records of its `example-keys.toml` after
/// [`KEYS_FILE_MARKER`], so text edits reach the config and its keys alike.
pub fn example_with_keys_text() -> String {
    let example =
        fs::read_to_string("tests/fixtures/config/example.toml").expect("example config reads");
    let keys =
        fs::read_to_string("tests/fixtures/config/example-keys.toml").expect("example keys read");
    let (_, records) = keys.split_once("version = 1\n").expect("keys file version");
    let config = example.replace("[keys]\nfile = \"example-keys.toml\"\n", KEYS_SECTION);
    assert_ne!(config, example);
    format!("{config}{KEYS_FILE_MARKER}{records}")
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A path under the system temp directory that is unique per process and call; nothing is created.
pub fn unique_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kanata-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Validates config text as `config.toml` in a temporary directory that is removed afterwards;
/// the directory is laid out by [`config_dir`].
pub fn load_text(contents: &str) -> Result<ValidatedConfig, ConfigError> {
    let dir = config_dir("config", contents);
    load(dir.join("config.toml"))
}

/// A scratch dir holding config text as `config.toml`. Text after [`KEYS_FILE_MARKER`] becomes
/// the sibling `keys.toml`; the directory also holds a copy of `example-keys.toml`, the keys file
/// of `tests/fixtures/config/example.toml`.
pub fn config_dir(label: &str, contents: &str) -> ScratchDir {
    let dir = ScratchDir::new(label);
    let (config, keys) = match contents.split_once(KEYS_FILE_MARKER) {
        Some((config, keys)) => (config, Some(keys)),
        None => (contents, None),
    };
    fs::write(dir.join("config.toml"), config).expect("write fixture config");
    if let Some(keys) = keys {
        fs::write(dir.join("keys.toml"), format!("version = 1\n{keys}")).expect("write keys");
    }
    fs::copy(
        "tests/fixtures/config/example-keys.toml",
        dir.join("example-keys.toml"),
    )
    .expect("copy example keys");
    dir
}

/// Like `load_text`, for text that must validate.
pub fn load_valid(contents: &str) -> ValidatedConfig {
    load_text(contents).unwrap_or_else(|error| panic!("fixture config: {error}"))
}

/// Loopback-only config text: client and admin listeners, the caller's adapters and routes in
/// `body`, one `fixture-client` key (secret [`TEST_KEY`]) granted the `permissions` array body,
/// and small limits.
pub fn fixture_text(body: &str, permissions: &str, max_audio_bytes: usize) -> String {
    let config = format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080

[listeners.admin]
bind = "127.0.0.1"
port = 18081

[publication]
tailnet_addresses = ["100.64.0.1"]

{body}
{KEYS_SECTION}
[limits]
max_queue = 2
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = {max_audio_bytes}
max_extension_bytes = 8192

[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#
    );
    let key = key_record(
        "fixture-client",
        TEST_KEY,
        &format!("permissions = [{permissions}]"),
    );
    format!("{config}{KEYS_FILE_MARKER}{key}")
}

/// Config text with client, public and admin listeners, `public_routes` (a TOML array), the
/// caller's adapter/route `body`, `keys` (`[[keys]]` records of the keys file), and small limits
/// and one-second timeouts.
pub fn compact_config(public_routes: &str, body: &str, keys: &str) -> String {
    let config = format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080
[listeners.public]
bind = "172.30.0.3"
port = 18081
[listeners.admin]
bind = "127.0.0.1"
port = 19090
[publication]
tailnet_addresses = ["100.64.0.10"]
public_routes = {public_routes}
{body}
{KEYS_SECTION}
[limits]
max_queue = 4
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = 1048576
max_extension_bytes = 1024
[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#
    );
    format!("{config}{KEYS_FILE_MARKER}{keys}")
}

/// A uniquely named `.{extension}` file under the temp directory, removed on drop.
pub struct TempFile(PathBuf);

impl TempFile {
    pub fn new(label: &str, extension: &str, contents: impl AsRef<[u8]>) -> Self {
        let path = unique_path(label).with_extension(extension);
        fs::write(&path, contents).expect("write temp file");
        Self(path)
    }

    /// The path as UTF-8, for CLI arguments and config values.
    pub fn as_str(&self) -> &str {
        self.0.to_str().expect("utf-8 temp path")
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// A unique directory, removed with its contents on drop.
pub struct ScratchDir(PathBuf);

impl ScratchDir {
    pub fn new(label: &str) -> Self {
        let path = unique_path(label);
        fs::create_dir_all(&path).expect("create scratch directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.0.join(relative)
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
