use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use tokio::time::{Sleep, sleep_until};

use crate::{
    config::ValidatedTimeouts,
    core::{GatewayError, TimeoutPhase},
};

use super::types::timeout;

#[derive(Clone, Copy)]
pub(super) struct Timeouts {
    pub(super) connect: Duration,
    pub(super) headers: Duration,
    pub(super) first_byte: Duration,
    pub(super) idle: Duration,
}

impl Timeouts {
    pub(super) fn from_validated(timeouts: &ValidatedTimeouts) -> Self {
        Self {
            connect: Duration::from_millis(timeouts.connect_ms()),
            headers: Duration::from_millis(timeouts.headers_ms()),
            first_byte: Duration::from_millis(timeouts.first_byte_ms()),
            idle: Duration::from_millis(timeouts.idle_ms()),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct PhaseDeadline {
    at: tokio::time::Instant,
    phase: TimeoutPhase,
}

impl PhaseDeadline {
    pub(super) fn from_now(duration: Duration, phase: TimeoutPhase) -> Result<Self, GatewayError> {
        let Some(at) = tokio::time::Instant::now().checked_add(duration) else {
            return Err(timeout(phase));
        };
        Ok(Self { at, phase })
    }

    pub(super) fn expired(self) -> bool {
        tokio::time::Instant::now() >= self.at
    }

    #[cfg(test)]
    pub(super) fn at(self) -> tokio::time::Instant {
        self.at
    }
}

pub(super) struct PhaseTimer {
    deadline: PhaseDeadline,
    sleep: Pin<Box<Sleep>>,
}

impl PhaseTimer {
    pub(super) fn from_now(duration: Duration, phase: TimeoutPhase) -> Result<Self, GatewayError> {
        let deadline = PhaseDeadline::from_now(duration, phase)?;
        Ok(Self {
            sleep: Box::pin(sleep_until(deadline.at)),
            deadline,
        })
    }

    pub(super) fn poll_expired(&mut self, cx: &mut Context<'_>) -> bool {
        if self.deadline.expired() {
            return true;
        }
        if self.sleep.as_mut().poll(cx).is_ready() {
            return true;
        }
        self.deadline.expired()
    }

    pub(super) fn expired(&self) -> bool {
        self.deadline.expired()
    }
}

pub(super) fn run<F, Fut, T>(deadline: PhaseDeadline, factory: F) -> Deadline<F, Fut>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, GatewayError>>,
{
    Deadline {
        deadline,
        timer: Box::pin(sleep_until(deadline.at)),
        state: Box::new(DeadlineState {
            factory: Some(factory),
            future: None,
        }),
    }
}

pub(super) struct Deadline<F, Fut> {
    deadline: PhaseDeadline,
    timer: Pin<Box<Sleep>>,
    state: Box<DeadlineState<F, Fut>>,
}

struct DeadlineState<F, Fut> {
    factory: Option<F>,
    future: Option<Pin<Box<Fut>>>,
}

impl<F, Fut, T> Future for Deadline<F, Fut>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, GatewayError>>,
{
    type Output = Result<T, GatewayError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.deadline.expired() {
            return Poll::Ready(Err(timeout(this.deadline.phase)));
        }
        if this.timer.as_mut().poll(cx).is_ready() || this.deadline.expired() {
            return Poll::Ready(Err(timeout(this.deadline.phase)));
        }
        if this.state.future.is_none() {
            let Some(factory) = this.state.factory.take() else {
                return Poll::Ready(Err(timeout(this.deadline.phase)));
            };
            this.state.future = Some(Box::pin(factory()));
        }
        if this.deadline.expired() {
            return Poll::Ready(Err(timeout(this.deadline.phase)));
        }
        let Some(future) = this.state.future.as_mut() else {
            return Poll::Ready(Err(timeout(this.deadline.phase)));
        };
        match future.as_mut().poll(cx) {
            Poll::Ready(output) => {
                if this.deadline.expired() {
                    Poll::Ready(Err(timeout(this.deadline.phase)))
                } else {
                    Poll::Ready(output)
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
