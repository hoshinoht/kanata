use std::fs;
use std::path::Path;

const FORBIDDEN_TERMS: &[&str] = &[
    "ollama",
    "openrouter",
    "openai",
    "cliproxy",
    "anthropic",
    "chatgpt",
    "vllm",
    "apple_fm",
    "codex",
];

/// Crate sources that must stay provider-neutral; `main.rs` and `cli.rs` are exempt.
const PROVIDER_NEUTRAL_SOURCES: &[&str] = &[
    "src",
    "crates/kanata-contract/src",
    "crates/kanata-core/src",
    "crates/kanata-keys/src",
    "crates/kanata-server/src",
];

#[test]
fn concrete_adapter_names_stay_outside_core_and_non_provider_layers() {
    for root in PROVIDER_NEUTRAL_SOURCES {
        assert_no_provider_terms(Path::new(root));
    }
    let server = dependencies("crates/kanata-server/Cargo.toml");
    assert!(
        !server.iter().any(|name| name == "kanata-adapters"),
        "kanata-server depends on kanata-adapters"
    );
}

fn assert_no_provider_terms(path: &Path) {
    for entry in fs::read_dir(path).expect("source directory is readable") {
        let entry = entry.expect("source entry is readable");
        let entry_path = entry.path();
        if entry_path.is_dir() {
            assert_no_provider_terms(&entry_path);
            continue;
        }
        if entry_path
            .extension()
            .is_none_or(|extension| extension != "rs")
        {
            continue;
        }
        if matches!(
            entry_path.file_name().and_then(|name| name.to_str()),
            Some("main.rs" | "cli.rs")
        ) {
            continue;
        }
        let source = fs::read_to_string(&entry_path)
            .expect("source file is UTF-8")
            .to_lowercase();
        for term in FORBIDDEN_TERMS {
            assert!(
                !source.contains(term),
                "{} contains concrete adapter term {term}",
                entry_path.display()
            );
        }
    }
}

/// Names under a manifest's `[dependencies]` table.
fn dependencies(manifest: &str) -> Vec<String> {
    let text = fs::read_to_string(manifest).expect("manifest is readable");
    let manifest: toml::Table = toml::from_str(&text).expect("manifest is TOML");
    manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default()
}

const NETWORK_TERMS: &[&str] = &[
    "tokio",
    "hyper",
    "axum",
    "std::net",
    "TcpStream",
    "UdpSocket",
];

const NETWORK_CRATES: &[&str] = &["tokio", "hyper", "axum", "http", "tower", "rustls"];

/// Key-file and key-store sources: the keys crate and the `keys.toml` schema in config.
fn key_sources() -> Vec<String> {
    let mut files = Vec::new();
    collect_rs(Path::new("crates/kanata-keys/src"), &mut files);
    collect_rs(Path::new("crates/kanata-config/src/keys"), &mut files);
    files
}

#[test]
fn key_management_files_have_no_network_path() {
    let keys = dependencies("crates/kanata-keys/Cargo.toml");
    for name in NETWORK_CRATES {
        assert!(
            !keys.iter().any(|dependency| dependency == name),
            "kanata-keys depends on network crate {name}"
        );
    }
    for path in key_sources() {
        let source = fs::read_to_string(&path).expect("key module is readable");
        for term in NETWORK_TERMS {
            assert!(
                !source.contains(term),
                "{path} contains network term {term}"
            );
        }
    }
}

#[test]
fn server_code_never_reaches_the_key_write_path() {
    // Every key module except the write path itself and the crate root declaring it.
    let mut files = key_sources();
    files.retain(|path| {
        !matches!(
            path.as_str(),
            "crates/kanata-keys/src/lib.rs"
                | "crates/kanata-keys/src/store.rs"
                | "crates/kanata-keys/src/cli.rs"
        ) && !path.starts_with("crates/kanata-keys/src/cli/")
    });
    files.push("src/serve.rs".into());
    collect_rs(Path::new("src/serve"), &mut files);
    collect_rs(Path::new("crates/kanata-server/src"), &mut files);
    for path in files {
        let source = fs::read_to_string(&path).expect("source file is UTF-8");
        let imports_write_path = source.contains("keys::store")
            || source.contains("keys::cli")
            || source.match_indices("keys::{").any(|(index, _)| {
                let group = &source[index..];
                let group = &group[..group.find('}').unwrap_or(group.len())];
                group.contains("store") || group.contains("cli")
            })
            || source.contains("super::store")
            || source.contains("super::cli");
        assert!(!imports_write_path, "{path} references the key write path");
    }
}

fn collect_rs(dir: &Path, files: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("source directory is readable") {
        let path = entry.expect("source entry is readable").path();
        if path.is_dir() {
            collect_rs(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path.to_string_lossy().into_owned());
        }
    }
}
