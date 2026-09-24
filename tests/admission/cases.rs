use std::task::Poll;

use crate::support;
use axum::http::StatusCode;

#[tokio::test]
async fn queue_is_bounded_and_rejects_without_dispatch() {
    let config = support::config(1, 1, 1_000);
    let (adapter, probe) = support::pending_adapter(
        "vllm-private",
        support::capabilities(&config, "vllm-private"),
    );
    let server = support::server(&config, vec![adapter]);

    let first_server = server.clone();
    let first = tokio::spawn(async move {
        first_server
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("first response")
    });
    probe.wait_for_dispatch(1).await;

    let second_server = server.clone();
    let mut second = Box::pin(async move {
        second_server
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("second response")
    });
    assert!(matches!(support::poll_once(second.as_mut()), Poll::Pending));

    let rejected = server
        .client_oneshot(support::chat_request("private-chat"))
        .await
        .expect("rejected response");
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = support::response_json(rejected).await;
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");
    assert_eq!(probe.dispatches(), 1);

    probe.release();
    let first = first.await.expect("first task");
    assert_eq!(first.status(), StatusCode::OK);
    assert!(matches!(support::poll_once(second.as_mut()), Poll::Pending));
    assert_eq!(probe.dispatches(), 2);
    probe.release();
    let second = loop {
        if let Poll::Ready(response) = support::poll_once(second.as_mut()) {
            break response;
        }
    };
    assert_eq!(second.status(), StatusCode::OK);
}

