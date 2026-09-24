use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, open, openat, renameat, statat, unlinkat};
use rustix::io::Errno;
use rustix::process::geteuid;

use super::StoreError;
use super::record::{MAX_RECORD_BYTES, decode};

const RECORD_NAME: &CStr = c"credential-v1.json";
const LOCK_NAME: &CStr = c"credential.lock";
const TEMP_PREFIX: &str = ".credential-tmp-";
const OWNER_DIR_MODE: u32 = 0o700;
const OWNER_FILE_MODE: u32 = 0o600;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitStage {
    Created,
    Written,
    FileSynced,
    Renamed,
    DirectorySynced,
}

pub(super) fn open_state_directory(path: &Path) -> Result<File, StoreError> {
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
        return Err(StoreError::InvalidStateDirectory);
    }

    let root = File::from(
        open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| StoreError::UnsafeStoragePath)?,
    );
    let mut current = root;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = CString::new(name.as_bytes()).map_err(|_| StoreError::InvalidStateDirectory)?;
        let next = openat(
            &current,
            name.as_c_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| StoreError::UnsafeStoragePath)?;
        current = File::from(next);
    }
    validate_directory(&current)?;
    Ok(current)
}

pub(super) fn open_lock_file(directory: &File) -> Result<File, StoreError> {
    let descriptor = match openat(
        directory,
        LOCK_NAME,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(OWNER_FILE_MODE as _),
    ) {
        Ok(descriptor) => {
            let file = File::from(descriptor);
            rustix::fs::fchmod(&file, Mode::from_raw_mode(OWNER_FILE_MODE as _))
                .map_err(|_| StoreError::UnsafeStoragePath)?;
            validate_regular_file(&file, OWNER_FILE_MODE)?;
            file
        }
        Err(error) if error == Errno::EXIST => {
            validate_named_file(directory, LOCK_NAME, OWNER_FILE_MODE)?;
            let descriptor = openat(
                directory,
                LOCK_NAME,
                OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| StoreError::UnsafeStoragePath)?;
            File::from(descriptor)
        }
        Err(_) => return Err(StoreError::StorageIo),
    };
    validate_regular_file(&descriptor, OWNER_FILE_MODE)?;
    Ok(descriptor)
}

pub(super) fn read_record(directory: &File) -> Result<Option<super::Credential>, StoreError> {
    let Some(file) = open_record_file(directory)? else {
        return Ok(None);
    };
    decode_record_file(file).map(Some)
}

fn decode_record_file(mut file: File) -> Result<super::Credential, StoreError> {
    let mut bytes = Vec::with_capacity(MAX_RECORD_BYTES + 1);
    Read::by_ref(&mut file)
        .take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| StoreError::StorageIo)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(StoreError::RecordTooLarge);
    }
    decode(&bytes)
}

pub(super) fn write_record(directory: &File, record: &[u8]) -> Result<(), StoreError> {
    commit_file_record(directory, record, &mut |_| Ok(()))
}

pub(super) fn remove_record(directory: &File) -> Result<(), StoreError> {
    let Some(file) = open_record_file(directory)? else {
        return Ok(());
    };
    drop(file);
    unlinkat(directory, RECORD_NAME, AtFlags::empty()).map_err(|_| StoreError::StorageIo)?;
    directory.sync_all().map_err(|_| StoreError::StorageIo)
}

