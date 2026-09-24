use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

fn key_new(extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["key", "new", "--id", "friend", "--chat", "local-chat"])
        .args(extra)
        .output()
        .expect("binary runs")
}

fn valid_key(key: &str) -> bool {
    key.len() == 53
        && key.strip_prefix("kanata_sk_").is_some_and(|rest| {
            rest.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
}

fn block_digest(block: &str) -> String {
    let parsed: toml::Table = toml::from_str(block).expect("block is TOML");
    let key = &parsed["application_keys"].as_array().expect("array")[0];
    assert_eq!(key["id"].as_str(), Some("friend"));
    assert!(key.get("owner").is_none());
    let permission = &key["permissions"].as_array().expect("permissions")[0];
    assert_eq!(permission["model_alias"].as_str(), Some("local-chat"));
    assert_eq!(permission["operation"].as_str(), Some("chat"));
    key["secret_ref"]
        .as_str()
        .and_then(|value| value.strip_prefix("sha256:"))
        .expect("digest reference")
        .into()
}

fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn printed_key_matches_the_digest_in_a_loadable_block() {
    let output = key_new(&[]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let mut lines = stdout.lines();
    assert!(lines.next().expect("label").contains("shown once"));
    let key = lines.next().expect("key line");
    assert!(valid_key(key), "unexpected key shape");
    let block = stdout.split_once("\n\n").expect("block follows key").1;
    assert_eq!(block_digest(block), sha256_hex(key));

    let example = fs::read_to_string("tests/fixtures/config/example.toml").expect("example");
    let path = std::env::temp_dir().join(format!("kanata-key-cli-{}.toml", std::process::id()));
    fs::write(&path, format!("{example}\n{block}")).expect("config writes");
    let loaded = kanata::config::load(&path);
    fs::remove_file(&path).expect("config removes");
    loaded.expect("generated block validates");
}

#[test]
fn key_out_writes_private_file_once_and_keeps_key_off_stdout() {
    let directory = std::env::temp_dir().join(format!("kanata-key-out-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir(&directory).expect("directory");
    let path = directory.join("friend.key");
    let path_arg = path.to_str().expect("utf8 path");

    let output = key_new(&["--key-out", path_arg]);
    assert!(output.status.success());
    let key_file = fs::read_to_string(&path).expect("key file");
    let key = key_file.strip_suffix('\n').expect("trailing newline");
    assert!(valid_key(key));
    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o600
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    assert!(!stdout.contains(key));
    assert_eq!(block_digest(&stdout), sha256_hex(key));

    let again = key_new(&["--key-out", path_arg]);
    assert_eq!(again.status.code(), Some(2));
    assert!(again.stdout.is_empty());
    assert_eq!(fs::read_to_string(&path).expect("unchanged"), key_file);
    fs::remove_dir_all(&directory).expect("cleanup");
}

#[test]
fn invalid_requests_fail_without_generating_output() {
    for args in [
        vec!["key", "new", "--id", "friend"],
        vec!["key", "new", "--id", "bad id", "--chat", "local-chat"],
        vec!["key", "new", "--id", "friend", "--chat", "local-chat:max"],
        vec!["key", "new", "--id", "friend", "--chat"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_kanata"))
            .args(&args)
            .output()
            .expect("binary runs");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
    }
}
