use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kanata::adapter::codex::auth::{Credential, CredentialStore, StoreError};
use kanata::config::CodexAuthStore;

const HELPER_MODE: &str = "KANATA_CODEX_LOCK_HELPER_MODE";
const HELPER_DIR: &str = "KANATA_CODEX_LOCK_HELPER_DIR";
const HELPER_TIMEOUT_MS: &str = "KANATA_CODEX_LOCK_HELPER_TIMEOUT_MS";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempState(PathBuf);

impl TempState {
    fn new() -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        let path = base.join(format!(
            "kanata-codex-lock-test-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create synthetic temp directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("secure synthetic temp directory");
        Self(path)
    }

    fn store(&self) -> CredentialStore {
        CredentialStore::new(CodexAuthStore::File, &self.0)
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn competing_process_times_out_cancels_wait_and_then_acquires_lock() {
    let temp = TempState::new();
    let store = temp.store();
    let held = store
        .lock(Duration::from_secs(1))
        .await
        .expect("parent lock");

    let started = Instant::now();
    run_helper(&temp.0, "timeout", 120);
    assert!(started.elapsed() >= Duration::from_millis(80));

    run_helper(&temp.0, "cancel", 10_000);
    drop(held);

    run_helper(&temp.0, "acquire", 2_000);
}

#[tokio::test]
async fn record_rotation_and_logout_keep_the_same_lock_inode() {
    let temp = TempState::new();
    let store = temp.store();

    let first = store
        .lock(Duration::from_secs(1))
        .await
        .expect("first lock");
    let inode = lock_inode(&temp.0);
    first
        .save(&Credential::new("TEST_ONLY_REFRESH_OLD", "TEST_ONLY_ACCOUNT").unwrap())
        .expect("initial save");
    drop(first);

    let second = store
        .lock(Duration::from_secs(1))
        .await
        .expect("rotation lock");
    assert_eq!(lock_inode(&temp.0), inode);
    second
        .save(&Credential::new("TEST_ONLY_REFRESH_NEW", "TEST_ONLY_ACCOUNT").unwrap())
        .expect("rotation save");
    assert_eq!(
        second
            .load()
            .expect("load rotated record")
            .unwrap()
            .refresh_token(),
        "TEST_ONLY_REFRESH_NEW"
    );
    second.logout().expect("logout");
    assert_eq!(lock_inode(&temp.0), inode);
    assert!(temp.0.join("credential.lock").exists());
    assert!(!temp.0.join("credential-v1.json").exists());
}

#[tokio::test]
async fn lock_wait_timeout_is_bounded_in_one_process() {
    let temp = TempState::new();
    let store = temp.store();
    let held = store.lock(Duration::from_secs(1)).await.expect("held lock");
    let started = Instant::now();
    let result = store.lock(Duration::from_millis(80)).await;
    assert!(matches!(result, Err(StoreError::LockTimeout)));
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(held);
}

#[tokio::test]
async fn cross_process_lock_helper() {
    let Ok(mode) = std::env::var(HELPER_MODE) else {
        return;
    };
    let directory = std::env::var_os(HELPER_DIR).expect("helper state directory");
    let timeout_ms = std::env::var(HELPER_TIMEOUT_MS)
        .expect("helper timeout")
        .parse::<u64>()
        .expect("numeric helper timeout");
    let store = CredentialStore::new(CodexAuthStore::File, directory);

    match mode.as_str() {
        "timeout" => assert!(matches!(
            store.lock(Duration::from_millis(timeout_ms)).await,
            Err(StoreError::LockTimeout)
        )),
        "cancel" => {
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(40),
                    store.lock(Duration::from_millis(timeout_ms))
                )
                .await
                .is_err()
            );
        }
        "acquire" => {
            let _lock = store
                .lock(Duration::from_millis(timeout_ms))
                .await
                .expect("child acquired lock");
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        _ => panic!("unknown helper mode"),
    }
}

fn run_helper(directory: &std::path::Path, mode: &str, timeout_ms: u64) {
    let status = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("cross_process_lock_helper")
        .arg("--nocapture")
        .env(HELPER_MODE, mode)
        .env(HELPER_DIR, directory)
        .env(HELPER_TIMEOUT_MS, timeout_ms.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn lock helper process");
    assert!(status.success(), "lock helper {mode} failed: {status}");
}

fn lock_inode(directory: &std::path::Path) -> (u64, u64) {
    let metadata = fs::metadata(directory.join("credential.lock")).expect("lock metadata");
    (metadata.dev(), metadata.ino())
}
