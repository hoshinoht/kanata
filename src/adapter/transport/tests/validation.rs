use bytes::Bytes;
use http::{HeaderMap, Method};
use http_body::Frame;
use serde_json::json;

use crate::core::ErrorKind;

use super::super::body::data_frame;
use super::super::origin::Origin;
use super::super::request::build_request;
use super::super::{Accept, COMPLETE_RESPONSE_BYTES, CredentialHeader, Endpoint, TransportRequest};

#[test]
fn empty_data_and_trailer_frames_do_not_reset_body_data_state() {
    assert!(data_frame(Frame::data(Bytes::new())).is_none());
    assert!(data_frame(Frame::trailers(HeaderMap::new())).is_none());
    assert_eq!(
        data_frame(Frame::data(Bytes::from_static(b"data"))),
        Some(Bytes::from_static(b"data"))
    );
}

#[test]
fn endpoint_segments_are_ascii_path_segments_only() {
    for segments in [&["a"][..], &["A0"][..], &["with-dash_under.score"][..]] {
        assert!(Endpoint::new(segments).is_ok());
    }
    for segments in [
        &[][..],
        &[""][..],
        &["."][..],
        &[".."][..],
        &["a/b"][..],
        &["a\\b"][..],
        &["%2f"][..],
        &["a?b"][..],
        &["a#b"][..],
        &["é"][..],
    ] {
        let error = match Endpoint::new(segments) {
            Err(error) => error,
            Ok(_) => panic!("invalid endpoint accepted"),
        };
        assert_eq!(error.kind, ErrorKind::Internal);
    }
}

#[test]
fn request_budget_is_finite_and_checked_before_dispatch() {
    let endpoint = Endpoint::new(&["v1", "chat"]).unwrap_or_else(|_| panic!("endpoint"));
    let error = match TransportRequest::json_for_test(
        Method::POST,
        endpoint,
        &json!({}),
        None,
        Some(Accept::Json),
        1,
        COMPLETE_RESPONSE_BYTES,
    ) {
        Err(error) => error,
        Ok(_) => panic!("oversized request accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);

    let endpoint = Endpoint::new(&["v1", "chat"]).unwrap_or_else(|_| panic!("endpoint"));
    let error = match TransportRequest::json_for_test(
        Method::POST,
        endpoint,
        &json!({}),
        None,
        Some(Accept::Json),
        0,
        COMPLETE_RESPONSE_BYTES,
    ) {
        Err(error) => error,
        Ok(_) => panic!("zero request budget accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);
}

#[test]
fn response_budget_validation_remains_explicit() {
    let endpoint = Endpoint::new(&["v1", "chat"]).unwrap_or_else(|_| panic!("endpoint"));
    let error = match TransportRequest::json(
        Method::POST,
        endpoint,
        &json!({}),
        None,
        Some(Accept::Json),
        1024,
        1023,
    ) {
        Err(error) => error,
        Ok(_) => panic!("invalid response budget accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);
}

#[test]
fn codex_account_header_is_validated_sensitive_and_not_arbitrary() {
    for value in ["", "  ", "bad\r\nX-Injected: yes", "bad\u{007f}", "é"] {
        assert!(matches!(
            CredentialHeader::authorization_with_chatgpt_account_id(
                "Bearer TEST_ONLY_ACCESS".into(),
                value,
            ),
            Err(error) if error.kind == ErrorKind::Internal
        ));
    }
    assert!(matches!(
        CredentialHeader::authorization_with_chatgpt_account_id(
            "Bearer TEST_ONLY_ACCESS".into(),
            &"x".repeat(4 * 1024 + 1),
        ),
        Err(error) if error.kind == ErrorKind::Internal
    ));

    let endpoint = Endpoint::new(&["backend-api", "codex", "responses"])
        .unwrap_or_else(|_| panic!("endpoint"));
    let request = TransportRequest::json(
        Method::POST,
        endpoint,
        &json!({"model":"fixture-model"}),
        Some(
            CredentialHeader::authorization_with_chatgpt_account_id(
                "Bearer TEST_ONLY_ACCESS".into(),
                "TEST_ONLY_ACCOUNT_ID",
            )
            .expect("Codex credential headers"),
        ),
        Some(Accept::EventStream),
        1024,
        COMPLETE_RESPONSE_BYTES,
    )
    .expect("request");
    let origin = Origin::pinned_https("chatgpt.com").expect("pinned origin");
    let uri = origin.uri(&request.endpoint).expect("uri");
    let request = build_request(&origin, uri, request).expect("request headers");

    assert_eq!(
        request.headers()["authorization"],
        "Bearer TEST_ONLY_ACCESS"
    );
    assert_eq!(
        request.headers()["chatgpt-account-id"],
        "TEST_ONLY_ACCOUNT_ID"
    );
    assert!(request.headers()["authorization"].is_sensitive());
    assert!(request.headers()["chatgpt-account-id"].is_sensitive());
    assert!(
        request.headers()["user-agent"]
            .to_str()
            .is_ok_and(|v| v.starts_with("kanata/"))
    );
    assert_eq!(request.headers().len(), 9);
}
