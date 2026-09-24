use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use http::Method;
use serde_json::json;

use crate::{
    config::{self, ValidatedConfig},
    core::ErrorKind,
};

use super::super::{
    Accept, COMPLETE_RESPONSE_BYTES, CredentialHeader, Endpoint, Transport, TransportRequest,
};
use super::super::{origin::Origin, request::build_request};

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

fn load_config(replacements: &[(&str, &str)]) -> Result<ValidatedConfig, config::ConfigError> {
    let mut contents = include_str!("../../../../tests/fixtures/config/example.toml").to_owned();
    for (from, to) in replacements {
        contents = contents.replace(from, to);
    }
    let id = NEXT_CONFIG.fetch_add(1, Ordering::Relaxed);
    let path: PathBuf = std::env::temp_dir().join(format!(
        "kanata-transport-origin-{}-{id}.toml",
        std::process::id()
    ));
    fs::write(&path, contents).unwrap_or_else(|_| panic!("write config"));
    let result = config::load(&path);
    let _ = fs::remove_file(path);
    result
}

fn endpoint() -> Endpoint {
    Endpoint::new(&["v1", "chat"]).unwrap_or_else(|_| panic!("endpoint"))
}

#[test]
fn validated_local_private_and_external_origins_keep_their_boundary() {
    let config = load_config(&[]).unwrap_or_else(|_| panic!("config"));
    let local = Origin::from_adapter(&config.adapters()[0]).unwrap_or_else(|_| panic!("local"));
    assert_eq!(
        local
            .uri(&endpoint())
            .unwrap_or_else(|_| panic!("local uri"))
            .to_string(),
        "http://ollama.invalid:11434/v1/chat"
    );
    assert!(!local.permits_credentials());

    let private = Origin::from_adapter(&config.adapters()[1]).unwrap_or_else(|_| panic!("private"));
    assert!(private.uri(&endpoint()).is_ok());
    assert!(!private.permits_credentials());

    let external =
        Origin::from_adapter(&config.adapters()[2]).unwrap_or_else(|_| panic!("external"));
    assert!(
        external
            .uri(&endpoint())
            .unwrap_or_else(|_| panic!("external uri"))
            .to_string()
            .starts_with("https://")
    );
    assert!(external.permits_credentials());
}

#[test]
fn origin_prefix_and_ipv6_authority_are_serialized_without_escape() {
    let config = load_config(&[(
        "http://ollama.invalid:11434",
        "http://ollama.invalid:11434/provider/",
    )])
    .unwrap_or_else(|_| panic!("prefix config"));
    let origin =
        Origin::from_adapter(&config.adapters()[0]).unwrap_or_else(|_| panic!("prefix origin"));
    assert_eq!(
        origin
            .uri(&endpoint())
            .unwrap_or_else(|_| panic!("prefix uri"))
            .to_string(),
        "http://ollama.invalid:11434/provider/v1/chat"
    );

    let config = load_config(&[("http://ollama.invalid:11434", "http://[::1]:11434/base/")])
        .unwrap_or_else(|_| panic!("ipv6 config"));
    let origin =
        Origin::from_adapter(&config.adapters()[0]).unwrap_or_else(|_| panic!("ipv6 origin"));
    assert_eq!(
        origin
            .uri(&endpoint())
            .unwrap_or_else(|_| panic!("ipv6 uri"))
            .to_string(),
        "http://[::1]:11434/base/v1/chat"
    );
}

#[test]
fn origin_rejects_external_http_and_url_metadata() {
    for replacement in [
        (
            "https://openrouter.invalid/api/v1",
            "http://openrouter.invalid/api/v1",
        ),
        (
            "http://ollama.invalid:11434",
            "http://user:secret@ollama.invalid:11434",
        ),
        (
            "http://ollama.invalid:11434",
            "http://ollama.invalid:11434?token=secret",
        ),
        (
            "http://ollama.invalid:11434",
            "http://ollama.invalid:11434#fragment",
        ),
    ] {
        assert!(load_config(&[replacement]).is_err());
    }
}

#[tokio::test]
async fn plaintext_credentials_fail_before_connecting() {
    let config = load_config(&[]).unwrap_or_else(|_| panic!("config"));
    let transport = Transport::new(
        config
            .adapters()
            .first()
            .unwrap_or_else(|| panic!("adapter")),
        config.timeouts(),
    )
    .unwrap_or_else(|_| panic!("transport"));
    let request = TransportRequest::json(
        Method::POST,
        endpoint(),
        &json!({}),
        Some(CredentialHeader::authorization("marker-secret".to_owned())),
        Some(Accept::Json),
        1024,
        COMPLETE_RESPONSE_BYTES,
    )
    .unwrap_or_else(|_| panic!("request"));
    let error = match transport.execute(request).await {
        Err(error) => error,
        Ok(_) => panic!("plaintext credential accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);

    let codex_request = TransportRequest::json(
        Method::POST,
        endpoint(),
        &json!({}),
        Some(
            CredentialHeader::authorization_with_chatgpt_account_id(
                "Bearer marker-secret".to_owned(),
                "account-marker",
            )
            .expect("Codex credential headers"),
        ),
        Some(Accept::EventStream),
        1024,
        COMPLETE_RESPONSE_BYTES,
    )
    .expect("codex request");
    let error = match transport.execute(codex_request).await {
        Err(error) => error,
        Ok(_) => panic!("plaintext account header accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);

    let form = TransportRequest::form(
        Method::POST,
        endpoint(),
        vec![("oauth_token".to_owned(), "form-secret".to_owned())],
        None,
        Some(Accept::Json),
        1024,
        COMPLETE_RESPONSE_BYTES,
    )
    .unwrap_or_else(|_| panic!("form request"));
    let error = match transport.execute(form).await {
        Err(error) => error,
        Ok(_) => panic!("plaintext form accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);
}

#[test]
fn control_bytes_in_credentials_are_internal_without_echoing_values() {
    let origin = Origin::test_http("127.0.0.1:1".to_owned());
    let request = TransportRequest::json(
        Method::POST,
        endpoint(),
        &json!({}),
        Some(CredentialHeader::authorization(
            "prompt-marker\r\nX-Leak: audio-marker".to_owned(),
        )),
        Some(Accept::Json),
        1024,
        COMPLETE_RESPONSE_BYTES,
    )
    .unwrap_or_else(|_| panic!("request"));
    let uri = origin
        .uri(&request.endpoint)
        .unwrap_or_else(|_| panic!("uri"));
    let error = match build_request(&origin, uri, request) {
        Err(error) => error,
        Ok(_) => panic!("control bytes accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);
}
