use super::*;
use clap::{ArgGroup, Args, Subcommand};

use crate::config::Plane;

/// `kanata key` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum KeyCommand {
    /// Create a key and show it once.
    #[command(after_help = "Without scope flags, a terminal offers a route picker.")]
    New(NewArgs),
    /// List keys without secrets or digests.
    List(ListArgs),
    /// Show one key.
    Show(ShowArgs),
    /// Change a key's scopes, expiry, limits or daily and monthly quotas.
    #[command(after_help = "Without change flags, a terminal offers a route picker.")]
    Edit(EditArgs),
    /// Revoke a key.
    Rm(RmArgs),
    /// Replace a key's secret.
    Rotate(RotateArgs),
    /// Report daily or monthly usage per key, model and plane.
    #[command(after_help = "Reads --usage-dir, or the configured [keys] usage_dir.")]
    Usage(UsageArgs),
}

/// Location for a mutating command.
#[derive(Debug, Args)]
pub(crate) struct WriteLocation {
    /// Configuration file.
    #[arg(long, value_name = "path")]
    pub(super) config: PathBuf,
    /// Keys file instead of the configured one.
    #[arg(long, value_name = "path")]
    pub(super) keys: Option<PathBuf>,
}

/// Location for a read-only command: `--config`, `--keys` or both.
#[derive(Debug, Args)]
#[group(required = true, multiple = true)]
pub(crate) struct ReadLocation {
    /// Configuration file.
    #[arg(long, value_name = "path")]
    pub(super) config: Option<PathBuf>,
    /// Keys file instead of the configured one.
    #[arg(long, value_name = "path")]
    pub(super) keys: Option<PathBuf>,
}

/// `--max-in-flight` and `--rate-limit`.
#[derive(Debug, Args)]
pub(crate) struct LimitArgs {
    /// Concurrent requests allowed for the key.
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub(super) max_in_flight: Option<u64>,
    /// Requests per window in milliseconds, e.g. 60/60000.
    #[arg(long, value_name = "N/MS", value_parser = parse_rate_limit)]
    pub(super) rate_limit: Option<KeyRateLimit>,
}

/// Daily and monthly quota flags; they need `[keys] usage_dir`.
#[derive(Debug, Args)]
pub(crate) struct QuotaArgs {
    /// Requests per UTC day.
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub(super) daily_requests: Option<u64>,
    /// Charged tokens per UTC day.
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub(super) daily_tokens: Option<u64>,
    /// Tokens reserved per request until usage is reported.
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub(super) reservation_tokens: Option<u64>,
    /// Requests per UTC calendar month.
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub(super) monthly_requests: Option<u64>,
    /// Charged tokens per UTC calendar month.
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub(super) monthly_tokens: Option<u64>,
    /// Tokens reserved per request against the monthly allowance.
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub(super) monthly_reservation_tokens: Option<u64>,
}

/// One allowance's `requests`, `tokens` and `reservation_tokens` flags.
#[derive(Clone, Copy)]
pub(super) struct QuotaFlags(pub(super) [Option<u64>; 3]);

impl QuotaFlags {
    pub(super) fn given(self) -> bool {
        self.0.iter().any(Option::is_some)
    }
}

impl QuotaArgs {
    pub(super) fn daily(&self) -> QuotaFlags {
        QuotaFlags([
            self.daily_requests,
            self.daily_tokens,
            self.reservation_tokens,
        ])
    }

    pub(super) fn monthly(&self) -> QuotaFlags {
        QuotaFlags([
            self.monthly_requests,
            self.monthly_tokens,
            self.monthly_reservation_tokens,
        ])
    }
}

/// `--period`: rows per UTC day or per UTC month.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum Period {
    #[default]
    Day,
    Month,
}

/// Expiry in days; `None` for `unlimited`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Expiry(pub(super) Option<u64>);

#[derive(Debug, Args)]
pub(crate) struct NewArgs {
    #[command(flatten)]
    pub(super) location: WriteLocation,
    /// Key id; never reused.
    #[arg(long, value_name = "id", value_parser = parse_id)]
    pub(super) id: String,
    /// Grant a chat route.
    #[arg(long, value_name = "alias")]
    pub(super) chat: Vec<String>,
    /// Grant a transcription route.
    #[arg(long, value_name = "alias")]
    pub(super) transcription: Vec<String>,
    /// Grant an embeddings route.
    #[arg(long, value_name = "alias")]
    pub(super) embeddings: Vec<String>,
    /// Grant a speech route.
    #[arg(long, value_name = "alias")]
    pub(super) speech: Vec<String>,
    /// Days until expiry.
    #[arg(long, value_name = "1|3|7|13|30|60|unlimited", value_parser = parse_expiry)]
    pub(super) expires: Expiry,
    /// Owner key; at most one may be active.
    #[arg(long)]
    pub(super) owner: bool,
    #[command(flatten)]
    pub(super) limits: LimitArgs,
    #[command(flatten)]
    pub(super) quota: QuotaArgs,
    /// Write the key to this new private file instead of stdout.
    #[arg(long, value_name = "path")]
    pub(super) key_out: Option<PathBuf>,
}

