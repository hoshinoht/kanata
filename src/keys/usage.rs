//! Per-key usage state: `usage-<plane>.json` format, merged reader and the server recorder.
//! Synchronous and std-only so the CLI can read it without a runtime.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Plane, valid_identifier};
use crate::keys::{file, time};

pub const USAGE_FILE_VERSION: u32 = 1;
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
const MAX_USAGE_FILE_BYTES: u64 = 1024 * 1024;

/// Usage of one key id.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyUsage {
    pub requests: u64,
    /// Unix seconds.
    pub last_used_at: u64,
}

impl KeyUsage {
    fn merge(&mut self, other: KeyUsage) {
        self.requests = self.requests.saturating_add(other.requests);
        self.last_used_at = self.last_used_at.max(other.last_used_at);
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawUsageFile {
    version: u32,
    plane: String,
    keys: BTreeMap<String, RawKeyUsage>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawKeyUsage {
    requests: u64,
    last_used_at: String,
}

pub fn file_name(plane: Plane) -> String {
    format!("usage-{}.json", plane.as_str())
}

/// Reads one usage file; `Ok(None)` when it does not exist, `Err` when it is invalid.
fn read_file(path: &Path) -> Result<Option<BTreeMap<String, KeyUsage>>, ()> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !file.metadata().map_err(|_| ())?.is_file() {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(MAX_USAGE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > MAX_USAGE_FILE_BYTES {
        return Err(());
    }
    parse(&bytes).map(Some)
}

fn parse(bytes: &[u8]) -> Result<BTreeMap<String, KeyUsage>, ()> {
    let raw: RawUsageFile = serde_json::from_slice(bytes).map_err(|_| ())?;
    if raw.version != USAGE_FILE_VERSION
        || Plane::parse(&raw.plane).is_none()
        || raw.keys.len() > file::MAX_KEY_RECORDS
    {
        return Err(());
    }
    raw.keys
        .into_iter()
        .map(|(id, usage)| {
            let last_used_at = time::parse(&usage.last_used_at).ok_or(())?;
            valid_identifier(&id).then_some(()).ok_or(())?;
            Ok((
                id,
                KeyUsage {
                    requests: usage.requests,
                    last_used_at,
                },
            ))
        })
        .collect()
}

fn render(plane: Plane, keys: &BTreeMap<String, KeyUsage>) -> Vec<u8> {
    let raw = RawUsageFile {
        version: USAGE_FILE_VERSION,
        plane: plane.as_str().to_owned(),
        keys: keys
            .iter()
            .map(|(id, usage)| {
                (
                    id.clone(),
                    RawKeyUsage {
                        requests: usage.requests,
                        last_used_at: time::format(usage.last_used_at),
                    },
                )
            })
            .collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&raw).expect("usage file serializes");
    bytes.push(b'\n');
    bytes
}

/// Merges every valid `usage-*.json` in `dir` and its immediate subdirectories:
/// requests are summed, `last_used_at` is the latest. Missing dirs and invalid files are skipped.
pub fn read_merged(dir: &Path) -> BTreeMap<String, KeyUsage> {
    let mut merged = BTreeMap::new();
    let mut dirs = vec![dir.to_path_buf()];
    if let Ok(entries) = fs::read_dir(dir) {
        let mut subdirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        subdirs.sort();
        dirs.extend(subdirs);
    }
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !(name.starts_with("usage-") && name.ends_with(".json")) {
                continue;
            }
            if let Ok(Some(keys)) = read_file(&entry.path()) {
                for (id, usage) in keys {
                    merged
                        .entry(id)
                        .or_insert_with(KeyUsage::default)
                        .merge(usage);
                }
            }
        }
    }
    merged
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushOutcome {
    /// Nothing recorded since the last successful write.
    Clean,
    Written,
    /// Write failed; the counts stay in memory and are retried on the next flush.
    Failed,
}

/// Server-side usage recorder for one plane; counts survive restarts via `usage-<plane>.json`.
#[derive(Clone)]
pub struct UsageHandle(Arc<Inner>);

struct Inner {
    path: PathBuf,
    keys_path: PathBuf,
    plane: Plane,
    state: Mutex<State>,
    /// Serializes writers; never taken on the request path.
    flush: Mutex<FlushState>,
}

#[derive(Default)]
struct State {
    keys: HashMap<String, KeyUsage>,
    dirty: bool,
}

#[derive(Default)]
struct FlushState {
    failing: bool,
}

impl UsageHandle {
    /// Loads this plane's file from `usage_dir`; an invalid file is replaced on the next flush.
    pub fn open(usage_dir: &Path, keys_path: &Path, plane: Plane) -> Self {
        let path = usage_dir.join(file_name(plane));
        let keys = match read_file(&path) {
            Ok(keys) => keys.unwrap_or_default().into_iter().collect(),
            Err(()) => {
                tracing::warn!(
                    target: "kanata::keys",
                    path = %path.display(),
                    "usage state file invalid; starting fresh",
                );
                HashMap::new()
            }
        };
        Self(Arc::new(Inner {
            path,
            keys_path: keys_path.to_path_buf(),
            plane,
            state: Mutex::new(State { keys, dirty: false }),
            flush: Mutex::new(FlushState::default()),
        }))
    }

    /// Counts one authenticated request.
    pub fn record(&self, key_id: &str) {
        let now = time::now();
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        // No allocation for ids already seen.
        if let Some(usage) = state.keys.get_mut(key_id) {
            usage.requests = usage.requests.saturating_add(1);
            usage.last_used_at = usage.last_used_at.max(now);
        } else {
            state.keys.insert(
                key_id.to_owned(),
                KeyUsage {
                    requests: 1,
                    last_used_at: now,
                },
            );
        }
        state.dirty = true;
    }

    /// Current in-memory counts.
    pub fn snapshot(&self) -> BTreeMap<String, KeyUsage> {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .keys
            .iter()
            .map(|(id, usage)| (id.clone(), *usage))
            .collect()
    }

    /// Writes the file when dirty (blocking IO), pruning ids absent from the keys file.
    #[doc(hidden)]
    pub fn flush_now(&self) -> FlushOutcome {
        let mut flush = self.0.flush.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = {
            let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            if !state.dirty {
                return FlushOutcome::Clean;
            }
            state.dirty = false;
            state
                .keys
                .iter()
                .map(|(id, usage)| (id.clone(), *usage))
                .collect::<BTreeMap<_, _>>()
        };
        let mut keys = snapshot.clone();
        let known = self.known_ids();
        if let Some(known) = &known {
            keys.retain(|id, _| known.contains(id));
        }
        match write_atomic(&self.0.path, &render(self.0.plane, &keys)) {
            Ok(()) => {
                if let Some(known) = known {
                    let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
                    // Ids recorded after the keys file was read are kept.
                    state
                        .keys
                        .retain(|id, _| known.contains(id) || !snapshot.contains_key(id));
                }
                flush.failing = false;
                FlushOutcome::Written
            }
            Err(error) => {
                self.0
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .dirty = true;
                if !flush.failing {
                    tracing::warn!(
                        target: "kanata::keys",
                        path = %self.0.path.display(),
                        error = %error.kind(),
                        "usage state write failed; counts kept in memory",
                    );
                    flush.failing = true;
                }
                FlushOutcome::Failed
            }
        }
    }

    /// Every id in the keys file, revoked included; `None` when it cannot be read or parsed.
    fn known_ids(&self) -> Option<BTreeSet<String>> {
        let bytes = file::read(&self.0.keys_path).ok()??;
        let keys = file::parse_without_routes(&bytes).ok()?;
        Some(
            keys.records()
                .iter()
                .map(|record| record.id().to_owned())
                .collect(),
        )
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::from(ErrorKind::InvalidInput))?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::from(ErrorKind::InvalidInput))?;
    let temp = dir.join(format!(".{}.tmp", name.to_string_lossy()));
    let _ = fs::remove_file(&temp);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_files_parse_back() {
        let keys = BTreeMap::from([(
            "alpha".to_owned(),
            KeyUsage {
                requests: 3,
                last_used_at: 1_790_000_000,
            },
        )]);
        assert_eq!(parse(&render(Plane::Public, &keys)), Ok(keys));
        for invalid in [
            r#"{"version":2,"plane":"all","keys":{}}"#,
            r#"{"version":1,"plane":"other","keys":{}}"#,
            r#"{"version":1,"plane":"all","keys":{"a b":{"requests":1,"last_used_at":"2026-01-01T00:00:00Z"}}}"#,
            r#"{"version":1,"plane":"all","keys":{"a":{"requests":1,"last_used_at":"yesterday"}}}"#,
        ] {
            assert_eq!(parse(invalid.as_bytes()), Err(()), "{invalid}");
        }
    }
}
