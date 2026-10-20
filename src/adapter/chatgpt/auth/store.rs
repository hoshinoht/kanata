use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

use super::{AuthError, record::MAX_RECORD_BYTES, record::State};
use crate::adapter::auth_files::{self, AuthFiles};

const FILES: AuthFiles = AuthFiles {
    record: c"chatgpt-v1.json",
    lock: c"chatgpt.lock",
    temp_prefix: ".chatgpt-tmp-",
    max_record_bytes: MAX_RECORD_BYTES,
};

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
        let directory = auth_files::open_state_directory(&self.directory)?;
        let lock = FILES
            .lock(
                &directory,
                Duration::from_secs(30),
                Duration::from_millis(10),
            )
            .await?;
        Ok(Locked {
            directory,
            _lock: lock,
        })
    }
}
impl Locked {
    pub fn load(&self) -> Result<Option<State>, AuthError> {
        FILES.read_record(&self.directory, State::decode)
    }
    pub fn save(&self, state: &State) -> Result<(), AuthError> {
        let _ = self.load()?;
        Ok(FILES.write_record(&self.directory, &state.encode()?)?)
    }
}
