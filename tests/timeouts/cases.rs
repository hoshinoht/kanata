use std::{sync::Arc, sync::atomic::Ordering, task::Poll, time::Duration};

use axum::{
    body::{Bytes, to_bytes},
    http::StatusCode,
};
use futures_util::StreamExt;
use kanata::core::{FinishReason, ModelAlias, NormalizedEvent, Usage};
use tokio::sync::mpsc;

use crate::support::{self, Mode};

fn started() -> support::Event {
    Ok(NormalizedEvent::ChatStarted {
        model: ModelAlias("private-chat".into()),
    })
}

fn text(value: &str) -> support::Event {
    Ok(NormalizedEvent::ChatTextDelta { text: value.into() })
}

fn completed() -> support::Event {
    Ok(NormalizedEvent::ChatCompleted {
        finish_reason: FinishReason::Stop,
        usage: Some(Usage {
            input_tokens: 1,
            output_tokens: 1,
            total_tokens: 2,
        }),
    })
}

#[tokio::test(start_paused = true)]
async fn overall_deadline_cancels_a_pending_nonstream_execute() {
    let config = support::config(10, 10, 10, 10);
    let (server, probe) = support::server(&config, Mode::Pending);
    let request = tokio::spawn(async move {
        server
            .client_oneshot(support::request(false))
            .await
            .expect("response")
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    let response = request.await.expect("request task");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn first_event_deadline_returns_a_redacted_504_before_headers() {
    let config = support::config(20, 5, 10, 20);
    let (_sender, receiver) = mpsc::unbounded_channel();
    let (server, _) = support::server(&config, Mode::Events(receiver));
    let request = tokio::spawn(async move {
        server
            .client_oneshot(support::request(true))
            .await
            .expect("response")
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(5)).await;
    let response = request.await.expect("request task");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf8");
    assert!(body.contains("upstream_timeout"));
}

#[tokio::test(start_paused = true)]
async fn idle_deadline_measures_upstream_polling_and_emits_no_done() {
    let config = support::config(20, 5, 5, 20);
    let (sender, receiver) = mpsc::unbounded_channel();
    sender
        .send(Ok(NormalizedEvent::ChatStarted {
            model: ModelAlias("private-chat".into()),
        }))
        .expect("start");
    let (server, _) = support::server(&config, Mode::Events(receiver));
    let response = server
        .client_oneshot(support::request(true))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    body.next()
        .await
        .expect("start frame")
        .expect("start bytes");
    tokio::time::advance(std::time::Duration::from_millis(5)).await;
    let error = body
        .next()
        .await
        .expect("error frame")
        .expect("error bytes");
    assert!(
        std::str::from_utf8(&error)
            .expect("utf8")
            .contains("upstream_timeout")
    );
    assert!(
        body.next().await.is_none(),
        "timeout terminates without DONE"
    );
}

#[tokio::test(start_paused = true)]
async fn trickling_body_cannot_extend_overall_and_is_cancelled() {
    let config = support::config(10, 10, 10, 10);
    let (server, probe) = support::server(&config, Mode::Pending);
    let (request, sender, body_probe) = support::trickling_request();
    let request =
        tokio::spawn(async move { server.client_oneshot(request).await.expect("response") });

    tokio::task::yield_now().await;
    sender
        .send(Bytes::from_static(
            br#"{"model":"private-chat","messages":"#,
        ))
        .await
        .expect("first body chunk");
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(4)).await;
    sender
        .send(Bytes::from_static(
            br#"[{"role":"user","content":"hi"}],"stream":"#,
        ))
        .await
        .expect("second body chunk");
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(6)).await;

    let response = request.await.expect("request task");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(probe.dispatches(), 0);
    assert_eq!(body_probe.drops(), 1);
    assert!(
        sender
            .send(Bytes::from_static(b"never-read"))
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn body_time_precedes_queue_and_expired_queue_slot_is_recovered() {
    let config = support::config_with_limits(1, 1, 10, 10, 10, 10);
    let (server, probe) = support::server(&config, Mode::Pending);
    let server = Arc::new(server);

    let (second_request, sender, body_probe) = support::trickling_request();
    let second_server = server.clone();
    let second = tokio::spawn(async move {
        second_server
            .client_oneshot(second_request)
            .await
            .expect("second response")
    });
    sender
        .send(Bytes::from_static(
            br#"{"model":"private-chat","messages":[{"role":"user","content":"hi"}],"stream":"#,
        ))
        .await
        .expect("slow body prefix");
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(6)).await;

    let first_server = server.clone();
    let first = tokio::spawn(async move {
        first_server
            .client_oneshot(support::request(false))
            .await
            .expect("first response")
    });
    probe.wait_for_dispatch(1).await;
    sender
        .send(Bytes::from_static(b"false}"))
        .await
        .expect("slow body suffix");
    drop(sender);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(4)).await;

    let second = second.await.expect("second task");
    assert_eq!(second.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(probe.dispatches(), 1);
    assert_eq!(body_probe.drops(), 1);

    let third_server = server.clone();
    let third = tokio::spawn(async move {
        third_server
            .client_oneshot(support::request(false))
            .await
            .expect("third response")
    });
    tokio::task::yield_now().await;
    assert_eq!(probe.dispatches(), 1);

    probe.release();
    assert_eq!(first.await.expect("first task").status(), StatusCode::OK);
    probe.wait_for_dispatch(2).await;
    probe.release();
    assert_eq!(third.await.expect("third task").status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn periodic_events_do_not_extend_the_overall_stream_cap() {
    let config = support::config(10, 10, 5, 20);
    let (sender, mode) = support::bounded_events(4);
    sender.send(started()).await.expect("start");
    let (server, probe) = support::server(&config, mode);
    let response = server
        .client_oneshot(support::request(true))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    body.next().await.expect("role frame").expect("role bytes");

    for value in ["one", "two", "three", "four"] {
        sender.send(text(value)).await.expect("periodic event");
        let frame = body.next().await.expect("text frame").expect("text bytes");
        assert!(String::from_utf8_lossy(&frame).contains(value));
        tokio::time::advance(Duration::from_millis(4)).await;
    }
    tokio::time::advance(Duration::from_millis(4)).await;

    let error = body
        .next()
        .await
        .expect("timeout frame")
        .expect("timeout bytes");
    let error = String::from_utf8_lossy(&error);
    assert!(error.contains("upstream_timeout"));
    assert!(!error.contains("[DONE]"));
    assert!(body.next().await.is_none());
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn delayed_execute_leaves_overall_budget_for_first_byte_or_idle() {
    let config = support::config(10, 8, 8, 10);
    let (_sender, mode) = support::delayed_events(4, 7);
    let (server, probe) = support::server(&config, mode);
    let request = tokio::spawn(async move {
        server
            .client_oneshot(support::request(true))
            .await
            .expect("first-byte response")
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(7)).await;
    tokio::time::advance(Duration::from_millis(3)).await;
    let response = request.await.expect("first-byte task");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("first-byte body");
    assert!(String::from_utf8_lossy(&body).contains("upstream_timeout"));
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);

    let (sender, mode) = support::delayed_events(4, 7);
    sender.send(started()).await.expect("delayed start");
    let (server, probe) = support::server(&config, mode);
    let request = tokio::spawn(async move {
        server
            .client_oneshot(support::request(true))
            .await
            .expect("idle response")
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(7)).await;
    let response = request.await.expect("idle task");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    body.next().await.expect("role frame").expect("role bytes");
    tokio::time::advance(Duration::from_millis(3)).await;
    let error = body
        .next()
        .await
        .expect("idle timeout frame")
        .expect("idle timeout bytes");
    assert!(String::from_utf8_lossy(&error).contains("upstream_timeout"));
    assert!(body.next().await.is_none());
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn downstream_backpressure_does_not_start_idle_timer_but_pending_upstream_does() {
    let config = support::config(10, 10, 5, 20);
    let (sender, mode) = support::bounded_events(4);
    sender.send(started()).await.expect("start");
    let (server, probe) = support::server(&config, mode);
    let response = server
        .client_oneshot(support::request(true))
        .await
        .expect("backpressure response");
    let mut body = response.into_body().into_data_stream();
    body.next().await.expect("role frame").expect("role bytes");
    sender.send(text("before")).await.expect("first event");
    body.next()
        .await
        .expect("first text frame")
        .expect("first text bytes");
    tokio::time::advance(Duration::from_millis(6)).await;
    sender.send(text("after")).await.expect("buffered event");
    let frame = body
        .next()
        .await
        .expect("buffered text frame")
        .expect("buffered text bytes");
    assert!(String::from_utf8_lossy(&frame).contains("after"));
    drop(body);
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);

    let (sender, mode) = support::bounded_events(2);
    sender.send(started()).await.expect("pending start");
    let (server, probe) = support::server(&config, mode);
    let response = server
        .client_oneshot(support::request(true))
        .await
        .expect("pending response");
    let mut body = response.into_body().into_data_stream();
    body.next()
        .await
        .expect("pending role frame")
        .expect("pending role bytes");

    let mut first_wait = Box::pin(body.next());
    assert!(matches!(
        support::poll_once(first_wait.as_mut()),
        Poll::Pending
    ));
    drop(first_wait);

    let mut second_wait = Box::pin(body.next());
    assert!(matches!(
        support::poll_once(second_wait.as_mut()),
        Poll::Pending
    ));
    tokio::time::advance(Duration::from_millis(5)).await;
    let error = second_wait
        .await
        .expect("pending timeout frame")
        .expect("pending timeout bytes");
    assert!(String::from_utf8_lossy(&error).contains("upstream_timeout"));
    assert!(body.next().await.is_none());
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn expiry_after_completion_drops_buffered_success_and_releases_permit() {
    let config = support::config_with_limits(1, 1, 10, 10, 10, 20);
    let (first_sender, first_mode) = support::bounded_events(4);
    first_sender.send(started()).await.expect("first start");
    first_sender
        .send(completed())
        .await
        .expect("first completion");
    let (second_sender, second_mode) = support::bounded_events(4);
    second_sender.send(started()).await.expect("second start");
    second_sender
        .send(completed())
        .await
        .expect("second completion");
    let (server, probe) = support::server_with_modes(&config, vec![first_mode, second_mode]);

    let response = server
        .client_oneshot(support::request_with_options(true, true))
        .await
        .expect("first response");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    body.next().await.expect("role frame").expect("role bytes");
    body.next()
        .await
        .expect("finish frame")
        .expect("finish bytes");
    assert_eq!(probe.drops.load(Ordering::SeqCst), 1);

    tokio::time::advance(Duration::from_millis(20)).await;
    let error = body
        .next()
        .await
        .expect("post-completion timeout frame")
        .expect("post-completion timeout bytes");
    let error = String::from_utf8_lossy(&error);
    assert_eq!(error.matches("upstream_timeout").count(), 1);
    assert!(!error.contains("usage"));
    assert!(!error.contains("[DONE]"));
    assert!(body.next().await.is_none());

    let response = server
        .client_oneshot(support::request_with_options(true, false))
        .await
        .expect("reacquired response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("reacquired body");
    assert!(String::from_utf8_lossy(&body).contains("[DONE]"));
    assert_eq!(probe.dispatches(), 2);
    assert_eq!(probe.drops.load(Ordering::SeqCst), 2);
}
