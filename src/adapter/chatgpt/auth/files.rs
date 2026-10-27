//! Owner-only storage of the ChatGPT sign-in record: state directory, lock and atomic writes.

use std::ffi::{CStr, CString};
use std::fs::{File, TryLockError};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, open, openat, renameat, statat, unlinkat};
use rustix::io::Errno;
use rustix::process::geteuid;

use super::record::MAX_RECORD_BYTES;

const OWNER_DIR_MODE: u32 = 0o700;
const OWNER_FILE_MODE: u32 = 0o600;
const RECORD: &CStr = c"chatgpt-v1.json";
const LOCK: &CStr = c"chatgpt.lock";
const TEMP_PREFIX: &str = ".chatgpt-tmp-";
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Storage failure categories, mapped onto `AuthError`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FileError {
    InvalidPath,
    Unsafe,
    Io,
    TooLarge,
    LockTimeout,
    LockFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitStage {
    Created,
    Written,
    FileSynced,
    Renamed,
    DirectorySynced,
}

/// Open an absolute directory path component by component without following symlinks.
pub(super) fn open_state_directory(path: &Path) -> Result<File, FileError> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
        || !path
            .components()
            .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err(FileError::InvalidPath);
    }

    let root = File::from(
        open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| FileError::Unsafe)?,
    );
    let mut current = root;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = CString::new(name.as_bytes()).map_err(|_| FileError::InvalidPath)?;
        let next = openat(
            &current,
            name.as_c_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| FileError::Unsafe)?;
        current = File::from(next);
    }
    validate_directory(&current)?;
    Ok(current)
}

/// Open the lock file and poll its exclusive lock until the lock timeout elapses.
pub(super) async fn lock(directory: &File) -> Result<File, FileError> {
    let lock = open_lock_file(directory)?;
    let started = tokio::time::Instant::now();
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) => {
                let remaining = LOCK_TIMEOUT.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(FileError::LockTimeout);
                }
                tokio::time::sleep(LOCK_POLL_INTERVAL.min(remaining)).await;
            }
            Err(TryLockError::Error(_)) => return Err(FileError::LockFailure),
        }
    }
}

fn open_lock_file(directory: &File) -> Result<File, FileError> {
    match openat(
        directory,
        LOCK,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(OWNER_FILE_MODE as _),
    ) {
        Ok(descriptor) => {
            let file = File::from(descriptor);
            secure_new_file(&file)?;
            Ok(file)
        }
        Err(error) if error == Errno::EXIST => {
            open_existing(directory, LOCK, OFlags::RDWR)?.ok_or(FileError::Unsafe)
        }
        Err(_) => Err(FileError::Io),
    }
}

/// Read and decode the record; `None` when it does not exist.
pub(super) fn read_record<T, E: From<FileError>>(
    directory: &File,
    decode: impl FnOnce(&[u8]) -> Result<T, E>,
) -> Result<Option<T>, E> {
    let Some(file) = open_record_file(directory)? else {
        return Ok(None);
    };
    decode(&read_bounded(file)?).map(Some)
}

fn read_bounded(mut file: File) -> Result<Vec<u8>, FileError> {
    let mut bytes = Vec::with_capacity(MAX_RECORD_BYTES + 1);
    Read::by_ref(&mut file)
        .take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| FileError::Io)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(FileError::TooLarge);
    }
    Ok(bytes)
}

/// Atomically replace the record: owner-only temp file, fsync, rename, directory fsync.
pub(super) fn write_record(directory: &File, record: &[u8]) -> Result<(), FileError> {
    commit_file_record(directory, record, &mut |_| Ok(()))
}

fn commit_file_record(
    directory: &File,
    record: &[u8],
    failpoint: &mut impl FnMut(CommitStage) -> std::io::Result<()>,
) -> Result<(), FileError> {
    if record.len() > MAX_RECORD_BYTES {
        return Err(FileError::TooLarge);
    }
    validate_directory(directory)?;
    let (temporary_name, mut temporary_file) = create_temp_file(directory)?;
    let temporary_cstr = CString::new(temporary_name.as_str()).map_err(|_| FileError::Io)?;
    let mut renamed = false;
    let result = (|| {
        failpoint(CommitStage::Created)?;
        temporary_file.write_all(record)?;
        failpoint(CommitStage::Written)?;
        temporary_file.sync_all()?;
        failpoint(CommitStage::FileSynced)?;
        renameat(directory, temporary_cstr.as_c_str(), directory, RECORD)
            .map_err(std::io::Error::from)?;
        renamed = true;
        drop(temporary_file);
        failpoint(CommitStage::Renamed)?;
        directory.sync_all()?;
        failpoint(CommitStage::DirectorySynced)?;
        Ok::<(), std::io::Error>(())
    })();

    if result.is_err() && !renamed {
        let _ = unlinkat(directory, temporary_cstr.as_c_str(), AtFlags::empty());
    }
    result.map_err(|_| FileError::Io)
}