impl NewArgs {
    pub(super) fn scopes(&self) -> [(Operation, &[String]); 4] {
        [
            (Operation::Chat, &self.chat),
            (Operation::Transcription, &self.transcription),
            (Operation::Embeddings, &self.embeddings),
            (Operation::Speech, &self.speech),
        ]
    }
}

#[derive(Debug, Args)]
pub(crate) struct ListArgs {
    #[command(flatten)]
    pub(super) location: ReadLocation,
    /// Usage directory instead of the configured one.
    #[arg(long, value_name = "path")]
    pub(super) usage_dir: Option<PathBuf>,
    /// Include revoked keys.
    #[arg(long)]
    pub(super) all: bool,
    /// Print JSON.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    /// Key id.
    #[arg(value_name = "id", value_parser = parse_id)]
    pub(super) id: String,
    #[command(flatten)]
    pub(super) location: ReadLocation,
    /// Usage directory instead of the configured one.
    #[arg(long, value_name = "path")]
    pub(super) usage_dir: Option<PathBuf>,
    /// Print JSON.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct EditArgs {
    /// Key id.
    #[arg(value_name = "id", value_parser = parse_id)]
    pub(super) id: String,
    #[command(flatten)]
    pub(super) location: WriteLocation,
    /// Grant a chat route.
    #[arg(long, value_name = "alias")]
    pub(super) add_chat: Vec<String>,
    /// Revoke a chat route.
    #[arg(long, value_name = "alias")]
    pub(super) remove_chat: Vec<String>,
    /// Grant a transcription route.
    #[arg(long, value_name = "alias")]
    pub(super) add_transcription: Vec<String>,
    /// Revoke a transcription route.
    #[arg(long, value_name = "alias")]
    pub(super) remove_transcription: Vec<String>,
    /// Grant an embeddings route.
    #[arg(long, value_name = "alias")]
    pub(super) add_embeddings: Vec<String>,
    /// Revoke an embeddings route.
    #[arg(long, value_name = "alias")]
    pub(super) remove_embeddings: Vec<String>,
    /// Grant a speech route.
    #[arg(long, value_name = "alias")]
    pub(super) add_speech: Vec<String>,
    /// Revoke a speech route.
    #[arg(long, value_name = "alias")]
    pub(super) remove_speech: Vec<String>,
    /// Days until expiry, from now.
    #[arg(long, value_name = "1|3|7|13|30|60|unlimited", value_parser = parse_expiry)]
    pub(super) expires: Option<Expiry>,
    #[command(flatten)]
    pub(super) limits: LimitArgs,
    #[command(flatten)]
    pub(super) quota: QuotaArgs,
    /// Remove the daily quota; the monthly quota is kept.
    #[arg(long, conflicts_with_all = ["daily_requests", "daily_tokens", "reservation_tokens"])]
    pub(super) clear_quota: bool,
    /// Remove the monthly quota.
    #[arg(long, conflicts_with_all = ["monthly_requests", "monthly_tokens", "monthly_reservation_tokens"])]
    pub(super) clear_monthly_quota: bool,
    /// Remove --max-in-flight and --rate-limit.
    #[arg(long, conflicts_with_all = ["max_in_flight", "rate_limit"])]
    pub(super) clear_limits: bool,
    /// Add a never-public route to a publicly usable key.
    #[arg(long)]
    pub(super) force: bool,
}

impl EditArgs {
    pub(super) fn added(&self) -> [(Operation, &[String]); 4] {
        [
            (Operation::Chat, &self.add_chat),
            (Operation::Transcription, &self.add_transcription),
            (Operation::Embeddings, &self.add_embeddings),
            (Operation::Speech, &self.add_speech),
        ]
    }

    pub(super) fn removed(&self) -> [(Operation, &[String]); 4] {
        [
            (Operation::Chat, &self.remove_chat),
            (Operation::Transcription, &self.remove_transcription),
            (Operation::Embeddings, &self.remove_embeddings),
            (Operation::Speech, &self.remove_speech),
        ]
    }

    pub(super) fn quota_given(&self) -> bool {
        self.daily_quota_given() || self.monthly_quota_given()
    }

    pub(super) fn daily_quota_given(&self) -> bool {
        self.clear_quota || self.quota.daily().given()
    }

    pub(super) fn monthly_quota_given(&self) -> bool {
        self.clear_monthly_quota || self.quota.monthly().given()
    }

    pub(super) fn limits_given(&self) -> bool {
        self.clear_limits || self.limits.max_in_flight.is_some() || self.limits.rate_limit.is_some()
    }
}

#[derive(Debug, Args)]
pub(crate) struct RmArgs {
    /// Key id.
    #[arg(value_name = "id", value_parser = parse_id)]
    pub(super) id: String,
    #[command(flatten)]
    pub(super) location: WriteLocation,
    /// Required to revoke the owner key.
    #[arg(long)]
    pub(super) force: bool,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("target").required(true).args(["id", "owner"])))]
