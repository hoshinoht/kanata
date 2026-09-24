use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use futures_util::StreamExt;
use tokio::{io::AsyncWriteExt, sync::oneshot};

use crate::core::ErrorKind;

use super::fixture;

#[tokio::test]
async fn redirect_location_is_returned_without_following_or_replaying() {
    let (first_listener, first_address) = fixture::listener().await;
    let (second_listener, second_address) = fixture::listener().await;
    let writes = Arc::new(AtomicUsize::new(0));
    let writes_for_server = writes.clone();
    let first = fixture::server_task(first_listener, move |mut socket| async move {
        fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        writes_for_server.fetch_add(1, Ordering::SeqCst);
        let response = format!(
            "HTTP/1.1 302 Found\r\nlocation: http://{second_address}/redirected\r\ncontent-length: 0\r\n\r\n"
        );
        socket
            .write_all(response.as_bytes())
            .await
            .unwrap_or_else(|_| panic!("redirect response"));
    });
    let mut response = fixture::transport(
        first_address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    )
    .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
    .await
    .unwrap_or_else(|_| panic!("redirect response"));
    assert_eq!(response.status, 302);
    assert!(response.body.next().await.is_none());
    first.await.unwrap_or_else(|_| panic!("first server"));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), second_listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn connection_error_has_one_source_write_and_no_second_connection() {
    let (listener, address) = fixture::listener().await;
    let writes = Arc::new(AtomicUsize::new(0));
    let writes_for_server = writes.clone();
    let (done_tx, done_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap_or_else(|_| panic!("accept"));
        fixture::read_headers(&mut socket)
            .await
            .unwrap_or_else(|_| panic!("request headers"));
        writes_for_server.fetch_add(1, Ordering::SeqCst);
        drop(socket);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        done_tx.send(()).unwrap_or_else(|_| panic!("done signal"));
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
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("reset response accepted"),
    };
    assert_eq!(error.kind, ErrorKind::UpstreamFailure);
    fixture::bounded_wait(done_rx).await;
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    server.await.unwrap_or_else(|_| panic!("server"));
}
