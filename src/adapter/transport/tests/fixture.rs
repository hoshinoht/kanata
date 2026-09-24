use std::time::Duration;

use bytes::Bytes;
use http::Method;
use serde_json::Value;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

use super::super::origin::Origin;
use super::super::time::Timeouts;
use super::super::{Accept, COMPLETE_RESPONSE_BYTES, Endpoint, Transport, TransportRequest};

pub(super) async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("listener"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("address"))
        .to_string();
    (listener, address)
}

pub(super) fn transport(address: String, timeouts: Timeouts) -> Transport {
    Transport {
        origin: Origin::test_http(address),
        connector: super::super::test_connector(),
        timeouts,
    }
}

pub(super) fn https_transport(address: String, timeouts: Timeouts) -> Transport {
    Transport {
        origin: Origin::test_https(address),
        connector: super::super::test_connector(),
        timeouts,
    }
}

pub(super) fn timeouts(
    connect: Duration,
    headers: Duration,
    first_byte: Duration,
    idle: Duration,
) -> Timeouts {
    Timeouts {
        connect,
        headers,
        first_byte,
        idle,
    }
}

pub(super) fn request(body: Bytes, request_budget: usize) -> TransportRequest {
    request_with_response_budget(body, request_budget, COMPLETE_RESPONSE_BYTES)
}

pub(super) fn request_with_response_budget(
    body: Bytes,
    request_budget: usize,
    response_budget: usize,
) -> TransportRequest {
    let endpoint = Endpoint::new(&["v1", "chat"]).unwrap_or_else(|_| panic!("endpoint"));
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| panic!("json body"));
    TransportRequest::json_for_test(
        Method::POST,
        endpoint,
        &value,
        None,
        Some(Accept::Json),
        request_budget,
        response_budget,
    )
    .unwrap_or_else(|_| panic!("request"))
}

pub(super) async fn read_headers(socket: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 256];
        let count = socket.read(&mut chunk).await?;
        if count == 0 {
            return Ok(request);
        }
        request.extend_from_slice(&chunk[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request);
        }
        if request.len() > 64 * 1024 {
            return Err(std::io::Error::other("request headers too large"));
        }
    }
}

pub(super) async fn wait_for_close(mut socket: TcpStream) -> std::io::Result<()> {
    let mut buffer = [0_u8; 1024];
    loop {
        if socket.read(&mut buffer).await? == 0 {
            return Ok(());
        }
    }
}

pub(super) fn signal_pair() -> (oneshot::Sender<()>, oneshot::Receiver<()>) {
    oneshot::channel()
}

pub(super) async fn bounded_wait(receiver: oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(2), receiver)
        .await
        .unwrap_or_else(|_| panic!("fixture timeout"))
        .unwrap_or_else(|_| panic!("fixture signal"));
}

pub(super) fn server_task<F, Fut>(listener: TcpListener, handler: F) -> JoinHandle<()>
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap_or_else(|_| panic!("accept"));
        handler(socket).await;
    })
}

pub(super) fn assert_request_headers(request: &[u8], address: &str) {
    let request = std::str::from_utf8(request).unwrap_or_else(|_| panic!("request utf8"));
    assert!(request.starts_with("POST /provider/v1/chat HTTP/1.1\r\n"));
    assert!(request.contains("connection: close\r\n"));
    assert!(request.contains(&format!("host: {address}\r\n")));
}
