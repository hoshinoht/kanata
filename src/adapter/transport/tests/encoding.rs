use std::{
    convert::Infallible,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use bytes::Bytes;
use futures_util::{StreamExt, future::poll_fn, stream};
use http::Method;
use http_body::Body;
use multer::Multipart;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
};

use crate::core::ErrorKind;

use super::super::encoded::EncodedBody;
use super::super::multipart::encode_with_scan_counter;
use super::super::types::MAX_CHUNK_BYTES;
use super::super::{
    Accept, COMPLETE_RESPONSE_BYTES, Endpoint, MultipartFile, MultipartRequest, TransportRequest,
};
use super::fixture;

async fn encoded_frames(mut body: EncodedBody, budget: usize) -> Vec<Bytes> {
    let mut frames = Vec::new();
    let mut total = 0usize;
    loop {
        let frame = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        let Some(frame) = frame else { break };
        let bytes = frame
            .unwrap_or_else(|never: Infallible| match never {})
            .into_data()
            .unwrap_or_else(|_| panic!("encoded body yielded trailers"));
        assert!(bytes.len() <= MAX_CHUNK_BYTES);
        total = total
            .checked_add(bytes.len())
            .unwrap_or_else(|| panic!("length"));
        assert!(total <= budget);
        frames.push(bytes);
    }
    frames
}

fn endpoint() -> Endpoint {
    Endpoint::new(&["v1", "encode"]).unwrap_or_else(|_| panic!("endpoint"))
}

#[tokio::test]
async fn json_encoding_round_trips_and_rejects_budget_overflow() {
    let value = json!({"unicode": "café", "number": 7});
    let request = TransportRequest::json(
        Method::POST,
        endpoint(),
        &value,
        None,
        Some(Accept::Json),
        128,
        COMPLETE_RESPONSE_BYTES,
    )
    .unwrap_or_else(|_| panic!("json request"));
    let bytes = encoded_frames(request.body, 128)
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let decoded: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| panic!("json"));
    assert_eq!(decoded, value);

    let exact = TransportRequest::json(
        Method::POST,
        endpoint(),
        &"ok",
        None,
        Some(Accept::Json),
        4,
        COMPLETE_RESPONSE_BYTES,
    );
    assert!(exact.is_ok());
    let over = TransportRequest::json(
        Method::POST,
        endpoint(),
        &"ok",
        None,
        Some(Accept::Json),
        3,
        COMPLETE_RESPONSE_BYTES,
    );
    let error = match over {
        Err(error) => error,
        Ok(_) => panic!("over-budget json accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);
}

#[tokio::test]
async fn sensitive_json_and_preencoded_form_bodies_preserve_their_contract() {
    let value = json!({
        "device_auth_id": "TEST_ONLY_DEVICE_AUTH_ID_NOT_SECRET",
        "user_code": "TEST-ONLY-CODE",
    });
    let request = TransportRequest::sensitive_json(
        Method::POST,
        endpoint(),
        &value,
        256,
        COMPLETE_RESPONSE_BYTES,
    )
    .unwrap_or_else(|_| panic!("sensitive json request"));
    assert!(request.body.is_sensitive());
    assert_eq!(request.body.content_type(), "application/json");
    let bytes = encoded_frames(request.body, 256)
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| panic!("sensitive json")),
        value
    );

    let form = "grant_type=authorization_code&code=TEST_ONLY_CODE&redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback";
    let request = TransportRequest::form_encoded(
        Method::POST,
        endpoint(),
        form,
        form.len(),
        COMPLETE_RESPONSE_BYTES,
    )
    .unwrap_or_else(|_| panic!("preencoded form request"));
    assert!(request.body.is_sensitive());
    assert_eq!(
        request.body.content_type(),
        "application/x-www-form-urlencoded"
    );
    let bytes = encoded_frames(request.body, form.len())
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(bytes, form.as_bytes());

    for (body, budget) in [
        ("code=%2", 16),
        ("code=unsafe\nvalue", 32),
        (form, form.len() - 1),
    ] {
        let error = match TransportRequest::form_encoded(
            Method::POST,
            endpoint(),
            body,
            budget,
            COMPLETE_RESPONSE_BYTES,
        ) {
            Err(error) => error,
            Ok(_) => panic!("invalid or over-budget form accepted"),
        };
        assert_eq!(error.kind, ErrorKind::Internal);
    }
}

