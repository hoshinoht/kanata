//! Durable daily admission reservations and per-model usage reports.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{config::Plane, core::Usage};

use super::usage::TokenUsage;

const DAY: u64 = 86_400;
const RETENTION_DAYS: u64 = 90;
const MAX_ROWS: usize = 100_000;
const MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DailyQuota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_tokens: Option<u64>,
}

impl DailyQuota {
    pub fn validate(self) -> Result<(), &'static str> {
        if self.requests.is_none() && self.tokens.is_none() {
            return Err("empty");
        }
        if [self.requests, self.tokens, self.reservation_tokens].contains(&Some(0)) {
            return Err("zero");
        }
        match (self.tokens, self.reservation_tokens) {
            (Some(total), Some(reservation)) if reservation <= total => Ok(()),
            (None, None) => Ok(()),
            _ => Err("invalid_token_reservation"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DailyUsage {
    pub day: u64,
    pub key_id: String,
    pub model: String,
    pub operation: String,
    pub requests: u64,
    pub charged_tokens: u64,
    pub retained_reservation_tokens: u64,
    pub overrun_tokens: u64,
    pub tokens: TokenUsage,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LedgerFile {
    version: u32,
    plane: String,
    latest_day: u64,
    rows: Vec<DailyUsage>,
}

#[derive(Clone)]
pub struct DailyLedger {
    dir: PathBuf,
    plane: Plane,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaError {
    Exhausted { retry_after: u64 },
    Unavailable,
    ClockRollback,
}

/// A reservation is charged even if dispatch, completion or the process is cancelled.
pub(crate) struct Reservation {
    ledger: DailyLedger,
    day: u64,
    key: String,
    model: String,
    operation: String,
    tokens: u64,
}

impl DailyLedger {
    pub fn new(dir: &Path, plane: Plane) -> Self {
        Self {
            dir: dir.to_owned(),
            plane,
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.join(format!("daily-{}.json", self.plane.as_str()))
    }

    pub fn read(&self) -> Result<Vec<DailyUsage>, QuotaError> {
        Ok(self.load()?.rows)
    }

    fn load(&self) -> Result<LedgerFile, QuotaError> {
        let file = match File::open(self.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LedgerFile {
                    version: 1,
                    plane: self.plane.as_str().into(),
                    latest_day: 0,
                    rows: Vec::new(),
                });
            }
            Err(_) => return Err(QuotaError::Unavailable),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| QuotaError::Unavailable)?;
        let data: LedgerFile =
            serde_json::from_slice(&bytes).map_err(|_| QuotaError::Unavailable)?;
        if bytes.len() as u64 > MAX_BYTES
            || data.version != 1
            || data.plane != self.plane.as_str()
            || data.latest_day
                > super::time::parse("9999-12-31T00:00:00Z").expect("valid date") / DAY
            || data.rows.len() > MAX_ROWS
        {
            return Err(QuotaError::Unavailable);
        }
        let mut selectors = std::collections::BTreeSet::new();
        for row in &data.rows {
            if row.day > data.latest_day
                || !crate::config::valid_identifier(&row.key_id)
                || crate::config::parse_model_alias(&row.model).is_none()
                || crate::core::Operation::parse(&row.operation).is_none()
                || row.retained_reservation_tokens > row.charged_tokens
                || row.tokens.reported.checked_add(row.tokens.missing) != Some(row.requests)
                || !selectors.insert((row.day, &row.key_id, &row.model, &row.operation))
            {
                return Err(QuotaError::Unavailable);
            }
        }
        Ok(data)
    }

    fn lock(&self) -> Result<File, QuotaError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(self.dir.join(format!("daily-{}.lock", self.plane.as_str())))
            .map_err(|_| QuotaError::Unavailable)?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) if start.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => return Err(QuotaError::Unavailable),
            }
        }
    }

