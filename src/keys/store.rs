//! Host-side keys file writes: advisory lock, atomic 0600 replace and the audit log.
//! Only the `kanata key` CLI uses this; the server reads the file and never locks it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{ConfigError, ValidatedRoute};
use crate::keys::file::{self, KeysFile};
use crate::keys::time;

pub const LOCK_FILE: &str = "keys.lock";
pub const AUDIT_FILE: &str = "audit.jsonl";
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_POLL: Duration = Duration::from_millis(25);

/// Exclusive hold on `keys.lock` beside the keys file; released on drop.
pub struct LockedKeys {
    path: PathBuf,
    dir: PathBuf,
    _lock: File,
}

/// Creates the keys dir (0700) if missing and takes the lock, retrying up to `timeout`.
pub fn lock(keys_path: &Path, timeout: Duration) -> Result<LockedKeys, String> {
    let dir = parent_dir(keys_path)?;
    create_private_dir(&dir)?;
    let lock_file = private_options(OpenOptions::new().read(true).write(true).create(true))
        .open(dir.join(LOCK_FILE))
        .map_err(|_| format!("could not open {}", dir.join(LOCK_FILE).display()))?;
    let started = Instant::now();
    loop {
        match lock_file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if started.elapsed() < timeout => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(TryLockError::WouldBlock) => {
                return Err("keys file is locked by another kanata key command".into());
            }
            Err(TryLockError::Error(_)) => return Err("keys file lock failed".into()),
        }
    }
    Ok(LockedKeys {
        path: keys_path.to_path_buf(),
        dir,
        _lock: lock_file,
    })
}

impl LockedKeys {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current file, syntax-checked only so route drift can be repaired; empty
    /// when it does not exist. [`Self::write`] validates the result against routes.
    pub fn read(&self) -> Result<KeysFile, String> {
        match file::read(&self.path).map_err(|error| error.to_string())? {
            Some(bytes) => file::parse_without_routes(&bytes).map_err(|error| error.to_string()),
            None => Ok(KeysFile::default()),
        }
    }

    /// Validates `keys` as the server would, then replaces the file atomically (0600).
    pub fn write(&self, keys: &KeysFile, routes: &[ValidatedRoute]) -> Result<(), String> {
        keys.validated(routes)
            .map_err(|error: ConfigError| error.to_string())?;
        let name = self
            .path
            .file_name()
            .ok_or("keys file path has no file name")?
            .to_string_lossy()
            .into_owned();
        let temp = self.dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&temp);
        let result = (|| {
            let mut out =
                private_options(OpenOptions::new().write(true).create_new(true)).open(&temp)?;
            out.write_all(keys.render().as_bytes())?;
            out.sync_all()?;
            drop(out);
            fs::rename(&temp, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
            return Err(format!("could not write {}", self.path.display()));
        }
        // The rename is applied; a failed dir fsync only weakens crash durability.
        let _ = File::open(&self.dir).and_then(|dir| dir.sync_all());
        Ok(())
    }

    /// Appends one line per event to `audit.jsonl` (0600).
    pub fn audit(&self, events: &[AuditEvent]) -> Result<(), String> {
        let failed = || "key change applied; audit log write failed".to_owned();
        let mut lines = String::new();
        let at = time::format(time::now());
        let user = audit_user();
        let uid = rustix::process::getuid().as_raw();
        for event in events {
            let line = AuditLine {
                at: &at,
                user: &user,
                uid,
                action: event.action,
                key_id: &event.key_id,
                owner: event.owner,
                changes: event.changes.as_ref(),
            };
            lines.push_str(&serde_json::to_string(&line).map_err(|_| failed())?);
            lines.push('\n');
        }
        let mut out = private_options(OpenOptions::new().append(true).create(true))
            .open(self.dir.join(AUDIT_FILE))
            .map_err(|_| failed())?;
        out.write_all(lines.as_bytes())
            .and_then(|()| out.sync_all())
            .map_err(|_| failed())
    }
}

/// One audited key change; never carries secrets or digests.
pub struct AuditEvent {
    pub action: &'static str,
    pub key_id: String,
    pub owner: bool,
    pub changes: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct AuditLine<'a> {
    at: &'a str,
    user: &'a str,
    uid: u32,
    action: &'static str,
    key_id: &'a str,
    owner: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    changes: Option<&'a serde_json::Value>,
}

fn audit_user() -> String {
    ["SUDO_USER", "USER", "LOGNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "unknown".into())
}

/// A new key's output file: written to a same-dir temp first and published only
/// after the keys file write succeeds.
pub struct SecretFile {
    temp: PathBuf,
    path: PathBuf,
    replace: bool,
}

impl SecretFile {
    /// `replace = false` refuses an existing `path` (and checks it up front).
    pub fn prepare(path: &Path, secret: &str, replace: bool) -> Result<Self, String> {
        if !replace && fs::symlink_metadata(path).is_ok() {
            return Err("key output file already exists".into());
        }
        let dir = parent_dir(path)?;
        let name = path
            .file_name()
            .ok_or("key output path has no file name")?
            .to_string_lossy()
            .into_owned();
        let temp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&temp);
        let result = (|| {
            let mut out =
                private_options(OpenOptions::new().write(true).create_new(true)).open(&temp)?;
            out.write_all(format!("{secret}\n").as_bytes())?;
            out.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
            return Err("key output file could not be written".into());
        }
        Ok(Self {
            temp,
            path: path.to_path_buf(),
            replace,
        })
    }

    pub fn publish(self) -> Result<(), String> {
        if self.replace {
            fs::rename(&self.temp, &self.path)
        } else {
            // hard_link fails if the target exists, unlike rename.
            fs::hard_link(&self.temp, &self.path)
        }
        .map_err(|_| "key output file could not be written".into())
    }
}

/// Unpublished temp files never outlive the command.
impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp);
    }
}

fn parent_dir(path: &Path) -> Result<PathBuf, String> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => Ok(dir.to_path_buf()),
        Some(_) => Ok(PathBuf::from(".")),
        None => Err(format!("{} has no parent directory", path.display())),
    }
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(_) => Err(format!("could not create {}", dir.display())),
    }
}

fn private_options(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
    options
}
