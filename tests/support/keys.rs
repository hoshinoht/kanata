#![allow(dead_code)]
//! Keys-file fixtures shared by the key lifecycle suites.

use std::{fs, path::Path};

use sha2::{Digest as _, Sha256};

/// The schema fixture's `[keys]` section, replaced by `keys_section` in keyed configs.
pub const EXAMPLE_KEYS_SECTION: &str = "[keys]\nfile = \"example-keys.toml\"\n";

/// The schema fixture with its `[keys]` section replaced by `keys_section`; `public` adds a
/// public listener publishing `local-chat`.
pub fn keyed_example(keys_section: &str, public: bool) -> String {
    let example = fs::read_to_string("tests/fixtures/config/example.toml").expect("example config");
    let mut config = example.replace(EXAMPLE_KEYS_SECTION, keys_section);
    assert_ne!(config, example);
    if public {
        config = config
            .replace(
                "[listeners.admin]",
                "[listeners.public]\nbind = \"172.30.0.3\"\nport = 8081\n\n[listeners.admin]",
            )
            .replace(
                "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]",
                "tailnet_addresses = [\"100.64.0.10\", \"fd7a:115c:a1e0::10\"]\npublic_routes = [{ model_alias = \"local-chat\", operation = \"chat\" }]",
            );
        assert!(config.contains("public_routes"));
    }
    config
}

/// The synthetic bearer secret for key `id`.
pub fn token(id: &str) -> String {
    format!("kanata_sk_SYNTHETIC_{id}")
}

/// One `[[keys]]` record whose secret is `token(id)`; `permissions` is the inline array body.
pub fn record(id: &str, permissions: &str, extra: &str) -> String {
    let digest: [u8; 32] = Sha256::digest(token(id).as_bytes()).into();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "[[keys]]\nid = \"{id}\"\ndigest = \"sha256:{hex}\"\ncreated_at = \"2026-01-01T00:00:00Z\"\npermissions = [{permissions}]\n{extra}\n"
    )
}

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}
