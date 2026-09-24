use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{
    Mutex, PoisonError,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use super::labels::{Endpoint, Outcome, Phase, StatusClass};

const BUCKETS: [f64; 6] = [0.01, 0.1, 1.0, 10.0, 60.0, f64::INFINITY];
const BUCKET_LABELS: [&str; 6] = ["0.01", "0.1", "1", "10", "60", "+Inf"];
const CELL_COUNT: usize = 4 * Outcome::COUNT * StatusClass::COUNT * Phase::COUNT;

struct CompletionCell {
    finished: AtomicU64,
    buckets: [AtomicU64; BUCKETS.len()],
    sum_nanos: AtomicU64,
}

impl CompletionCell {
    fn new() -> Self {
        Self {
            finished: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_nanos: AtomicU64::new(0),
        }
    }
}

pub(crate) struct Metrics {
    started: [AtomicU64; 4],
    inflight: [AtomicU64; 4],
    completions: [CompletionCell; CELL_COUNT],
    // Keys and aliases come only from configuration, which bounds cardinality.
    by_key_model: Mutex<BTreeMap<(String, String, usize), u64>>,
}

impl Metrics {
    pub(crate) fn new() -> Self {
        Self {
            started: std::array::from_fn(|_| AtomicU64::new(0)),
            inflight: std::array::from_fn(|_| AtomicU64::new(0)),
            completions: std::array::from_fn(|_| CompletionCell::new()),
            by_key_model: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn start(&self, endpoint: Endpoint) {
        self.started[endpoint.index()].fetch_add(1, Ordering::Relaxed);
        self.inflight[endpoint.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn finish(
        &self,
        endpoint: Endpoint,
        outcome: Outcome,
        status_class: StatusClass,
        phase: Phase,
        duration: Duration,
    ) {
        self.inflight[endpoint.index()].fetch_sub(1, Ordering::Relaxed);
        let cell = &self.completions[index(endpoint, outcome, status_class, phase)];
        cell.finished.fetch_add(1, Ordering::Relaxed);
        let seconds = duration.as_secs_f64();
        for (bucket, limit) in cell.buckets.iter().zip(BUCKETS) {
            if seconds <= limit {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        let nanos = duration.as_nanos().min(u128::from(u64::MAX)) as u64;
        cell.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub(crate) fn finish_keyed(&self, key: &str, model: &str, status_class: StatusClass) {
        *self
            .by_key_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry((key.to_owned(), model.to_owned(), status_class.index()))
            .or_insert(0) += 1;
    }

    pub(crate) fn render(&self, live: bool, ready: bool) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "# TYPE kanata_process_live gauge");
        let _ = writeln!(output, "kanata_process_live {}", u8::from(live));
        let _ = writeln!(output, "# TYPE kanata_process_ready gauge");
        let _ = writeln!(output, "kanata_process_ready {}", u8::from(ready));

        let _ = writeln!(output, "# TYPE kanata_requests_started_total counter");
        for endpoint in Endpoint::ALL {
            let started = self.started[endpoint.index()].load(Ordering::Relaxed);
            let _ = writeln!(
                output,
                "kanata_requests_started_total{{endpoint=\"{}\"}} {}",
                endpoint.as_str(),
                started
            );
        }

        let _ = writeln!(output, "# TYPE kanata_requests_inflight gauge");
        for endpoint in Endpoint::ALL {
            let inflight = self.inflight[endpoint.index()].load(Ordering::Relaxed);
            let _ = writeln!(
                output,
                "kanata_requests_inflight{{endpoint=\"{}\"}} {}",
                endpoint.as_str(),
                inflight
            );
        }

        let _ = writeln!(output, "# TYPE kanata_requests_finished_total counter");
        let _ = writeln!(output, "# TYPE kanata_request_duration_seconds histogram");
        for endpoint in Endpoint::ALL {
            for outcome in [
                Outcome::Success,
                Outcome::ClientError,
                Outcome::UpstreamError,
                Outcome::InternalError,
                Outcome::Timeout,
                Outcome::Cancelled,
                Outcome::Draining,
            ] {
                for status_class in [
                    StatusClass::Unknown,
                    StatusClass::Informational,
                    StatusClass::Success,
                    StatusClass::Redirection,
                    StatusClass::ClientError,
                    StatusClass::ServerError,
                ] {
                    for phase in [
                        Phase::None,
                        Phase::Queue,
                        Phase::Connect,
                        Phase::Headers,
                        Phase::FirstByte,
                        Phase::Idle,
                        Phase::Overall,
                    ] {
                        self.render_cell(&mut output, endpoint, outcome, status_class, phase);
                    }
                }
            }
        }

        let _ = writeln!(output, "# TYPE kanata_requests_by_key_model_total counter");
        let counts = self
            .by_key_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for ((key, model, status_class), count) in counts.iter() {
            let _ = writeln!(
                output,
                "kanata_requests_by_key_model_total{{key=\"{}\",model=\"{}\",status_class=\"{}\"}} {count}",
                escape(key),
                escape(model),
                StatusClass::ALL[*status_class].as_str()
            );
        }
        output
    }

    fn render_cell(
        &self,
        output: &mut String,
        endpoint: Endpoint,
        outcome: Outcome,
        status_class: StatusClass,
        phase: Phase,
    ) {
        let cell = &self.completions[index(endpoint, outcome, status_class, phase)];
        let finished = cell.finished.load(Ordering::Relaxed);
        if finished == 0 {
            return;
        }
        let labels = format!(
            "endpoint=\"{}\",outcome=\"{}\",status_class=\"{}\",timeout_phase=\"{}\"",
            endpoint.as_str(),
            outcome.as_str(),
            status_class.as_str(),
            phase.as_str()
        );
        let _ = writeln!(
            output,
            "kanata_requests_finished_total{{{labels}}} {finished}"
        );
        for (bucket, label) in cell.buckets.iter().zip(BUCKET_LABELS) {
            let value = bucket.load(Ordering::Relaxed);
            let _ = writeln!(
                output,
                "kanata_request_duration_seconds_bucket{{{labels},le=\"{label}\"}} {value}"
            );
        }
        let sum = cell.sum_nanos.load(Ordering::Relaxed) as f64 / 1_000_000_000.0;
        let _ = writeln!(
            output,
            "kanata_request_duration_seconds_sum{{{labels}}} {sum:.9}"
        );
        let _ = writeln!(
            output,
            "kanata_request_duration_seconds_count{{{labels}}} {finished}"
        );
    }
}

fn index(endpoint: Endpoint, outcome: Outcome, status_class: StatusClass, phase: Phase) -> usize {
    (((endpoint.index() * Outcome::COUNT + outcome.index()) * StatusClass::COUNT
        + status_class.index())
        * Phase::COUNT)
        + phase.index()
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
