use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::core::{ErrorKind, TimeoutPhase};

use super::super::types::{self, ResponseContentType};
use super::fixture;

async fn rejected_response(response: Vec<u8>) -> crate::core::GatewayError {
    let (listener, address) = fixture::listener().await;
    let server = fixture::server_task(listener, |mut socket| async move {
        fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        socket
            .write_all(&response)
            .await
            .unwrap_or_else(|_| panic!("response"));
    });
    let result = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    )
    .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
    .await;
    server.await.unwrap_or_else(|_| panic!("server"));
    match result {
        Err(error) => error,
        Ok(_) => panic!("invalid response accepted"),
    }
}

fn empty_response(extra_headers: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\n{extra_headers}content-length: 0\r\n\r\n").into_bytes()
}

#[tokio::test]
async fn header_count_and_bytes_limits_are_upstream_failures() {
    let mut headers = String::new();
    for index in 0..=types::MAX_HEADERS {
        headers.push_str(&format!("x-limit-{index}: v\r\n"));
    }
    let error = rejected_response(empty_response(&headers)).await;
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);

    let value = "x".repeat(types::MAX_HEADER_BYTES);
    let error = rejected_response(empty_response(&format!("x-large: {value}\r\n"))).await;
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);
}

#[tokio::test]
async fn every_content_encoding_token_and_duplicate_is_checked() {
    for encoding in ["gzip", "identity, gzip", "gzip, identity", "identity,", ""] {
        let response = empty_response(&format!("content-encoding: {encoding}\r\n"));
        let error = rejected_response(response).await;
        assert_eq!(error.kind, ErrorKind::UpstreamFailure);
    }

    let response = empty_response("content-encoding: identity\r\ncontent-encoding: gzip\r\n");
    let error = rejected_response(response).await;
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);
}

#[tokio::test]
async fn sse_content_type_is_recognized_and_ambiguous_metadata_is_rejected() {
    let (listener, address) = fixture::listener().await;
    let server = fixture::server_task(listener, |mut socket| async move {
        fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\ncontent-length: 0\r\n\r\n",
            )
            .await
            .unwrap_or_else(|_| panic!("response"));
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
    assert!(matches!(
        response.content_type,
        Some(ResponseContentType::EventStream)
    ));
    assert!(response.body.next().await.is_none());
    server.await.unwrap_or_else(|_| panic!("server"));

    let error = rejected_response(empty_response(
        "content-type: application/json\r\ncontent-type: text/event-stream\r\n",
    ))
    .await;
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);

    let error = rejected_response(empty_response("content-type: \r\n")).await;
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);

    for content_type in [
        "application/json;",
        "application/json; charset",
        "application/json; =utf-8",
        "application/json; charset=",
        "application/json; charset=\"unterminated",
        "application/json; charset=\"a,b\"",
        "application/json; charset=utf 8",
        "application/json, text/plain",
    ] {
        let error =
            rejected_response(empty_response(&format!("content-type: {content_type}\r\n"))).await;
        assert_eq!(error.kind, ErrorKind::UpstreamFailure);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_response_head_has_a_distinct_headers_timeout_and_closes() {
    let (listener, address) = fixture::listener().await;
    let (seen_tx, seen_rx) = fixture::signal_pair();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = fixture::server_task(listener, |mut socket| async move {
        fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        seen_tx.send(()).unwrap_or_else(|_| panic!("seen signal"));
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
        result = seen_rx => result.unwrap_or_else(|_| panic!("seen signal")),
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5)).await;
    let error = match execute.await {
        Err(error) => error,
        Ok(_) => panic!("missing headers timeout"),
    };
    assert_eq!(
        error.kind,
        ErrorKind::Timeout {
            phase: TimeoutPhase::Headers
        }
    );
    tokio::time::resume();
    fixture::bounded_wait(closed_rx).await;
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test]
async fn low_budget_seam_enforces_exact_body_limit_and_one_terminal_error() {
    let body = vec![b'a'; types::MAX_CHUNK_BYTES * 2 + 5];
    let body_len = body.len();
    let (listener, address) = fixture::listener().await;
    let server = fixture::server_task(listener, move |mut socket| async move {
        fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body_len);
        socket
            .write_all(head.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("response head"));
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
    .execute(fixture::request_with_response_budget(
        Bytes::from_static(b"{}"),
        1024,
        body_len,
    ))
    .await
    .unwrap_or_else(|_| panic!("response"));
    let mut total = 0;
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk.unwrap_or_else(|_| panic!("exact budget error"));
        assert!(chunk.len() <= types::MAX_CHUNK_BYTES);
        total += chunk.len();
    }
    assert_eq!(total, body_len);
    server.await.unwrap_or_else(|_| panic!("server"));

    let (listener, address) = fixture::listener().await;
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let over = vec![b'b'; 7];
    let server = fixture::server_task(listener, |mut socket| async move {
        fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", over.len());
        socket
            .write_all(head.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("response head"));
        socket
            .write_all(&over)
            .await
            .unwrap_or_else(|_| panic!("response body"));
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
    .execute(fixture::request_with_response_budget(
        Bytes::from_static(b"{}"),
        1024,
        6,
    ))
    .await
    .unwrap_or_else(|_| panic!("response"));
    let mut saw_error = false;
    while let Some(chunk) = response.body.next().await {
        match chunk {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error.kind, ErrorKind::UpstreamFailure);
                saw_error = true;
                break;
            }
        }
    }
    assert!(saw_error);
    assert!(response.body.next().await.is_none());
    fixture::bounded_wait(closed_rx).await;
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test]
async fn cumulative_limit_rejects_second_chunk_and_closes_socket() {
    let (listener, address) = fixture::listener().await;
    let (first_tx, first_rx) = fixture::signal_pair();
    let (continue_tx, continue_rx) = fixture::signal_pair();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = fixture::server_task(listener, |mut socket| async move {
        fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\nabcd\r\n")
            .await
            .unwrap_or_else(|_| panic!("first chunk"));
        first_tx.send(()).unwrap_or_else(|_| panic!("first signal"));
        continue_rx
            .await
            .unwrap_or_else(|_| panic!("continue signal"));
        socket
            .write_all(b"3\r\nefg\r\n0\r\n\r\n")
            .await
            .unwrap_or_else(|_| panic!("second chunk"));
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
    .execute(fixture::request_with_response_budget(
        Bytes::from_static(b"{}"),
        1024,
        6,
    ))
    .await
    .unwrap_or_else(|_| panic!("response"));
    fixture::bounded_wait(first_rx).await;
    assert_eq!(
        response.body.next().await,
        Some(Ok(Bytes::from_static(b"abcd")))
    );
    continue_tx
        .send(())
        .unwrap_or_else(|_| panic!("continue signal"));
    let error = response
        .body
        .next()
        .await
        .unwrap_or_else(|| panic!("missing cumulative error"))
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);
    assert!(response.body.next().await.is_none());
    fixture::bounded_wait(closed_rx).await;
    server.await.unwrap_or_else(|_| panic!("server"));
}
