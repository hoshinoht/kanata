use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use keyring::v1::{Entry, Error as KeyringError};
use sha2::{Digest, Sha256};

use super::StoreError;

const SERVICE_NAME: &str = "com.kanata.codex-auth";
const IDENTITY_PREFIX: &str = "state-";
const IDENTITY_DOMAIN: &[u8] = b"kanata-codex-state-dir-v1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KeyringErrorKind {
    Unavailable,
    OperationFailed,
}

pub(super) trait KeyringBackend: Send + Sync {
    fn read_record(&self, identity: &str) -> Result<Option<String>, KeyringErrorKind>;
    fn write_record(&self, identity: &str, record: &str) -> Result<(), KeyringErrorKind>;
    fn delete_record(&self, identity: &str) -> Result<(), KeyringErrorKind>;
}

pub(super) struct SystemKeyring;

impl SystemKeyring {
    fn entry(identity: &str) -> Result<Entry, KeyringErrorKind> {
        Entry::new(SERVICE_NAME, identity).map_err(|_| KeyringErrorKind::Unavailable)
    }
}

pub(super) fn state_dir_identity(path: &Path) -> Result<String, StoreError> {
    if !path.is_absolute() {
        return Err(StoreError::InvalidStateDirectory);
    }

    let mut normalized = Vec::with_capacity(IDENTITY_DOMAIN.len() + path.as_os_str().len());
    normalized.extend_from_slice(IDENTITY_DOMAIN);
    normalized.push(b'/');
    let mut first = true;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                if !first {
                    normalized.push(b'/');
                }
                normalized.extend_from_slice(name.as_bytes());
                first = false;
            }
            _ => return Err(StoreError::InvalidStateDirectory),
        }
    }
    if first {
        return Err(StoreError::InvalidStateDirectory);
    }

    let digest = Sha256::digest(normalized);
    let mut identity = String::with_capacity(IDENTITY_PREFIX.len() + digest.len() * 2);
    identity.push_str(IDENTITY_PREFIX);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        identity.push(HEX[(byte >> 4) as usize] as char);
        identity.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(identity)
}

impl KeyringBackend for SystemKeyring {
    fn read_record(&self, identity: &str) -> Result<Option<String>, KeyringErrorKind> {
        match Self::entry(identity)?.get_password() {
            Ok(record) => Ok(Some(record)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(_) => Err(KeyringErrorKind::Unavailable),
        }
    }

    fn write_record(&self, identity: &str, record: &str) -> Result<(), KeyringErrorKind> {
        Self::entry(identity)?
            .set_password(record)
            .map_err(|_| KeyringErrorKind::OperationFailed)
    }

    fn delete_record(&self, identity: &str) -> Result<(), KeyringErrorKind> {
        match Self::entry(identity)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(_) => Err(KeyringErrorKind::OperationFailed),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::state_dir_identity;

    #[test]
    fn state_identity_is_stable_for_lexically_equivalent_paths_and_hides_path() {
        let first = state_dir_identity(Path::new("/owner-only/kanata/state"))
            .expect("absolute state directory");
        let equivalent = state_dir_identity(Path::new("/owner-only//kanata/./state/"))
            .expect("normalized absolute state directory");
        assert_eq!(first, equivalent);
        assert!(first.starts_with("state-"));
        assert_eq!(first.len(), "state-".len() + 64);
        assert!(!first.contains("owner-only"));
    }
}
