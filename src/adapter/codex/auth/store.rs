use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustix::fs::{AtFlags, unlinkat};

use crate::adapter::auth_files::{self, AuthFiles};
use crate::config::{CodexAuthStore, ValidatedCodexAuth};

use super::keyring::{KeyringBackend, KeyringErrorKind, SystemKeyring, state_dir_identity};
use super::record::{MAX_RECORD_BYTES, decode, encode};
use super::{Credential, StoreError};

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);
const FILES: AuthFiles = AuthFiles {
    record: c"credential-v1.json",
    lock: c"credential.lock",
    temp_prefix: ".credential-tmp-",
    max_record_bytes: MAX_RECORD_BYTES,
};

enum Backend {
    File,
    Keyring(Arc<dyn KeyringBackend>),
}

pub struct CredentialStore {
    state_dir: PathBuf,
    backend: Backend,
}

pub struct LockedStore<'a> {
    backend: &'a Backend,
    directory: File,
    _lock_file: File,
    keyring_identity: Option<String>,
}

impl CredentialStore {
    pub fn new(store: CodexAuthStore, state_dir: impl AsRef<Path>) -> Self {
        Self::with_keyring_backend(store, state_dir, Arc::new(SystemKeyring))
    }

    fn with_keyring_backend(
        store: CodexAuthStore,
        state_dir: impl AsRef<Path>,
        keyring: Arc<dyn KeyringBackend>,
    ) -> Self {
        let backend = match store {
            CodexAuthStore::File => Backend::File,
            CodexAuthStore::Keyring => Backend::Keyring(keyring),
        };
        Self {
            state_dir: state_dir.as_ref().to_path_buf(),
            backend,
        }
    }

    pub fn from_config(config: &ValidatedCodexAuth) -> Self {
        Self::new(config.store(), config.state_dir())
    }

    /// File mode is intended for local filesystems; network-filesystem crash guarantees are not claimed.
    pub async fn lock(&self, timeout: Duration) -> Result<LockedStore<'_>, StoreError> {
        let directory = auth_files::open_state_directory(&self.state_dir)?;
        let keyring_identity = match self.backend {
            Backend::File => None,
            Backend::Keyring(_) => Some(state_dir_identity(&self.state_dir)?),
        };
        let lock_file = FILES.lock(&directory, timeout, LOCK_POLL_INTERVAL).await?;
        Ok(LockedStore {
            backend: &self.backend,
            directory,
            _lock_file: lock_file,
            keyring_identity,
        })
    }
}

impl LockedStore<'_> {
    pub fn load(&self) -> Result<Option<Credential>, StoreError> {
        match self.backend {
            Backend::File => FILES.read_record(&self.directory, decode),
            Backend::Keyring(keyring) => {
                let identity = self.identity()?;
                let record = keyring
                    .read_record(identity)
                    .map_err(map_keyring_read_error)?;
                record.map(|record| decode(record.as_bytes())).transpose()
            }
        }
    }

    /// A keyring write is accepted only after a matching read-back; the provider may still lose it on crash.
    pub fn save(&self, credential: &Credential) -> Result<(), StoreError> {
        let encoded = encode(credential)?;
        match self.backend {
            Backend::File => {
                let _ = FILES.read_record(&self.directory, decode)?;
                Ok(FILES.write_record(&self.directory, &encoded)?)
            }
            Backend::Keyring(keyring) => {
                let _ = self.load()?;
                let identity = self.identity()?;
                let encoded =
                    std::str::from_utf8(&encoded).map_err(|_| StoreError::InvalidRecord)?;
                keyring
                    .write_record(identity, encoded)
                    .map_err(|_| StoreError::KeyringWriteUncertain)?;
                match keyring.read_record(identity) {
                    Ok(Some(read_back)) if read_back == encoded => {
                        decode(read_back.as_bytes())?;
                        Ok(())
                    }
                    _ => Err(StoreError::KeyringWriteUncertain),
                }
            }
        }
    }

    pub fn logout(&self) -> Result<(), StoreError> {
        match self.backend {
            Backend::File => remove_record(&self.directory),
            Backend::Keyring(keyring) => keyring
                .delete_record(self.identity()?)
                .map_err(|_| StoreError::KeyringFailure),
        }
    }

    fn identity(&self) -> Result<&str, StoreError> {
        self.keyring_identity
            .as_deref()
            .ok_or(StoreError::InvalidStateDirectory)
    }
}

