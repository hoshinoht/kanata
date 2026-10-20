use std::fmt;

use crate::adapter::auth_files::FileError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreError {
    InvalidStateDirectory,
    UnsafeStoragePath,
    InvalidCredential,
    InvalidRecord,
    RecordTooLarge,
    StorageIo,
    LockTimeout,
    LockFailure,
    KeyringUnavailable,
    KeyringFailure,
    KeyringWriteUncertain,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidStateDirectory => "invalid Codex state directory",
            Self::UnsafeStoragePath => "unsafe Codex credential storage path",
            Self::InvalidCredential => "invalid Codex credential",
            Self::InvalidRecord => "invalid Codex credential record",
            Self::RecordTooLarge => "Codex credential record exceeds its size limit",
            Self::StorageIo => "Codex credential storage operation failed",
            Self::LockTimeout => "timed out waiting for Codex credential storage lock",
            Self::LockFailure => "Codex credential storage lock failed",
            Self::KeyringUnavailable => "Codex host keyring is unavailable",
            Self::KeyringFailure => "Codex host keyring operation failed",
            Self::KeyringWriteUncertain => "Codex host keyring write could not be verified",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for StoreError {}

impl From<FileError> for StoreError {
    fn from(error: FileError) -> Self {
        match error {
            FileError::InvalidPath => Self::InvalidStateDirectory,
            FileError::Unsafe => Self::UnsafeStoragePath,
            FileError::Io => Self::StorageIo,
            FileError::TooLarge => Self::RecordTooLarge,
            FileError::LockTimeout => Self::LockTimeout,
            FileError::LockFailure => Self::LockFailure,
        }
    }
}
