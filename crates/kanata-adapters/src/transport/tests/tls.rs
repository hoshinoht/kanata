use std::{pin::Pin, sync::Arc, time::Duration};

use bytes::Bytes;
use tokio::{io::AsyncReadExt, sync::oneshot, task::JoinHandle};

use crate::core::{ErrorKind, GatewayError, TimeoutPhase};

use super::super::{Transport, TransportResponse};
use super::fixture::{self, SECOND};

type Execution = Pin<Box<dyn Future<Output = Result<TransportResponse, GatewayError>>>>;

fn https(address: String) -> Arc<Transport> {
    Arc::new(fixture::https_transport(
        address,
        fixture::timeouts(Duration::from_secs(5), SECOND, SECOND, SECOND),
    ))
}

fn execution(transport: &Arc<Transport>) -> Execution {
    let transport = transport.clone();
    Box::pin(async move {
        transport
            .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
            .await
    })
}

/// Starts an HTTPS request against a server that reads the client hello and never answers.
/// Returns once the hello arrived; the transport is returned so it outlives the request.
async fn stalled_handshake() -> (
    Arc<Transport>,
    Execution,
    oneshot::Receiver<()>,
    JoinHandle<()>,
) {
    let (listener, address) = fixture::listener().await;
    let (accepted_tx, accepted_rx) = fixture::signal_pair();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = fixture::server_task(listener, |mut socket| async move {
        let mut byte = [0_u8; 1];
        socket
            .read_exact(&mut byte)
            .await
            .unwrap_or_else(|_| panic!("client hello"));
        accepted_tx
            .send(())
            .unwrap_or_else(|_| panic!("accepted signal"));
        fixture::signal_close(socket, closed_tx).await;
    });

    let transport = https(address);
    let mut execute = execution(&transport);
    tokio::select! {
        result = &mut execute => match result {
            Err(error) => panic!("handshake unexpectedly failed: {:?}", error.kind),
            Ok(_) => panic!("handshake unexpectedly completed"),
        },
        result = accepted_rx => result.unwrap_or_else(|_| panic!("accepted signal")),
    }
    (transport, execute, closed_rx, server)
}

#[tokio::test]
async fn native_verifier_rejects_an_ephemeral_self_signed_certificate() {
    let (listener, address) = fixture::listener().await;
    let (_, acceptor) = crate::test_support::tls_fixture("localhost");
    let server = fixture::server_task(listener, |socket| async move {
        let _ = acceptor.accept(socket).await;
    });
    let error = match execution(&https(address)).await {
        Err(error) => error,
        Ok(_) => panic!("unexpected transport success"),
    };
    assert_eq!(error.kind, ErrorKind::UpstreamUnavailable);
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_tls_handshake_times_out_as_connect_and_closes_socket() {
    let (_transport, execute, closed_rx, server) = stalled_handshake().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5)).await;
    let error = match execute.await {
        Err(error) => error,
        Ok(_) => panic!("missing connect timeout"),
    };
    assert_eq!(
        error.kind,
        ErrorKind::Timeout {
            phase: TimeoutPhase::Connect
        }
    );
    tokio::time::resume();
    fixture::bounded_wait(closed_rx).await;
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_a_stalled_tls_handshake_closes_socket() {
    let (_transport, execute, closed_rx, server) = stalled_handshake().await;
    drop(execute);
    fixture::bounded_wait(closed_rx).await;
    server.await.unwrap_or_else(|_| panic!("server"));
}
