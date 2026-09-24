use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    body::{Body, Bytes},
    http::{StatusCode, header},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::stream;
use kanata::core::{ChatContent, ChatRole, InputAudioFormat};
use serde_json::{Value, json};

use crate::{
    audio_support::{adapters, config_with_audio},
    support::{
        chat_request, chat_request_with, core_chat, recorded_len, response_body, server_with,
        take_request,
    },
};

const AUDIO_CHAT: &str = include_str!("../fixtures/openai/chat-input-audio.json");

fn audio_server(
    max_body_bytes: usize,
    max_audio_bytes: usize,
) -> (kanata::server::TwoPlaneServer, crate::support::Requests) {
    let config = config_with_audio(max_body_bytes, max_audio_bytes);
    server_with(&config, adapters(&config))
}

#[tokio::test]
async fn input_audio_fixture_dispatches_decoded_parts_in_wire_order() {
    let (server, requests) = audio_server(1_048_576, 6);
    let response = server
        .client_oneshot(chat_request(AUDIO_CHAT))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(recorded_len(&requests), 1);

    let chat = core_chat(take_request(&requests));
    assert_eq!(chat.messages[0].role, ChatRole::User);
    assert_eq!(
        chat.messages[0].content,
        vec![
            ChatContent::Text {
                text: "before the recording".into(),
            },
            ChatContent::InputAudio {
                audio: kanata::core::ValidatedAudio::new(InputAudioFormat::Wav, vec![1, 2, 3, 4],)
                    .expect("opaque wav bytes"),
            },
            ChatContent::Text {
                text: "after the recording".into(),
            },
            ChatContent::InputAudio {
                audio: kanata::core::ValidatedAudio::new(InputAudioFormat::Mp3, vec![5, 6])
                    .expect("opaque mp3 bytes"),
            },
        ]
    );
}

#[tokio::test]
async fn configured_decoded_audio_limit_is_aggregate_and_inclusive() {
    let (server, requests) = audio_server(1024, 6);
    let at_limit = server
        .client_oneshot(chat_request(AUDIO_CHAT))
        .await
        .expect("response");
    assert_eq!(at_limit.status(), StatusCode::OK);
    assert_eq!(recorded_len(&requests), 1);

    let mut oversized: Value = serde_json::from_str(AUDIO_CHAT).expect("fixture json");
    oversized["messages"][0]["content"][3]["input_audio"]["data"] =
        STANDARD.encode([5, 6, 7]).into();
    let response = server
        .client_oneshot(chat_request(&oversized.to_string()))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response_body(response).await;
    assert!(!String::from_utf8_lossy(&body).contains(&STANDARD.encode([5, 6, 7])));
    assert_eq!(recorded_len(&requests), 1);
}

#[tokio::test]
async fn inline_audio_larger_than_the_text_body_limit_is_accepted() {
    let (server, requests) = audio_server(1_048_576, 2 * 1_048_576);
    let audio = vec![0x5a; 1_048_577];
    let body = json!({
        "model": "private-chat",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "input_audio",
                "input_audio": {"data": STANDARD.encode(&audio), "format": "wav"}
            }]
        }]
    })
    .to_string();
    assert!(body.len() > 1_048_576);

    let response = server
        .client_oneshot(chat_request(&body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let chat = core_chat(take_request(&requests));
    let ChatContent::InputAudio {
        audio: routed_audio,
    } = &chat.messages[0].content[0]
    else {
        panic!("input audio content");
    };
    assert_eq!(routed_audio.bytes(), audio.as_slice());
    assert_eq!(recorded_len(&requests), 0);
}

#[tokio::test]
async fn oversized_text_only_and_encoded_envelope_bodies_are_rejected() {
    let (text_server, text_requests) = audio_server(128, 3);
    let base = json!({
        "model": "private-chat",
        "messages": [{"role": "user", "content": ""}]
    })
    .to_string();
    let target_len = 129;
    let content_len = target_len - base.len();
    let text_only = json!({
        "model": "private-chat",
        "messages": [{"role": "user", "content": "x".repeat(content_len)}]
    })
    .to_string();
    assert_eq!(text_only.len(), target_len);
    let response = text_server
        .client_oneshot(chat_request(&text_only))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(recorded_len(&text_requests), 0);

    let (envelope_server, envelope_requests) = audio_server(128, 3);
    let max_envelope = 128 + 4;
    let valid = json!({
        "model": "private-chat",
        "messages": [{"role": "user", "content": "x"}]
    })
    .to_string();
    let oversized_envelope = format!("{}{}", valid, " ".repeat(max_envelope + 1 - valid.len()));
    assert_eq!(oversized_envelope.len(), max_envelope + 1);
    let response = envelope_server
        .client_oneshot(chat_request(&oversized_envelope))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(recorded_len(&envelope_requests), 0);

    let (audio_envelope_server, audio_envelope_requests) = audio_server(1_048_576, 3);
    let encoded_overflow = json!({
        "model": "private-chat",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "input_audio",
                "input_audio": {"data": "A".repeat(1_048_580), "format": "wav"}
            }]
        }]
    })
    .to_string();
    let response = audio_envelope_server
        .client_oneshot(chat_request(&encoded_overflow))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(recorded_len(&audio_envelope_requests), 0);
}

