use std::task::Poll;

use axum::http::StatusCode;
use futures_util::StreamExt;

use crate::support;

#[tokio::test]
async fn dropping_active_request_cancels_adapter_and_releases_permit() {
    let config = support::config(1, 1, 1_000);
    let (adapter, probe) = support::pending_adapter(
        "vllm-private",
        support::capabilities(&config, "vllm-private"),
    );
    let server = support::server(&config, vec![adapter]);

    let server_for_request = server.clone();
    let mut request = Box::pin(async move {
        server_for_request
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("active response")
    });
    assert!(matches!(
        support::poll_once(request.as_mut()),
        Poll::Pending
    ));
    probe.wait_for_dispatch(1).await;
    drop(request);
    assert_eq!(probe.cancellations(), 1);

    let server_for_next = server.clone();
    let mut next = Box::pin(async move {
        server_for_next
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("next response")
    });
    assert!(matches!(support::poll_once(next.as_mut()), Poll::Pending));
    probe.wait_for_dispatch(2).await;
    probe.release();
    let next = loop {
        if let Poll::Ready(response) = support::poll_once(next.as_mut()) {
            break response;
        }
    };
    assert_eq!(next.status(), StatusCode::OK);
}

#[tokio::test]
async fn stream_permit_survives_normalized_completion_until_done_and_eof() {
    let config = support::config(1, 1, 1_000);
    let (adapter, probe) = support::stream_adapter(
        "vllm-private",
        support::capabilities(&config, "vllm-private"),
    );
    let server = support::server(&config, vec![adapter]);
    let first_events = probe.prepare();
    first_events
        .send(support::started("private-chat"))
        .expect("first start");
    let response = server
        .client_oneshot(support::streaming_chat_request("private-chat", true))
        .await
        .expect("stream response");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    body.next()
        .await
        .expect("start frame")
        .expect("start bytes");
    first_events.send(support::completed()).expect("completion");
    body.next()
        .await
        .expect("finish frame")
        .expect("finish bytes");
    assert_eq!(probe.drops(), 1, "normalized completion drops upstream");

    let second_events = probe.prepare();
    second_events
        .send(support::started("private-chat"))
        .expect("second start");
    let server_for_next = server.clone();
    let mut next = Box::pin(async move {
        server_for_next
            .client_oneshot(support::streaming_chat_request("private-chat", false))
            .await
            .expect("next stream response")
    });
    assert!(matches!(support::poll_once(next.as_mut()), Poll::Pending));
    assert_eq!(probe.dispatches(), 1, "permit remains through finish");

    body.next()
        .await
        .expect("usage frame")
        .expect("usage bytes");
    assert!(matches!(support::poll_once(next.as_mut()), Poll::Pending));
    body.next().await.expect("done frame").expect("done bytes");
    assert!(matches!(support::poll_once(next.as_mut()), Poll::Pending));
    assert!(body.next().await.is_none(), "body reaches EOF");

    let next = loop {
        if let Poll::Ready(response) = support::poll_once(next.as_mut()) {
            break response;
        }
    };
    assert_eq!(next.status(), StatusCode::OK);
    assert_eq!(probe.dispatches(), 2);
}

#[tokio::test]
async fn dropping_stream_body_releases_permit_and_upstream() {
    let config = support::config(1, 1, 1_000);
    let (adapter, probe) = support::stream_adapter(
        "vllm-private",
        support::capabilities(&config, "vllm-private"),
    );
    let server = support::server(&config, vec![adapter]);
    let first_events = probe.prepare();
    first_events
        .send(support::started("private-chat"))
        .expect("first start");
    let response = server
        .client_oneshot(support::streaming_chat_request("private-chat", false))
        .await
        .expect("stream response");
    let mut body = response.into_body().into_data_stream();
    body.next()
        .await
        .expect("start frame")
        .expect("start bytes");

    let second_events = probe.prepare();
    second_events
        .send(support::started("private-chat"))
        .expect("second start");
    let server_for_next = server.clone();
    let mut next = Box::pin(async move {
        server_for_next
            .client_oneshot(support::streaming_chat_request("private-chat", false))
            .await
            .expect("next stream response")
    });
    assert!(matches!(support::poll_once(next.as_mut()), Poll::Pending));
    drop(body);
    assert_eq!(probe.drops(), 1, "body drop releases upstream");

    let next = loop {
        if let Poll::Ready(response) = support::poll_once(next.as_mut()) {
            break response;
        }
    };
    assert_eq!(next.status(), StatusCode::OK);
    assert_eq!(probe.dispatches(), 2);
}
