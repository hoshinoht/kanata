use axum::http::StatusCode;

use crate::support::{
    MODEL, MULTIPART_CONTENT_TYPE, Part, config_with_limits, multipart_with_file, response_body,
    server, server_from_config, transcription_request,
};

#[tokio::test]
async fn oversized_text_field_returns_413_without_dispatch() {
    let mut oversized = b"TEXT_LIMIT_MARKER".to_vec();
    oversized.resize(8193, b'x');
    let body = multipart_with_file(
        MODEL,
        Some("voice.wav"),
        Some("audio/wav"),
        b"audio",
        &[Part::Field {
            name: "prompt",
            bytes: &oversized,
        }],
    );
    let (server, requests) = server();
    let response = server
        .client_oneshot(transcription_request(
            body,
            Some("Bearer test-key"),
            Some(MULTIPART_CONTENT_TYPE),
            17,
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response_body(response).await;
    let body = String::from_utf8_lossy(&body);
    assert!(!body.contains("TEXT_LIMIT_MARKER"));
    assert!(requests.lock().expect("lock").is_empty());
}

struct LimitCase {
    name: &'static str,
    body: Vec<u8>,
    max_body: usize,
    max_audio: usize,
    expected: StatusCode,
}

#[tokio::test]
async fn audio_field_limit_is_independent_of_text_body_limit() {
    let audio_at_limit = [1_u8, 2, 3, 4];
    let audio_over_limit = [1_u8, 2, 3, 4, 5];
    let cases = vec![
        LimitCase {
            name: "audio-at-limit",
            max_body: 1,
            max_audio: audio_at_limit.len(),
            body: multipart_with_file(
                MODEL,
                Some("voice.wav"),
                Some("audio/wav"),
                &audio_at_limit,
                &[],
            ),
            expected: StatusCode::OK,
        },
        LimitCase {
            name: "audio-over-limit",
            max_body: 1,
            max_audio: audio_at_limit.len(),
            body: multipart_with_file(
                MODEL,
                Some("voice.wav"),
                Some("audio/wav"),
                &audio_over_limit,
                &[],
            ),
            expected: StatusCode::PAYLOAD_TOO_LARGE,
        },
    ];
    for case in cases {
        let config = config_with_limits(case.max_body, case.max_audio);
        let (server, requests) = server_from_config(&config);
        let response = server
            .client_oneshot(transcription_request(
                case.body,
                Some("Bearer test-key"),
                Some(MULTIPART_CONTENT_TYPE),
                13,
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), case.expected, "{}", case.name);
        let body = response_body(response).await;
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("LIMIT_MARKER"), "{} leaked", case.name);
        let calls = requests.lock().expect("lock").len();
        if case.expected == StatusCode::OK {
            assert_eq!(calls, 1, "{} call count", case.name);
        } else {
            assert_eq!(calls, 0, "{} dispatched", case.name);
        }
    }
}

#[tokio::test]
async fn over_envelope_multipart_returns_413_without_dispatch() {
    let mut body = vec![b'P'; 256 * 1024];
    body.extend_from_slice(&multipart_with_file(
        MODEL,
        Some("voice.wav"),
        Some("audio/wav"),
        b"FILE_MARKER",
        &[],
    ));
    let config = config_with_limits(1, 16);
    let (server, requests) = server_from_config(&config);

    let response = server
        .client_oneshot(transcription_request(
            body,
            Some("Bearer test-key"),
            Some(MULTIPART_CONTENT_TYPE),
            8192,
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response_body(response).await;
    assert!(!String::from_utf8_lossy(&body).contains("FILE_MARKER"));
    assert!(requests.lock().expect("lock").is_empty());
}
