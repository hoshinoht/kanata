use std::{collections::BTreeMap, sync::atomic::Ordering, time::Duration};

use kanata::{
    adapter::{Adapter, AdapterOutput, vllm::VllmAdapter},
    core::{
        Capabilities, ErrorKind, ExtensionKey, Extensions, ModelAlias, Operation, Request,
        TranscriptionRequest, ValidatedFile,
    },
};
use serde_json::json;

use crate::support::common::MockServer;
use crate::support::{
    ConfigOptions, NATIVE_ASR_RESPONSE, TRANSCRIPTION_RESPONSE, audio_chat_native_asr_config,
    chat_request, config_for, routed, text, transcription_request,
};

fn file_request(
    alias: &str,
    filename: &str,
    media_type: &str,
    bytes: Vec<u8>,
    language: Option<String>,
    prompt: Option<String>,
) -> Request {
    Request::Transcription(TranscriptionRequest {
        model: ModelAlias(alias.to_owned()),
        file: ValidatedFile::new(filename, media_type, bytes)
            .unwrap_or_else(|_| panic!("fixture transcription file")),
        language,
        prompt,
        extensions: Extensions::default(),
    })
}

fn take_transcription(result: Result<AdapterOutput, kanata::core::GatewayError>) -> String {
    match result.unwrap_or_else(|error| panic!("adapter error: {error:?}")) {
        AdapterOutput::Complete(kanata::core::Response::Transcription(response)) => response.text,
        AdapterOutput::Complete(_) => panic!("wrong response operation"),
        AdapterOutput::Events(_) => panic!("unexpected event stream"),
    }
}

fn request_record(mock: &MockServer, index: usize) -> super::support::common::RequestRecord {
    mock.requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"))[index]
        .clone()
}

