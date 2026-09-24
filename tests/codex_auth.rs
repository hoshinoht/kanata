use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kanata::adapter::codex::auth::{Credential, CredentialStore, StoreError};
use kanata::config::CodexAuthStore;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempState {
    root: PathBuf,
    state: PathBuf,
}

impl TempState {
    fn new() -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        let root = base.join(format!(
            "kanata-codex-auth-test-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create synthetic temp directory");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("secure synthetic temp directory");
        let state = root.join("state");
        fs::create_dir(&state).expect("create owner state directory");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))
            .expect("secure owner state directory");
        Self { root, state }
    }

    fn store(&self) -> CredentialStore {
        CredentialStore::new(CodexAuthStore::File, &self.state)
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn synthetic_credential(refresh: &str) -> Credential {
    Credential::new(refresh, "TEST_ONLY_ACCOUNT_ID_0001").expect("valid synthetic credential")
}

#[tokio::test]
async fn file_store_round_trips_only_versioned_refresh_and_account_data() {
    let temp = TempState::new();
    let store = temp.store();
    let locked = store
        .lock(Duration::from_secs(1))
        .await
        .expect("lock store");
    assert!(locked.load().expect("empty store").is_none());

    let credential = synthetic_credential("TEST_ONLY_REFRESH_TOKEN_0001");
    locked.save(&credential).expect("save record");
    let loaded = locked.load().expect("load record").expect("record exists");
    assert_eq!(loaded.refresh_token(), "TEST_ONLY_REFRESH_TOKEN_0001");
    assert_eq!(loaded.account_id(), "TEST_ONLY_ACCOUNT_ID_0001");

    let raw = fs::read(temp.state.join("credential-v1.json")).expect("read synthetic record");
    let raw = String::from_utf8(raw).expect("record is UTF-8");
    assert!(raw.contains("\"version\":1"));
    assert!(raw.contains("TEST_ONLY_REFRESH_TOKEN_0001"));
    assert!(raw.contains("TEST_ONLY_ACCOUNT_ID_0001"));
    assert!(!raw.contains("access_token"));

    let directory_metadata = fs::metadata(&temp.state).expect("directory metadata");
    let record_metadata =
        fs::metadata(temp.state.join("credential-v1.json")).expect("record metadata");
    let lock_metadata = fs::metadata(temp.state.join("credential.lock")).expect("lock metadata");
    assert_eq!(directory_metadata.mode() & 0o7777, 0o700);
    assert_eq!(record_metadata.mode() & 0o7777, 0o600);
    assert_eq!(lock_metadata.mode() & 0o7777, 0o600);
    assert_eq!(record_metadata.uid(), directory_metadata.uid());
    assert_eq!(lock_metadata.uid(), directory_metadata.uid());
}

#[test]
fn credentials_redact_debug_and_display_and_reject_blank_values() {
    let marker = "TEST_ONLY_DO_NOT_PRINT_REFRESH";
    let credential = Credential::new(marker, "TEST_ONLY_ACCOUNT_ID").expect("credential");
    assert!(!format!("{credential:?}").contains(marker));
    assert!(!format!("{credential}").contains(marker));
    assert_eq!(
        Credential::new("  \n", "TEST_ONLY_ACCOUNT_ID").unwrap_err(),
        StoreError::InvalidCredential
    );
    assert_eq!(
        Credential::new("TEST_ONLY_REFRESH", "\u{7f}").unwrap_err(),
        StoreError::InvalidCredential
    );
}

#[tokio::test]
async fn file_store_rejects_wrong_directory_and_record_modes() {
    let temp = TempState::new();
    fs::set_permissions(&temp.state, fs::Permissions::from_mode(0o755))
        .expect("make invalid directory mode");
    assert!(matches!(
        temp.store().lock(Duration::from_secs(1)).await,
        Err(StoreError::UnsafeStoragePath)
    ));

    fs::set_permissions(&temp.state, fs::Permissions::from_mode(0o700))
        .expect("restore secure directory mode");
    fs::write(
        temp.state.join("credential-v1.json"),
        br#"{"version":1,"refresh_token":"TEST_ONLY_REFRESH","account_id":"TEST_ONLY_ACCOUNT"}"#,
    )
    .expect("create synthetic record");
    fs::set_permissions(
        temp.state.join("credential-v1.json"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("make invalid record mode");
    let store = temp.store();
    let locked = store
        .lock(Duration::from_secs(1))
        .await
        .expect("lock store");
    assert_eq!(locked.load().unwrap_err(), StoreError::UnsafeStoragePath);
}

#[tokio::test]
async fn file_store_rejects_wrong_lock_mode_and_lock_symlinks() {
    let temp = TempState::new();
    let lock_path = temp.state.join("credential.lock");
    fs::write(&lock_path, b"").expect("create invalid lock file");
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644))
        .expect("make lock mode too broad");
    assert!(matches!(
        temp.store().lock(Duration::from_secs(1)).await,
        Err(StoreError::UnsafeStoragePath)
    ));

    let temp = TempState::new();
    let outside = temp.root.join("lock-target");
    fs::write(&outside, b"TEST_ONLY_SENTINEL").expect("synthetic lock target");
    symlink(&outside, temp.state.join("credential.lock")).expect("lock symlink");
    assert!(matches!(
        temp.store().lock(Duration::from_secs(1)).await,
        Err(StoreError::UnsafeStoragePath)
    ));
    assert_eq!(
        fs::read(outside).expect("target unchanged"),
        b"TEST_ONLY_SENTINEL"
    );
}

#[tokio::test]
async fn file_store_rejects_nonregular_record_and_lock_paths() {
    let temp = TempState::new();
    fs::create_dir(temp.state.join("credential-v1.json")).expect("nonregular record path");
    let store = temp.store();
    let locked = store
        .lock(Duration::from_secs(1))
        .await
        .expect("lock store");
    assert_eq!(locked.load().unwrap_err(), StoreError::UnsafeStoragePath);

    let temp = TempState::new();
    fs::create_dir(temp.state.join("credential.lock")).expect("nonregular lock path");
    assert!(matches!(
        temp.store().lock(Duration::from_secs(1)).await,
        Err(StoreError::UnsafeStoragePath)
    ));
}

#[tokio::test]
async fn file_store_rejects_corrupt_unknown_schema_and_oversized_records() {
    for raw in [
        br#"{"version":2,"refresh_token":"TEST_ONLY_REFRESH","account_id":"TEST_ONLY_ACCOUNT"}"#.as_slice(),
        br#"{"version":1,"refresh_token":"TEST_ONLY_REFRESH","account_id":"TEST_ONLY_ACCOUNT","extra":true}"#.as_slice(),
        b"{broken",
    ] {
        let temp = TempState::new();
        let path = temp.state.join("credential-v1.json");
        fs::write(&path, raw).expect("write synthetic invalid record");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("secure synthetic record");
        let store = temp.store();
        let locked = store.lock(Duration::from_secs(1)).await.expect("lock store");
        assert_eq!(locked.load().unwrap_err(), StoreError::InvalidRecord);
    }

    let temp = TempState::new();
    let path = temp.state.join("credential-v1.json");
    fs::write(&path, vec![b'x'; 32 * 1024]).expect("write oversized synthetic record");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("secure synthetic record");
    let store = temp.store();
    let locked = store
        .lock(Duration::from_secs(1))
        .await
        .expect("lock store");
    assert_eq!(locked.load().unwrap_err(), StoreError::RecordTooLarge);
}

#[tokio::test]
async fn file_store_rejects_final_and_intermediate_symlinks_without_traversing_them() {
    let temp = TempState::new();
    let outside = temp.root.join("outside");
    fs::write(&outside, b"TEST_ONLY_SENTINEL").expect("sentinel");
    symlink(&outside, temp.state.join("credential-v1.json")).expect("final symlink");
    let store = temp.store();
    let locked = store
        .lock(Duration::from_secs(1))
        .await
        .expect("lock store");
    assert_eq!(locked.load().unwrap_err(), StoreError::UnsafeStoragePath);
    assert_eq!(
        fs::read(&outside).expect("sentinel unchanged"),
        b"TEST_ONLY_SENTINEL"
    );

    let actual_parent = temp.root.join("actual-parent");
    fs::create_dir(&actual_parent).expect("actual parent");
    fs::set_permissions(&actual_parent, fs::Permissions::from_mode(0o700))
        .expect("secure actual parent");
    let target = actual_parent.join("state");
    fs::create_dir(&target).expect("actual state");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).expect("secure actual state");
    let redirect = temp.root.join("redirect");
    symlink(&actual_parent, &redirect).expect("intermediate symlink");
    let redirected_store = CredentialStore::new(CodexAuthStore::File, redirect.join("state"));
    assert!(matches!(
        redirected_store.lock(Duration::from_secs(1)).await,
        Err(StoreError::UnsafeStoragePath)
    ));
    assert!(!target.join("credential.lock").exists());
}

#[tokio::test]
async fn logout_is_local_idempotent_and_preserves_the_lock_file() {
    let temp = TempState::new();
    let store = temp.store();
    let locked = store
        .lock(Duration::from_secs(1))
        .await
        .expect("lock store");
    let inode = lock_inode(&temp.state);
    locked
        .save(&synthetic_credential("TEST_ONLY_REFRESH_TOKEN_0001"))
        .expect("save credential");
    locked.logout().expect("logout");
    locked.logout().expect("idempotent logout");
    assert!(locked.load().expect("empty after logout").is_none());
    assert_eq!(lock_inode(&temp.state), inode);
    assert!(temp.state.join("credential.lock").exists());
    assert!(!temp.state.join("credential-v1.json").exists());
}

fn lock_inode(directory: &Path) -> (u64, u64) {
    let metadata = fs::metadata(directory.join("credential.lock")).expect("lock metadata");
    (metadata.dev(), metadata.ino())
}
