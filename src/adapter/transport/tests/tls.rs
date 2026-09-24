use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rcgen::generate_simple_self_signed;
use tokio::io::AsyncReadExt;
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

use crate::core::{ErrorKind, TimeoutPhase};

use super::fixture;

fn ephemeral_acceptor() -> TlsAcceptor {
    let certified = generate_simple_self_signed(vec!["localhost".to_owned()])
        .unwrap_or_else(|_| panic!("certificate"));
    let certificate = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap_or_else(|_| panic!("server config"));
    TlsAcceptor::from(Arc::new(config))
}

fn error_of(
    result: Result<super::super::TransportResponse, crate::core::GatewayError>,
) -> crate::core::GatewayError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("unexpected transport success"),
    }
}

#[tokio::test]
async fn native_verifier_rejects_an_ephemeral_self_signed_certificate() {
    let (listener, address) = fixture::listener().await;
    let acceptor = ephemeral_acceptor();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap_or_else(|_| panic!("accept"));
        let _ = acceptor.accept(socket).await;
    });
    let error = error_of(
        fixture::https_transport(
            address,
            fixture::timeouts(
                Duration::from_secs(5),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
        )
        .execute(fixture::request(Bytes::from_static(b"{}"), 1024))
        .await,
    );
    assert_eq!(error.kind, ErrorKind::UpstreamUnavailable);
    server.await.unwrap_or_else(|_| panic!("server"));
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_tls_handshake_times_out_as_connect_and_closes_socket() {
    let (listener, address) = fixture::listener().await;
    let (accepted_tx, accepted_rx) = fixture::signal_pair();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap_or_else(|_| panic!("accept"));
        let mut byte = [0_u8; 1];
        socket
            .read_exact(&mut byte)
            .await
            .unwrap_or_else(|_| panic!("client hello"));
        accepted_tx
            .send(())
            .unwrap_or_else(|_| panic!("accepted signal"));
        fixture::wait_for_close(socket)
            .await
            .unwrap_or_else(|_| panic!("close"));
        closed_tx
            .send(())
            .unwrap_or_else(|_| panic!("close signal"));
    });

    let transport = fixture::https_transport(
        address,
        fixture::timeouts(
            Duration::from_secs(5),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    );
    let mut execute =
        Box::pin(transport.execute(fixture::request(Bytes::from_static(b"{}"), 1024)));
    tokio::select! {
        result = &mut execute => match result {
            Err(error) => panic!("handshake unexpectedly failed: {:?}", error.kind),
            Ok(_) => panic!("handshake unexpectedly completed"),
        },
        result = accepted_rx => result.unwrap_or_else(|_| panic!("accepted signal")),
    }
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
    let (listener, address) = fixture::listener().await;
    let (accepted_tx, accepted_rx) = fixture::signal_pair();
    let (closed_tx, closed_rx) = fixture::signal_pair();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap_or_else(|_| panic!("accept"));
        let mut byte = [0_u8; 1];
        socket
            .read_exact(&mut byte)
            .await
            .unwrap_or_else(|_| panic!("client hello"));
        accepted_tx
            .send(())
            .unwrap_or_else(|_| panic!("accepted signal"));
        fixture::wait_for_close(socket)
            .await
            .unwrap_or_else(|_| panic!("close"));
        closed_tx
            .send(())
            .unwrap_or_else(|_| panic!("close signal"));
    });

    let transport = fixture::https_transport(
        address,
        fixture::timeouts(
            Duration::from_secs(5),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    );
    let mut execute =
        Box::pin(transport.execute(fixture::request(Bytes::from_static(b"{}"), 1024)));
    tokio::select! {
        _result = &mut execute => panic!("handshake unexpectedly completed"),
        result = accepted_rx => result.unwrap_or_else(|_| panic!("accepted signal")),
    }
    drop(execute);
    fixture::bounded_wait(closed_rx).await;
    server.await.unwrap_or_else(|_| panic!("server"));
}