fn assert_multipart(
    record: &super::support::common::RequestRecord,
    fields: &[(&str, &str)],
    filename: &str,
    media_type: &str,
    audio: &[u8],
) {
    assert_eq!(record.method, "POST");
    assert_eq!(record.path, "/v1/audio/transcriptions");
    assert_eq!(
        record.headers.get("accept").map(String::as_str),
        Some("application/json")
    );
    assert!(!record.headers.contains_key("authorization"));
    assert_eq!(
        record.headers.get("content-length"),
        Some(&record.body.len().to_string())
    );
    let content_type = record
        .headers
        .get("content-type")
        .unwrap_or_else(|| panic!("multipart content type"));
    let boundary = content_type
        .strip_prefix("multipart/form-data; boundary=")
        .unwrap_or_else(|| panic!("multipart boundary"));
    assert!(!boundary.is_empty());

    let mut expected = Vec::new();
    for (name, value) in fields {
        expected.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    expected.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {media_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    expected.extend_from_slice(audio);
    expected.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    assert_eq!(record.body, expected);
}

#[tokio::test]
async fn audio_chat_and_native_asr_routes_stay_on_their_bound_origins() {
    let mut audio_origin = MockServer::json(TRANSCRIPTION_RESPONSE).await;
    let mut native_origin = MockServer::json(NATIVE_ASR_RESPONSE).await;
    let config = audio_chat_native_asr_config(&audio_origin.address, &native_origin.address);
    let audio_adapter =
        VllmAdapter::from_config(&config, "vllm-audio-chat", "audio-chat-transcription")
            .unwrap_or_else(|error| panic!("audio-chat route adapter: {error:?}"));
    let native_adapter =
        VllmAdapter::from_config(&config, "vllm-native-asr", "native-asr-transcription")
            .unwrap_or_else(|error| panic!("native ASR route adapter: {error:?}"));

    assert_eq!(
        native_adapter.capabilities(),
        &Capabilities::new([Operation::Transcription])
    );
    assert!(!native_adapter.capabilities().input_audio);
    assert!(!native_adapter.capabilities().streaming_chat);
    assert!(!native_adapter.capabilities().function_tools);
    assert!(!native_adapter.capabilities().audio_streaming_chat);
    assert!(!native_adapter.capabilities().audio_function_tools);
    assert!(VllmAdapter::new(&config.adapters()[1], config.timeouts(), config.limits(),).is_err());

    let audio_route_request = routed(
        &config,
        "audio-chat-transcription",
        transcription_request("audio-public", "audio/wav", vec![1, 2], None, None),
    );
    assert_eq!(
        crate::support::common::error_kind(native_adapter.execute(audio_route_request).await),
        ErrorKind::InvalidRequest
    );
    let native_route_request = routed(
        &config,
        "native-asr-transcription",
        file_request(
            "native-public",
            "voice.wav",
            "audio/wav",
            vec![3, 4],
            None,
            None,
        ),
    );
    assert_eq!(
        crate::support::common::error_kind(audio_adapter.execute(native_route_request).await),
        ErrorKind::InvalidRequest
    );
    let chat_request = routed(
        &config,
        "audio-chat-chat",
        chat_request("audio-public", vec![text("not supported by native ASR")]),
    );
    assert_eq!(
        crate::support::common::error_kind(native_adapter.execute(chat_request).await),
        ErrorKind::UnsupportedOperation
    );
    assert_eq!(audio_origin.calls.load(Ordering::SeqCst), 0);
    assert_eq!(native_origin.calls.load(Ordering::SeqCst), 0);

    assert_eq!(
        take_transcription(
            audio_adapter
                .execute(routed(
                    &config,
                    "audio-chat-transcription",
                    transcription_request("audio-public", "audio/wav", vec![5, 6], None, None),
                ))
                .await
        ),
        "fixture transcript"
    );
    assert_eq!(audio_origin.calls.load(Ordering::SeqCst), 1);
    assert_eq!(native_origin.calls.load(Ordering::SeqCst), 0);

    assert_eq!(
        take_transcription(
            native_adapter
                .execute(routed(
                    &config,
                    "native-asr-transcription",
                    file_request(
                        "native-public",
                        "voice.wav",
                        "audio/wav",
                        vec![7, 8],
                        None,
                        None,
                    ),
                ))
                .await
        ),
        "fixture native transcript"
    );
    assert_eq!(audio_origin.calls.load(Ordering::SeqCst), 1);
    assert_eq!(native_origin.calls.load(Ordering::SeqCst), 1);
    audio_origin.finish().await;
    native_origin.finish().await;

    let audio_record = request_record(&audio_origin, 0);
    assert_eq!(audio_record.method, "POST");
    assert_eq!(audio_record.path, "/v1/chat/completions");
    let native_record = request_record(&native_origin, 0);
    assert_multipart(
        &native_record,
        &[("model", "served-native-asr"), ("response_format", "json")],
        "voice.wav",
        "audio/wav",
        &[7, 8],
    );
}

#[tokio::test]
async fn native_asr_encodes_exact_multipart_and_omits_absent_hints() {
    let mut mock = MockServer::json_many(NATIVE_ASR_RESPONSE, 2).await;
    let config = config_for(
        &mock.address,
        ConfigOptions {
            transcription_mode: Some("native_asr"),
            ..ConfigOptions::default()
        },
    );
    let adapter = VllmAdapter::from_config(&config, "vllm-fixture", "vllm-transcription")
        .unwrap_or_else(|error| panic!("native ASR adapter: {error:?}"));

    let no_hints_audio = [0, 255, 0x41, 10];
    assert_eq!(
        take_transcription(
            adapter
                .execute(routed(
                    &config,
                    "vllm-transcription",
                    file_request(
                        "vllm-public",
                        "voice.wav",
                        "audio/wav",
                        no_hints_audio.to_vec(),
                        None,
                        None,
                    ),
                ))
                .await
        ),
        "fixture native transcript"
    );

    let hinted_audio = [9, 0, 254, 8];
    let language = "l".repeat(80);
    let prompt = "p".repeat(1000);
    assert_eq!(
        take_transcription(
            adapter
                .execute(routed(
                    &config,
                    "vllm-transcription",
                    file_request(
                        "vllm-public",
                        "spoken.flac",
                        "audio/flac",
                        hinted_audio.to_vec(),
                        Some(language.clone()),
                        Some(prompt.clone()),
                    ),
                ))
                .await
        ),
        "fixture native transcript"
    );
    mock.finish().await;

    assert_multipart(
        &request_record(&mock, 0),
        &[("model", "served-transcriber"), ("response_format", "json")],
        "voice.wav",
        "audio/wav",
        &no_hints_audio,
    );
    assert_multipart(
        &request_record(&mock, 1),
        &[
            ("model", "served-transcriber"),
            ("language", &language),
            ("prompt", &prompt),
            ("response_format", "json"),
        ],
        "spoken.flac",
        "audio/flac",
        &hinted_audio,
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn native_asr_forwards_the_ingress_mime_matrix_without_rewriting() {
    let cases = [
        ("audio/flac", "speech.flac"),
        ("audio/mpeg", "speech.mp3"),
        ("audio/mp4", "speech.mp4"),
        ("audio/ogg", "speech.ogg"),
        ("audio/wav", "speech.wav"),
        ("audio/webm", "speech.webm"),
        ("audio/x-wav", "speech.wav"),
    ];
    let mut mock = MockServer::json_many(NATIVE_ASR_RESPONSE, cases.len()).await;
    let config = config_for(
        &mock.address,
        ConfigOptions {
            transcription_mode: Some("native_asr"),
            ..ConfigOptions::default()
        },
    );
    let adapter = VllmAdapter::from_config(&config, "vllm-fixture", "vllm-transcription")
        .unwrap_or_else(|error| panic!("native ASR adapter: {error:?}"));
    let audio = [0, 255, 7];
    for (index, (media_type, filename)) in cases.iter().enumerate() {
        take_transcription(
            adapter
                .execute(routed(
                    &config,
                    "vllm-transcription",
                    file_request(
                        "vllm-public",
                        filename,
                        media_type,
                        audio.to_vec(),
                        None,
                        None,
                    ),
                ))
                .await,
        );
        assert_eq!(mock.calls.load(Ordering::SeqCst), index + 1);
    }
    mock.finish().await;
    for (index, (media_type, filename)) in cases.iter().enumerate() {
        assert_multipart(
            &request_record(&mock, index),
            &[("model", "served-transcriber"), ("response_format", "json")],
            filename,
            media_type,
            &audio,
        );
    }
}

#[tokio::test]
async fn native_asr_accepts_audio_above_text_cap_with_checked_multipart_budget() {
    let mut mock = MockServer::json(NATIVE_ASR_RESPONSE).await;
    let config = config_for(
        &mock.address,
        ConfigOptions {
            transcription_mode: Some("native_asr"),
            max_audio_bytes: 2 * 1024 * 1024,
            ..ConfigOptions::default()
        },
    );
    let adapter = VllmAdapter::from_config(&config, "vllm-fixture", "vllm-transcription")
        .unwrap_or_else(|error| panic!("native ASR adapter: {error:?}"));
    let audio = vec![0xa5; 2 * 1024 * 1024];
    assert_eq!(
        take_transcription(
            adapter
                .execute(routed(
                    &config,
                    "vllm-transcription",
                    file_request(
                        "vllm-public",
                        "large.wav",
                        "audio/wav",
                        audio.clone(),
                        None,
                        None,
                    ),
                ))
                .await
        ),
        "fixture native transcript"
    );
    mock.finish().await;
    let record = request_record(&mock, 0);
    assert!(record.body.len() > 1_048_576);
    assert_multipart(
        &record,
        &[("model", "served-transcriber"), ("response_format", "json")],
        "large.wav",
        "audio/wav",
        &audio,
    );
}

#[tokio::test]
async fn native_asr_rejects_unsafe_files_hints_extensions_and_oversize_before_dispatch() {
    let mock = MockServer::json(NATIVE_ASR_RESPONSE).await;
    let config = config_for(
        &mock.address,
        ConfigOptions {
            transcription_mode: Some("native_asr"),
            max_audio_bytes: 8,
            ..ConfigOptions::default()
        },
    );
    let adapter = VllmAdapter::from_config(&config, "vllm-fixture", "vllm-transcription")
        .unwrap_or_else(|error| panic!("native ASR adapter: {error:?}"));

    let invalid = [
        file_request(
            "vllm-public",
            "voice.wav",
            "audio/wav",
            vec![0; 9],
            None,
            None,
        ),
        file_request(
            "vllm-public",
            "../voice.wav",
            "audio/wav",
            vec![1],
            None,
            None,
        ),
        file_request("vllm-public", "voice.wav", "audio/aac", vec![1], None, None),
        file_request(
            "vllm-public",
            "voice\".wav",
            "audio/wav",
            vec![1],
            None,
            None,
        ),
        file_request(
            "vllm-public",
            "voice.wav",
            "audio/wav",
            vec![1],
            Some("l".repeat(81)),
            None,
        ),
        file_request(
            "vllm-public",
            "voice.wav",
            "audio/wav",
            vec![1],
            None,
            Some("p".repeat(1001)),
        ),
        file_request(
            "vllm-public",
            "voice.wav",
            "audio/wav",
            vec![1],
            Some("en\nsecret".to_owned()),
            None,
        ),
    ];
    for request in invalid {
        assert_eq!(
            crate::support::common::error_kind(
                adapter
                    .execute(routed(&config, "vllm-transcription", request))
                    .await
            ),
            ErrorKind::InvalidRequest
        );
    }

    let mut request = file_request("vllm-public", "voice.wav", "audio/wav", vec![1], None, None);
    let Request::Transcription(transcription) = &mut request else {
        panic!("transcription request")
    };
    let mut extensions = BTreeMap::new();
    extensions.insert(
        ExtensionKey::parse("vendor.extra").unwrap_or_else(|_| panic!("extension key")),
        json!("untrusted"),
    );
    transcription.extensions =
        Extensions::try_from_map(extensions).unwrap_or_else(|_| panic!("extension fixture"));
    assert_eq!(
        crate::support::common::error_kind(
            adapter
                .execute(routed(&config, "vllm-transcription", request))
                .await
        ),
        ErrorKind::InvalidRequest
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn native_asr_rejects_undeclared_capabilities_and_wrong_operations_before_dispatch() {
    let mock = MockServer::json(NATIVE_ASR_RESPONSE).await;
    for options in [
        ConfigOptions {
            transcription_mode: Some("native_asr"),
            streaming_chat: true,
            ..ConfigOptions::default()
        },
        ConfigOptions {
            transcription_mode: Some("native_asr"),
            function_tools: true,
            ..ConfigOptions::default()
        },
        ConfigOptions {
            transcription_mode: Some("native_asr"),
            secret_ref: true,
            ..ConfigOptions::default()
        },
    ] {
        let config = config_for(&mock.address, options);
        assert!(VllmAdapter::from_config(&config, "vllm-fixture", "vllm-transcription").is_err());
    }

    let mixed_config = audio_chat_native_asr_config(&mock.address, &mock.address);
    let native_adapter =
        VllmAdapter::from_config(&mixed_config, "vllm-native-asr", "native-asr-transcription")
            .unwrap_or_else(|error| panic!("native ASR adapter: {error:?}"));
    let wrong_operation = routed(
        &mixed_config,
        "audio-chat-chat",
        chat_request("audio-public", vec![text("must not dispatch")]),
    );
    assert_eq!(
        crate::support::common::error_kind(native_adapter.execute(wrong_operation).await),
        ErrorKind::UnsupportedOperation
    );

    let wrong_route = routed(
        &mixed_config,
        "audio-chat-transcription",
        transcription_request("audio-public", "audio/wav", vec![1], None, None),
    );
    assert_eq!(
        crate::support::common::error_kind(native_adapter.execute(wrong_route).await),
        ErrorKind::InvalidRequest
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
}

async fn assert_native_redacted_failure(mut mock: MockServer, expected: ErrorKind, marker: &str) {
    let config = config_for(
        &mock.address,
        ConfigOptions {
            transcription_mode: Some("native_asr"),
            ..ConfigOptions::default()
        },
    );
    let adapter = VllmAdapter::from_config(&config, "vllm-fixture", "vllm-transcription")
        .unwrap_or_else(|error| panic!("native ASR adapter: {error:?}"));
    let error = match adapter
        .execute(routed(
            &config,
            "vllm-transcription",
            file_request(
                "vllm-public",
                "private.wav",
                "audio/wav",
                vec![1, 2],
                None,
                None,
            ),
        ))
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("invalid upstream response accepted"),
    };
    assert_eq!(error.kind, expected);
    assert!(!format!("{error:?}").contains(marker));
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    mock.finish().await;
}

#[tokio::test]
async fn native_asr_status_format_and_parse_errors_are_static_and_redacted() {
    assert_native_redacted_failure(
        MockServer::status(500, r#"{"detail":"NATIVE_STATUS_SECRET"}"#).await,
        ErrorKind::UpstreamFailure,
        "NATIVE_STATUS_SECRET",
    )
    .await;
    assert_native_redacted_failure(
        MockServer::response(200, "text/plain", "NATIVE_FORMAT_SECRET").await,
        ErrorKind::UpstreamFailure,
        "NATIVE_FORMAT_SECRET",
    )
    .await;
    assert_native_redacted_failure(
        MockServer::json("malformed NATIVE_PARSE_SECRET").await,
        ErrorKind::UpstreamFailure,
        "NATIVE_PARSE_SECRET",
    )
    .await;
    assert_native_redacted_failure(
        MockServer::json(r#"{"text":"  ","reasoning":"NATIVE_EMPTY_SECRET"}"#).await,
        ErrorKind::UpstreamFailure,
        "NATIVE_EMPTY_SECRET",
    )
    .await;
}

#[tokio::test]
async fn native_asr_rejects_unknown_success_fields_without_echoing_them() {
    assert_native_redacted_failure(
        MockServer::json(include_str!(
            "../fixtures/vllm/native-asr-transcription-extra-field.json"
        ))
        .await,
        ErrorKind::UpstreamFailure,
        "NATIVE_REASONING_SECRET",
    )
    .await;
}

#[tokio::test]
async fn cancelling_native_asr_closes_the_upstream_request() {
    let mut mock = MockServer::incomplete_body().await;
    let config = config_for(
        &mock.address,
        ConfigOptions {
            transcription_mode: Some("native_asr"),
            ..ConfigOptions::default()
        },
    );
    let adapter = VllmAdapter::from_config(&config, "vllm-fixture", "vllm-transcription")
        .unwrap_or_else(|error| panic!("native ASR adapter: {error:?}"));
    let request = routed(
        &config,
        "vllm-transcription",
        file_request(
            "vllm-public",
            "voice.wav",
            "audio/wav",
            vec![1, 2],
            None,
            None,
        ),
    );
    let task = tokio::spawn(async move { adapter.execute(request).await });

    mock.wait_for_request().await;
    mock.wait_for_response().await;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(2), mock.wait_for_client_close())
        .await
        .unwrap_or_else(|_| panic!("upstream connection remained open after cancellation"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    mock.finish().await;
}
