use serde::{Deserialize, Serialize};

use super::{Credential, StoreError};

const RECORD_VERSION: u32 = 1;
pub(super) const MAX_RECORD_BYTES: usize = 20 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRecord {
    version: u32,
    refresh_token: String,
    account_id: String,
}

#[derive(Serialize)]
struct StoredRecordRef<'a> {
    version: u32,
    refresh_token: &'a str,
    account_id: &'a str,
}

pub(super) fn encode(credential: &Credential) -> Result<Vec<u8>, StoreError> {
    let record = StoredRecordRef {
        version: RECORD_VERSION,
        refresh_token: credential.refresh_token(),
        account_id: credential.account_id(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|_| StoreError::InvalidRecord)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(StoreError::RecordTooLarge);
    }
    Ok(bytes)
}

pub(super) fn decode(bytes: &[u8]) -> Result<Credential, StoreError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(StoreError::RecordTooLarge);
    }
    let record: StoredRecord =
        serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidRecord)?;
    if record.version != RECORD_VERSION {
        return Err(StoreError::InvalidRecord);
    }
    Credential::new(record.refresh_token, record.account_id).map_err(|_| StoreError::InvalidRecord)
}
