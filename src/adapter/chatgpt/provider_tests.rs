use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

use super::*;
use crate::core::{ChatContent, FinishReason, NormalizedEvent, ToolCall, Usage};

const TOKEN: &str = "TEST_ONLY_CHATGPT_ACCESS_TOKEN_NOT_SECRET";
const TEXT: &str = "Fixture answer";

struct RequestRecord {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct Fixture {
    transport: Transport,
    request: oneshot::Receiver<RequestRecord>,
    release: oneshot::Sender<()>,
    server: JoinHandle<()>,
}

fn tls_fixture(host: &str) -> (CertificateDer<'static>, TlsAcceptor) {
    let certified = rcgen::generate_simple_self_signed(vec![host.to_owned()]).unwrap();
    let certificate = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)
        .unwrap();
    (certificate, TlsAcceptor::from(Arc::new(config)))
}

async fn read_request(reader: &mut (impl AsyncRead + Unpin)) -> RequestRecord {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let length = reader.read(&mut chunk).await.unwrap();
        assert_ne!(length, 0);
        bytes.extend_from_slice(&chunk[..length]);
        assert!(bytes.len() < 64 * 1024);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = headers.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let path = first.next().unwrap().to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers["content-length"].parse().unwrap();
    assert!(length < 64 * 1024);
    let received = bytes.len();
    assert!(received <= header_end + length);
    bytes.resize(header_end + length, 0);
    if received < header_end + length {
        reader.read_exact(&mut bytes[received..]).await.unwrap();
    }
    RequestRecord {
        method,
        path,
        headers,
        body: serde_json::from_slice(&bytes[header_end..]).unwrap(),
    }
}

async fn write_chunk(writer: &mut (impl AsyncWrite + Unpin), body: &str) {
    if !body.is_empty() {
        writer
            .write_all(format!("{:x}\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        writer.write_all(body.as_bytes()).await.unwrap();
        writer.write_all(b"\r\n").await.unwrap();
    }
    writer.flush().await.unwrap();
}

async fn fixture(
    config: &ValidatedConfig,
    status: u16,
    head: String,
    tail: Option<String>,
) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (certificate, acceptor) = tls_fixture(HOST);
    let transport =
        Transport::new_pinned_https_with_test_root(HOST, config.timeouts(), certificate, address)
            .unwrap();
    let (request_tx, request) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(socket).await.unwrap();
        request_tx
            .send(read_request(&mut stream).await)
            .unwrap_or_else(|_| panic!("request receiver dropped"));
        stream.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nLocation: https://credential-trap.invalid/stolen\r\n\r\n").as_bytes()).await.unwrap();
        write_chunk(&mut stream, &head).await;
        if let Some(tail) = tail {
            let _ = wait.await;
            if !tail.is_empty() {
                write_chunk(&mut stream, &tail).await;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(80), listener.accept())
                .await
                .is_err(),
            "unexpected retry"
        );
    });
    Fixture {
        transport,
        request,
        release,
        server,
    }
}

fn record(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn started_text() -> String {
    record(
        json!({"type":"response.created","response":{"id":"response_fixture","created_at":1700000000}}),
    ) + &record(json!({"type":"response.output_text.delta","delta":TEXT}))
}

fn completed() -> String {
    record(
        json!({"type":"response.completed","response":{"id":"response_fixture",
        "usage":{"input_tokens":4,"output_tokens":3,"total_tokens":7}}}),
    )
}

fn tools(namespace: Option<&str>) -> String {
    let mut item = json!({"type":"function_call","id":"function_fixture","call_id":"call_new",
        "name":"lookup","arguments":"{\"query\":\"fixture\"}"});
    if let Some(namespace) = namespace {
        item["namespace"] = json!(namespace);
    }
    started_text()
        + &record(json!({"type":"response.output_item.done","output_index":0,"item":item}))
        + &completed()
}

fn inference(config: &ValidatedConfig, streaming: bool) -> Inference {
    let mut chat = tests::chat();
    chat.stream = streaming;
    let routed = tests::routed(config, chat);
    let payload = request::encode(
        &routed,
        config.adapters()[0].capabilities(),
        &[config.routes()[0].identity().clone()],
    )
    .unwrap();
    Inference {
        payload,
        model: ModelAlias("chatgpt-chat".into()),
        streaming,
        deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        label: UpstreamLabel::new("fixture", "chatgpt"),
        request_budget: 65536,
    }
}

#[tokio::test]
async fn namespaced_functions_round_trip_through_pinned_tls() {
    let config = tests::config();
    let fixture = fixture(&config, 200, tools(Some(NAMESPACE)), None).await;
    let output = execute_with_token(&fixture.transport, TOKEN, inference(&config, false))
        .await
        .unwrap();
    let AdapterOutput::Complete(Response::Chat(response)) = output else {
        panic!("chat response")
    };
    assert_eq!(response.model.0, "chatgpt-chat");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Some(Usage {
            input_tokens: 4,
            output_tokens: 3,
            total_tokens: 7,
            reasoning_tokens: None
        })
    );
    assert_eq!(
        response.message.content,
        vec![
            ChatContent::Text { text: TEXT.into() },
            ChatContent::ToolCall {
                call: ToolCall {
                    id: "call_new".into(),
                    name: "lookup".into(),
                    arguments: "{\"query\":\"fixture\"}".into()
                }
            }
        ]
    );
    let request = fixture.request.await.unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(request.headers["host"], HOST);
    assert_eq!(request.headers["authorization"], format!("Bearer {TOKEN}"));
    assert_eq!(request.headers["accept"], "text/event-stream");
    assert!(!request.headers.contains_key("chatgpt-account-id"));
    assert_eq!(request.body["model"], "replace-with-account-model-slug");
    assert_eq!(request.body["tools"][0]["name"], NAMESPACE);
    assert_eq!(request.body["tools"][0]["tools"][0]["name"], "lookup");
    assert_eq!(request.body["input"][1]["namespace"], NAMESPACE);
    assert_eq!(request.body["input"][2]["call_id"], "call_1");
    assert_eq!(request.body["input"][2]["type"], "function_call_output");
    fixture.server.await.unwrap();
}