    fn write(&self, data: &LedgerFile) -> Result<(), QuotaError> {
        let bytes = serde_json::to_vec(data).map_err(|_| QuotaError::Unavailable)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(QuotaError::Unavailable);
        }
        let temp = self.dir.join(format!(".daily-{}.tmp", self.plane.as_str()));
        let _ = fs::remove_file(&temp);
        let result = (|| -> std::io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut out = options.open(&temp)?;
            out.write_all(&bytes)?;
            out.sync_all()?;
            fs::rename(&temp, self.path())?;
            File::open(&self.dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result.map_err(|_| QuotaError::Unavailable)
    }

    pub(crate) fn reserve(
        &self,
        key: &str,
        model: &str,
        operation: &str,
        quota: Option<DailyQuota>,
        now: u64,
    ) -> Result<Reservation, QuotaError> {
        let _lock = self.lock()?;
        let mut data = self.load()?;
        let day = now / DAY;
        if day < data.latest_day {
            return Err(QuotaError::ClockRollback);
        }
        let reserved = quota
            .and_then(|quota| quota.reservation_tokens)
            .unwrap_or(0);
        let mut requests = 0u64;
        let mut tokens = 0u64;
        for row in data
            .rows
            .iter()
            .filter(|row| row.day == day && row.key_id == key)
        {
            requests = requests.saturating_add(row.requests);
            tokens = tokens.saturating_add(row.charged_tokens);
        }
        if quota.is_some_and(|quota| {
            quota.requests.is_some_and(|limit| requests >= limit)
                || quota.tokens.is_some_and(|limit| {
                    tokens
                        .checked_add(reserved)
                        .is_none_or(|total| total > limit)
                })
        }) {
            return Err(QuotaError::Exhausted {
                retry_after: DAY - now % DAY,
            });
        }
        data.latest_day = day;
        data.rows
            .retain(|row| row.day.saturating_add(RETENTION_DAYS) > day);
        let index = match data.rows.iter().position(|row| {
            row.day == day && row.key_id == key && row.model == model && row.operation == operation
        }) {
            Some(index) => index,
            None if data.rows.len() < MAX_ROWS => {
                data.rows.push(DailyUsage {
                    day,
                    key_id: key.into(),
                    model: model.into(),
                    operation: operation.into(),
                    requests: 0,
                    charged_tokens: 0,
                    retained_reservation_tokens: 0,
                    overrun_tokens: 0,
                    tokens: TokenUsage::default(),
                });
                data.rows.len() - 1
            }
            None => return Err(QuotaError::Unavailable),
        };
        let row = &mut data.rows[index];
        row.requests = row.requests.checked_add(1).ok_or(QuotaError::Unavailable)?;
        row.tokens.missing = row
            .tokens
            .missing
            .checked_add(1)
            .ok_or(QuotaError::Unavailable)?;
        row.charged_tokens = row
            .charged_tokens
            .checked_add(reserved)
            .ok_or(QuotaError::Unavailable)?;
        row.retained_reservation_tokens = row
            .retained_reservation_tokens
            .checked_add(reserved)
            .ok_or(QuotaError::Unavailable)?;
        self.write(&data)?;
        Ok(Reservation {
            ledger: self.clone(),
            day,
            key: key.into(),
            model: model.into(),
            operation: operation.into(),
            tokens: reserved,
        })
    }
}

impl Reservation {
    pub(crate) fn complete(self, usage: Option<&Usage>) -> Result<(), QuotaError> {
        let Some(usage) = usage else {
            return Ok(());
        };
        if usage.input_tokens.checked_add(usage.output_tokens) != Some(usage.total_tokens) {
            return Ok(());
        }
        let _lock = self.ledger.lock()?;
        let mut data = self.ledger.load()?;
        let Some(row) = data.rows.iter_mut().find(|row| {
            row.day == self.day
                && row.key_id == self.key
                && row.model == self.model
                && row.operation == self.operation
        }) else {
            return Ok(());
        };
        row.charged_tokens = row
            .charged_tokens
            .saturating_sub(self.tokens)
            .saturating_add(usage.total_tokens);
        row.retained_reservation_tokens =
            row.retained_reservation_tokens.saturating_sub(self.tokens);
        if self.tokens > 0 {
            row.overrun_tokens = row
                .overrun_tokens
                .saturating_add(usage.total_tokens.saturating_sub(self.tokens));
        }
        row.tokens.missing = row.tokens.missing.saturating_sub(1);
        row.tokens.reported = row.tokens.reported.saturating_add(1);
        row.tokens.input_tokens = row.tokens.input_tokens.saturating_add(usage.input_tokens);
        row.tokens.output_tokens = row.tokens.output_tokens.saturating_add(usage.output_tokens);
        row.tokens.reasoning_tokens = row
            .tokens
            .reasoning_tokens
            .saturating_add(usage.reasoning_tokens.unwrap_or(0));
        row.tokens.reasoning_reported = row
            .tokens
            .reasoning_reported
            .saturating_add(u64::from(usage.reasoning_tokens.is_some()));
        self.ledger.write(&data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("kanata-daily-{}", getrandom::u64().unwrap()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn ledger(&self, plane: Plane) -> DailyLedger {
            DailyLedger::new(&self.0, plane)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn quota(
        requests: Option<u64>,
        tokens: Option<u64>,
        reservation_tokens: Option<u64>,
    ) -> Option<DailyQuota> {
        Some(DailyQuota {
            requests,
            tokens,
            reservation_tokens,
        })
    }
    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn durable_request_allowance_is_atomic_across_independent_handles() {
        let fixture = Fixture::new();
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..12)
                .map(|_| {
                    let ledger = fixture.ledger(Plane::Private);
                    scope.spawn(move || {
                        ledger
                            .reserve("alpha", "chat", "chat", quota(Some(3), None, None), DAY)
                            .is_ok()
                    })
                })
                .collect();
            assert_eq!(
                workers
                    .into_iter()
                    .map(|worker| u64::from(worker.join().unwrap()))
                    .sum::<u64>(),
                3
            );
        });
        let restarted = fixture.ledger(Plane::Private);
        assert!(matches!(
            restarted.reserve(
                "alpha",
                "other-model",
                "chat",
                quota(Some(3), None, None),
                DAY
            ),
            Err(QuotaError::Exhausted { .. })
        ));
        assert_eq!(restarted.read().unwrap()[0].requests, 3);
        assert!(
            fixture
                .ledger(Plane::Public)
                .reserve("alpha", "chat", "chat", quota(Some(3), None, None), DAY)
                .is_ok()
        );
    }

    #[test]
    fn cancellation_and_missing_usage_retain_the_durable_reservation() {
        let fixture = Fixture::new();
        let ledger = fixture.ledger(Plane::All);
        let limit = quota(None, Some(100), Some(60));
        let first = ledger.reserve("alpha", "chat", "chat", limit, DAY).unwrap();
        assert!(matches!(
            ledger.reserve("alpha", "chat", "chat", limit, DAY),
            Err(QuotaError::Exhausted { .. })
        ));
        first.complete(Some(&usage(10, 10))).unwrap();
        let next = ledger.reserve("alpha", "chat", "chat", limit, DAY).unwrap();
        drop(next);
        let row = ledger.read().unwrap().remove(0);
        assert_eq!(
            (
                row.charged_tokens,
                row.retained_reservation_tokens,
                row.tokens.reported,
                row.tokens.missing
            ),
            (80, 60, 1, 1)
        );
        assert!(matches!(
            fixture
                .ledger(Plane::All)
                .reserve("alpha", "chat", "chat", limit, DAY),
            Err(QuotaError::Exhausted { .. })
        ));
    }

    #[test]
    fn reported_overrun_is_counted_and_blocks_later_attempts() {
        let fixture = Fixture::new();
        let ledger = fixture.ledger(Plane::All);
        let limit = quota(None, Some(100), Some(60));
        ledger
            .reserve("alpha", "chat", "chat", limit, DAY)
            .unwrap()
            .complete(Some(&usage(80, 70)))
            .unwrap();
        let row = ledger.read().unwrap().remove(0);
        assert_eq!(
            (
                row.charged_tokens,
                row.overrun_tokens,
                row.retained_reservation_tokens
            ),
            (150, 90, 0)
        );
        assert!(matches!(
            ledger.reserve("alpha", "chat", "chat", limit, DAY),
            Err(QuotaError::Exhausted { .. })
        ));
    }

    #[test]
    fn utc_boundaries_keep_late_usage_on_original_day_and_reject_clock_rollback() {
        let fixture = Fixture::new();
        let ledger = fixture.ledger(Plane::All);
        let limit = quota(Some(1), Some(100), Some(100));
        let yesterday = ledger
            .reserve("alpha", "chat", "chat", limit, 2 * DAY - 1)
            .unwrap();
        assert!(matches!(
            ledger.reserve("alpha", "chat", "chat", limit, 2 * DAY - 1),
            Err(QuotaError::Exhausted { retry_after: 1 })
        ));
        let today = ledger
            .reserve("alpha", "chat", "chat", limit, 2 * DAY)
            .unwrap();
        yesterday.complete(Some(&usage(1, 2))).unwrap();
        today.complete(None).unwrap();
        let rows = ledger.read().unwrap();
        assert_eq!(rows[0].charged_tokens, 3);
        assert_eq!(rows[1].charged_tokens, 100);
        assert!(matches!(
            fixture
                .ledger(Plane::All)
                .reserve("beta", "chat", "chat", limit, DAY),
            Err(QuotaError::ClockRollback)
        ));
    }

    #[test]
    fn malformed_state_is_never_replaced_with_empty_allowances() {
        let fixture = Fixture::new();
        let ledger = fixture.ledger(Plane::All);
        fs::write(ledger.path(), b"broken").unwrap();
        assert!(matches!(
            ledger.reserve("alpha", "chat", "chat", quota(Some(1), None, None), DAY),
            Err(QuotaError::Unavailable)
        ));
        assert_eq!(fs::read(ledger.path()).unwrap(), b"broken");
    }

    #[test]
    fn changing_limits_does_not_clear_prior_requests_or_missing_tokens() {
        let fixture = Fixture::new();
        let ledger = fixture.ledger(Plane::All);
        ledger
            .reserve(
                "alpha",
                "chat",
                "chat",
                quota(Some(3), Some(90), Some(30)),
                DAY,
            )
            .unwrap()
            .complete(None)
            .unwrap();
        assert!(matches!(
            ledger.reserve("alpha", "chat", "chat", quota(Some(1), None, None), DAY),
            Err(QuotaError::Exhausted { .. })
        ));
        assert!(matches!(
            ledger.reserve(
                "alpha",
                "chat",
                "chat",
                quota(None, Some(40), Some(20)),
                DAY
            ),
            Err(QuotaError::Exhausted { .. })
        ));
    }
}
