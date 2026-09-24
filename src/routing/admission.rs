use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError},
    time::Instant,
};

use crate::config::{KeyRateLimit, ValidatedConfig};
use crate::core::{ErrorKind, GatewayError};

use super::{
    Registry, RouteEntry,
    breaker::{Breaker, BreakerTicket, Outcome},
};

pub(crate) struct Admission {
    routes: BTreeMap<String, RouteAdmission>,
    adapters: BTreeMap<String, AdapterAdmission>,
    keys: BTreeMap<String, KeyAdmission>,
    limits: AdmissionLimits,
    queue_timeout: Duration,
    closed: AtomicBool,
}

/// Per-route admission limits, as configured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdmissionLimits {
    pub(crate) max_queue: u64,
    pub(crate) max_in_flight: u64,
    pub(crate) queue_ms: u64,
}

struct RouteAdmission {
    tickets: Arc<Semaphore>,
    active: Arc<Semaphore>,
}

struct AdapterAdmission {
    max_in_flight: Option<u64>,
    active: Option<Arc<Semaphore>>,
    breaker: Arc<Breaker>,
}

struct KeyAdmission {
    active: Option<Arc<Semaphore>>,
    bucket: Option<Mutex<TokenBucket>>,
}

struct TokenBucket {
    capacity: f64,
    per_ms: f64,
    tokens: f64,
    updated: Instant,
}

pub(crate) struct AdmissionPermit {
    breaker: Option<BreakerTicket>,
    // Release capacity innermost first: adapter, route, queue ticket, then key.
    _adapter: Option<OwnedSemaphorePermit>,
    _active: OwnedSemaphorePermit,
    _ticket: OwnedSemaphorePermit,
    _key: Option<OwnedSemaphorePermit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdmissionBuildError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionError {
    QueueFull,
    QueueTimeout,
    KeyBusy,
    KeyRateLimited { wait: Duration },
    CircuitOpen { wait: Duration },
    UnknownRoute,
    Closed,
}

impl Admission {
    pub(crate) fn from_config(
        registry: &Registry,
        config: &ValidatedConfig,
    ) -> Result<Self, AdmissionBuildError> {
        let limits = AdmissionLimits {
            max_queue: config.limits().max_queue(),
            max_in_flight: config.limits().max_in_flight(),
            queue_ms: config.timeouts().queue_ms(),
        };
        let max_queue = permits(limits.max_queue)?;
        let max_in_flight = permits(limits.max_in_flight)?;
        let ticket_capacity = max_queue
            .checked_add(max_in_flight)
            .filter(|capacity| *capacity <= Semaphore::MAX_PERMITS)
            .ok_or(AdmissionBuildError)?;

        let routes = registry
            .routes()
            .map(|route| {
                (
                    route.identity.route_id.clone(),
                    RouteAdmission {
                        tickets: Arc::new(Semaphore::new(ticket_capacity)),
                        active: Arc::new(Semaphore::new(max_in_flight)),
                    },
                )
            })
            .collect();
        let adapters = config
            .adapters()
            .iter()
            .map(|adapter| {
                let active = adapter
                    .max_in_flight()
                    .map(|limit| permits(limit).map(|limit| Arc::new(Semaphore::new(limit))))
                    .transpose()?;
                Ok((
                    adapter.id().to_owned(),
                    AdapterAdmission {
                        max_in_flight: adapter.max_in_flight(),
                        active,
                        breaker: Arc::new(Breaker::new(
                            adapter.id().to_owned(),
                            adapter.circuit_breaker(),
                        )),
                    },
                ))
            })
            .collect::<Result<_, AdmissionBuildError>>()?;
        let keys = config
            .application_keys()
            .iter()
            .filter(|key| key.max_in_flight().is_some() || key.rate_limit().is_some())
            .map(|key| {
                let active = key
                    .max_in_flight()
                    .map(|limit| permits(limit).map(|limit| Arc::new(Semaphore::new(limit))))
                    .transpose()?;
                Ok((
                    key.id().to_owned(),
                    KeyAdmission {
                        active,
                        bucket: key
                            .rate_limit()
                            .map(|limit| Mutex::new(TokenBucket::new(limit))),
                    },
                ))
            })
            .collect::<Result<_, AdmissionBuildError>>()?;

        Ok(Self {
            routes,
            adapters,
            keys,
            limits,
            queue_timeout: Duration::from_millis(limits.queue_ms),
            closed: AtomicBool::new(false),
        })
    }

    pub(crate) fn limits(&self) -> AdmissionLimits {
        self.limits
    }

    pub(crate) fn adapter_max_in_flight(&self, adapter_id: &str) -> Option<u64> {
        self.adapters
            .get(adapter_id)
            .and_then(|adapter| adapter.max_in_flight)
    }

    pub(crate) fn open_breakers(&self) -> usize {
        self.adapters
            .values()
            .filter(|adapter| adapter.breaker.is_open())
            .count()
    }

    /// Jittered whole-second `Retry-After` for an admission rejection.
    pub(crate) fn retry_after_secs(&self, error: AdmissionError) -> u64 {
        let base = match error {
            AdmissionError::KeyRateLimited { wait } | AdmissionError::CircuitOpen { wait } => {
                u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)
            }
            _ => self.limits.queue_ms,
        }
        .div_ceil(1_000)
        .max(1);
        let spread = base.div_ceil(2);
        let jitter = getrandom::u64().map_or(0, |random| random % (spread + 1));
        base.saturating_add(jitter)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        for route in self.routes.values() {
            route.tickets.close();
            route.active.close();
        }
        for semaphore in self
            .adapters
            .values()
            .filter_map(|adapter| adapter.active.as_ref())
            .chain(self.keys.values().filter_map(|key| key.active.as_ref()))
        {
            semaphore.close();
        }
    }