#[tokio::test]
async fn completed_is_terminal_even_when_connection_stays_open() {
    let config = tests::config();
    for streaming in [false, true] {
        let fixture = fixture(
            &config,
            200,
            started_text() + &completed(),
            Some(String::new()),
        )
        .await;
        let output = tokio::time::timeout(
            Duration::from_secs(1),
            execute_with_token(&fixture.transport, TOKEN, inference(&config, streaming)),
        )
        .await
        .unwrap()
        .unwrap();
        if let AdapterOutput::Events(mut events) = output {
            let mut completed = 0;
            while let Some(event) = tokio::time::timeout(Duration::from_millis(250), events.next())
                .await
                .expect("completed stream must release the open connection")
            {
                if matches!(event.unwrap(), NormalizedEvent::ChatCompleted { .. }) {
                    completed += 1;
                }
            }
            assert_eq!(completed, 1);
        } else {
            assert!(!streaming);
        }
        fixture.release.send(()).unwrap();
        fixture.server.await.unwrap();
    }
}

#[tokio::test]
async fn failed_after_text_is_terminal_error_without_success() {
    let config = tests::config();
    let failed = record(
        json!({"type":"response.failed","response":{"id":"response_fixture","error":{"message":"TEST_ONLY_PRIVATE_UPSTREAM_TEXT"}}}),
    );
    let fixture = fixture(&config, 200, started_text(), Some(failed)).await;
    let AdapterOutput::Events(mut events) =
        execute_with_token(&fixture.transport, TOKEN, inference(&config, true))
            .await
            .unwrap()
    else {
        panic!("events")
    };
    assert!(matches!(
        events.next().await.unwrap().unwrap(),
        NormalizedEvent::ChatStarted { .. }
    ));
    assert_eq!(
        events.next().await.unwrap().unwrap(),
        NormalizedEvent::ChatTextDelta { text: TEXT.into() }
    );
    fixture.release.send(()).unwrap();
    assert_eq!(
        events.next().await.unwrap().unwrap_err().kind,
        ErrorKind::UpstreamFailure
    );
    assert!(events.next().await.is_none());
    fixture.server.await.unwrap();
}

#[tokio::test]
async fn missing_completion_and_wrong_namespaces_fail_closed() {
    let config = tests::config();
    for body in [started_text(), tools(None), tools(Some("other"))] {
        for streaming in [false, true] {
            let fixture = fixture(&config, 200, body.clone(), None).await;
            let output =
                execute_with_token(&fixture.transport, TOKEN, inference(&config, streaming)).await;
            if streaming {
                let AdapterOutput::Events(mut events) = output.unwrap() else {
                    panic!("events")
                };
                let mut failed = false;
                while let Some(event) = events.next().await {
                    match event {
                        Err(error) => {
                            assert_eq!(error.kind, ErrorKind::UpstreamFailure);
                            failed = true;
                        }
                        Ok(NormalizedEvent::ChatCompleted { .. }) => {
                            panic!("invalid response completed")
                        }
                        _ => {}
                    }
                }
                assert!(failed);
            } else {
                assert!(matches!(
                    output,
                    Err(GatewayError {
                        kind: ErrorKind::UpstreamFailure
                    })
                ));
            }
            fixture.server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn unauthorized_and_redirect_responses_are_never_retried() {
    let config = tests::config();
    for (status, kind) in [
        (401, ErrorKind::UpstreamUnavailable),
        (302, ErrorKind::UpstreamFailure),
    ] {
        let fixture = fixture(&config, status, String::new(), None).await;
        let output = execute_with_token(&fixture.transport, TOKEN, inference(&config, false)).await;
        assert!(matches!(output, Err(GatewayError {kind:actual}) if actual == kind));
        let request = fixture.request.await.unwrap();
        assert_eq!(request.headers["host"], HOST);
        fixture.server.await.unwrap();
    }
}

#[tokio::test]
async fn wrong_tls_hostname_cannot_receive_bearer_credentials() {
    let config = tests::config();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (certificate, acceptor) = tls_fixture("credential-trap.invalid");
    let transport = Transport::new_pinned_https_with_test_root(
        HOST,
        config.timeouts(),
        certificate,
        listener.local_addr().unwrap(),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        assert!(
            acceptor.accept(socket).await.is_err(),
            "TLS must fail before HTTP credentials"
        );
    });
    assert!(
        execute_with_token(&transport, TOKEN, inference(&config, false))
            .await
            .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn queued_events_obey_overall_deadline_for_slow_consumers() {
    let config = tests::config();
    let fixture = fixture(&config, 200, started_text() + &completed(), None).await;
    let AdapterOutput::Events(mut events) =
        execute_with_token(&fixture.transport, TOKEN, inference(&config, true))
            .await
            .unwrap()
    else {
        panic!("events")
    };
    assert!(matches!(
        events.next().await.unwrap().unwrap(),
        NormalizedEvent::ChatStarted { .. }
    ));
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    assert_eq!(
        events.next().await.unwrap().unwrap_err().kind,
        ErrorKind::Timeout {
            phase: crate::core::TimeoutPhase::Overall
        }
    );
    assert!(events.next().await.is_none());
    tokio::time::resume();
    fixture.server.await.unwrap();
}