fn create_temp_file(directory: &File) -> Result<(String, File), FileError> {
    for _ in 0..32 {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("{TEMP_PREFIX}{}-{stamp}-{counter}", std::process::id());
        let c_name = CString::new(name.as_str()).map_err(|_| FileError::Io)?;
        match openat(
            directory,
            c_name.as_c_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(OWNER_FILE_MODE as _),
        ) {
            Ok(descriptor) => {
                let file = File::from(descriptor);
                if let Err(error) = secure_new_file(&file) {
                    drop(file);
                    let _ = unlinkat(directory, c_name.as_c_str(), AtFlags::empty());
                    return Err(error);
                }
                return Ok((name, file));
            }
            Err(error) if error == Errno::EXIST => continue,
            Err(_) => return Err(FileError::Io),
        }
    }
    Err(FileError::Io)
}

/// Open the validated record read-only; `None` when it does not exist.
fn open_record_file(directory: &File) -> Result<Option<File>, FileError> {
    validate_directory(directory)?;
    let Some(file) = open_existing(directory, RECORD, OFlags::RDONLY)? else {
        return Ok(None);
    };
    let metadata = file.metadata().map_err(|_| FileError::Io)?;
    if metadata.len() > MAX_RECORD_BYTES as u64 {
        return Err(FileError::TooLarge);
    }
    Ok(Some(file))
}

/// Open an existing owner-only regular file without following symlinks; `None` when absent.
fn open_existing(directory: &File, name: &CStr, access: OFlags) -> Result<Option<File>, FileError> {
    let stat = match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(error) if error == Errno::NOENT => return Ok(None),
        Err(_) => return Err(FileError::Unsafe),
    };
    // st_mode and st_nlink are u16 on macOS.
    validate_owner_file(
        FileType::from_raw_mode(stat.st_mode).is_file(),
        stat.st_uid,
        stat.st_mode as u32,
        stat.st_nlink as u64,
    )?;
    let descriptor = openat(
        directory,
        name,
        access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| FileError::Unsafe)?;
    let file = File::from(descriptor);
    validate_regular_file(&file)?;
    Ok(Some(file))
}

fn secure_new_file(file: &File) -> Result<(), FileError> {
    rustix::fs::fchmod(file, Mode::from_raw_mode(OWNER_FILE_MODE as _))
        .map_err(|_| FileError::Unsafe)?;
    validate_regular_file(file)
}

fn validate_regular_file(file: &File) -> Result<(), FileError> {
    let metadata = file.metadata().map_err(|_| FileError::Io)?;
    validate_owner_file(
        metadata.file_type().is_file(),
        metadata.uid(),
        metadata.mode(),
        metadata.nlink(),
    )
}

fn validate_owner_file(is_file: bool, uid: u32, mode: u32, nlink: u64) -> Result<(), FileError> {
    if !is_file || uid != geteuid().as_raw() || mode & 0o7777 != OWNER_FILE_MODE || nlink != 1 {
        return Err(FileError::Unsafe);
    }
    Ok(())
}

fn validate_directory(directory: &File) -> Result<(), FileError> {
    let metadata = directory.metadata().map_err(|_| FileError::Unsafe)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o7777 != OWNER_DIR_MODE
    {
        return Err(FileError::Unsafe);
    }
    Ok(())
}

/// Owner-only (0700) temporary state directory, removed on drop.
#[cfg(test)]
pub(super) struct TestDir(pub(super) std::path::PathBuf);

#[cfg(test)]
impl TestDir {
    pub(super) fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        let path = base.join(format!(
            "kanata-auth-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create synthetic directory");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("secure synthetic directory");
        Self(path)
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::{
        CommitStage, FileError, MAX_RECORD_BYTES, TestDir, commit_file_record, open_record_file,
        open_state_directory, read_bounded,
    };

    #[test]
    fn injected_pre_rename_failures_remove_temporary_files() {
        for failed_stage in [
            CommitStage::Created,
            CommitStage::Written,
            CommitStage::FileSynced,
        ] {
            let temp = TestDir::new();
            let directory = open_state_directory(&temp.0).expect("open secure directory");
            let result = commit_file_record(&directory, b"synthetic", &mut |stage| {
                if stage == failed_stage {
                    Err(std::io::Error::other("injected failure"))
                } else {
                    Ok(())
                }
            });
            assert_eq!(result, Err(FileError::Io));
            assert_eq!(fs::read_dir(&temp.0).expect("list directory").count(), 0);
        }
    }

    #[test]
    fn failure_after_rename_is_not_acknowledged_and_keeps_no_temp() {
        let temp = TestDir::new();
        let directory = open_state_directory(&temp.0).expect("open secure directory");
        let result = commit_file_record(&directory, b"synthetic", &mut |stage| {
            if stage == CommitStage::Renamed {
                Err(std::io::Error::other("injected failure"))
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(FileError::Io));
        let names = fs::read_dir(&temp.0)
            .expect("list directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, [std::ffi::OsString::from("chatgpt-v1.json")]);
    }

    #[test]
    fn record_read_is_bounded_if_file_grows_after_open() {
        let temp = TestDir::new();
        let directory = open_state_directory(&temp.0).expect("open secure directory");
        let path = temp.0.join("chatgpt-v1.json");
        fs::write(&path, b"synthetic").expect("create synthetic record");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("secure synthetic record");

        let opened = open_record_file(&directory)
            .expect("open record")
            .expect("record exists");
        let mut writer = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open synthetic append handle");
        writer
            .write_all(&vec![b'x'; MAX_RECORD_BYTES + 1])
            .expect("grow record after open");

        assert_eq!(read_bounded(opened), Err(FileError::TooLarge));
    }
}
