use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kanata::{
    adapter::{Adapter, vllm::VllmAdapter},
    auth::{SecretResolutionError, SecretResolver},
    config::{self, SecretReference, ValidatedConfig},
    core::{ErrorKind, Operation, TrustZone},
};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

use crate::support::{
    RequestRecord, TEXT_RESPONSE, config_text_at, error_kind, expected_text, read_request,
    routed_with_zone, take_chat, text_request, write_response,
};

static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

struct Resolver;

impl SecretResolver for Resolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        match reference {
            SecretReference::Env(name) if name == "VLLM_KEY" => Ok(b"fixture-vllm-key".to_vec()),
            _ => Err(SecretResolutionError),
        }
    }
}

/// A file in the temp directory, removed on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn new(contents: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "kanata-ca-file-{}-{}.pem",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, contents).unwrap_or_else(|_| panic!("write temp file"));
        Self(path)
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap_or_else(|| panic!("utf-8 temp path"))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn pem(label: &str, der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_else(|_| panic!("base64")));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

struct Ca {
    issuer: Issuer<'static, KeyPair>,
    pem: String,
}

fn ca(name: &str) -> Ca {
    let mut params =
        CertificateParams::new(Vec::<String>::new()).unwrap_or_else(|_| panic!("ca params"));
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let key = KeyPair::generate().unwrap_or_else(|_| panic!("ca key"));
    let certificate = params
        .self_signed(&key)
        .unwrap_or_else(|_| panic!("ca certificate"));
    Ca {
        pem: pem("CERTIFICATE", certificate.der()),
        issuer: Issuer::new(params, key),
    }
}

/// A TLS acceptor whose leaf certificate, issued by `ca`, names only `names`.
fn acceptor(ca: &Ca, names: &[&str]) -> TlsAcceptor {
    let mut params = CertificateParams::new(
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| panic!("leaf params"));
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "kanata-test-upstream");
    let key = KeyPair::generate().unwrap_or_else(|_| panic!("leaf key"));
    let certificate = params
        .signed_by(&key, &ca.issuer)
        .unwrap_or_else(|_| panic!("leaf certificate"));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap_or_else(|_| panic!("server config"));
    TlsAcceptor::from(Arc::new(config))
}

/// Serves one TLS connection; `None` when the client abandons the handshake.
async fn tls_upstream(acceptor: TlsAcceptor) -> (String, JoinHandle<Option<RequestRecord>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("listener"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("address"))
        .to_string();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.ok()?;
        let mut stream = acceptor.accept(socket).await.ok()?;
        let record = read_request(&mut stream).await.ok()?;
        write_response(
            &mut stream,
            200,
            "application/json",
            TEXT_RESPONSE.as_bytes(),
        )
        .await
        .ok()?;
        Some(record)
    });
    (address, task)
}

/// An external vLLM adapter at `base_url` with a bearer key and an optional `ca_file`.
fn config_text(base_url: &str, ca_file: Option<&str>) -> String {
    let text = config_text_at(base_url, "external", Operation::Chat, false, false, true);
    match ca_file {
        Some(path) => text.replace(
            "secret_ref = \"env:VLLM_KEY\"\n",
            &format!("secret_ref = \"env:VLLM_KEY\"\nca_file = \"{path}\"\n"),
        ),
        None => text,
    }
}

fn load(contents: &str) -> Result<ValidatedConfig, String> {
    crate::scratch::load_text(contents).map_err(|error| error.to_string())
}

async fn call(
    config: &ValidatedConfig,
) -> Result<kanata::adapter::AdapterOutput, kanata::core::GatewayError> {
    let adapter =
        VllmAdapter::from_config_with_secrets(config, "vllm-fixture", "vllm-chat", &Resolver)
            .unwrap_or_else(|error| panic!("adapter: {error:?}"));
    adapter
        .execute(routed_with_zone(
            config,
            text_request(),
            TrustZone::External,
        ))
        .await
}

#[tokio::test]
async fn private_ca_connects_and_carries_the_bearer_key() {
    let trusted = ca("Kanata test CA");
    let bundle = TempFile::new(trusted.pem.as_bytes());
    let (address, upstream) = tls_upstream(acceptor(&trusted, &["127.0.0.1"])).await;
    let config = load(&config_text(
        &format!("https://{address}/v1"),
        Some(bundle.path()),
    ))
    .unwrap_or_else(|error| panic!("{error}"));

    let response = take_chat(call(&config).await);
    assert_eq!(response.message.content, expected_text());
    let record = upstream
        .await
        .unwrap_or_else(|_| panic!("upstream task"))
        .unwrap_or_else(|| panic!("upstream saw the request"));
    assert_eq!(record.path, "/v1/chat/completions");
    assert_eq!(record.headers["authorization"], "Bearer fixture-vllm-key");
}

