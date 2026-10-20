#![allow(dead_code)]
//! Unique temporary paths, directories and config files for integration tests.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use kanata::config::{ConfigError, ValidatedConfig, load};

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A path under the system temp directory that is unique per process and call; nothing is created.
pub fn unique_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kanata-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Validates config text through a temporary file that is removed afterwards.
pub fn load_text(contents: &str) -> Result<ValidatedConfig, ConfigError> {
    let path = unique_path("config").with_extension("toml");
    fs::write(&path, contents).expect("write fixture config");
    let result = load(&path);
    let _ = fs::remove_file(path);
    result
}

/// Like `load_text`, for text that must validate.
pub fn load_valid(contents: &str) -> ValidatedConfig {
    load_text(contents).unwrap_or_else(|error| panic!("fixture config: {error}"))
}

/// Loopback-only config text: client and admin listeners, the caller's adapters and routes in
/// `body`, one `fixture-client` key granted the `permissions` array body, and small limits.
pub fn fixture_text(body: &str, permissions: &str, max_audio_bytes: usize) -> String {
    format!(
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
[[application_keys]]
id = "fixture-client"
secret_ref = "env:KANATA_TEST_KEY"
permissions = [{permissions}]

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
    )
}

/// Config text with client, public and admin listeners, `public_routes` (a TOML array), the
/// caller's adapter/route/key `body`, and small limits and one-second timeouts.
pub fn compact_config(public_routes: &str, body: &str) -> String {
    format!(
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
    )
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
