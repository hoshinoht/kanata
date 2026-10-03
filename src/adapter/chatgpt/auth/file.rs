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

use super::AuthError;
use super::record::{MAX_RECORD_BYTES, State};

const RECORD_NAME: &CStr = c"chatgpt-v1.json";
const LOCK_NAME: &CStr = c"chatgpt.lock";
const TEMP_PREFIX: &str = ".chatgpt-tmp-";
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

pub(super) fn open_state_directory(path: &Path) -> Result<File, AuthError> {
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
        return Err(AuthError::Storage);
    }

    let root = File::from(
        open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| AuthError::Storage)?,
    );
    let mut current = root;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = CString::new(name.as_bytes()).map_err(|_| AuthError::Storage)?;
        let next = openat(
            &current,
            name.as_c_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| AuthError::Storage)?;
        current = File::from(next);
    }
    validate_directory(&current)?;
    Ok(current)
}

pub(super) fn open_lock_file(directory: &File) -> Result<File, AuthError> {
    let descriptor = match openat(
        directory,
        LOCK_NAME,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(OWNER_FILE_MODE as _),
    ) {
        Ok(descriptor) => {
            let file = File::from(descriptor);
            rustix::fs::fchmod(&file, Mode::from_raw_mode(OWNER_FILE_MODE as _))
                .map_err(|_| AuthError::Storage)?;
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
            .map_err(|_| AuthError::Storage)?;
            File::from(descriptor)
        }
        Err(_) => return Err(AuthError::Storage),
    };
    validate_regular_file(&descriptor, OWNER_FILE_MODE)?;
    Ok(descriptor)
}

pub(super) fn read_record(directory: &File) -> Result<Option<State>, AuthError> {
    let Some(file) = open_record_file(directory)? else {
        return Ok(None);
    };
    decode_record_file(file).map(Some)
}

fn decode_record_file(mut file: File) -> Result<State, AuthError> {
    let mut bytes = Vec::with_capacity(MAX_RECORD_BYTES + 1);
    Read::by_ref(&mut file)
        .take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| AuthError::Storage)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(AuthError::Storage);
    }
    State::decode(&bytes)
}

pub(super) fn write_record(directory: &File, record: &[u8]) -> Result<(), AuthError> {
    commit_file_record(directory, record, &mut |_| Ok(()))
}

fn commit_file_record(
    directory: &File,
    record: &[u8],
    failpoint: &mut impl FnMut(CommitStage) -> std::io::Result<()>,
) -> Result<(), AuthError> {
    if record.len() > MAX_RECORD_BYTES {
        return Err(AuthError::Storage);
    }
    validate_directory(directory)?;
    let (temporary_name, mut temporary_file) = create_temp_file(directory)?;
    let temporary_cstr = CString::new(temporary_name.as_str()).map_err(|_| AuthError::Storage)?;
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
    result.map_err(|_| AuthError::Storage)
}

fn create_temp_file(directory: &File) -> Result<(String, File), AuthError> {
    for _ in 0..32 {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("{TEMP_PREFIX}{}-{stamp}-{counter}", std::process::id());
        let c_name = CString::new(name.as_str()).map_err(|_| AuthError::Storage)?;
        match openat(
            directory,
            c_name.as_c_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(OWNER_FILE_MODE as _),
        ) {
            Ok(descriptor) => {
                let file = File::from(descriptor);
                let secured = rustix::fs::fchmod(&file, Mode::from_raw_mode(OWNER_FILE_MODE as _))
                    .map_err(|_| AuthError::Storage)
                    .and_then(|()| validate_regular_file(&file, OWNER_FILE_MODE));
                if let Err(error) = secured {
                    drop(file);
                    let _ = unlinkat(directory, c_name.as_c_str(), AtFlags::empty());
                    return Err(error);
                }
                return Ok((name, file));
            }
            Err(error) if error == Errno::EXIST => continue,
            Err(_) => return Err(AuthError::Storage),
        }
    }
    Err(AuthError::Storage)
}

fn open_record_file(directory: &File) -> Result<Option<File>, AuthError> {
    validate_directory(directory)?;
    let stat = match statat(directory, RECORD_NAME, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(error) if error == Errno::NOENT => return Ok(None),
        Err(_) => return Err(AuthError::Storage),
    };
    validate_stat(&stat, OWNER_FILE_MODE)?;
    let descriptor = openat(
        directory,
        RECORD_NAME,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| AuthError::Storage)?;
    let file = File::from(descriptor);
    validate_regular_file(&file, OWNER_FILE_MODE)?;
    let metadata = file.metadata().map_err(|_| AuthError::Storage)?;
    if metadata.len() > MAX_RECORD_BYTES as u64 {
        return Err(AuthError::Storage);
    }
    Ok(Some(file))
}

fn validate_named_file(directory: &File, name: &CStr, mode: u32) -> Result<(), AuthError> {
    let stat =
        statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| AuthError::Storage)?;
    validate_stat(&stat, mode)
}

// st_mode is u16 on macOS and u32 on Linux.
#[allow(clippy::unnecessary_cast)]
fn validate_stat(stat: &rustix::fs::Stat, expected_mode: u32) -> Result<(), AuthError> {
    let file_type = FileType::from_raw_mode(stat.st_mode);
    if !file_type.is_file()
        || stat.st_uid != geteuid().as_raw()
        || (stat.st_mode as u32) & 0o7777 != expected_mode
        || stat.st_nlink != 1
    {
        return Err(AuthError::Storage);
    }
    Ok(())
}

fn validate_regular_file(file: &File, expected_mode: u32) -> Result<(), AuthError> {
    let metadata = file.metadata().map_err(|_| AuthError::Storage)?;
    if !metadata.file_type().is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o7777 != expected_mode
        || metadata.nlink() != 1
    {
        return Err(AuthError::Storage);
    }
    Ok(())
}

fn validate_directory(directory: &File) -> Result<(), AuthError> {
    let metadata = directory.metadata().map_err(|_| AuthError::Storage)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o7777 != OWNER_DIR_MODE
    {
        return Err(AuthError::Storage);
    }
    Ok(())
}
