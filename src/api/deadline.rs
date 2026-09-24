use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use crate::core::{ErrorKind, GatewayError, TimeoutPhase};

#[derive(Clone, Copy)]
pub(super) struct RequestDeadline {
    at: tokio::time::Instant,
}

impl RequestDeadline {
    pub(super) fn new(overall_ms: u64) -> Result<Self, GatewayError> {
        if overall_ms > crate::config::MAX_TIMEOUT_MS {
            return Err(timeout(TimeoutPhase::Overall));
        }
        let duration = Duration::from_millis(overall_ms);
        let Some(at) = tokio::time::Instant::now().checked_add(duration) else {
            return Err(timeout(TimeoutPhase::Overall));
        };
        Ok(Self { at })
    }

    pub(super) fn at(self) -> tokio::time::Instant {
        self.at
    }

    pub(super) fn expired(self) -> bool {
        tokio::time::Instant::now() >= self.at
    }

    pub(super) async fn run<F, Fut>(self, factory: F) -> Result<Fut::Output, GatewayError>
    where
        F: FnOnce() -> Fut,
        Fut: Future,
    {
        if self.expired() {
            return Err(timeout(TimeoutPhase::Overall));
        }
        let output = match tokio::time::timeout_at(
            self.at,
            LazyDeadline {
                at: self.at,
                state: Box::new(LazyState {
                    factory: Some(factory),
                    future: None,
                }),
            },
        )
        .await
        {
            Err(_) => return Err(timeout(TimeoutPhase::Overall)),
            Ok(Err(error)) => return Err(error),
            Ok(Ok(output)) => output,
        };
        if self.expired() {
            return Err(timeout(TimeoutPhase::Overall));
        }
        Ok(output)
    }
}

struct LazyDeadline<F, Fut> {
    at: tokio::time::Instant,
    state: Box<LazyState<F, Fut>>,
}

struct LazyState<F, Fut> {
    factory: Option<F>,
    future: Option<Pin<Box<Fut>>>,
}

impl<F, Fut> Future for LazyDeadline<F, Fut>
where
    F: FnOnce() -> Fut,
    Fut: Future,
{
    type Output = Result<Fut::Output, GatewayError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if tokio::time::Instant::now() >= this.at {
            return Poll::Ready(Err(timeout(TimeoutPhase::Overall)));
        }
        if this.state.future.is_none() {
            let Some(factory) = this.state.factory.take() else {
                return Poll::Ready(Err(timeout(TimeoutPhase::Overall)));
            };
            this.state.future = Some(Box::pin(factory()));
        }
        let Some(future) = this.state.future.as_mut() else {
            return Poll::Ready(Err(timeout(TimeoutPhase::Overall)));
        };
        match future.as_mut().poll(context) {
            Poll::Ready(output) => Poll::Ready(Ok(output)),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub(super) fn timeout(phase: TimeoutPhase) -> GatewayError {
    GatewayError {
        kind: ErrorKind::Timeout { phase },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct ProbeFuture {
        polls: Arc<AtomicUsize>,
        ready: bool,
    }

    impl Future for ProbeFuture {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            if self.ready {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn expired_run_does_not_invoke_or_poll_a_ready_factory() {
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let deadline = RequestDeadline::new(0).unwrap_or_else(|_| panic!("deadline"));
        let result = deadline
            .run({
                let factory_calls = factory_calls.clone();
                let polls = polls.clone();
                move || {
                    factory_calls.fetch_add(1, Ordering::SeqCst);
                    ProbeFuture { polls, ready: true }
                }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_gate_does_not_repoll_an_inner_future_at_boundary() {
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let deadline = RequestDeadline::new(10).unwrap_or_else(|_| panic!("deadline"));
        let mut run = Box::pin(deadline.run({
            let factory_calls = factory_calls.clone();
            let polls = polls.clone();
            move || {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                ProbeFuture {
                    polls,
                    ready: false,
                }
            }
        }));
        let waker = futures_util::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);
        assert!(matches!(run.as_mut().poll(&mut context), Poll::Pending));
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
        assert_eq!(polls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_millis(10)).await;
        assert!(matches!(
            run.as_mut().poll(&mut context),
            Poll::Ready(Err(_))
        ));
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn extreme_overall_deadline_is_rejected_without_overflow() {
        assert!(RequestDeadline::new(u64::MAX).is_err());
    }
}
