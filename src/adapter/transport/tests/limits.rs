use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::core::{ErrorKind, TimeoutPhase};

use super::super::types::{self, ResponseContentType};
use super::fixture::{self, SECOND};

async fn rejected_response(response: Vec<u8>) -> crate::core::GatewayError {
    let (listener, address) = fixture::listener().await;
    let server = fixture::reply(listener, None, response);
    let result = fixture::execute(address, fixture::uniform_timeouts()).await;
    server.await.unwrap_or_else(|_| panic!("server"));
    match result {
        Err(error) => error,
        Ok(_) => panic!("invalid response accepted"),
    }
}

fn empty_response(extra_headers: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\n{extra_headers}content-length: 0\r\n\r\n").into_bytes()
}

/// Requests with a `response_budget` against an upstream sending `body` with an exact length.
async fn budgeted(
    body: Vec<u8>,
    response_budget: usize,
    closed: Option<tokio::sync::oneshot::Sender<()>>,
) -> (types::ResponseBody, tokio::task::JoinHandle<()>) {
    let (listener, address) = fixture::listener().await;
    let server = fixture::serve(listener, move |mut socket| async move {
        let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
        socket
            .write_all(head.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("response head"));
        socket
            .write_all(&body)
            .await
            .unwrap_or_else(|_| panic!("response body"));
        if let Some(closed) = closed {
            fixture::signal_close(socket, closed).await;
        }
    });
    let response = fixture::transport(address, fixture::uniform_timeouts())
        .execute(fixture::request_with_response_budget(
            Bytes::from_static(b"{}"),
            1024,
            response_budget,
        ))
        .await
        .unwrap_or_else(|_| panic!("response"));
    (response.body, server)
}

#[tokio::test]
async fn header_count_and_bytes_limits_are_upstream_failures() {
    let mut headers = String::new();
    for index in 0..=types::MAX_HEADERS {
        headers.push_str(&format!("x-limit-{index}: v\r\n"));
    }
    let large = format!("x-large: {}\r\n", "x".repeat(types::MAX_HEADER_BYTES));
    for headers in [headers, large] {
        let error = rejected_response(empty_response(&headers)).await;
        assert_eq!(error.kind, ErrorKind::UpstreamFailure);
    }
}

#[tokio::test]
async fn every_content_encoding_token_and_duplicate_is_checked() {
    for headers in [
        "content-encoding: gzip\r\n",
        "content-encoding: identity, gzip\r\n",
        "content-encoding: gzip, identity\r\n",
        "content-encoding: identity,\r\n",
        "content-encoding: \r\n",
        "content-encoding: identity\r\ncontent-encoding: gzip\r\n",
    ] {
        let error = rejected_response(empty_response(headers)).await;
        assert_eq!(error.kind, ErrorKind::UpstreamFailure, "{headers:?}");
    }
}

#[tokio::test]
async fn sse_content_type_is_recognized_and_ambiguous_metadata_is_rejected() {
    let (listener, address) = fixture::listener().await;
    let server = fixture::reply(
        listener,
        None,
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\ncontent-length: 0\r\n\r\n"
            .to_vec(),
    );
    let mut response = fixture::respond(address, fixture::uniform_timeouts()).await;
    assert!(matches!(
        response.content_type,
        Some(ResponseContentType::EventStream)
    ));
    assert!(response.body.next().await.is_none());
    server.await.unwrap_or_else(|_| panic!("server"));

    let mut rejected = vec![
        "content-type: application/json\r\ncontent-type: text/event-stream\r\n".to_owned(),
        "content-type: \r\n".to_owned(),
    ];
    rejected.extend(
        [
            "application/json;",
            "application/json; charset",
            "application/json; =utf-8",
            "application/json; charset=",
            "application/json; charset=\"unterminated",
            "application/json; charset=\"a,b\"",
            "application/json; charset=utf 8",
            "application/json, text/plain",
        ]
        .map(|content_type| format!("content-type: {content_type}\r\n")),
    );
    for headers in rejected {
        let error = rejected_response(empty_response(&headers)).await;
        assert_eq!(error.kind, ErrorKind::UpstreamFailure, "{headers:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_response_head_has_a_distinct_headers_timeout_and_closes() {
    let (listener, address) = fixture::listener().await;
    let (seen_tx, seen_rx) = fixture::signal_pair();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = fixture::serve(listener, |socket| async move {
        seen_tx.send(()).unwrap_or_else(|_| panic!("seen signal"));
        fixture::signal_close(socket, closed_tx).await;
    });

    let transport = fixture::transport(
        address,
        fixture::timeouts(SECOND, Duration::from_secs(5), SECOND, SECOND),
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
    let body_len = types::MAX_CHUNK_BYTES * 2 + 5;
    let (mut body, server) = budgeted(vec![b'a'; body_len], body_len, None).await;
    let mut total = 0;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.unwrap_or_else(|_| panic!("exact budget error"));
        assert!(chunk.len() <= types::MAX_CHUNK_BYTES);
        total += chunk.len();
    }
    assert_eq!(total, body_len);
    server.await.unwrap_or_else(|_| panic!("server"));

    let (closed_tx, closed_rx) = fixture::signal_pair();
    let (mut body, server) = budgeted(vec![b'b'; 7], 6, Some(closed_tx)).await;
    let mut saw_error = false;
    while let Some(chunk) = body.next().await {
        if let Err(error) = chunk {
            assert_eq!(error.kind, ErrorKind::UpstreamFailure);
            saw_error = true;
            break;
        }
    }
    assert!(saw_error);
    assert!(body.next().await.is_none());
    fixture::bounded_wait(closed_rx).await;
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test]
async fn cumulative_limit_rejects_second_chunk_and_closes_socket() {
    let (listener, address) = fixture::listener().await;
    let (first_tx, first_rx) = fixture::signal_pair();
    let (continue_tx, continue_rx) = fixture::signal_pair();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = fixture::serve(listener, |mut socket| async move {
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
        fixture::signal_close(socket, closed_tx).await;
    });
    let mut response = fixture::transport(address, fixture::uniform_timeouts())
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
