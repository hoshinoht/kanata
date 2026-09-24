use std::time::Duration;

use axum::http::StatusCode;
use tokio::{io::AsyncReadExt, time::advance};

use super::support;

#[tokio::test]
async fn draining_rejects_new_inference_preserves_auth_and_admin_visibility() {
    let (mut server, probe) = support::pending_server(Duration::from_secs(30)).await;
    let mut active = support::connected(server.client_addr).await;
    support::send(&mut active, support::chat_request()).await;
    probe.wait_for_dispatch(1).await;

    let mut queued = support::connected(server.client_addr).await;
    support::send(&mut queued, support::chat_request()).await;
    tokio::task::yield_now().await;

    server.signal();
    while server.readiness.is_ready() {
        tokio::task::yield_now().await;
    }

    let mut invalid = support::connected(server.client_addr).await;
    support::send(&mut invalid, support::invalid_auth_request_without_body()).await;
    assert_eq!(
        support::response(&mut invalid).await.status,
        StatusCode::UNAUTHORIZED.as_u16()
    );

    let mut fresh = support::connected(server.client_addr).await;
    support::send(&mut fresh, support::draining_request_without_body()).await;
    let fresh = support::response(&mut fresh).await;
    assert_eq!(fresh.status, StatusCode::SERVICE_UNAVAILABLE.as_u16());
    assert_eq!(fresh.body_json()["error"]["code"], "server_draining");

    let mut models = support::connected(server.client_addr).await;
    support::send(&mut models, support::models_request()).await;
    assert_eq!(
        support::response(&mut models).await.status,
        StatusCode::OK.as_u16()
    );

    let mut live = support::connected(server.admin_addr).await;
    support::send(&mut live, support::admin_request("/live")).await;
    assert_eq!(
        support::response(&mut live).await.status,
        StatusCode::OK.as_u16()
    );

    let mut ready = support::connected(server.admin_addr).await;
    support::send(&mut ready, support::admin_request("/ready")).await;
    assert_eq!(
        support::response(&mut ready).await.status,
        StatusCode::SERVICE_UNAVAILABLE.as_u16()
    );
    server.readiness.set_ready(true);
    let mut still_not_ready = support::connected(server.admin_addr).await;
    support::send(&mut still_not_ready, support::admin_request("/ready")).await;
    assert_eq!(
        support::response(&mut still_not_ready).await.status,
        StatusCode::SERVICE_UNAVAILABLE.as_u16()
    );

    let mut metrics = support::connected(server.admin_addr).await;
    support::send(&mut metrics, support::admin_request("/metrics")).await;
    let metrics = support::response(&mut metrics).await;
    assert_eq!(metrics.status, StatusCode::OK.as_u16());
    assert!(metrics.body_text().contains("kanata_process_ready 0"));

    assert_eq!(
        support::response(&mut queued).await.status,
        StatusCode::SERVICE_UNAVAILABLE.as_u16()
    );
    assert_eq!(probe.dispatches(), 1);
    probe.release();
    assert_eq!(
        support::response(&mut active).await.status,
        StatusCode::OK.as_u16()
    );
    assert_eq!(probe.dispatches(), 1);
    assert!(server.join().await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn active_connection_survives_until_exact_grace_boundary() {
    let grace = Duration::from_secs(5);
    let (mut server, probe) = support::pending_server(grace).await;
    let mut active = support::connected(server.client_addr).await;
    support::send(&mut active, support::chat_request()).await;
    probe.wait_for_dispatch(1).await;

    server.signal();
    while server.readiness.is_ready() {
        tokio::task::yield_now().await;
    }
    advance(grace - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(!server.is_finished());
    assert_eq!(probe.cancellations(), 0);

    advance(Duration::from_millis(1)).await;
    assert!(server.join().await.is_ok());
    assert_eq!(probe.cancellations(), 1);
    let mut byte = [0_u8; 1];
    assert_eq!(active.read(&mut byte).await.expect("socket read"), 0);
}

#[tokio::test]
async fn interrupted_sse_drops_upstream_and_admission_permit() {
    let (mut server, probe, sender) = support::stream_server(Duration::from_secs(30)).await;
    sender.send(support::started()).expect("stream start");
    sender.send(support::text("first")).expect("stream text");
    let mut client = support::connected(server.client_addr).await;
    support::send(&mut client, support::streaming_request()).await;
    let prefix = support::response_prefix(&mut client).await;
    assert!(prefix.windows(5).any(|window| window == b"data:"));
    drop(client);

    server.signal();
    assert!(server.join().await.is_ok());
    assert_eq!(probe.drops(), 1);
}

#[tokio::test]
async fn bound_listeners_require_the_exact_validated_pair() {
    let (server, client, admin, _) = support::parts(support::pending_adapter().await.0).await;
    assert!(server.with_bound_listeners(admin, client).is_err());

    let (server, client, _admin, _) = support::parts(support::pending_adapter().await.0).await;
    let external = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("external listener");
    assert!(server.with_bound_listeners(client, external).is_err());

    let (server, client, admin, _) = support::parts(support::pending_adapter().await.0).await;
    let bound = server
        .with_bound_listeners(client, admin)
        .expect("exact pair");
    assert!(bound.client_addr().ip().is_loopback());
    assert!(bound.admin_addr().ip().is_loopback());
}

#[tokio::test]
async fn enormous_grace_is_rejected_before_runtime_start() {
    let (server, client, admin, _) = support::parts(support::pending_adapter().await.0).await;
    let bound = server
        .with_bound_listeners(client, admin)
        .expect("exact pair");
    assert!(
        bound
            .serve_until(std::future::pending::<()>(), Duration::MAX)
            .await
            .is_err()
    );
}