#[tokio::test]
async fn the_platform_roots_a_wrong_ca_or_a_wrong_name_fail_before_any_request() {
    let trusted = ca("Kanata test CA");
    let wrong = TempFile::new(ca("Unrelated CA").pem.as_bytes());
    let right = TempFile::new(trusted.pem.as_bytes());
    for (name, names, ca_file) in [
        ("platform roots", &["127.0.0.1"][..], None),
        ("wrong CA", &["127.0.0.1"][..], Some(wrong.path())),
        // A DNS name does not cover an IP-literal base_url.
        ("hostname mismatch", &["localhost"][..], Some(right.path())),
    ] {
        let (address, upstream) = tls_upstream(acceptor(&trusted, names)).await;
        let config = load(&config_text(&format!("https://{address}/v1"), ca_file))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            error_kind(call(&config).await),
            ErrorKind::UpstreamUnavailable,
            "{name}"
        );
        assert!(
            upstream
                .await
                .unwrap_or_else(|_| panic!("upstream task"))
                .is_none(),
            "{name}: request reached the upstream"
        );
    }
}

#[test]
fn ca_file_is_validated_when_the_config_loads() {
    let first = ca("First CA");
    let second = ca("Second CA");
    let bundle = TempFile::new(format!("{}{}", first.pem, second.pem).as_bytes());
    let config = load(&config_text("https://vllm.invalid/v1", Some(bundle.path())))
        .unwrap_or_else(|error| panic!("{error}"));
    let loaded = config.adapters()[0]
        .ca_bundle()
        .unwrap_or_else(|| panic!("bundle"));
    assert_eq!(loaded.path(), bundle.0.as_path());
    assert!(format!("{loaded:?}").contains("certificates: 2"));

    let empty = TempFile::new(b"");
    let garbage = TempFile::new(b"not a certificate\n");
    let bad_base64 =
        TempFile::new(b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n");
    let not_der = TempFile::new(pem("CERTIFICATE", b"not der").as_bytes());
    let key = TempFile::new(format!("{}{}", first.pem, pem("PRIVATE KEY", b"key")).as_bytes());
    let large = TempFile::new(first.pem.repeat(64 * 1024 / first.pem.len() + 1).as_bytes());
    let missing = std::env::temp_dir().join("kanata-ca-file-missing.pem");
    let missing = missing.to_str().unwrap_or_else(|| panic!("utf-8 path"));
    for (path, class) in [
        (missing, "read_error"),
        (empty.path(), "no_certificates"),
        (garbage.path(), "no_certificates"),
        (bad_base64.path(), "invalid_pem"),
        (not_der.path(), "invalid_certificate"),
        (key.path(), "certificates_only"),
        (large.path(), "too_large"),
        ("relative/ca.pem", "absolute_path_required"),
        ("", "empty"),
    ] {
        assert_eq!(
            load(&config_text("https://vllm.invalid/v1", Some(path))).unwrap_err(),
            format!("config error at adapters[0].ca_file: {class}"),
            "{path}"
        );
    }

    let http = config_text_at(
        "http://vllm.invalid/v1",
        "local",
        Operation::Chat,
        false,
        false,
        false,
    )
    .replace(
        "trust_zone = \"local\"\n",
        &format!("trust_zone = \"local\"\nca_file = \"{}\"\n", bundle.path()),
    );
    assert_eq!(
        load(&http).unwrap_err(),
        "config error at adapters[0].ca_file: https_base_url_required"
    );

    // Pinned or plain-HTTP origins cannot take a private CA.
    for kind in ["chatgpt", "apple_fm"] {
        let text = config_text("https://api.openai.com/v1", Some(bundle.path()))
            .replace("kind = \"vllm\"", &format!("kind = \"{kind}\""));
        assert_eq!(
            load(&text).unwrap_err(),
            "config error at adapters[0].ca_file: unsupported_by_adapter_kind",
            "{kind}"
        );
    }
}

#[test]
fn deferred_loading_skips_the_file_but_never_falls_back_to_platform_roots() {
    let missing = std::env::temp_dir().join("kanata-ca-file-deferred-missing.pem");
    let text = config_text(
        "https://vllm.invalid/v1",
        Some(missing.to_str().unwrap_or_else(|| panic!("utf-8 path"))),
    );
    let dir = crate::scratch::config_dir("ca-file", &text);
    let config = config::load_deferring_keys(dir.join("config.toml"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(config.adapters()[0].ca_bundle().is_some());
    assert!(
        VllmAdapter::from_config_with_secrets(&config, "vllm-fixture", "vllm-chat", &Resolver)
            .is_err()
    );
}
