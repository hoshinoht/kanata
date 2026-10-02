#[path = "admission/support.rs"]
mod support;
use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use std::{convert::Infallible, time::Duration};
use support::*;

fn stalled(length: Option<usize>) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json");
    if let Some(length) = length {
        request = request.header("content-length", length);
    }
    request
        .body(Body::from_stream(futures_util::stream::pending::<
            Result<Bytes, Infallible>,
        >()))
        .expect("request")
}

#[tokio::test(start_paused = true)]
async fn upload_slots_cover_reading_and_dispatch_and_release_on_cancellation() {
    let config = config_with(2, 1, 1000, &[("max_uploads = 64", "max_uploads = 1")]);
    let (adapter, probe) = pending_adapter("ollama-local", capabilities(&config, "ollama-local"));
    let server = server(&config, vec![adapter]);
    let mut first = Box::pin(server.client_oneshot(stalled(Some(100))));
    assert!(poll_once(first.as_mut()).is_pending());
    let rejected = server
        .client_oneshot(stalled(Some(100)))
        .await
        .expect("response");
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response_json(rejected).await["error"]["code"],
        "gateway_upload_busy"
    );
    drop(first);
    let mut executing = Box::pin(server.client_oneshot(chat_request("local-chat")));
    assert!(poll_once(executing.as_mut()).is_pending());
    probe.wait_for_dispatch(1).await;
    assert_eq!(
        server
            .client_oneshot(stalled(Some(100)))
            .await
            .expect("response")
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(executing);
    let mut recovered = Box::pin(server.client_oneshot(stalled(Some(100))));
    assert!(poll_once(recovered.as_mut()).is_pending());
}

#[tokio::test(start_paused = true)]
async fn byte_budget_rejects_chunked_uploads_and_upload_deadline_releases_capacity() {
    let config = config_with(2, 1, 1000, &[("[timeouts]", "[timeouts]\nupload_ms = 50")]);
    let server = server(&config, vec![]);
    let mut a = Box::pin(server.client_oneshot(stalled(None)));
    let mut b = Box::pin(server.client_oneshot(stalled(None)));
    assert!(poll_once(a.as_mut()).is_pending());
    assert!(poll_once(b.as_mut()).is_pending());
    assert_eq!(
        server
            .client_oneshot(stalled(None))
            .await
            .expect("response")
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    tokio::time::advance(Duration::from_millis(50)).await;
    let response = a.await.expect("timeout");
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        response_json(response).await["error"]["code"],
        "request_upload_timeout"
    );
    drop(b);
    let mut recovered = Box::pin(server.client_oneshot(stalled(None)));
    assert!(poll_once(recovered.as_mut()).is_pending());
}

#[tokio::test(start_paused = true)]
async fn queue_and_upstream_metrics_measure_separate_phases() {
    let config = config(2, 1, 1000);
    let (adapter, probe) = pending_adapter("ollama-local", capabilities(&config, "ollama-local"));
    let server = server(&config, vec![adapter]);
    let mut first = Box::pin(server.client_oneshot(chat_request("local-chat")));
    assert!(poll_once(first.as_mut()).is_pending());
    let mut second = Box::pin(server.client_oneshot(chat_request("local-chat")));
    assert!(poll_once(second.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(100)).await;
    let scrape = || async {
        let response = server
            .admin_oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("metrics");
        String::from_utf8(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body")
                .to_vec(),
        )
        .expect("text")
    };
    assert!(scrape().await.contains("kanata_requests_queued 1\n"));
    probe.release();
    let response = first.await.expect("first response");
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    assert!(poll_once(second.as_mut()).is_pending());
    probe.release();
    let response = second.await.expect("second response");
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let metrics = scrape().await;
    assert!(metrics.contains("kanata_requests_queued 0\n"));
    assert!(metrics.contains("kanata_reserved_request_bytes 0\n"));
    for metric in ["queue_wait", "first_content", "upstream_duration"] {
        assert!(
            metrics.contains(&format!(
                "kanata_{metric}_seconds_count{{endpoint=\"chat\"}} 2"
            )),
            "{metrics}"
        );
    }
}
