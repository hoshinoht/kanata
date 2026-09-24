use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use tokio::time::Instant;

use crate::config::CircuitBreakerPolicy;
use crate::core::{ErrorKind, GatewayError, TimeoutPhase};

/// Per-adapter circuit breaker: closed, open until a deadline, then half-open with one probe.
pub(crate) struct Breaker {
    adapter_id: String,
    policy: CircuitBreakerPolicy,
    state: Mutex<BreakerState>,
}

#[derive(Default)]
struct BreakerState {
    consecutive_failures: u32,
    open_until: Option<Instant>,
    /// Epoch of the half-open probe in flight; stale probes never touch the state.
    probe: Option<u64>,
    epoch: u64,
}

/// How one upstream attempt reflects on backend health.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    /// The backend answered, even if with an error.
    Reachable,
    /// The backend could not be reached.
    Unreachable,
    /// Says nothing about the backend (cancelled, rejected locally, gateway error).
    Neutral,
}

impl Outcome {
    pub(crate) fn of<T>(result: &Result<T, GatewayError>) -> Self {
        match result {
            Ok(_) => Self::Reachable,
            Err(error) => match error.kind {
                ErrorKind::UpstreamUnavailable
                | ErrorKind::Timeout {
                    phase: TimeoutPhase::Connect,
                } => Self::Unreachable,
                // Adapters reject invalid or unsupported requests before any upstream call.
                ErrorKind::Timeout { .. }
                | ErrorKind::Cancelled
                | ErrorKind::Internal
                | ErrorKind::InvalidRequest
                | ErrorKind::UnsupportedOperation => Self::Neutral,
                _ => Self::Reachable,
            },
        }
    }
}

/// Admission ticket through the breaker; releases a half-open probe if dropped unrecorded.
pub(crate) struct BreakerTicket {
    breaker: Arc<Breaker>,
    probe: Option<u64>,
    recorded: bool,
}

impl Breaker {
    pub(crate) fn new(adapter_id: String, policy: CircuitBreakerPolicy) -> Self {
        Self {
            adapter_id,
            policy,
            state: Mutex::new(BreakerState::default()),
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.lock().open_until.is_some()
    }

    /// Admits a request, or returns how long the breaker stays open.
    pub(crate) fn admit(self: &Arc<Self>) -> Result<Option<BreakerTicket>, Duration> {
        if !self.policy.enabled {
            return Ok(None);
        }
        let mut state = self.lock();
        let probe = match state.open_until {
            None => None,
            Some(until) => {
                let now = Instant::now();
                if now < until {
                    return Err(until - now);
                }
                if state.probe.is_some() {
                    return Err(Duration::from_millis(self.policy.cooldown_ms));
                }
                state.epoch = state.epoch.wrapping_add(1);
                state.probe = Some(state.epoch);
                state.probe
            }
        };
        Ok(Some(BreakerTicket {
            breaker: self.clone(),
            probe,
            recorded: false,
        }))
    }

    fn record(&self, probe: Option<u64>, outcome: Outcome) {
        let mut state = self.lock();
        let current_probe = probe.is_some() && state.probe == probe;
        if current_probe {
            state.probe = None;
        }
        match outcome {
            Outcome::Reachable => {
                let was_open = state.open_until.take().is_some();
                state.consecutive_failures = 0;
                state.probe = None;
                if was_open {
                    tracing::info!(
                        target: "kanata::lifecycle",
                        adapter = %self.adapter_id,
                        "circuit breaker closed",
                    );
                }
            }
            Outcome::Unreachable => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                // A failed current probe reopens; late failures never extend an open breaker.
                let reopen = if current_probe {
                    state.open_until.is_some()
                } else {
                    state.open_until.is_none() && state.consecutive_failures >= self.policy.failures
                };
                if reopen {
                    state.open_until =
                        Some(Instant::now() + Duration::from_millis(self.policy.cooldown_ms));
                    tracing::warn!(
                        target: "kanata::lifecycle",
                        adapter = %self.adapter_id,
                        failures = state.consecutive_failures,
                        cooldown_ms = self.policy.cooldown_ms,
                        "circuit breaker opened",
                    );
                }
            }
            Outcome::Neutral => {}
        }
    }