fn remove_record(directory: &File) -> Result<(), StoreError> {
    let Some(file) = FILES.open_record_file(directory)? else {
        return Ok(());
    };
    drop(file);
    unlinkat(directory, FILES.record, AtFlags::empty()).map_err(|_| StoreError::StorageIo)?;
    directory.sync_all().map_err(|_| StoreError::StorageIo)
}

fn map_keyring_read_error(error: KeyringErrorKind) -> StoreError {
    match error {
        KeyringErrorKind::Unavailable => StoreError::KeyringUnavailable,
        KeyringErrorKind::OperationFailed => StoreError::KeyringFailure,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::adapter::auth_files::TestDir;
    use crate::adapter::codex::auth::Credential;
    use crate::adapter::codex::auth::error::StoreError;
    use crate::adapter::codex::auth::keyring::{
        KeyringBackend, KeyringErrorKind, state_dir_identity,
    };
    use crate::config::CodexAuthStore;

    use super::CredentialStore;

    #[derive(Default)]
    struct FakeKeyring {
        records: Mutex<HashMap<String, String>>,
        fail_read: bool,
        fail_write: bool,
        corrupt_readback: bool,
    }

    impl KeyringBackend for FakeKeyring {
        fn read_record(&self, identity: &str) -> Result<Option<String>, KeyringErrorKind> {
            if self.fail_read {
                return Err(KeyringErrorKind::Unavailable);
            }
            let record = self
                .records
                .lock()
                .expect("fake lock")
                .get(identity)
                .cloned();
            Ok(record.map(|mut record| {
                if self.corrupt_readback {
                    record.push(' ');
                }
                record
            }))
        }

        fn write_record(&self, identity: &str, record: &str) -> Result<(), KeyringErrorKind> {
            self.records
                .lock()
                .expect("fake lock")
                .insert(identity.to_owned(), record.to_owned());
            if self.fail_write {
                return Err(KeyringErrorKind::OperationFailed);
            }
            Ok(())
        }

        fn delete_record(&self, identity: &str) -> Result<(), KeyringErrorKind> {
            self.records.lock().expect("fake lock").remove(identity);
            Ok(())
        }
    }

    fn keyring_store(temp: &TestDir, backend: Arc<dyn KeyringBackend>) -> CredentialStore {
        CredentialStore::with_keyring_backend(CodexAuthStore::Keyring, &temp.0, backend)
    }

    #[tokio::test]
    async fn keyring_requires_write_and_matching_versioned_readback() {
        let temp = TestDir::new();
        let backend = Arc::new(FakeKeyring::default());
        let store = keyring_store(&temp, backend.clone());
        let lock = store.lock(Duration::from_secs(1)).await.expect("lock");
        let credential = Credential::new("TEST_ONLY_REFRESH_MARKER", "TEST_ONLY_ACCOUNT_MARKER")
            .expect("credential");
        lock.save(&credential).expect("verified fake keyring write");
        assert_eq!(
            lock.load()
                .expect("load fake keyring")
                .expect("record")
                .refresh_token(),
            "TEST_ONLY_REFRESH_MARKER"
        );
        assert!(!temp.0.join("credential-v1.json").exists());
    }

    #[tokio::test]
    async fn unavailable_or_uncertain_keyring_never_falls_back_to_a_file() {
        let temp = TestDir::new();
        let backend = Arc::new(FakeKeyring {
            fail_read: true,
            ..FakeKeyring::default()
        });
        let store = keyring_store(&temp, backend);
        let lock = store.lock(Duration::from_secs(1)).await.expect("lock");
        assert!(matches!(lock.load(), Err(StoreError::KeyringUnavailable)));
        assert_eq!(
            lock.save(&Credential::new("TEST_ONLY_REFRESH", "TEST_ONLY_ACCOUNT").unwrap()),
            Err(StoreError::KeyringUnavailable)
        );
        assert!(!temp.0.join("credential-v1.json").exists());
        drop(lock);

        let uncertain = Arc::new(FakeKeyring {
            fail_write: true,
            ..FakeKeyring::default()
        });
        let store = keyring_store(&temp, uncertain);
        let lock = store.lock(Duration::from_secs(1)).await.expect("lock");
        assert_eq!(
            lock.save(&Credential::new("TEST_ONLY_ROTATED", "TEST_ONLY_ACCOUNT").unwrap()),
            Err(StoreError::KeyringWriteUncertain)
        );
        assert!(!temp.0.join("credential-v1.json").exists());
    }

    #[tokio::test]
    async fn keyring_readback_mismatch_fails_closed() {
        let temp = TestDir::new();
        let backend = Arc::new(FakeKeyring {
            corrupt_readback: true,
            ..FakeKeyring::default()
        });
        let store = keyring_store(&temp, backend);
        let lock = store.lock(Duration::from_secs(1)).await.expect("lock");
        assert_eq!(
            lock.save(&Credential::new("TEST_ONLY_REFRESH", "TEST_ONLY_ACCOUNT").unwrap()),
            Err(StoreError::KeyringWriteUncertain)
        );
        assert!(!temp.0.join("credential-v1.json").exists());
    }

    #[tokio::test]
    async fn validated_store_selection_keeps_keyring_and_file_distinct() {
        let temp = TestDir::new();
        let fake_keyring = Arc::new(FakeKeyring::default());
        let keyring_store = keyring_store(&temp, fake_keyring.clone());
        let keyring_lock = keyring_store
            .lock(Duration::from_secs(1))
            .await
            .expect("keyring lock");
        keyring_lock
            .save(&Credential::new("TEST_ONLY_KEYRING_REFRESH", "TEST_ONLY_ACCOUNT").unwrap())
            .expect("keyring backend write");
        drop(keyring_lock);

        let file_store = CredentialStore::new(CodexAuthStore::File, &temp.0);
        let guard = file_store
            .lock(Duration::from_secs(1))
            .await
            .expect("file lock");
        assert!(guard.load().expect("file store remains empty").is_none());
        guard
            .save(&Credential::new("TEST_ONLY_FILE_REFRESH", "TEST_ONLY_ACCOUNT").unwrap())
            .expect("file write");
        assert!(temp.0.join("credential-v1.json").exists());
        let identity = state_dir_identity(&temp.0).expect("state identity");
        assert_eq!(
            fake_keyring
                .records
                .lock()
                .expect("fake lock")
                .get(&identity)
                .map(String::as_str),
            Some(
                r#"{"version":1,"refresh_token":"TEST_ONLY_KEYRING_REFRESH","account_id":"TEST_ONLY_ACCOUNT"}"#
            )
        );
    }

    #[tokio::test]
    async fn same_state_directory_shares_identity_and_contends_on_its_lock() {
        let temp = TestDir::new();
        let backend = Arc::new(FakeKeyring::default());
        let first = keyring_store(&temp, backend.clone());
        let normalized_path = PathBuf::from(format!("{}/./", temp.0.display()));
        let second = CredentialStore::with_keyring_backend(
            CodexAuthStore::Keyring,
            normalized_path,
            backend,
        );

        let first_lock = first
            .lock(Duration::from_secs(1))
            .await
            .expect("first lock");
        assert!(matches!(
            second.lock(Duration::from_millis(30)).await,
            Err(StoreError::LockTimeout)
        ));
        first_lock
            .save(&Credential::new("TEST_ONLY_REFRESH_A", "TEST_ONLY_ACCOUNT_A").unwrap())
            .expect("save shared record");
        drop(first_lock);

        let second_lock = second
            .lock(Duration::from_secs(1))
            .await
            .expect("second lock");
        assert_eq!(
            second_lock
                .load()
                .expect("read shared record")
                .expect("record")
                .refresh_token(),
            "TEST_ONLY_REFRESH_A"
        );
        second_lock.logout().expect("delete shared record");
        assert!(second_lock.load().expect("empty after logout").is_none());
    }

    #[tokio::test]
    async fn distinct_state_directories_isolate_shared_keyring_backend_records() {
        let first_dir = TestDir::new();
        let second_dir = TestDir::new();
        let backend = Arc::new(FakeKeyring::default());
        let first = keyring_store(&first_dir, backend.clone());
        let second = keyring_store(&second_dir, backend);

        let first_lock = first
            .lock(Duration::from_secs(1))
            .await
            .expect("first lock");
        first_lock
            .save(&Credential::new("TEST_ONLY_REFRESH_A", "TEST_ONLY_ACCOUNT_A").unwrap())
            .expect("save first profile");
        let second_lock = second
            .lock(Duration::from_secs(1))
            .await
            .expect("second lock");
        assert!(second_lock.load().expect("empty second profile").is_none());
        second_lock
            .save(&Credential::new("TEST_ONLY_REFRESH_B", "TEST_ONLY_ACCOUNT_B").unwrap())
            .expect("save second profile");

        assert_eq!(
            first_lock
                .load()
                .expect("first profile remains isolated")
                .expect("first record")
                .refresh_token(),
            "TEST_ONLY_REFRESH_A"
        );
        second_lock.logout().expect("logout second profile");
        assert_eq!(
            first_lock
                .load()
                .expect("first profile survives other logout")
                .expect("first record")
                .refresh_token(),
            "TEST_ONLY_REFRESH_A"
        );
    }
}
