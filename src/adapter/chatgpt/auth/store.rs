use std::{
    fs::{File, TryLockError},
    path::{Path, PathBuf},
    time::Duration,
};

use super::{AuthError, file, record::State};

#[derive(Clone)]
pub(super) struct Store {
    directory: PathBuf,
}
pub(super) struct Locked {
    directory: File,
    _lock: File,
}
impl Store {
    pub fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_path_buf(),
        }
    }
    pub async fn lock(&self) -> Result<Locked, AuthError> {
        let directory = file::open_state_directory(&self.directory)?;
        let lock = file::open_lock_file(&directory)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match lock.try_lock() {
                Ok(()) => {
                    return Ok(Locked {
                        directory,
                        _lock: lock,
                    });
                }
                Err(TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(TryLockError::WouldBlock) => return Err(AuthError::LockTimeout),
                Err(TryLockError::Error(_)) => return Err(AuthError::Storage),
            }
        }
    }
}
impl Locked {
    pub fn load(&self) -> Result<Option<State>, AuthError> {
        file::read_record(&self.directory)
    }
    pub fn save(&self, state: &State) -> Result<(), AuthError> {
        let _ = self.load()?;
        file::write_record(&self.directory, &state.encode()?)
    }
}
