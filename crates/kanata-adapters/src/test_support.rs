//! Shared loopback HTTPS fixtures for in-crate adapter tests.

use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::TcpListener,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

use crate::config::{self, ValidatedConfig};

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct RequestRecord {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) body: Vec<u8>,
}

/// A self-signed certificate for `hostname` and an acceptor serving it.
pub(crate) fn tls_fixture(hostname: &str) -> (CertificateDer<'static>, TlsAcceptor) {
    let certified =
        rcgen::generate_simple_self_signed(vec![hostname.to_owned()]).expect("fixture certificate");
    let certificate = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)
        .expect("fixture TLS config");
    (certificate, TlsAcceptor::from(Arc::new(server_config)))
}

pub(crate) async fn loopback_listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture listener");
    let address = listener.local_addr().expect("fixture address");
    (listener, address)
}

/// Loads config text through a unique temporary file that is removed afterwards.
pub(crate) fn load_config(contents: &str) -> ValidatedConfig {
    let path = std::env::temp_dir().join(format!(
        "kanata-unit-config-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, contents).expect("write fixture config");
    let config = config::load(&path);
    let _ = std::fs::remove_file(path);
    config.unwrap_or_else(|error| panic!("fixture config: {error}"))
}

/// Reads one `content-length` framed HTTP/1.1 request.
pub(crate) async fn read_request<S: AsyncRead + Unpin>(
    socket: &mut S,
) -> io::Result<RequestRecord> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let mut bytes = Vec::new();
    let head_end = loop {
        let mut chunk = [0_u8; 2048];
        let count = socket.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request head"));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() > 64 * 1024 {
            return Err(invalid("request headers"));
        }
    };
    let head =
        std::str::from_utf8(&bytes[..head_end - 4]).map_err(|_| invalid("request headers"))?;
    let mut lines = head.split("\r\n");
    let mut parts = lines
        .next()
        .ok_or_else(|| invalid("request line"))?
        .split_whitespace();
    let method = parts.next().ok_or_else(|| invalid("method"))?.to_owned();
    let path = parts.next().ok_or_else(|| invalid("path"))?.to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(|| invalid("header"))?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers
        .get("content-length")
        .ok_or_else(|| invalid("content length"))?
        .parse::<usize>()
        .map_err(|_| invalid("content length"))?;
    let mut body = bytes.split_off(head_end);
    if body.len() > length {
        return Err(invalid("request body"));
    }
    let already_read = body.len();
    body.resize(length, 0);
    if already_read < length {
        socket.read_exact(&mut body[already_read..]).await?;
    }
    Ok(RequestRecord {
        method,
        path,
        headers,
        body,
    })
}

/// Drains `socket` until the peer closes it or the read fails.
pub(crate) async fn wait_for_close<S: AsyncRead + Unpin>(socket: &mut S) {
    let mut buffer = [0_u8; 1024];
    while let Ok(1..) = socket.read(&mut buffer).await {}
}
