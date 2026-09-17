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
        if matches!(
            entry_path.file_name().and_then(|name| name.to_str()),
            Some("main.rs" | "config.rs")
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
