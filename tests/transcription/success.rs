use std::sync::{Arc, Mutex};

use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode, header},
};
use futures_util::stream;
use kanata::{
    adapter::Adapter,
    config::load,
    core::{Capabilities, Operation, Request as CoreRequest},
    server::{Readiness, TwoPlaneServer},
};

use crate::support::{
    AUDIO, MODEL, MULTIPART_CONTENT_TYPE, Part, RecordingAdapter, Resolver, multipart_with_file,
    public_config, response_body, server_from_config, transcription_request,
};

#[tokio::test]
async fn multipart_preserves_fragmented_binary_file_and_exact_route() {
    let config = load("tests/fixtures/config/example.toml").expect("config");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let adapter: Arc<dyn Adapter> = Arc::new(RecordingAdapter {
        requests: requests.clone(),
        capabilities: Capabilities {
            operations: [Operation::Chat, Operation::Transcription]
                .into_iter()
                .collect(),
            streaming_chat: true,
            function_tools: true,
            ..Capabilities::default()
        },
    });
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &Resolver,
        Readiness::new(true),
        vec![adapter],
    )
    .expect("server");
    let boundary = "quoted-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(b"--quoted-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nprivate-transcribe\r\n");
    body.extend_from_slice(b"--quoted-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"voice.wav\"\r\nContent-Type: audio/wav\r\n\r\n");
    let binary = [0_u8, 255, b'\r', b'\n', b'-', b'-', b'q', b'u', b'o', b't'];
    body.extend_from_slice(&binary);
    body.extend_from_slice(b"\r\n--quoted-boundary--\r\n");
    let split = body.len() / 2;
    let response = server
        .client_oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/audio/transcriptions")
                .header("authorization", "Bearer test-key")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary=\"{boundary}\""),
                )
                .body(Body::from_stream(stream::iter(vec![
                    Ok::<_, std::convert::Infallible>(Bytes::copy_from_slice(&body[..split])),
                    Ok(Bytes::copy_from_slice(&body[split..])),
                ])))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await, "{\"text\":\"transcribed\"}");
    let mut recorded = requests.lock().expect("lock");
    assert_eq!(recorded.len(), 1);
    let (context, request) = recorded.pop().expect("request").into_parts();
    assert_eq!(context.route.upstream_id, "whisper-1");
    let kanata::core::Request::Transcription(request) = request else {
        panic!("transcription")
    };
    assert_eq!(request.file.bytes(), binary);
}