#[tokio::test]
async fn malformed_audio_shapes_roles_formats_and_encodings_do_not_dispatch() {
    let (server, requests) = audio_server(1_048_576, 1024);
    let cases = [
        (
            "bad-base64",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"%%%","format":"wav"}}]}]}"#,
        ),
        (
            "missing-padding",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AQ","format":"wav"}}]}]}"#,
        ),
        (
            "noncanonical-trailing-bits",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AB==","format":"wav"}}]}]}"#,
        ),
        (
            "data-url",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"data:audio/wav;base64,AQ==","format":"wav"}}]}]}"#,
        ),
        (
            "unknown-audio-field",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AQ==","format":"wav","url":"https://example.invalid/audio"}}]}]}"#,
        ),
        (
            "unknown-format",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AQ==","format":"ogg"}}]}]}"#,
        ),
        (
            "non-user-audio",
            r#"{"model":"private-chat","messages":[{"role":"assistant","content":[{"type":"input_audio","input_audio":{"data":"AQ==","format":"wav"}}]}]}"#,
        ),
        (
            "unknown-content-kind",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/image"}}]}]}"#,
        ),
    ];
    for (name, body) in cases {
        let response = server
            .client_oneshot(chat_request(body))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        let body = response_body(response).await;
        assert!(
            !String::from_utf8_lossy(&body).contains("example.invalid"),
            "{name}"
        );
    }
    assert_eq!(recorded_len(&requests), 0);
}

#[tokio::test]
async fn text_only_route_and_audio_stream_or_tool_combinations_do_not_dispatch() {
    let (server, requests) = audio_server(1_048_576, 1024);
    let mut text_only_route: Value = serde_json::from_str(AUDIO_CHAT).expect("fixture json");
    text_only_route["model"] = "local-chat".into();
    let cases = [
        (
            "text-only-route",
            text_only_route.to_string(),
        ),
        (
            "audio-stream",
            {
                let mut body: Value = serde_json::from_str(AUDIO_CHAT).expect("fixture json");
                body["stream"] = true.into();
                body.to_string()
            },
        ),
        (
            "audio-tool-declaration",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AQ==","format":"wav"}}]}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]}"#.into(),
        ),
        (
            "audio-tool-history",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AQ==","format":"wav"}}]},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_1","content":"done"}]}"#.into(),
        ),
    ];
    for (name, body) in cases {
        let response = server
            .client_oneshot(chat_request(&body))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
    }
    assert_eq!(recorded_len(&requests), 0);
}

#[tokio::test]
async fn auth_precedes_body_poll_and_alias_permission_follows_bounded_parse() {
    let (server, requests) = audio_server(1_048_576, 1024);
    let polled = Arc::new(AtomicBool::new(false));
    let body_polled = polled.clone();
    let body = Body::from_stream(stream::poll_fn(move |_| {
        body_polled.store(true, Ordering::SeqCst);
        std::task::Poll::<Option<Result<Bytes, std::convert::Infallible>>>::Pending
    }));
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .expect("request");
    let response = server.client_oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!polled.load(Ordering::SeqCst));
    assert_eq!(recorded_len(&requests), 0);

    let body = AUDIO_CHAT.replace("private-chat", "private-transcribe");
    let polled = Arc::new(AtomicBool::new(false));
    let body_polled = polled.clone();
    let mut chunk = Some(Bytes::from(body));
    let body = Body::from_stream(stream::poll_fn(move |_| {
        body_polled.store(true, Ordering::SeqCst);
        std::task::Poll::Ready(chunk.take().map(Ok::<Bytes, std::convert::Infallible>))
    }));
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, "Bearer test-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .expect("request");
    let response = server.client_oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(polled.load(Ordering::SeqCst));
    assert_eq!(recorded_len(&requests), 0);
}

#[tokio::test]
async fn cancelling_a_pending_audio_body_drops_the_body_without_dispatch() {
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let (server, requests) = audio_server(1_048_576, 1024);
    let polled = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let body_polled = polled.clone();
    let drop_flag = DropFlag(dropped.clone());
    let body = Body::from_stream(stream::poll_fn(move |_| {
        let _keep_until_drop = &drop_flag;
        body_polled.store(true, Ordering::SeqCst);
        std::task::Poll::<Option<Result<Bytes, std::convert::Infallible>>>::Pending
    }));
    let mut request = chat_request_with("", Some("application/json"), Some("Bearer test-key"), &[]);
    *request.body_mut() = body;

    let task = tokio::spawn(async move { server.client_oneshot(request).await });
    while !polled.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    task.abort();
    let _ = task.await;
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(recorded_len(&requests), 0);
}
