use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::http::{HeaderValue, StatusCode, header};

use crate::support::{
    BOUNDARY, FILE_MARKER, FORMAT_MARKER, HEADER_MARKER, LANGUAGE_MARKER, MODEL, MODEL_MARKER,
    MULTIPART_CONTENT_TYPE, PROMPT_MARKER, Part, REDACTION_MARKERS, UNKNOWN_FIELD_MARKER,
    multipart_body, multipart_with_file, response_body, server, transcription_request,
    unpolled_request,
};

struct RejectionCase {
    name: &'static str,
    body: Vec<u8>,
    content_type: Option<&'static str>,
    expected: StatusCode,
}

#[tokio::test]
async fn invalid_multipart_inputs_are_rejected_without_dispatch_or_leakage() {
    let mut malformed_header = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\nX-{HEADER_MARKER}\r\n\r\n{MODEL_MARKER}\r\n--{BOUNDARY}--\r\n"
    )
    .into_bytes();
    malformed_header.extend_from_slice(b"\x00");
    let truncated = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{MODEL_MARKER}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"voice.wav\"\r\nContent-Type: audio/wav\r\n\r\nTRUNCATED_MARKER"
    )
    .into_bytes();
    let cases = vec![
        RejectionCase {
            name: "missing-file",
            body: multipart_body(&[Part::Field {
                name: "model",
                bytes: MODEL_MARKER.as_bytes(),
            }]),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "missing-model",
            body: multipart_body(&[Part::File {
                name: "file",
                filename: Some("missing-model.wav"),
                content_type: Some("audio/wav"),
                bytes: FILE_MARKER.as_bytes(),
            }]),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "duplicate-file",
            body: multipart_body(&[
                Part::Field {
                    name: "model",
                    bytes: MODEL,
                },
                Part::File {
                    name: "file",
                    filename: Some("first.wav"),
                    content_type: Some("audio/wav"),
                    bytes: b"first",
                },
                Part::File {
                    name: "file",
                    filename: Some("FILENAME_MARKER.wav"),
                    content_type: Some("audio/wav"),
                    bytes: b"second",
                },
            ]),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "duplicate-model",
            body: multipart_body(&[
                Part::Field {
                    name: "model",
                    bytes: MODEL,
                },
                Part::Field {
                    name: "model",
                    bytes: MODEL_MARKER.as_bytes(),
                },
                Part::File {
                    name: "file",
                    filename: Some("voice.wav"),
                    content_type: Some("audio/wav"),
                    bytes: b"audio",
                },
            ]),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "model-file-part-confusion",
            body: multipart_body(&[
                Part::File {
                    name: "model",
                    filename: Some("MODEL_MARKER.wav"),
                    content_type: Some("audio/wav"),
                    bytes: MODEL,
                },
                Part::File {
                    name: "file",
                    filename: Some("voice.wav"),
                    content_type: Some("audio/wav"),
                    bytes: b"audio",
                },
            ]),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "duplicate-language",
            body: multipart_with_file(
                MODEL,
                Some("voice.wav"),
                Some("audio/wav"),
                b"audio",
                &[
                    Part::Field {
                        name: "language",
                        bytes: b"en",
                    },
                    Part::Field {
                        name: "language",
                        bytes: LANGUAGE_MARKER.as_bytes(),
                    },
                ],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "unknown-field",
            body: multipart_with_file(
                MODEL,
                Some("voice.wav"),
                Some("audio/wav"),
                b"audio",
                &[Part::Field {
                    name: UNKNOWN_FIELD_MARKER,
                    bytes: b"unknown",
                }],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "non-utf8-model",
            body: multipart_with_file(
                &[0xff, 0xfe],
                Some("voice.wav"),
                Some("audio/wav"),
                b"audio",
                &[],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "empty-file",
            body: multipart_with_file(
                MODEL,
                Some("EMPTY_FILE_MARKER.wav"),
                Some("audio/wav"),
                b"",
                &[],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "remote-file-string-without-filename",
            body: multipart_with_file(MODEL, None, Some("audio/wav"), b"FILE_MARKER", &[]),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "unsupported-output-format",
            body: multipart_with_file(
                MODEL,
                Some("voice.wav"),
                Some("audio/wav"),
                b"audio",
                &[Part::Field {
                    name: "response_format",
                    bytes: FORMAT_MARKER.as_bytes(),
                }],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "selector-permission-mismatch",
            body: multipart_with_file(
                b"private-chat",
                Some("voice.wav"),
                Some("audio/wav"),
                b"audio",
                &[Part::Field {
                    name: "prompt",
                    bytes: PROMPT_MARKER.as_bytes(),
                }],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::FORBIDDEN,
        },
        RejectionCase {
            name: "unknown-exact-route",
            body: multipart_with_file(
                b"UNKNOWN_ROUTE_MARKER",
                Some("voice.wav"),
                Some("audio/wav"),
                b"audio",
                &[],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::FORBIDDEN,
        },
        RejectionCase {
            name: "text-plain-mime",
            body: multipart_with_file(
                MODEL,
                Some("FILENAME_MARKER.wav"),
                Some("text/plain"),
                b"FILE_MARKER",
                &[],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "missing-file-mime",
            body: multipart_with_file(MODEL, Some("FILENAME_MARKER.wav"), None, b"audio", &[]),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "filename-traversal",
            body: multipart_with_file(
                MODEL,
                Some("../secret"),
                Some("audio/wav"),
                b"FILENAME_MARKER",
                &[],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "filename-forward-slash",
            body: multipart_with_file(
                MODEL,
                Some("nested/secret.wav"),
                Some("audio/wav"),
                b"FILENAME_MARKER",
                &[],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "filename-backslash",
            body: multipart_with_file(
                MODEL,
                Some("nested\\secret.wav"),
                Some("audio/wav"),
                b"FILENAME_MARKER",
                &[],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "filename-control",
            body: multipart_with_file(
                MODEL,
                Some("voice\tFILENAME_MARKER.wav"),
                Some("audio/wav"),
                b"audio",
                &[],
            ),
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "truncated-multipart",
            body: truncated,
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "malformed-header",
            body: malformed_header,
            content_type: Some(MULTIPART_CONTENT_TYPE),
            expected: StatusCode::BAD_REQUEST,
        },
        RejectionCase {
            name: "missing-boundary",
            body: multipart_with_file(
                MODEL_MARKER.as_bytes(),
                Some("voice.wav"),
                Some("audio/wav"),
                b"audio",
                &[],
            ),
            content_type: Some("multipart/form-data"),
            expected: StatusCode::BAD_REQUEST,
        },
    ];
    let (server, requests) = server();
    for case in cases {
        let response = server
            .client_oneshot(transcription_request(
                case.body,
                Some("Bearer test-key"),
                case.content_type,
                11,
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), case.expected, "{}", case.name);
        let body = response_body(response).await;
        let body = String::from_utf8_lossy(&body);
        for marker in REDACTION_MARKERS {
            assert!(!body.contains(marker), "{} leaked {marker}", case.name);
        }
        assert_eq!(
            requests.lock().expect("lock").len(),
            0,
            "{} dispatched",
            case.name
        );
    }
}

#[tokio::test]
async fn duplicate_content_type_headers_are_rejected_before_dispatch() {
    let cases = [
        ("identical", MULTIPART_CONTENT_TYPE, MULTIPART_CONTENT_TYPE),
        (
            "valid-then-invalid",
            MULTIPART_CONTENT_TYPE,
            "multipart/form-data",
        ),
        (
            "invalid-then-valid",
            "multipart/form-data",
            MULTIPART_CONTENT_TYPE,
        ),
    ];
    let (server, requests) = server();
    for (name, first, second) in cases {
        let mut request = transcription_request(
            multipart_with_file(MODEL, Some("voice.wav"), Some("audio/wav"), b"audio", &[]),
            Some("Bearer test-key"),
            Some(first),
            11,
        );
        request
            .headers_mut()
            .append(header::CONTENT_TYPE, HeaderValue::from_static(second));
        let response = server.client_oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        assert!(
            requests.lock().expect("lock").is_empty(),
            "{name} dispatched"
        );
    }
}

#[tokio::test]
async fn authentication_rejects_before_polling_multipart_body() {
    let (server, requests) = server();
    let mut expected_body = None;
    for authorization in [None, Some("Bearer wrong-key"), Some("Basic wrong-key")] {
        let polled = Arc::new(AtomicBool::new(false));
        let response = server
            .client_oneshot(unpolled_request(polled.clone(), authorization))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            "Bearer realm=\"kanata\""
        );
        let body = response_body(response).await;
        if let Some(expected_body) = &expected_body {
            assert_eq!(&body, expected_body);
        } else {
            expected_body = Some(body);
        }
        assert!(
            !polled.load(Ordering::SeqCst),
            "body polled for {authorization:?}"
        );
        assert!(requests.lock().expect("lock").is_empty());
    }
}
