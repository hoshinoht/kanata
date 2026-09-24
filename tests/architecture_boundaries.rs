use std::fs;
use std::path::Path;

const FORBIDDEN_TERMS: &[&str] = &["ollama", "openrouter", "codex", "openai"];

#[test]
fn concrete_adapter_names_stay_outside_core_and_non_provider_layers() {
    assert_no_provider_terms(Path::new("src"));
}

fn assert_no_provider_terms(path: &Path) {
    for entry in fs::read_dir(path).expect("source directory is readable") {
        let entry = entry.expect("source entry is readable");
        let entry_path = entry.path();
        if entry_path.is_dir() {
            if entry_path != Path::new("src/adapter") {
                assert_no_provider_terms(&entry_path);
            }
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
            Some("main.rs" | "config.rs" | "cli.rs")
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

const NETWORK_TERMS: &[&str] = &[
    "tokio",
    "hyper",
    "axum",
    "std::net",
    "TcpStream",
    "UdpSocket",
];

#[test]
fn key_management_files_have_no_network_path() {
    for file in ["cli", "store", "file", "time", "usage"] {
        let path = format!("src/keys/{file}.rs");
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
    let mut files = vec![
        "src/keys/reload.rs".into(),
        "src/keys/usage.rs".into(),
        "src/serve.rs".into(),
    ];
    collect_rs(Path::new("src/server"), &mut files);
    collect_rs(Path::new("src/api"), &mut files);
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