    fn lock(&self) -> MutexGuard<'_, BreakerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl BreakerTicket {
    pub(crate) fn record(mut self, outcome: Outcome) {
        self.recorded = true;
        self.breaker.record(self.probe, outcome);
    }
}

impl Drop for BreakerTicket {
    fn drop(&mut self) {
        if !self.recorded && self.probe.is_some() {
            self.breaker.record(self.probe, Outcome::Neutral);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker(failures: u32) -> Arc<Breaker> {
        Arc::new(Breaker::new(
            "test".into(),
            CircuitBreakerPolicy {
                enabled: true,
                failures,
                cooldown_ms: 1_000,
            },
        ))
    }

    fn fail(breaker: &Arc<Breaker>) {
        breaker
            .admit()
            .expect("admitted")
            .expect("ticket")
            .record(Outcome::Unreachable);
    }

    #[tokio::test(start_paused = true)]
    async fn opens_after_consecutive_failures_and_probes_after_cooldown() {
        let breaker = breaker(2);
        fail(&breaker);
        breaker
            .admit()
            .expect("admitted")
            .expect("ticket")
            .record(Outcome::Reachable);
        fail(&breaker);
        assert!(!breaker.is_open(), "success resets the failure count");
        fail(&breaker);
        assert!(breaker.is_open());
        assert_eq!(breaker.admit().err(), Some(Duration::from_millis(1_000)));

        tokio::time::advance(Duration::from_millis(1_000)).await;
        let probe = breaker.admit().expect("probe").expect("ticket");
        assert!(breaker.admit().is_err(), "only one probe at a time");
        probe.record(Outcome::Unreachable);
        assert!(breaker.admit().is_err(), "a failed probe reopens");

        tokio::time::advance(Duration::from_millis(1_000)).await;
        drop(breaker.admit().expect("probe").expect("ticket"));
        let probe = breaker
            .admit()
            .expect("dropped probe is released")
            .expect("ticket");
        probe.record(Outcome::Reachable);
        assert!(!breaker.is_open());
    }

    #[tokio::test(start_paused = true)]
    async fn locally_rejected_probe_keeps_the_breaker_open() {
        let breaker = breaker(1);
        fail(&breaker);
        tokio::time::advance(Duration::from_millis(1_000)).await;
        let probe = breaker.admit().expect("probe").expect("ticket");
        let rejected: Result<(), GatewayError> = Err(GatewayError {
            kind: ErrorKind::InvalidRequest,
        });
        probe.record(Outcome::of(&rejected));
        assert!(breaker.is_open());
        assert!(breaker.admit().is_ok(), "the next request probes instead");
    }

    #[tokio::test(start_paused = true)]
    async fn stale_probe_cannot_reopen_a_closed_breaker() {
        let breaker = breaker(2);
        let late = breaker.admit().expect("admitted").expect("ticket");
        fail(&breaker);
        fail(&breaker);
        tokio::time::advance(Duration::from_millis(1_000)).await;
        let probe = breaker.admit().expect("probe").expect("ticket");
        late.record(Outcome::Reachable);
        assert!(!breaker.is_open());
        probe.record(Outcome::Unreachable);
        assert!(
            !breaker.is_open(),
            "one failure after closing stays under the threshold"
        );
    }

    #[test]
    fn disabled_breaker_never_opens() {
        let breaker = Arc::new(Breaker::new(
            "test".into(),
            CircuitBreakerPolicy {
                enabled: false,
                failures: 1,
                cooldown_ms: 1_000,
            },
        ));
        assert!(breaker.admit().expect("admitted").is_none());
    }
}
