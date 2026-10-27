//! Durable daily and monthly admission reservations and per-model usage reports.
//! Month-to-date usage is the sum of the current UTC month's day rows.

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

pub use crate::config::keys::quota::DailyQuota;

/// A key's daily and monthly allowances.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyQuotas {
    pub daily: Option<DailyQuota>,
    pub monthly: Option<DailyQuota>,
}

impl KeyQuotas {
    pub fn is_some(self) -> bool {
        self.daily.is_some() || self.monthly.is_some()
    }

    /// Tokens reserved per request: the larger of the two `reservation_tokens`.
    pub fn reservation_tokens(self) -> u64 {
        [self.daily, self.monthly]
            .into_iter()
            .flatten()
            .filter_map(|quota| quota.reservation_tokens)
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaPeriod {
    Day,
    Month,
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

impl DailyUsage {
    /// Adds `other`'s counters; the selector fields are kept.
    pub(crate) fn merge(&mut self, other: &Self) {
        self.requests = self.requests.saturating_add(other.requests);
        self.charged_tokens = self.charged_tokens.saturating_add(other.charged_tokens);
        self.retained_reservation_tokens = self
            .retained_reservation_tokens
            .saturating_add(other.retained_reservation_tokens);
        self.overrun_tokens = self.overrun_tokens.saturating_add(other.overrun_tokens);
        self.tokens.merge(other.tokens);
    }
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
    /// `retry_after` is seconds to the start of the next UTC day or month.
    Exhausted {
        retry_after: u64,
        period: QuotaPeriod,
    },
    Unavailable,
    ClockRollback,
}

/// A reservation is charged even if dispatch, completion or the process is cancelled.
pub struct Reservation {
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

    pub fn reserve(
        &self,
        key: &str,
        model: &str,
        operation: &str,
        quotas: KeyQuotas,
        now: u64,
    ) -> Result<Reservation, QuotaError> {
        let _lock = self.lock()?;
        let mut data = self.load()?;
        let day = now / DAY;
        if day < data.latest_day {
            return Err(QuotaError::ClockRollback);
        }
        let reserved = quotas.reservation_tokens();
        let (month_start, next_month) = super::time::month_bounds(day);
        let (mut daily, mut monthly) = ((0u64, 0u64), (0u64, 0u64));
        for row in data.rows.iter().filter(|row| row.key_id == key) {
            let add = |(requests, tokens): &mut (u64, u64)| {
                *requests = requests.saturating_add(row.requests);
                *tokens = tokens.saturating_add(row.charged_tokens);
            };
            if row.day == day {
                add(&mut daily);
            }
            if (month_start..=day).contains(&row.day) {
                add(&mut monthly);
            }
        }
        let exhausted = |quota: Option<DailyQuota>, (requests, tokens): (u64, u64)| {
            quota.is_some_and(|quota| {
                quota.requests.is_some_and(|limit| requests >= limit)
                    || quota.tokens.is_some_and(|limit| {
                        tokens
                            .checked_add(reserved)
                            .is_none_or(|total| total > limit)
                    })
            })
        };
        // The monthly reset is never earlier than the daily one, so it is reported first.
        if exhausted(quotas.monthly, monthly) {
            return Err(QuotaError::Exhausted {
                retry_after: next_month * DAY - now,
                period: QuotaPeriod::Month,
            });
        }
        if exhausted(quotas.daily, daily) {
            return Err(QuotaError::Exhausted {
                retry_after: DAY - now % DAY,
                period: QuotaPeriod::Day,
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
    pub fn complete(self, usage: Option<&Usage>) -> Result<(), QuotaError> {
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
    ) -> KeyQuotas {
        KeyQuotas {
            daily: Some(DailyQuota {
                requests,
                tokens,
                reservation_tokens,
            }),
            monthly: None,
        }
    }
    fn monthly(
        requests: Option<u64>,
        tokens: Option<u64>,
        reservation_tokens: Option<u64>,
    ) -> KeyQuotas {
        KeyQuotas {
            daily: None,
            monthly: quota(requests, tokens, reservation_tokens).daily,
        }
    }
    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            reasoning_tokens: None,
            cached_input_tokens: None,
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
            Err(QuotaError::Exhausted {
                retry_after: 1,
                period: QuotaPeriod::Day
            })
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
    fn at(text: &str) -> u64 {
        super::super::time::parse(text).unwrap()
    }

    #[test]
    fn monthly_allowance_sums_the_current_utc_month_across_days_and_models() {
        let fixture = Fixture::new();
        let ledger = fixture.ledger(Plane::All);
        let limit = monthly(Some(3), None, None);
        ledger
            .reserve("alpha", "chat", "chat", limit, at("2026-03-01T00:00:00Z"))
            .unwrap();
        ledger
            .reserve("alpha", "other", "chat", limit, at("2026-03-02T08:00:00Z"))
            .unwrap();
        ledger
            .reserve("beta", "chat", "chat", limit, at("2026-03-03T08:00:00Z"))
            .unwrap();
        ledger
            .reserve("alpha", "chat", "chat", limit, at("2026-03-03T08:00:00Z"))
            .unwrap();
        let now = at("2026-03-31T23:00:00Z");
        assert_eq!(
            ledger.reserve("alpha", "chat", "chat", limit, now).err(),
            Some(QuotaError::Exhausted {
                retry_after: at("2026-04-01T00:00:00Z") - now,
                period: QuotaPeriod::Month
            })
        );
    }

    #[test]
    fn monthly_allowance_resets_at_the_next_utc_month() {
        let fixture = Fixture::new();
        let ledger = fixture.ledger(Plane::All);
        let limit = monthly(None, Some(100), Some(60));
        let last_day = at("2026-12-31T23:59:59Z");
        let late = ledger
            .reserve("alpha", "chat", "chat", limit, last_day)
            .unwrap();
        assert_eq!(
            ledger
                .reserve("alpha", "chat", "chat", limit, last_day)
                .err(),
            Some(QuotaError::Exhausted {
                retry_after: 1,
                period: QuotaPeriod::Month
            })
        );
        ledger
            .reserve("alpha", "chat", "chat", limit, at("2027-01-01T00:00:00Z"))
            .unwrap();
        late.complete(Some(&usage(30, 20))).unwrap();
        let rows = ledger.read().unwrap();
        assert_eq!((rows[0].charged_tokens, rows[1].charged_tokens), (50, 60));
    }

    #[test]
    fn both_allowances_reserve_the_larger_amount_once() {
        let fixture = Fixture::new();
        let ledger = fixture.ledger(Plane::All);
        let both = KeyQuotas {
            daily: quota(None, Some(100), Some(10)).daily,
            monthly: quota(None, Some(1_000), Some(50)).daily,
        };
        let reservation = ledger.reserve("alpha", "chat", "chat", both, DAY).unwrap();
        let row = ledger.read().unwrap().remove(0);
        assert_eq!(
            (row.charged_tokens, row.retained_reservation_tokens),
            (50, 50)
        );
        reservation.complete(Some(&usage(5, 5))).unwrap();
        let row = ledger.read().unwrap().remove(0);
        assert_eq!(
            (row.charged_tokens, row.retained_reservation_tokens),
            (10, 0)
        );
        ledger.reserve("alpha", "chat", "chat", both, DAY).unwrap();
        assert_eq!(
            ledger.reserve("alpha", "chat", "chat", both, DAY).err(),
            Some(QuotaError::Exhausted {
                retry_after: DAY,
                period: QuotaPeriod::Day
            })
        );
    }

    #[test]
    fn exhausting_both_allowances_reports_the_later_monthly_reset() {
        let fixture = Fixture::new();
        let ledger = fixture.ledger(Plane::All);
        let now = at("2026-03-10T12:00:00Z");
        let both = KeyQuotas {
            daily: quota(Some(1), None, None).daily,
            monthly: quota(Some(1), None, None).daily,
        };
        ledger.reserve("alpha", "chat", "chat", both, now).unwrap();
        assert_eq!(
            ledger.reserve("alpha", "chat", "chat", both, now).err(),
            Some(QuotaError::Exhausted {
                retry_after: at("2026-04-01T00:00:00Z") - now,
                period: QuotaPeriod::Month
            })
        );
    }
}
