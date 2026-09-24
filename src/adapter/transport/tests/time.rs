use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use futures_util::task::noop_waker_ref;

use crate::core::{ErrorKind, TimeoutPhase};

use super::super::time::{PhaseDeadline, PhaseTimer, run};

struct Probe {
    polls: Arc<AtomicUsize>,
}

impl Future for Probe {
    type Output = Result<(), crate::core::GatewayError>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Ok(()))
    }
}

#[tokio::test(start_paused = true)]
async fn phase_deadline_precheck_does_not_poll_at_exact_boundary() {
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let deadline = PhaseDeadline::from_now(Duration::from_millis(5), TimeoutPhase::Headers)
        .unwrap_or_else(|_| panic!("deadline"));
    let run = run(deadline, {
        let factory_calls = factory_calls.clone();
        let polls = polls.clone();
        move || {
            factory_calls.fetch_add(1, Ordering::SeqCst);
            Probe { polls }
        }
    });
    tokio::time::advance(Duration::from_millis(5)).await;
    let error = run.await.expect_err("boundary timeout");
    assert_eq!(
        error.kind,
        ErrorKind::Timeout {
            phase: TimeoutPhase::Headers
        }
    );
    assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn body_timer_uses_checked_absolute_boundary() {
    let mut timer = PhaseTimer::from_now(Duration::from_millis(5), TimeoutPhase::Idle)
        .unwrap_or_else(|_| panic!("timer"));
    let at = tokio::time::Instant::now() + Duration::from_millis(5);
    assert!(at >= tokio::time::Instant::now());
    tokio::time::advance(Duration::from_millis(5)).await;
    let waker = noop_waker_ref();
    let mut context = Context::from_waker(waker);
    assert!(timer.poll_expired(&mut context));
}

#[tokio::test(start_paused = true)]
async fn reused_phase_deadline_does_not_reset_between_steps() {
    let deadline = PhaseDeadline::from_now(Duration::from_millis(10), TimeoutPhase::Headers)
        .unwrap_or_else(|_| panic!("deadline"));
    let first = run(deadline, || async {
        tokio::time::sleep(Duration::from_millis(4)).await;
        Ok(())
    });
    assert!(first.await.is_ok());

    let second = run(deadline, || async {
        std::future::pending::<Result<(), crate::core::GatewayError>>().await
    });
    tokio::time::advance(Duration::from_millis(6)).await;
    let error = second.await.expect_err("shared deadline");
    assert_eq!(
        error.kind,
        ErrorKind::Timeout {
            phase: TimeoutPhase::Headers
        }
    );
}

#[test]
fn phase_deadline_rejects_unrepresentable_instant() {
    assert!(PhaseDeadline::from_now(Duration::MAX, TimeoutPhase::Connect).is_err());
}