#[tokio::test]
async fn queued_requests_are_served_in_fifo_order() {
    let config = support::config(2, 1, 1_000);
    let (adapter, probe) = support::pending_adapter(
        "vllm-private",
        support::capabilities(&config, "vllm-private"),
    );
    let server = support::server(&config, vec![adapter]);

    let first_server = server.clone();
    let first = tokio::spawn(async move {
        first_server
            .client_oneshot(support::chat_request_with_id("private-chat", "first"))
            .await
            .expect("first response")
    });
    probe.wait_for_dispatch(1).await;

    let second_server = server.clone();
    let mut second = Box::pin(async move {
        second_server
            .client_oneshot(support::chat_request_with_id("private-chat", "second"))
            .await
            .expect("second response")
    });
    assert!(matches!(support::poll_once(second.as_mut()), Poll::Pending));
    let third_server = server.clone();
    let mut third = Box::pin(async move {
        third_server
            .client_oneshot(support::chat_request_with_id("private-chat", "third"))
            .await
            .expect("third response")
    });
    assert!(matches!(support::poll_once(third.as_mut()), Poll::Pending));

    probe.release();
    let first = first.await.expect("first task");
    assert_eq!(first.status(), StatusCode::OK);
    assert!(matches!(support::poll_once(second.as_mut()), Poll::Pending));
    assert_eq!(probe.request_ids(), vec!["first", "second"]);

    probe.release();
    let second = loop {
        if let Poll::Ready(response) = support::poll_once(second.as_mut()) {
            break response;
        }
    };
    assert_eq!(second.status(), StatusCode::OK);
    assert!(matches!(support::poll_once(third.as_mut()), Poll::Pending));
    assert_eq!(probe.request_ids(), vec!["first", "second", "third"]);

    probe.release();
    let third = loop {
        if let Poll::Ready(response) = support::poll_once(third.as_mut()) {
            break response;
        }
    };
    assert_eq!(third.status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn queued_request_times_out_as_queue_timeout() {
    let config = support::config(1, 1, 10);
    let (adapter, probe) = support::pending_adapter(
        "vllm-private",
        support::capabilities(&config, "vllm-private"),
    );
    let server = support::server(&config, vec![adapter]);

    let first_server = server.clone();
    let first = tokio::spawn(async move {
        first_server
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("first response")
    });
    probe.wait_for_dispatch(1).await;

    let second_server = server.clone();
    let mut second = Box::pin(async move {
        second_server
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("second response")
    });
    assert!(matches!(support::poll_once(second.as_mut()), Poll::Pending));
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    let second = loop {
        if let Poll::Ready(response) = support::poll_once(second.as_mut()) {
            break response;
        }
    };
    assert_eq!(second.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = support::response_json(second).await;
    assert_eq!(body["error"]["code"], "upstream_timeout");
    assert_eq!(probe.dispatches(), 1);

    probe.release();
    assert_eq!(first.await.expect("first task").status(), StatusCode::OK);
}

#[tokio::test]
async fn cancelled_waiter_releases_queue_slot_without_execution() {
    let config = support::config(1, 1, 1_000);
    let (adapter, probe) = support::pending_adapter(
        "vllm-private",
        support::capabilities(&config, "vllm-private"),
    );
    let server = support::server(&config, vec![adapter]);

    let first_server = server.clone();
    let first = tokio::spawn(async move {
        first_server
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("first response")
    });
    probe.wait_for_dispatch(1).await;

    let second_server = server.clone();
    let mut second = Box::pin(async move {
        second_server
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("second response")
    });
    assert!(matches!(support::poll_once(second.as_mut()), Poll::Pending));
    drop(second);

    let third_server = server.clone();
    let mut third = Box::pin(async move {
        third_server
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("third response")
    });
    assert!(matches!(support::poll_once(third.as_mut()), Poll::Pending));
    assert_eq!(probe.dispatches(), 1);

    probe.release();
    assert_eq!(first.await.expect("first task").status(), StatusCode::OK);
    assert!(matches!(support::poll_once(third.as_mut()), Poll::Pending));
    assert_eq!(probe.dispatches(), 2);
    probe.release();
    let third = loop {
        if let Poll::Ready(response) = support::poll_once(third.as_mut()) {
            break response;
        }
    };
    assert_eq!(third.status(), StatusCode::OK);
}

#[tokio::test]
async fn routes_have_independent_admission_pools() {
    let config = support::config(1, 1, 1_000);
    let (local_adapter, local_probe) = support::pending_adapter(
        "ollama-local",
        support::capabilities(&config, "ollama-local"),
    );
    let (private_adapter, private_probe) = support::pending_adapter(
        "vllm-private",
        support::capabilities(&config, "vllm-private"),
    );
    let server = support::server(&config, vec![local_adapter, private_adapter]);

    let local_server = server.clone();
    let local = tokio::spawn(async move {
        local_server
            .client_oneshot(support::chat_request("local-chat"))
            .await
            .expect("local response")
    });
    local_probe.wait_for_dispatch(1).await;

    let local_queue_server = server.clone();
    let mut local_queue = Box::pin(async move {
        local_queue_server
            .client_oneshot(support::chat_request("local-chat"))
            .await
            .expect("local queue response")
    });
    assert!(matches!(
        support::poll_once(local_queue.as_mut()),
        Poll::Pending
    ));

    let private_server = server.clone();
    let mut private = Box::pin(async move {
        private_server
            .client_oneshot(support::chat_request("private-chat"))
            .await
            .expect("private response")
    });
    assert!(matches!(
        support::poll_once(private.as_mut()),
        Poll::Pending
    ));
    private_probe.wait_for_dispatch(1).await;
    private_probe.release();
    let private = loop {
        if let Poll::Ready(response) = support::poll_once(private.as_mut()) {
            break response;
        }
    };
    assert_eq!(private.status(), StatusCode::OK);

    let rejected = server
        .client_oneshot(support::chat_request("local-chat"))
        .await
        .expect("local rejection");
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(local_probe.dispatches(), 1);
    assert_eq!(private_probe.dispatches(), 1);

    local_probe.release();
    assert_eq!(local.await.expect("local task").status(), StatusCode::OK);
    assert!(matches!(
        support::poll_once(local_queue.as_mut()),
        Poll::Pending
    ));
    local_probe.release();
    let local_queue = loop {
        if let Poll::Ready(response) = support::poll_once(local_queue.as_mut()) {
            break response;
        }
    };
    assert_eq!(local_queue.status(), StatusCode::OK);
}
