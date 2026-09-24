use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use tokio::{io::AsyncWriteExt, sync::oneshot};

use crate::core::{ErrorKind, TimeoutPhase};

use super::super::ResponseContentType;
use super::super::types;
use super::fixture;

#[tokio::test]
async fn request_reads_complete_headers_and_drains_the_response() {
    let (listener, address) = fixture::listener().await;
    let server_address = address.clone();
    let server = fixture::server_task(listener, |mut socket| async move {
        let request = fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        fixture::assert_request_headers(&request, &server_address);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-type: application/json; charset=utf-8\r\n\r\nok",
            )
            .await
            .unwrap_or_else(|_| panic!("response"));
    });

    let request = fixture::request(Bytes::from_static(b"{}"), 1024);
    let mut response = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    )
    .execute(request)
    .await
    .unwrap_or_else(|_| panic!("response"));
    assert_eq!(response.status, 200);
    assert!(matches!(
        response.content_type,
        Some(ResponseContentType::Json)
    ));
    let mut body = Vec::new();
    while let Some(chunk) = response.body.next().await {
        body.extend_from_slice(&chunk.unwrap_or_else(|_| panic!("body")));
    }
    assert_eq!(body, b"ok");
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test]
async fn dropping_waiting_headers_closes_the_socket() {
    let (listener, address) = fixture::listener().await;
    let server_address = address.clone();
    let (headers_tx, headers_rx) = fixture::signal_pair();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = fixture::server_task(listener, |mut socket| async move {
        let request = fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        fixture::assert_request_headers(&request, &server_address);
        headers_tx
            .send(())
            .unwrap_or_else(|_| panic!("headers signal"));
        fixture::wait_for_close(socket)
            .await
            .unwrap_or_else(|_| panic!("close"));
        closed_tx
            .send(())
            .unwrap_or_else(|_| panic!("close signal"));
    });

    let transport = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    );
    let mut execute =
        Box::pin(transport.execute(fixture::request(Bytes::from_static(b"{}"), 1024)));
    tokio::select! {
        _result = &mut execute => panic!("headers unexpectedly completed"),
        result = headers_rx => result.unwrap_or_else(|_| panic!("headers signal")),
    }
    drop(execute);
    fixture::bounded_wait(closed_rx).await;
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test]
async fn dropping_active_body_closes_the_socket() {
    let (listener, address) = fixture::listener().await;
    let server_address = address.clone();
    let (body_tx, body_rx) = fixture::signal_pair();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = fixture::server_task(listener, |mut socket| async move {
        let request = fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        fixture::assert_request_headers(&request, &server_address);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\na")
            .await
            .unwrap_or_else(|_| panic!("response"));
        body_tx.send(()).unwrap_or_else(|_| panic!("body signal"));
        fixture::wait_for_close(socket)
            .await
            .unwrap_or_else(|_| panic!("close"));
        closed_tx
            .send(())
            .unwrap_or_else(|_| panic!("close signal"));
    });

    let mut response = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    )
    .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
    .await
    .unwrap_or_else(|_| panic!("response"));
    fixture::bounded_wait(body_rx).await;
    assert_eq!(
        response.body.next().await,
        Some(Ok(Bytes::from_static(b"a")))
    );
    drop(response.body);
    fixture::bounded_wait(closed_rx).await;
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test(flavor = "current_thread")]
async fn first_byte_timeout_fuses_body_and_releases_socket_before_drop() {
    let (listener, address) = fixture::listener().await;
    let server_address = address.clone();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = fixture::server_task(listener, |mut socket| async move {
        let request = fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        fixture::assert_request_headers(&request, &server_address);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n")
            .await
            .unwrap_or_else(|_| panic!("response"));
        fixture::wait_for_close(socket)
            .await
            .unwrap_or_else(|_| panic!("close"));
        closed_tx
            .send(())
            .unwrap_or_else(|_| panic!("close signal"));
    });

    let mut body = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(5),
            Duration::from_secs(1),
        ),
    )
    .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
    .await
    .unwrap_or_else(|error| panic!("response: {error:?}"))
    .body;
    tokio::time::pause();
    let (item_tx, item_rx) = oneshot::channel();
    let (continue_tx, continue_rx) = oneshot::channel();
    let body_task = tokio::spawn(async move {
        let item = body.next().await;
        let kind = item.map(|item| item.map(|_| ()).map_err(|error| error.kind));
        item_tx.send(kind).unwrap_or_else(|_| panic!("item signal"));
        continue_rx
            .await
            .unwrap_or_else(|_| panic!("continue signal"));
        body.next().await
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(5)).await;
    let first = item_rx
        .await
        .unwrap_or_else(|_| panic!("first item"))
        .unwrap_or_else(|| panic!("missing timeout"));
    assert_eq!(
        first,
        Err(ErrorKind::Timeout {
            phase: TimeoutPhase::FirstByte
        })
    );
    tokio::time::resume();
    fixture::bounded_wait(closed_rx).await;
    continue_tx
        .send(())
        .unwrap_or_else(|_| panic!("continue signal"));
    assert!(
        body_task
            .await
            .unwrap_or_else(|_| panic!("body task"))
            .is_none()
    );
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test(flavor = "current_thread")]
async fn idle_timeout_follows_data_and_does_not_repeat() {
    let (listener, address) = fixture::listener().await;
    let server_address = address.clone();
    let (done_tx, done_rx) = oneshot::channel();
    let server = fixture::server_task(listener, |mut socket| async move {
        let request = fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        fixture::assert_request_headers(&request, &server_address);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\na")
            .await
            .unwrap_or_else(|_| panic!("response"));
        done_rx
            .await
            .unwrap_or_else(|_| panic!("server completion"));
        drop(socket);
    });

    let mut body = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(5),
        ),
    )
    .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
    .await
    .unwrap_or_else(|error| panic!("response: {error:?}"))
    .body;
    assert_eq!(body.next().await, Some(Ok(Bytes::from_static(b"a"))));
    tokio::time::pause();
    let (item_tx, item_rx) = oneshot::channel();
    let body_task = tokio::spawn(async move {
        let item = body.next().await;
        let kind = item.map(|item| item.map(|_| ()).map_err(|error| error.kind));
        item_tx.send(kind).unwrap_or_else(|_| panic!("item signal"));
        body.next().await
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(5)).await;
    let error = item_rx
        .await
        .unwrap_or_else(|_| panic!("idle item"))
        .unwrap_or_else(|| panic!("missing idle timeout"));
    assert_eq!(
        error,
        Err(ErrorKind::Timeout {
            phase: TimeoutPhase::Idle
        })
    );
    assert!(
        body_task
            .await
            .unwrap_or_else(|_| panic!("body task"))
            .is_none()
    );
    done_tx
        .send(())
        .unwrap_or_else(|_| panic!("server completion"));
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test(flavor = "current_thread")]
async fn downstream_pause_does_not_age_ready_buffered_data() {
    let (listener, address) = fixture::listener().await;
    let server_address = address.clone();
    let server = fixture::server_task(listener, |mut socket| async move {
        let request = fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        fixture::assert_request_headers(&request, &server_address);
        let body = vec![b'x'; types::MAX_CHUNK_BYTES + 1];
        let response = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
        socket
            .write_all(response.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("response headers"));
        socket
            .write_all(&body)
            .await
            .unwrap_or_else(|_| panic!("response body"));
    });

    let mut body = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(5),
        ),
    )
    .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
    .await
    .unwrap_or_else(|error| panic!("response: {error:?}"))
    .body;
    let first = body.next().await.unwrap_or_else(|| panic!("first chunk"));
    assert!(
        !first
            .as_ref()
            .unwrap_or_else(|_| panic!("first error"))
            .is_empty()
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    let second = body.next().await.unwrap_or_else(|| panic!("second chunk"));
    assert!(!second.unwrap_or_else(|_| panic!("second error")).is_empty());
    assert!(body.next().await.is_none());
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test]
async fn completed_connection_does_not_drop_buffered_multi_chunk_body() {
    let (listener, address) = fixture::listener().await;
    let server_address = address.clone();
    let server = fixture::server_task(listener, |mut socket| async move {
        let request = fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        fixture::assert_request_headers(&request, &server_address);
        let body = vec![b'z'; types::MAX_CHUNK_BYTES * 2 + 7];
        let response = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
        socket
            .write_all(response.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("response headers"));
        socket
            .write_all(&body)
            .await
            .unwrap_or_else(|_| panic!("response body"));
    });

    let mut response = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    )
    .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
    .await
    .unwrap_or_else(|_| panic!("response"));
    let mut body = Vec::new();
    while let Some(chunk) = response.body.next().await {
        body.extend_from_slice(&chunk.unwrap_or_else(|_| panic!("body")));
    }
    assert_eq!(body.len(), types::MAX_CHUNK_BYTES * 2 + 7);
    assert!(body.iter().all(|byte| *byte == b'z'));
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test]
async fn eof_is_fused_and_truncated_body_errors_once() {
    let (listener, address) = fixture::listener().await;
    let server_address = address.clone();
    let server = fixture::server_task(listener, |mut socket| async move {
        let request = fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        fixture::assert_request_headers(&request, &server_address);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\nx")
            .await
            .unwrap_or_else(|_| panic!("response"));
    });
    let mut body = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    )
    .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
    .await
    .unwrap_or_else(|_| panic!("response"))
    .body;
    assert_eq!(body.next().await, Some(Ok(Bytes::from_static(b"x"))));
    assert!(body.next().await.is_none());
    assert!(body.next().await.is_none());
    server.await.unwrap_or_else(|_| panic!("server"));

    let (listener, address) = fixture::listener().await;
    let server_address = address.clone();
    let server = fixture::server_task(listener, |mut socket| async move {
        let request = fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        fixture::assert_request_headers(&request, &server_address);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nx")
            .await
            .unwrap_or_else(|_| panic!("response"));
    });
    let mut body = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    )
    .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
    .await
    .unwrap_or_else(|_| panic!("response"))
    .body;
    assert_eq!(body.next().await, Some(Ok(Bytes::from_static(b"x"))));
    let error = body
        .next()
        .await
        .unwrap_or_else(|| panic!("truncated error"))
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);
    assert!(body.next().await.is_none());
    server.await.unwrap_or_else(|_| panic!("server"));
}