    pub(crate) async fn acquire(
        &self,
        route: &RouteEntry,
        key_id: &str,
    ) -> Result<AdmissionPermit, AdmissionError> {
        if self.is_closed() {
            return Err(AdmissionError::Closed);
        }
        let (Some(route_admission), Some(adapter)) = (
            self.routes.get(&route.identity.route_id),
            self.adapters.get(&route.adapter_id),
        ) else {
            return Err(AdmissionError::UnknownRoute);
        };
        let breaker = adapter
            .breaker
            .admit()
            .map_err(|wait| AdmissionError::CircuitOpen { wait })?;
        let key_admission = self.keys.get(key_id);
        let key = match key_admission {
            Some(key) => key.acquire()?,
            None => None,
        };
        let ticket = try_acquire(&route_admission.tickets, AdmissionError::QueueFull)?;
        // Charge the rate limit only once the request is queued, not when it is turned away.
        if let Some(key) = key_admission {
            key.take_token()?;
        }
        if self.is_closed() {
            return Err(AdmissionError::Closed);
        }
        let wait = async {
            let active = route_admission.active.clone().acquire_owned().await?;
            let adapter = match &adapter.active {
                Some(semaphore) => Some(semaphore.clone().acquire_owned().await?),
                None => None,
            };
            Ok::<_, tokio::sync::AcquireError>((active, adapter))
        };
        let (active, adapter) = match tokio::time::timeout(self.queue_timeout, wait).await {
            Ok(Ok(permits)) => permits,
            Ok(Err(_)) => return Err(AdmissionError::Closed),
            Err(_) => return Err(AdmissionError::QueueTimeout),
        };
        if self.is_closed() {
            return Err(AdmissionError::Closed);
        }
        Ok(AdmissionPermit {
            breaker,
            _adapter: adapter,
            _active: active,
            _ticket: ticket,
            _key: key,
        })
    }
}

impl KeyAdmission {
    fn acquire(&self) -> Result<Option<OwnedSemaphorePermit>, AdmissionError> {
        self.active
            .as_ref()
            .map(|semaphore| try_acquire(semaphore, AdmissionError::KeyBusy))
            .transpose()
    }

    fn take_token(&self) -> Result<(), AdmissionError> {
        let Some(bucket) = &self.bucket else {
            return Ok(());
        };
        bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .map_err(|wait| AdmissionError::KeyRateLimited { wait })
    }
}

impl TokenBucket {
    fn new(limit: KeyRateLimit) -> Self {
        let capacity = limit.requests as f64;
        Self {
            capacity,
            per_ms: capacity / limit.per_ms as f64,
            tokens: capacity,
            updated: Instant::now(),
        }
    }

    /// Takes one token, or returns the wait until one is available.
    fn take(&mut self) -> Result<(), Duration> {
        let now = Instant::now();
        let elapsed_ms = now.duration_since(self.updated).as_secs_f64() * 1_000.0;
        self.tokens = (self.tokens + elapsed_ms * self.per_ms).min(self.capacity);
        self.updated = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        let wait_ms = ((1.0 - self.tokens) / self.per_ms).ceil();
        Err(Duration::from_millis(wait_ms as u64))
    }
}

impl AdmissionPermit {
    /// Reports the upstream attempt's result to the adapter's circuit breaker.
    pub(crate) fn record_outcome<T>(&mut self, result: &Result<T, GatewayError>) {
        if let Some(ticket) = self.breaker.take() {
            ticket.record(Outcome::of(result));
        }
    }
}

impl AdmissionError {
    pub(crate) fn gateway_error(self) -> GatewayError {
        let kind = match self {
            Self::QueueFull | Self::KeyBusy | Self::KeyRateLimited { .. } => ErrorKind::RateLimited,
            Self::QueueTimeout => ErrorKind::Timeout {
                phase: crate::core::TimeoutPhase::Queue,
            },
            Self::CircuitOpen { .. } => ErrorKind::UpstreamUnavailable,
            Self::UnknownRoute | Self::Closed => ErrorKind::Internal,
        };
        GatewayError { kind }
    }
}

fn permits(value: u64) -> Result<usize, AdmissionBuildError> {
    usize::try_from(value)
        .ok()
        .filter(|value| *value <= Semaphore::MAX_PERMITS)
        .ok_or(AdmissionBuildError)
}

fn try_acquire(
    semaphore: &Arc<Semaphore>,
    exhausted: AdmissionError,
) -> Result<OwnedSemaphorePermit, AdmissionError> {
    semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|error| match error {
            TryAcquireError::NoPermits => exhausted,
            TryAcquireError::Closed => AdmissionError::Closed,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn token_bucket_refills_over_its_window() {
        let mut bucket = TokenBucket::new(KeyRateLimit {
            requests: 2,
            per_ms: 1_000,
        });
        assert!(bucket.take().is_ok());
        assert!(bucket.take().is_ok());
        assert_eq!(bucket.take(), Err(Duration::from_millis(500)));
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(bucket.take().is_ok());
        assert!(bucket.take().is_err());
    }
}
