use std::{
    fs::File,
    path::{Path, PathBuf},
};

use super::{AuthError, files, record::State};

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
        let directory = files::open_state_directory(&self.directory)?;
        let lock = files::lock(&directory).await?;
        Ok(Locked {
            directory,
            _lock: lock,
        })
    }
}
impl Locked {
    pub fn load(&self) -> Result<Option<State>, AuthError> {
        files::read_record(&self.directory, State::decode)
    }
    pub fn save(&self, state: &State) -> Result<(), AuthError> {
        let _ = self.load()?;
        Ok(files::write_record(&self.directory, &state.encode()?)?)
    }
}
