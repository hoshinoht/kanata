use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::core::{ErrorKind, GatewayError};

use super::{Registry, RouteEntry};

pub(crate) struct Admission {
    routes: BTreeMap<String, RouteAdmission>,
    queue_timeout: Duration,
    closed: AtomicBool,
}

struct RouteAdmission {
    tickets: Arc<Semaphore>,
    active: Arc<Semaphore>,
}

pub(crate) struct AdmissionPermit {
    // Release active capacity before admission capacity.
    _active: OwnedSemaphorePermit,
    _ticket: OwnedSemaphorePermit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdmissionBuildError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionError {
    QueueFull,
    QueueTimeout,
    UnknownRoute,
    Closed,
}

impl Admission {
    pub(crate) fn from_registry(
        registry: &Registry,
        max_queue: u64,
        max_in_flight: u64,
        queue_ms: u64,
    ) -> Result<Self, AdmissionBuildError> {
        let max_queue = usize::try_from(max_queue).map_err(|_| AdmissionBuildError)?;
        let max_in_flight = usize::try_from(max_in_flight).map_err(|_| AdmissionBuildError)?;
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

        Ok(Self {
            routes,
            queue_timeout: Duration::from_millis(queue_ms),
            closed: AtomicBool::new(false),
        })
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
    }

    pub(crate) async fn acquire(
        &self,
        route: &RouteEntry,
    ) -> Result<AdmissionPermit, AdmissionError> {
        if self.is_closed() {
            return Err(AdmissionError::Closed);
        }
        let Some(route_admission) = self.routes.get(&route.identity.route_id) else {
            return Err(AdmissionError::UnknownRoute);
        };
        if self.is_closed() {
            return Err(AdmissionError::Closed);
        }
        let ticket = route_admission
            .tickets
            .clone()
            .try_acquire_owned()
            .map_err(|error| match error {
                TryAcquireError::NoPermits => AdmissionError::QueueFull,
                TryAcquireError::Closed => AdmissionError::Closed,
            })?;
        if self.is_closed() {
            drop(ticket);
            return Err(AdmissionError::Closed);
        }
        let active = match tokio::time::timeout(
            self.queue_timeout,
            route_admission.active.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                drop(ticket);
                return Err(AdmissionError::Closed);
            }
            Err(_) => {
                drop(ticket);
                return Err(AdmissionError::QueueTimeout);
            }
        };
        if self.is_closed() {
            drop(active);
            drop(ticket);
            return Err(AdmissionError::Closed);
        }
        Ok(AdmissionPermit {
            _ticket: ticket,
            _active: active,
        })
    }
}

impl AdmissionError {
    pub(crate) fn gateway_error(self) -> GatewayError {
        let kind = match self {
            Self::QueueFull => ErrorKind::RateLimited,
            Self::QueueTimeout => ErrorKind::Timeout {
                phase: crate::core::TimeoutPhase::Queue,
            },
            Self::UnknownRoute | Self::Closed => ErrorKind::Internal,
        };
        GatewayError { kind }
    }
}
