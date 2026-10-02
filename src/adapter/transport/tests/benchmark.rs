use super::fixture;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[tokio::test]
#[ignore = "manual loopback transport benchmark"]
async fn connection_reuse_benchmark() {
    let (listener, address) = fixture::listener().await;
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    let server = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.expect("accept");
            count.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    |request: http::Request<hyper::body::Incoming>| async move {
                        request.into_body().collect().await.expect("request body");
                        Ok::<_, Infallible>(
                            http::Response::builder()
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from_static(b"{}")))
                                .expect("response"),
                        )
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    let transport = fixture::transport(
        address.clone(),
        fixture::timeouts(
            Duration::from_secs(3),
            Duration::from_secs(3),
            Duration::from_secs(3),
            Duration::from_secs(3),
        ),
    );
    let mut fresh = Vec::new();
    for _ in 0..200 {
        let at = Instant::now();
        let mut response = transport
            .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
            .await
            .expect("response");
        while let Some(chunk) = response.body.next().await {
            chunk.expect("body");
        }
        fresh.push(at.elapsed());
    }
    assert_eq!(connections.load(Ordering::Relaxed), 200);
    let socket = tokio::net::TcpStream::connect(&address)
        .await
        .expect("connect");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .expect("handshake");
    let driver = tokio::spawn(connection);
    let mut reused = Vec::new();
    for _ in 0..200 {
        let at = Instant::now();
        let request = http::Request::builder()
            .method("POST")
            .uri("/v1/chat")
            .header("host", &address)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from_static(b"{}")))
            .expect("request");
        sender
            .send_request(request)
            .await
            .expect("response")
            .into_body()
            .collect()
            .await
            .expect("body");
        reused.push(at.elapsed());
    }
    assert_eq!(connections.load(Ordering::Relaxed), 201);
    for (name, mut samples) in [
        ("current transport", fresh),
        ("single-connection prototype", reused),
    ] {
        samples.sort();
        eprintln!(
            "{name}: n=200 p50={}us p95={}us",
            samples[100].as_micros(),
            samples[190].as_micros()
        );
    }
    drop(sender);
    driver.abort();
    server.abort();
}
