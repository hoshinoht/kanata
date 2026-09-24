use axum::http::StatusCode;
use kanata::core::{Extensions, Request as CoreRequest};

use super::support::{
    EXTENSION_KEY, EXTENSIONS_JSON, Part, multipart_with_file, response_body, server,
    transcription_request,
};

#[tokio::test]
async fn multipart_extensions_are_relayed_identically_to_request_and_context() {
    let (server, requests) = server(&[EXTENSION_KEY], &[], &[EXTENSION_KEY], 8192);
    let body = multipart_with_file(&[Part::Field {
        name: "extensions",
        bytes: EXTENSIONS_JSON.as_bytes(),
    }]);
    let response = server
        .client_oneshot(transcription_request(body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_body(response).await,
        b"{\"text\":\"transcribed\"}".as_slice()
    );

    let routed = requests.lock().expect("lock").pop().expect("dispatch");
    let (context, request) = routed.into_parts();
    let CoreRequest::Transcription(request) = request else {
        panic!("transcription request");
    };
    let expected: Extensions = serde_json::from_str(EXTENSIONS_JSON).expect("extensions");
    assert_eq!(request.extensions, expected);
    assert_eq!(context.extensions, expected);
}

#[tokio::test]
async fn multipart_duplicate_or_malformed_extensions_are_rejected_without_dispatch() {
    let cases = [
        (
            vec![
                Part::Field {
                    name: "extensions",
                    bytes: EXTENSIONS_JSON.as_bytes(),
                },
                Part::Field {
                    name: "extensions",
                    bytes: EXTENSIONS_JSON.as_bytes(),
                },
            ],
            "duplicate",
        ),
        (
            vec![Part::Field {
                name: "extensions",
                bytes: b"not-json",
            }],
            "malformed",
        ),
    ];
    for (extras, name) in cases {
        let (server, requests) = server(&[EXTENSION_KEY], &[], &[EXTENSION_KEY], 8192);
        let response = server
            .client_oneshot(transcription_request(multipart_with_file(&extras)))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        assert!(
            requests.lock().expect("lock").is_empty(),
            "{name} dispatched"
        );
    }
}
