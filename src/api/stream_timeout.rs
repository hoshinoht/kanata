use std::time::Duration;

use futures_util::StreamExt;

use crate::adapter::EventStream;
use crate::core::{GatewayError, NormalizedEvent, TimeoutPhase};

use super::deadline::{RequestDeadline, timeout};

pub(super) struct StreamTimeout {
    request: RequestDeadline,
    first_byte: Duration,
    idle: Duration,
}

impl StreamTimeout {
    pub(super) fn new(request: RequestDeadline, first_byte_ms: u64, idle_ms: u64) -> Self {
        Self {
            request,
            first_byte: Duration::from_millis(first_byte_ms),
            idle: Duration::from_millis(idle_ms),
        }
    }

    pub(super) async fn first(
        &self,
        events: &mut EventStream,
    ) -> Result<Option<Result<NormalizedEvent, GatewayError>>, GatewayError> {
        self.wait(events, self.first_byte, TimeoutPhase::FirstByte)
            .await
    }

    pub(super) async fn next(
        &self,
        events: &mut EventStream,
    ) -> Result<Option<Result<NormalizedEvent, GatewayError>>, GatewayError> {
        self.wait(events, self.idle, TimeoutPhase::Idle).await
    }

    pub(super) fn expired(&self) -> bool {
        self.request.expired()
    }

    async fn wait(
        &self,
        events: &mut EventStream,
        duration: Duration,
        phase: TimeoutPhase,
    ) -> Result<Option<Result<NormalizedEvent, GatewayError>>, GatewayError> {
        let now = tokio::time::Instant::now();
        let (at, timeout_phase) = match now.checked_add(duration) {
            Some(phase_at) if self.request.at() > phase_at => (phase_at, phase),
            _ => (self.request.at(), TimeoutPhase::Overall),
        };
        if let Some(error) = deadline_error(self.request, at, timeout_phase) {
            return Err(error);
        }

        let next = match tokio::time::timeout_at(at, events.next()).await {
            Ok(next) => next,
            Err(_) => {
                if let Some(error) = deadline_error(self.request, at, timeout_phase) {
                    return Err(error);
                }
                return Err(timeout(timeout_phase));
            }
        };
        if let Some(error) = deadline_error(self.request, at, timeout_phase) {
            return Err(error);
        }

        Ok(next)
    }
}

fn deadline_error(
    request: RequestDeadline,
    at: tokio::time::Instant,
    phase: TimeoutPhase,
) -> Option<GatewayError> {
    if request.expired() {
        Some(timeout(TimeoutPhase::Overall))
    } else if tokio::time::Instant::now() >= at {
        Some(timeout(phase))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ErrorKind, ModelAlias};
    use futures_util::stream;
    use std::task::Poll;

    fn deadline(milliseconds: u64) -> RequestDeadline {
        RequestDeadline::new(milliseconds).unwrap_or_else(|_| panic!("deadline"))
    }

    #[tokio::test(start_paused = true)]
    async fn phase_deadlines_preserve_phase_and_overall_precedence() {
        let first = StreamTimeout::new(deadline(20), 5, 20);
        let mut events: EventStream = Box::pin(stream::pending());
        tokio::time::advance(Duration::from_millis(5)).await;
        let error = first.first(&mut events).await.expect_err("first timeout");
        assert_eq!(
            error.kind,
            ErrorKind::Timeout {
                phase: TimeoutPhase::FirstByte
            }
        );

        let idle = StreamTimeout::new(deadline(20), 20, 5);
        let mut events: EventStream = Box::pin(stream::pending());
        tokio::time::advance(Duration::from_millis(5)).await;
        let error = idle.next(&mut events).await.expect_err("idle timeout");
        assert_eq!(
            error.kind,
            ErrorKind::Timeout {
                phase: TimeoutPhase::Idle
            }
        );

        let overall = StreamTimeout::new(deadline(10), 20, 20);
        let mut events: EventStream = Box::pin(stream::iter([Ok::<_, GatewayError>(
            NormalizedEvent::ChatStarted {
                model: ModelAlias("private-chat".into()),
            },
        )]));
        tokio::time::advance(Duration::from_millis(10)).await;
        let error = overall
            .first(&mut events)
            .await
            .expect_err("overall timeout");
        assert_eq!(
            error.kind,
            ErrorKind::Timeout {
                phase: TimeoutPhase::Overall
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ready_event_at_first_byte_deadline_is_timeout() {
        let timeout = StreamTimeout::new(deadline(20), 5, 20);
        let ready_at = tokio::time::Instant::now() + Duration::from_millis(5);
        let mut events: EventStream = Box::pin(stream::poll_fn(move |_| {
            if tokio::time::Instant::now() < ready_at {
                Poll::Pending
            } else {
                Poll::Ready(Some(Ok(NormalizedEvent::ChatStarted {
                    model: ModelAlias("private-chat".into()),
                })))
            }
        }));
        let error = timeout
            .first(&mut events)
            .await
            .expect_err("first-byte boundary timeout");
        assert_eq!(
            error.kind,
            ErrorKind::Timeout {
                phase: TimeoutPhase::FirstByte
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ready_event_at_idle_deadline_is_timeout() {
        let timeout = StreamTimeout::new(deadline(20), 20, 5);
        let ready_at = tokio::time::Instant::now() + Duration::from_millis(5);
        let mut events: EventStream = Box::pin(stream::poll_fn(move |_| {
            if tokio::time::Instant::now() < ready_at {
                Poll::Pending
            } else {
                Poll::Ready(Some(Ok(NormalizedEvent::ChatTextDelta {
                    text: "late".into(),
                })))
            }
        }));
        let error = timeout
            .next(&mut events)
            .await
            .expect_err("idle boundary timeout");
        assert_eq!(
            error.kind,
            ErrorKind::Timeout {
                phase: TimeoutPhase::Idle
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn extreme_phase_duration_does_not_panic() {
        let timeout = StreamTimeout::new(deadline(10), u64::MAX, u64::MAX);
        let mut events: EventStream = Box::pin(stream::pending());
        let task = tokio::spawn(async move { timeout.first(&mut events).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        let error = task
            .await
            .expect("phase task")
            .expect_err("overall timeout");
        assert_eq!(
            error.kind,
            ErrorKind::Timeout {
                phase: TimeoutPhase::Overall
            }
        );
    }
}
