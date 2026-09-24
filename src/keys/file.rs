//! `keys.toml` v1: bounded read, strict validation and rendering.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Read as _};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::{
    ConfigError, KeyRateLimit, RawPermission, RawRateLimit, ValidatedRoute, parse_sha256_hex,
    parse_toml, valid_identifier, validate_key_limits, validate_permissions,
};
use crate::core::RouteSelector;
use crate::keys::time;

pub const KEYS_FILE_VERSION: i64 = 1;
pub const MAX_KEYS_FILE_BYTES: u64 = 1024 * 1024;
/// Includes revoked records; ids are never reused.
pub const MAX_KEY_RECORDS: usize = 1000;

const FILE_PATH: &str = "keys.file";
const ROOT: &str = "keys_file";

/// Reads a keys file; `Ok(None)` only when it does not exist.
pub fn read(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ConfigError::new(FILE_PATH, "read_error")),
    };
    let metadata = file
        .metadata()
        .map_err(|_| ConfigError::new(FILE_PATH, "read_error"))?;
    if !metadata.is_file() {
        return Err(ConfigError::new(FILE_PATH, "read_error"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(ConfigError::new(FILE_PATH, "insecure_permissions"));
        }
    }
    if metadata.len() > MAX_KEYS_FILE_BYTES {
        return Err(ConfigError::new(FILE_PATH, "too_large"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_KEYS_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ConfigError::new(FILE_PATH, "read_error"))?;
    if bytes.len() as u64 > MAX_KEYS_FILE_BYTES {
        return Err(ConfigError::new(FILE_PATH, "too_large"));
    }
    Ok(Some(bytes))
}

/// Parses and validates a keys file against the config's routes.
/// Revoked records skip the route check so removing a route never blocks startup.
pub fn parse(bytes: &[u8], routes: &[ValidatedRoute]) -> Result<KeysFile, ConfigError> {
    parse_inner(bytes, Some(routes))
}

/// Like [`parse`] without route checks, for listing a file without its config.
pub fn parse_without_routes(bytes: &[u8]) -> Result<KeysFile, ConfigError> {
    parse_inner(bytes, None)
}

/// Every record of a validated keys file, including revoked ones.
#[derive(Clone, Debug, Default)]
pub struct KeysFile {
    records: Vec<StoredKey>,
    sha256: [u8; 32],
}

impl KeysFile {
    pub fn records(&self) -> &[StoredKey] {
        &self.records
    }
    pub fn records_mut(&mut self) -> &mut [StoredKey] {
        &mut self.records
    }
    pub fn push(&mut self, record: StoredKey) {
        self.records.push(record);
    }
    pub fn active(&self) -> impl Iterator<Item = &StoredKey> {
        self.records.iter().filter(|record| !record.is_revoked())
    }
    /// SHA-256 of the bytes this file was parsed from.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// Canonical `keys.toml` text.
    pub fn render(&self) -> String {
        let raw = RawKeysFile {
            version: Some(KEYS_FILE_VERSION),
            keys: self.records.iter().map(StoredKey::to_raw).collect(),
        };
        toml::to_string(&raw).expect("keys file schema serializes")
    }

    /// Re-validates the rendered form, as the server would read it.
    pub fn validated(&self, routes: &[ValidatedRoute]) -> Result<KeysFile, ConfigError> {
        parse(self.render().as_bytes(), routes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredKey {
    id: String,
    digest: [u8; 32],
    owner: bool,
    permissions: Vec<RouteSelector>,
    max_in_flight: Option<u64>,
    rate_limit: Option<KeyRateLimit>,
    created_at: u64,
    expires_at: Option<u64>,
    rotated_at: Option<u64>,
    revoked_at: Option<u64>,
}

impl StoredKey {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        digest: [u8; 32],
        owner: bool,
        permissions: Vec<RouteSelector>,
        max_in_flight: Option<u64>,
        rate_limit: Option<KeyRateLimit>,
        created_at: u64,
        expires_at: Option<u64>,
    ) -> Self {
        Self {
            id,
            digest,
            owner,
            permissions,
            max_in_flight,
            rate_limit,
            created_at,
            expires_at,
            rotated_at: None,
            revoked_at: None,
        }
    }

    pub fn revoke(&mut self, at: u64) {
        self.revoked_at = Some(at);
    }

    pub fn rotate(&mut self, digest: [u8; 32], at: u64, expires_at: Option<u64>) {
        self.digest = digest;
        self.rotated_at = Some(at);
        self.expires_at = expires_at;
    }

    pub fn set_permissions(&mut self, permissions: Vec<RouteSelector>) {
        self.permissions = permissions;
    }

    pub fn set_expires_at(&mut self, expires_at: Option<u64>) {
        self.expires_at = expires_at;
    }

    pub fn set_limits(&mut self, max_in_flight: Option<u64>, rate_limit: Option<KeyRateLimit>) {
        self.max_in_flight = max_in_flight;
        self.rate_limit = rate_limit;
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
    pub fn is_owner(&self) -> bool {
        self.owner
    }
    pub fn permissions(&self) -> &[RouteSelector] {
        &self.permissions
    }
    pub fn max_in_flight(&self) -> Option<u64> {
        self.max_in_flight
    }
    pub fn rate_limit(&self) -> Option<KeyRateLimit> {
        self.rate_limit
    }
    pub fn created_at(&self) -> u64 {
        self.created_at
    }
    pub fn expires_at(&self) -> Option<u64> {
        self.expires_at
    }
    pub fn rotated_at(&self) -> Option<u64> {
        self.rotated_at
    }
    pub fn revoked_at(&self) -> Option<u64> {
        self.revoked_at
    }
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }

    fn to_raw(&self) -> RawStoredKey {
        RawStoredKey {
            id: self.id.clone(),
            digest: format!(
                "sha256:{}",
                self.digest
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
            owner: self.owner,
            permissions: self
                .permissions
                .iter()
                .map(|selector| RawPermission {
                    model_alias: selector.model_alias.0.clone(),
                    operation: selector.operation,
                })
                .collect(),
            max_in_flight: self.max_in_flight,
            rate_limit: self.rate_limit.map(|limit| RawRateLimit {
                requests: limit.requests,
                per_ms: limit.per_ms,
            }),
            created_at: time::format(self.created_at),
            expires_at: self.expires_at.map(time::format),
            rotated_at: self.rotated_at.map(time::format),
            revoked_at: self.revoked_at.map(time::format),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawKeysFile {
    #[serde(default)]
    version: Option<i64>,
    #[serde(default)]
    keys: Vec<RawStoredKey>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawStoredKey {
    id: String,
    digest: String,
    #[serde(default)]
    owner: bool,
    permissions: Vec<RawPermission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_in_flight: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rate_limit: Option<RawRateLimit>,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rotated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at: Option<String>,
}

fn parse_inner(bytes: &[u8], routes: Option<&[ValidatedRoute]>) -> Result<KeysFile, ConfigError> {
    let sha256 = Sha256::digest(bytes).into();
    let contents = std::str::from_utf8(bytes).map_err(|_| ConfigError::new(ROOT, "parse_error"))?;
    let raw: RawKeysFile = parse_toml(contents, ROOT)?;
    match raw.version {
        None => return Err(ConfigError::new(format!("{ROOT}.version"), "required")),
        Some(KEYS_FILE_VERSION) => {}
        Some(_) => {
            return Err(ConfigError::new(
                format!("{ROOT}.version"),
                "unsupported_version",
            ));
        }
    }
    if raw.keys.len() > MAX_KEY_RECORDS {
        return Err(ConfigError::new(format!("{ROOT}.keys"), "too_many"));
    }
    let mut ids = BTreeSet::new();
    let mut digests = BTreeSet::new();
    let mut owner_seen = false;
    let mut records = Vec::with_capacity(raw.keys.len());
    for (index, key) in raw.keys.into_iter().enumerate() {
        let path = format!("{ROOT}.keys[{index}]");
        let timestamp = |value: &str, field: &str| {
            time::parse(value)
                .ok_or_else(|| ConfigError::new(format!("{path}.{field}"), "invalid_timestamp"))
        };
        let optional_timestamp = |value: Option<&str>, field: &str| {
            value.map(|value| timestamp(value, field)).transpose()
        };
        if !valid_identifier(&key.id) || !ids.insert(key.id.clone()) {
            return Err(ConfigError::new(
                format!("{path}.id"),
                "invalid_or_duplicate_id",
            ));
        }
        let digest = key
            .digest
            .strip_prefix("sha256:")
            .and_then(parse_sha256_hex)
            .ok_or_else(|| ConfigError::new(format!("{path}.digest"), "invalid_digest"))?;
        if !digests.insert(digest) {
            return Err(ConfigError::new(
                format!("{path}.digest"),
                "duplicate_secret",
            ));
        }
        let created_at = timestamp(&key.created_at, "created_at")?;
        let expires_at = optional_timestamp(key.expires_at.as_deref(), "expires_at")?;
        let rotated_at = optional_timestamp(key.rotated_at.as_deref(), "rotated_at")?;
        let revoked_at = optional_timestamp(key.revoked_at.as_deref(), "revoked_at")?;
        if key.owner && revoked_at.is_none() {
            if owner_seen {
                return Err(ConfigError::new(format!("{path}.owner"), "multiple_owners"));
            }
            owner_seen = true;
        }
        let route_check = if revoked_at.is_some() { None } else { routes };
        let permissions = validate_permissions(&key.permissions, &path, route_check)?;
        let (max_in_flight, rate_limit) =
            validate_key_limits(key.max_in_flight, key.rate_limit, &path)?;
        records.push(StoredKey {
            id: key.id,
            digest,
            owner: key.owner,
            permissions,
            max_in_flight,
            rate_limit,
            created_at,
            expires_at,
            rotated_at,
            revoked_at,
        });
    }
    Ok(KeysFile { records, sha256 })
}