#[tokio::test]
async fn multipart_accepts_audio_above_text_body_limit_without_leaking_payload() {
    const AUDIO_MARKER: &[u8] = b"TRANSCRIPTION_AUDIO_SECRET";
    let config = load("tests/fixtures/config/example.toml").expect("config");
    let (server, requests) = server_from_config(&config);
    let mut audio = vec![0xa5; 2 * 1024 * 1024];
    audio[..AUDIO_MARKER.len()].copy_from_slice(AUDIO_MARKER);

    let response = server
        .client_oneshot(transcription_request(
            multipart_with_file(MODEL, Some("voice.wav"), Some("audio/wav"), &audio, &[]),
            Some("Bearer test-key"),
            Some(MULTIPART_CONTENT_TYPE),
            8192,
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    assert_eq!(body, "{\"text\":\"transcribed\"}");
    assert!(!String::from_utf8_lossy(&body).contains("TRANSCRIPTION_AUDIO_SECRET"));

    let mut recorded = requests.lock().expect("lock");
    assert_eq!(recorded.len(), 1);
    let (context, request) = recorded.pop().expect("request").into_parts();
    assert_eq!(context.route.selector.model_alias.0, "private-transcribe");
    assert_eq!(context.route.selector.operation, Operation::Transcription);
    assert_eq!(context.route.upstream_id, "whisper-1");
    let CoreRequest::Transcription(request) = request else {
        panic!("transcription")
    };
    assert_eq!(request.file.bytes(), audio.as_slice());
}

#[tokio::test]
async fn multipart_relays_optional_fields_and_formats() {
    let config = load("tests/fixtures/config/example.toml").expect("config");
    let (server, requests) = server_from_config(&config);
    let cases = [
        (
            None,
            "application/json",
            b"{\"text\":\"transcribed\"}".as_slice(),
        ),
        (
            Some("json"),
            "application/json",
            b"{\"text\":\"transcribed\"}".as_slice(),
        ),
        (
            Some("text"),
            "text/plain; charset=utf-8",
            b"transcribed".as_slice(),
        ),
    ];
    for (response_format, content_type, expected_body) in cases {
        let mut extras = vec![
            Part::Field {
                name: "language",
                bytes: b"en",
            },
            Part::Field {
                name: "prompt",
                bytes: b"PROMPT_MARKER",
            },
        ];
        if let Some(response_format) = response_format {
            extras.push(Part::Field {
                name: "response_format",
                bytes: response_format.as_bytes(),
            });
        }
        let response = server
            .client_oneshot(transcription_request(
                multipart_with_file(MODEL, Some("voice.wav"), Some("audio/wav"), AUDIO, &extras),
                Some("Bearer test-key"),
                Some(MULTIPART_CONTENT_TYPE),
                3,
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        assert_eq!(response_body(response).await, expected_body);

        let mut recorded = requests.lock().expect("lock");
        assert_eq!(recorded.len(), 1);
        let (context, request) = recorded.pop().expect("request").into_parts();
        assert_eq!(context.route.selector.model_alias.0, "private-transcribe");
        assert_eq!(context.route.selector.operation, Operation::Transcription);
        assert_eq!(context.route.upstream_id, "whisper-1");
        let CoreRequest::Transcription(request) = request else {
            panic!("transcription")
        };
        assert_eq!(request.model.0, "private-transcribe");
        assert_eq!(request.file.file_name(), "voice.wav");
        assert_eq!(request.file.media_type(), "audio/wav");
        assert_eq!(request.file.bytes(), AUDIO);
        assert_eq!(request.language.as_deref(), Some("en"));
        assert_eq!(request.prompt.as_deref(), Some("PROMPT_MARKER"));
    }
}

#[tokio::test]
async fn all_supported_audio_mime_types_are_relayed() {
    let config = load("tests/fixtures/config/example.toml").expect("config");
    let (server, requests) = server_from_config(&config);
    for media_type in [
        "audio/flac",
        "audio/mpeg",
        "audio/mp4",
        "audio/ogg",
        "audio/wav",
        "audio/webm",
        "audio/x-wav",
    ] {
        let response = server
            .client_oneshot(transcription_request(
                multipart_with_file(MODEL, Some("voice.audio"), Some(media_type), b"audio", &[]),
                Some("Bearer test-key"),
                Some(MULTIPART_CONTENT_TYPE),
                7,
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{media_type}");
        let _ = response_body(response).await;
        let mut recorded = requests.lock().expect("lock");
        assert_eq!(recorded.len(), 1, "{media_type}");
        let (_, request) = recorded.pop().expect("request").into_parts();
        let CoreRequest::Transcription(request) = request else {
            panic!("transcription")
        };
        assert_eq!(request.file.media_type(), media_type);
    }
}

#[tokio::test]
async fn public_transcription_requires_its_independent_allowlist_entry() {
    let config = public_config("[]");
    let (server, requests) = server_from_config(&config);
    let denied = server
        .public_oneshot(transcription_request(
            multipart_with_file(MODEL, Some("voice.wav"), Some("audio/wav"), AUDIO, &[]),
            Some("Bearer test-key"),
            Some(MULTIPART_CONTENT_TYPE),
            11,
        ))
        .await
        .expect("configured public router");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(requests.lock().expect("lock").is_empty());

    let config = public_config(r#"[{ model_alias = "private-chat", operation = "chat" }]"#);
    let (server, requests) = server_from_config(&config);
    let denied = server
        .public_oneshot(transcription_request(
            multipart_with_file(MODEL, Some("voice.wav"), Some("audio/wav"), AUDIO, &[]),
            Some("Bearer test-key"),
            Some(MULTIPART_CONTENT_TYPE),
            11,
        ))
        .await
        .expect("configured public router");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(requests.lock().expect("lock").is_empty());

    let private = server
        .client_oneshot(transcription_request(
            multipart_with_file(MODEL, Some("voice.wav"), Some("audio/wav"), AUDIO, &[]),
            Some("Bearer test-key"),
            Some(MULTIPART_CONTENT_TYPE),
            11,
        ))
        .await
        .expect("private transcription response");
    assert_eq!(private.status(), StatusCode::OK);
    assert_eq!(requests.lock().expect("lock").len(), 1);

    let config =
        public_config(r#"[{ model_alias = "private-transcribe", operation = "transcription" }]"#);
    let (server, requests) = server_from_config(&config);
    let allowed = server
        .public_oneshot(transcription_request(
            multipart_with_file(MODEL, Some("voice.wav"), Some("audio/wav"), AUDIO, &[]),
            Some("Bearer test-key"),
            Some(MULTIPART_CONTENT_TYPE),
            11,
        ))
        .await
        .expect("configured public router");
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(requests.lock().expect("lock").len(), 1);
}