fn commit_file_record(
    directory: &File,
    record: &[u8],
    failpoint: &mut impl FnMut(CommitStage) -> std::io::Result<()>,
) -> Result<(), StoreError> {
    if record.len() > MAX_RECORD_BYTES {
        return Err(StoreError::RecordTooLarge);
    }
    validate_directory(directory)?;
    let (temporary_name, mut temporary_file) = create_temp_file(directory)?;
    let temporary_cstr =
        CString::new(temporary_name.as_str()).map_err(|_| StoreError::StorageIo)?;
    let mut renamed = false;
    let result = (|| {
        failpoint(CommitStage::Created)?;
        temporary_file.write_all(record)?;
        failpoint(CommitStage::Written)?;
        temporary_file.sync_all()?;
        failpoint(CommitStage::FileSynced)?;
        renameat(directory, temporary_cstr.as_c_str(), directory, RECORD_NAME)
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
    result.map_err(|_| StoreError::StorageIo)
}

fn create_temp_file(directory: &File) -> Result<(String, File), StoreError> {
    for _ in 0..32 {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("{TEMP_PREFIX}{}-{stamp}-{counter}", std::process::id());
        let c_name = CString::new(name.as_str()).map_err(|_| StoreError::StorageIo)?;
        match openat(
            directory,
            c_name.as_c_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(OWNER_FILE_MODE as _),
        ) {
            Ok(descriptor) => {
                let file = File::from(descriptor);
                let secured = rustix::fs::fchmod(&file, Mode::from_raw_mode(OWNER_FILE_MODE as _))
                    .map_err(|_| StoreError::UnsafeStoragePath)
                    .and_then(|()| validate_regular_file(&file, OWNER_FILE_MODE));
                if let Err(error) = secured {
                    drop(file);
                    let _ = unlinkat(directory, c_name.as_c_str(), AtFlags::empty());
                    return Err(error);
                }
                return Ok((name, file));
            }
            Err(error) if error == Errno::EXIST => continue,
            Err(_) => return Err(StoreError::StorageIo),
        }
    }
    Err(StoreError::StorageIo)
}

fn open_record_file(directory: &File) -> Result<Option<File>, StoreError> {
    validate_directory(directory)?;
    let stat = match statat(directory, RECORD_NAME, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(error) if error == Errno::NOENT => return Ok(None),
        Err(_) => return Err(StoreError::UnsafeStoragePath),
    };
    validate_stat(&stat, OWNER_FILE_MODE)?;
    let descriptor = openat(
        directory,
        RECORD_NAME,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| StoreError::UnsafeStoragePath)?;
    let file = File::from(descriptor);
    validate_regular_file(&file, OWNER_FILE_MODE)?;
    let metadata = file.metadata().map_err(|_| StoreError::StorageIo)?;
    if metadata.len() > MAX_RECORD_BYTES as u64 {
        return Err(StoreError::RecordTooLarge);
    }
    Ok(Some(file))
}

fn validate_named_file(directory: &File, name: &CStr, mode: u32) -> Result<(), StoreError> {
    let stat = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| StoreError::UnsafeStoragePath)?;
    validate_stat(&stat, mode)
}

fn validate_stat(stat: &rustix::fs::Stat, expected_mode: u32) -> Result<(), StoreError> {
    let file_type = FileType::from_raw_mode(stat.st_mode);
    if !file_type.is_file()
        || stat.st_uid != geteuid().as_raw()
        || (stat.st_mode as u32) & 0o7777 != expected_mode
        || stat.st_nlink != 1
    {
        return Err(StoreError::UnsafeStoragePath);
    }
    Ok(())
}

fn validate_regular_file(file: &File, expected_mode: u32) -> Result<(), StoreError> {
    let metadata = file.metadata().map_err(|_| StoreError::StorageIo)?;
    if !metadata.file_type().is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o7777 != expected_mode
        || metadata.nlink() != 1
    {
        return Err(StoreError::UnsafeStoragePath);
    }
    Ok(())
}

fn validate_directory(directory: &File) -> Result<(), StoreError> {
    let metadata = directory
        .metadata()
        .map_err(|_| StoreError::UnsafeStoragePath)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o7777 != OWNER_DIR_MODE
    {
        return Err(StoreError::UnsafeStoragePath);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        CommitStage, MAX_RECORD_BYTES, commit_file_record, decode_record_file, open_record_file,
        open_state_directory,
    };
    use crate::adapter::codex::auth::StoreError;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let base = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
            let path = base.join(format!(
                "kanata-codex-auth-{}-{}",
                std::process::id(),
                TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create synthetic directory");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .expect("secure synthetic directory");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn injected_pre_rename_failures_remove_temporary_files() {
        for failed_stage in [
            CommitStage::Created,
            CommitStage::Written,
            CommitStage::FileSynced,
        ] {
            let temp = TestDir::new();
            let directory = open_state_directory(temp.path()).expect("open secure directory");
            let result = commit_file_record(&directory, b"synthetic", &mut |stage| {
                if stage == failed_stage {
                    Err(std::io::Error::other("injected failure"))
                } else {
                    Ok(())
                }
            });
            assert_eq!(result, Err(StoreError::StorageIo));
            assert_eq!(
                fs::read_dir(temp.path()).expect("list directory").count(),
                0
            );
        }
    }

    #[test]
    fn failure_after_rename_is_not_acknowledged_and_keeps_no_temp() {
        let temp = TestDir::new();
        let directory = open_state_directory(temp.path()).expect("open secure directory");
        let result = commit_file_record(&directory, b"synthetic", &mut |stage| {
            if stage == CommitStage::Renamed {
                Err(std::io::Error::other("injected failure"))
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(StoreError::StorageIo));
        let names = fs::read_dir(temp.path())
            .expect("list directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, [std::ffi::OsString::from("credential-v1.json")]);
    }

    #[test]
    fn record_read_is_bounded_if_file_grows_after_open() {
        let temp = TestDir::new();
        let directory = open_state_directory(temp.path()).expect("open secure directory");
        let path = temp.path().join("credential-v1.json");
        fs::write(
            &path,
            br#"{"version":1,"refresh_token":"TEST_ONLY_REFRESH","account_id":"TEST_ONLY_ACCOUNT"}"#,
        )
        .expect("create synthetic record");
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

        assert!(matches!(
            decode_record_file(opened),
            Err(StoreError::RecordTooLarge)
        ));
    }
}