pub(crate) struct RotateArgs {
    /// Key id.
    #[arg(value_name = "id", value_parser = parse_id)]
    pub(super) id: Option<String>,
    /// Rotate the active owner key.
    #[arg(long)]
    pub(super) owner: bool,
    #[command(flatten)]
    pub(super) location: WriteLocation,
    /// Days until expiry, from now.
    #[arg(long, value_name = "1|3|7|13|30|60|unlimited", value_parser = parse_expiry)]
    pub(super) expires: Expiry,
    /// Write the key to this new private file instead of stdout.
    #[arg(long, value_name = "path")]
    pub(super) key_out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct UsageArgs {
    /// Configuration file.
    #[arg(long, value_name = "path")]
    pub(super) config: Option<PathBuf>,
    /// Usage directory instead of the configured one.
    #[arg(long, value_name = "path")]
    pub(super) usage_dir: Option<PathBuf>,
    /// Only this plane's ledger; all three by default.
    #[arg(long, value_name = "all|private|public", value_parser = crate::cli::parse_plane)]
    pub(super) plane: Option<Plane>,
    /// Only this key.
    #[arg(long, value_name = "id", value_parser = parse_id)]
    pub(super) key_id: Option<String>,
    /// Only this model alias.
    #[arg(long, value_name = "alias")]
    pub(super) model: Option<String>,
    /// First UTC day.
    #[arg(long, value_name = "YYYY-MM-DD", value_parser = parse_day)]
    pub(super) from: Option<u64>,
    /// Last UTC day.
    #[arg(long, value_name = "YYYY-MM-DD", value_parser = parse_day)]
    pub(super) to: Option<u64>,
    /// One row per UTC day or per UTC month (`YYYY-MM`).
    #[arg(long, value_enum, default_value_t = Period::Day)]
    pub(super) period: Period,
    /// Input token rate per million for a cost estimate.
    #[arg(long, value_name = "N", value_parser = parse_rate)]
    pub(super) input_cost_per_million: Option<f64>,
    /// Output token rate per million for a cost estimate.
    #[arg(long, value_name = "N", value_parser = parse_rate)]
    pub(super) output_cost_per_million: Option<f64>,
    /// Label for the cost estimate.
    #[arg(long, value_name = "label")]
    pub(super) cost_unit: Option<String>,
    /// Print JSON.
    #[arg(long)]
    pub(super) json: bool,
}

/// `Some(days)`, or `None` for `unlimited`.
pub(crate) fn parse_expires(value: &str) -> Result<Option<u64>, String> {
    match value {
        "1" | "3" | "7" | "13" | "30" | "60" => value
            .parse()
            .map(Some)
            .map_err(|_| "--expires must be one of 1, 3, 7, 13, 30, 60 or unlimited".into()),
        "unlimited" => Ok(None),
        _ => Err("--expires must be one of 1, 3, 7, 13, 30, 60 or unlimited".into()),
    }
}

fn parse_expiry(value: &str) -> Result<Expiry, String> {
    parse_expires(value).map(Expiry)
}

pub(crate) fn expires_at(now: u64, days: Option<u64>) -> Option<u64> {
    days.map(|days| now + days * SECONDS_PER_DAY)
}

pub(super) fn long_lived_warning(id: &str, days: Option<u64>) {
    match days {
        None => eprintln!("warning: long-lived key {id:?} never expires"),
        Some(days) if days >= 60 => {
            eprintln!("warning: long-lived key {id:?} expires in {days} days")
        }
        Some(_) => {}
    }
}

fn parse_positive(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "must be a positive integer".into())
}

fn parse_rate_limit(value: &str) -> Result<KeyRateLimit, String> {
    let error = || "--rate-limit must be N/MS, e.g. 60/60000".to_owned();
    let (requests, per_ms) = value.split_once('/').ok_or_else(error)?;
    Ok(KeyRateLimit {
        requests: parse_positive(requests).map_err(|_| error())?,
        per_ms: parse_positive(per_ms).map_err(|_| error())?,
    })
}

pub(super) fn parse_id(value: &str) -> Result<String, String> {
    if valid_identifier(value) {
        Ok(value.to_owned())
    } else {
        Err("key id must use only ASCII letters, digits, '-', '_' or '.'".into())
    }
}

/// UTC day number of a `YYYY-MM-DD` date.
fn parse_day(value: &str) -> Result<u64, String> {
    if value.len() != 10 {
        return Err("requires YYYY-MM-DD".into());
    }
    time::parse(&format!("{value}T00:00:00Z"))
        .map(|seconds| seconds / SECONDS_PER_DAY)
        .ok_or_else(|| "invalid date".into())
}

/// A cost rate from 0 to 1e9.
fn parse_rate(value: &str) -> Result<f64, String> {
    value
        .parse::<f64>()
        .ok()
        .filter(|rate| rate.is_finite() && *rate >= 0.0 && *rate <= 1_000_000_000.0)
        .ok_or_else(|| "must be a number from 0 to 1000000000".into())
}