#[tokio::test]
async fn form_encoding_preserves_order_unicode_spaces_and_reserved_bytes() {
    let request = TransportRequest::form(
        Method::POST,
        endpoint(),
        vec![
            ("a b".to_owned(), "c+d & café".to_owned()),
            ("a b".to_owned(), "second".to_owned()),
        ],
        None,
        Some(Accept::Json),
        128,
        COMPLETE_RESPONSE_BYTES,
    )
    .unwrap_or_else(|_| panic!("form request"));
    assert!(request.body.is_sensitive());
    let bytes = encoded_frames(request.body, 128)
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        String::from_utf8(bytes).unwrap_or_else(|_| panic!("form utf8")),
        "a+b=c%2Bd+%26+caf%C3%A9&a+b=second"
    );

    let exact = TransportRequest::form(
        Method::POST,
        endpoint(),
        vec![("x".to_owned(), "y".to_owned())],
        None,
        Some(Accept::Json),
        3,
        COMPLETE_RESPONSE_BYTES,
    );
    assert!(exact.is_ok());
    let over = TransportRequest::form(
        Method::POST,
        endpoint(),
        vec![("x".to_owned(), "y".to_owned())],
        None,
        Some(Accept::Json),
        2,
        COMPLETE_RESPONSE_BYTES,
    );
    let error = match over {
        Err(error) => error,
        Ok(_) => panic!("over-budget form accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);
}

#[tokio::test]
async fn multipart_preserves_binary_file_fields_and_exact_budget() {
    let marker = b"\x00--kanata-boundary-0000000000000000\xff\r\n";
    let mut file_data = vec![b'x'; MAX_CHUNK_BYTES * 2 + 1];
    file_data[..marker.len()].copy_from_slice(marker);
    let file_bytes = Bytes::from(file_data);
    let file = MultipartFile::new(
        "file".to_owned(),
        "audio.bin".to_owned(),
        "application/octet-stream".to_owned(),
        file_bytes.clone(),
    )
    .unwrap_or_else(|_| panic!("file"));
    let multipart = MultipartRequest::new(vec![("prompt".to_owned(), "hello".to_owned())], file)
        .unwrap_or_else(|_| panic!("multipart"));
    let request = TransportRequest::multipart(
        Method::POST,
        endpoint(),
        multipart,
        None,
        Some(Accept::Json),
        65536,
        COMPLETE_RESPONSE_BYTES,
    )
    .unwrap_or_else(|error| panic!("multipart request: {error:?}"));
    let content_type = request.body.content_type().to_owned();
    let content_length = request.body.len();
    let frames = encoded_frames(request.body, 65536).await;
    assert_eq!(frames.iter().map(Bytes::len).sum::<usize>(), content_length);
    assert!(frames.iter().all(|frame| frame.len() <= MAX_CHUNK_BYTES));
    let bytes = frames.into_iter().flatten().collect::<Vec<_>>();
    let boundary = multer::parse_boundary(&content_type).unwrap_or_else(|_| panic!("boundary"));
    let mut multipart = Multipart::new(
        stream::iter([Ok::<Bytes, Infallible>(Bytes::from(bytes))]),
        boundary,
    );
    let mut text = None;
    let mut received_file = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .unwrap_or_else(|_| panic!("parse multipart"))
    {
        let name = field
            .name()
            .unwrap_or_else(|| panic!("field name"))
            .to_owned();
        let data = field
            .bytes()
            .await
            .unwrap_or_else(|_| panic!("field bytes"));
        if name == "prompt" {
            text = Some(data);
        } else if name == "file" {
            received_file = Some(data);
        }
    }
    assert_eq!(text, Some(Bytes::from_static(b"hello")));
    assert_eq!(received_file, Some(file_bytes));
}

#[test]
fn multipart_metadata_and_boundary_failures_are_internal() {
    let invalid_file = MultipartFile::new(
        "file\r\nX-Leak: yes".to_owned(),
        "audio.bin".to_owned(),
        "application/octet-stream".to_owned(),
        Bytes::from_static(b"x"),
    );
    assert!(invalid_file.is_err());

    let invalid_media = MultipartFile::new(
        "file".to_owned(),
        "audio.bin".to_owned(),
        "audio/wav\r\nX-Leak: yes".to_owned(),
        Bytes::from_static(b"x"),
    );
    assert!(invalid_media.is_err());

    for filename in ["audio\".bin", "audio\\.bin", "audio\r\n.bin"] {
        assert!(
            MultipartFile::new(
                "file".to_owned(),
                filename.to_owned(),
                "application/octet-stream".to_owned(),
                Bytes::from_static(b"x"),
            )
            .is_err()
        );
    }
    assert!(
        MultipartFile::new(
            "file".to_owned(),
            "empty.bin".to_owned(),
            "application/octet-stream".to_owned(),
            Bytes::new(),
        )
        .is_err()
    );

    let file = MultipartFile::new(
        "file".to_owned(),
        "audio.bin".to_owned(),
        "application/octet-stream".to_owned(),
        Bytes::from_static(b"x"),
    )
    .unwrap_or_else(|_| panic!("file"));
    let error = match TransportRequest::multipart(
        Method::POST,
        endpoint(),
        MultipartRequest::new(Vec::new(), file).unwrap_or_else(|_| panic!("multipart metadata")),
        None,
        Some(Accept::Json),
        1,
        COMPLETE_RESPONSE_BYTES,
    ) {
        Err(error) => error,
        Ok(_) => panic!("multipart envelope exceeded budget"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);

    let file = MultipartFile::new(
        "file".to_owned(),
        "audio.bin".to_owned(),
        "application/octet-stream".to_owned(),
        Bytes::from_static(b"x"),
    )
    .unwrap_or_else(|_| panic!("file"));
    let fields = vec![(
        "payload".to_owned(),
        (0..64)
            .map(|index| format!("kanata-boundary-{index:016x}"))
            .collect::<String>(),
    )];
    let request =
        MultipartRequest::new(fields, file).unwrap_or_else(|_| panic!("multipart metadata"));
    let error = match TransportRequest::multipart(
        Method::POST,
        endpoint(),
        request,
        None,
        Some(Accept::Json),
        8192,
        COMPLETE_RESPONSE_BYTES,
    ) {
        Err(error) => error,
        Ok(_) => panic!("boundary collision accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);
}

#[test]
fn tiny_multipart_budget_fails_before_boundary_payload_scans() {
    let file = MultipartFile::new(
        "file".to_owned(),
        "audio.bin".to_owned(),
        "application/octet-stream".to_owned(),
        Bytes::from_static(b"x"),
    )
    .unwrap_or_else(|_| panic!("file"));
    let request = MultipartRequest::new(vec![("payload".to_owned(), "x".repeat(100_000))], file)
        .unwrap_or_else(|_| panic!("multipart"));
    let scans = AtomicUsize::new(0);
    assert!(encode_with_scan_counter(request, 8, &scans).is_err());
    assert_eq!(scans.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn captured_local_multipart_request_has_exact_length_and_parses() {
    let file_bytes = Bytes::from_static(b"binary\x00payload\xff");
    let file = MultipartFile::new(
        "file".to_owned(),
        "audio.bin".to_owned(),
        "application/octet-stream".to_owned(),
        file_bytes.clone(),
    )
    .unwrap_or_else(|_| panic!("file"));
    let request = TransportRequest::multipart(
        Method::POST,
        endpoint(),
        MultipartRequest::new(vec![("prompt".to_owned(), "hello".to_owned())], file)
            .unwrap_or_else(|_| panic!("multipart")),
        None,
        Some(Accept::Json),
        4096,
        COMPLETE_RESPONSE_BYTES,
    )
    .unwrap_or_else(|error| panic!("request: {error:?}"));
    let expected_length = request.body.len();
    let (listener, address) = fixture::listener().await;
    let (captured_tx, captured_rx) = oneshot::channel();
    let server = fixture::server_task(listener, |mut socket| async move {
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let count = socket
                .read(&mut chunk)
                .await
                .unwrap_or_else(|_| panic!("request"));
            assert!(count != 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_end = index + 4;
                let headers = String::from_utf8(bytes[..header_end].to_vec())
                    .unwrap_or_else(|_| panic!("headers"));
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or_else(|| panic!("content length"));
                while bytes.len() < header_end + length {
                    let count = socket
                        .read(&mut chunk)
                        .await
                        .unwrap_or_else(|_| panic!("request body"));
                    assert!(count != 0, "request ended before body");
                    bytes.extend_from_slice(&chunk[..count]);
                }
                captured_tx
                    .send((headers, bytes[header_end..header_end + length].to_vec()))
                    .unwrap_or_else(|_| panic!("capture"));
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                    .await
                    .unwrap_or_else(|_| panic!("response"));
                return;
            }
        }
    });
    let mut response = fixture::transport(
        address,
        fixture::timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
    )
    .execute(request)
    .await
    .unwrap_or_else(|_| panic!("response"));
    assert!(response.body.next().await.is_none());
    let (headers, body) = captured_rx.await.unwrap_or_else(|_| panic!("capture"));
    assert!(headers.contains(&format!("content-length: {expected_length}")));
    let boundary = multer::parse_boundary(
        headers
            .lines()
            .find_map(|line| line.strip_prefix("content-type: "))
            .unwrap_or_else(|| panic!("content type")),
    )
    .unwrap_or_else(|_| panic!("boundary"));
    let mut multipart = Multipart::new(
        stream::iter([Ok::<Bytes, Infallible>(Bytes::from(body))]),
        boundary,
    );
    let mut seen_file = None;
    let mut seen_prompt = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .unwrap_or_else(|_| panic!("parse"))
    {
        let name = field.name().unwrap_or_else(|| panic!("name")).to_owned();
        let value = field.bytes().await.unwrap_or_else(|_| panic!("bytes"));
        if name == "file" {
            seen_file = Some(value);
        } else if name == "prompt" {
            seen_prompt = Some(value);
        }
    }
    assert_eq!(seen_file, Some(file_bytes));
    assert_eq!(seen_prompt, Some(Bytes::from_static(b"hello")));
    server.await.unwrap_or_else(|_| panic!("server"));
}
