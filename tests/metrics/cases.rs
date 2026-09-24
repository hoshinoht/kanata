use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use futures_util::StreamExt;
use kanata::{
    core::{ErrorKind, FinishReason, GatewayError, ModelAlias, NormalizedEvent, TimeoutPhase},
    server::TwoPlaneServer,
};
use std::task::Poll;

use crate::{gateway as support, sse_support};

const CHAT_BODY: &str =
    r#"{"model":"private-chat","messages":[{"role":"user","content":"hello"}]}"#;

async fn scrape(server: &TwoPlaneServer) -> String {
    let response = server
        .admin_oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("metrics request"),
        )
        .await
        .expect("metrics response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; version=0.0.4"
    );
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("metrics body")
            .to_vec(),
    )
    .expect("metrics utf8")
}

async fn drain(response: axum::response::Response) {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
}

fn chat_server(outcome: support::Outcome) -> TwoPlaneServer {
    let config = support::config();
    support::server_with(
        &config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(&config, "vllm-private"),
            outcome,
        )],
    )
    .0
}

#[tokio::test]
async fn scrape_has_fixed_zero_state_and_tracks_client_lifecycle() {
    let server = chat_server(support::chat_outcome("synthetic output"));
    let initial = scrape(&server).await;
    assert!(initial.contains("kanata_process_live 1\n"));
    assert!(initial.contains("kanata_process_ready 1\n"));
    assert!(initial.contains("kanata_requests_started_total{endpoint=\"chat\"} 0"));
    assert!(initial.contains("kanata_requests_inflight{endpoint=\"other\"} 0"));
    assert!(!initial.contains("kanata_requests_finished_total{"));

    let success = server
        .client_oneshot(support::chat_request(CHAT_BODY))
        .await
        .expect("success response");
    assert_eq!(success.status(), StatusCode::OK);
    drain(success).await;

    let models = server
        .client_oneshot(support::models_request())
        .await
        .expect("models response");
    assert_eq!(models.status(), StatusCode::OK);
    drain(models).await;

    let unauthorized = server
        .client_oneshot(support::chat_request_with(
            CHAT_BODY,
            Some(support::CHAT_CONTENT_TYPE),
            None,
            &[],
        ))
        .await
        .expect("unauthorized response");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    drain(unauthorized).await;

    let unknown = server
        .client_oneshot(
            Request::builder()
                .uri("/not-a-client-route?model=SYNTHETIC_MODEL")
                .body(Body::empty())
                .expect("unknown request"),
        )
        .await
        .expect("unknown response");
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    drain(unknown).await;

    let client_metrics = server
        .client_oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("client metrics request"),
        )
        .await
        .expect("client metrics response");
    assert_eq!(client_metrics.status(), StatusCode::NOT_FOUND);
    drain(client_metrics).await;

    for index in 0..16 {
        let body = format!(
            r#"{{"model":"UNIQUE_MODEL_MARKER_{index}","messages":[],"extensions":{{"evil.key.{index}":"SYNTHETIC_SECRET"}}}}"#
        );
        let request_id = format!("SYNTHETIC_REQUEST_ID_{index}");
        let response = server
            .client_oneshot(support::chat_request_with(
                &body,
                Some(support::CHAT_CONTENT_TYPE),
                Some("Bearer test-key"),
                &[request_id.as_str()],
            ))
            .await
            .expect("malicious model response");
        drain(response).await;

        let response = server
            .client_oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/UNIQUE_PATH_MARKER_{index}?key=SYNTHETIC_SECRET"
                    ))
                    .body(Body::empty())
                    .expect("malicious path request"),
            )
            .await
            .expect("malicious path response");
        drain(response).await;
    }

    let metrics = scrape(&server).await;
    assert!(metrics.contains("kanata_requests_started_total{endpoint=\"chat\"} 18"));
    assert!(metrics.contains("kanata_requests_started_total{endpoint=\"models\"} 1"));
    assert!(metrics.contains("kanata_requests_started_total{endpoint=\"other\"} 18"));
    assert!(metrics.contains("kanata_requests_inflight{endpoint=\"chat\"} 0"));
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"success\",status_class=\"2xx\",timeout_phase=\"none\"} 1"
    ));
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"models\",outcome=\"success\",status_class=\"2xx\",timeout_phase=\"none\"} 1"
    ));
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"client_error\",status_class=\"4xx\",timeout_phase=\"none\"} 1"
    ));
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"other\",outcome=\"client_error\",status_class=\"4xx\",timeout_phase=\"none\"} 18"
    ));
    assert!(metrics.contains("le=\"+Inf\"} 1"));
    assert!(!metrics.contains("SYNTHETIC_MODEL"));
    assert!(!metrics.contains("UNIQUE_MODEL_MARKER"));
    assert!(!metrics.contains("UNIQUE_PATH_MARKER"));
    assert_eq!(
        metrics
            .lines()
            .filter(|line| line.starts_with("kanata_requests_finished_total{"))
            .count(),
        4
    );
}

#[tokio::test]
async fn explicit_timeout_phase_is_preserved_without_dynamic_labels() {
    let config = support::config();
    let (server, _) = support::server_with(
        &config,
        vec![support::adapter_spec(
            "vllm-private",
            support::capabilities(&config, "vllm-private"),
            support::Outcome::Error(ErrorKind::Timeout {
                phase: TimeoutPhase::Queue,
            }),
        )],
    );
    let response = server
        .client_oneshot(support::chat_request(CHAT_BODY))
        .await
        .expect("timeout response");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    drain(response).await;

    let metrics = scrape(&server).await;
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"timeout\",status_class=\"5xx\",timeout_phase=\"queue\"} 1"
    ));
    assert!(!metrics.contains("vllm-private"));
    assert!(metrics.contains(
        "kanata_requests_by_key_model_total{key=\"personal-client\",model=\"private-chat\",status_class=\"5xx\"} 1"
    ));
}

#[tokio::test]
async fn streaming_completion_stays_inflight_until_eof_and_drop_is_cancelled() {
    let (server, _) = sse_support::server(vec![
        Ok(NormalizedEvent::ChatStarted {
            model: ModelAlias("private-chat".into()),
        }),
        Ok(NormalizedEvent::ChatTextDelta {
            text: "safe".into(),
        }),
        Ok(NormalizedEvent::ChatCompleted {
            finish_reason: FinishReason::Stop,
            usage: None,
        }),
    ]);
    let response = server
        .client_oneshot(sse_support::request(include_str!(
            "../fixtures/openai/sse-basic.json"
        )))
        .await
        .expect("stream response");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    body.next()
        .await
        .expect("start frame")
        .expect("start bytes");
    let metrics = scrape(&server).await;
    assert!(metrics.contains("kanata_requests_inflight{endpoint=\"chat\"} 1"));
    assert!(!metrics.contains("kanata_requests_finished_total{"));

    drop(body);
    let metrics = scrape(&server).await;
    assert!(metrics.contains("kanata_requests_inflight{endpoint=\"chat\"} 0"));
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"cancelled\",status_class=\"2xx\",timeout_phase=\"none\"} 1"
    ));
}

#[tokio::test]
async fn streaming_success_finishes_at_body_eof() {
    let (server, _) = sse_support::server(vec![
        Ok(NormalizedEvent::ChatStarted {
            model: ModelAlias("private-chat".into()),
        }),
        Ok(NormalizedEvent::ChatCompleted {
            finish_reason: FinishReason::Stop,
            usage: None,
        }),
    ]);
    let response = server
        .client_oneshot(sse_support::request(include_str!(
            "../fixtures/openai/sse-basic.json"
        )))
        .await
        .expect("stream response");
    let mut body = response.into_body().into_data_stream();
    while body.next().await.is_some() {}

    let metrics = scrape(&server).await;
    assert!(metrics.contains("kanata_requests_inflight{endpoint=\"chat\"} 0"));
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"success\",status_class=\"2xx\",timeout_phase=\"none\"} 1"
    ));
}

#[tokio::test]
async fn post_header_stream_timeout_is_failure_not_success() {
    let (server, _) = sse_support::server(vec![
        Ok(NormalizedEvent::ChatStarted {
            model: ModelAlias("private-chat".into()),
        }),
        Err(GatewayError {
            kind: ErrorKind::Timeout {
                phase: TimeoutPhase::Idle,
            },
        }),
    ]);
    let response = server
        .client_oneshot(sse_support::request(include_str!(
            "../fixtures/openai/sse-basic.json"
        )))
        .await
        .expect("stream response");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("stream body");
    assert!(String::from_utf8_lossy(&body).contains("upstream_timeout"));

    let metrics = scrape(&server).await;
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"timeout\",status_class=\"2xx\",timeout_phase=\"idle\"} 1"
    ));
    assert!(!metrics.contains("outcome=\"success\""));
}

#[tokio::test]
async fn queue_full_is_a_client_error_and_does_not_finish_inflight_work() {
    let config = crate::admission_support::config(1, 1, 1_000);
    let (adapter, probe) = crate::admission_support::pending_adapter(
        "vllm-private",
        crate::admission_support::capabilities(&config, "vllm-private"),
    );
    let server = crate::admission_support::server(&config, vec![adapter]);

    let first_server = server.clone();
    let first = tokio::spawn(async move {
        first_server
            .client_oneshot(crate::admission_support::chat_request("private-chat"))
            .await
            .expect("first response")
    });
    probe.wait_for_dispatch(1).await;

    let second_server = server.clone();
    let mut second = Box::pin(async move {
        second_server
            .client_oneshot(crate::admission_support::chat_request("private-chat"))
            .await
            .expect("second response")
    });
    assert!(matches!(
        crate::admission_support::poll_once(second.as_mut()),
        Poll::Pending
    ));

    let rejected = server
        .client_oneshot(crate::admission_support::chat_request("private-chat"))
        .await
        .expect("queue response");
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    drain(rejected).await;

    let metrics = scrape(&server).await;
    assert!(metrics.contains("kanata_requests_inflight{endpoint=\"chat\"} 2"));
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"client_error\",status_class=\"4xx\",timeout_phase=\"none\"} 1"
    ));

    probe.release();
    let response = first.await.expect("first task");
    assert_eq!(response.status(), StatusCode::OK);
    drain(response).await;
    while probe.dispatches() < 2 {
        assert!(matches!(
            crate::admission_support::poll_once(second.as_mut()),
            Poll::Pending
        ));
        tokio::task::yield_now().await;
    }
    probe.release();
    let response = loop {
        if let Poll::Ready(response) = crate::admission_support::poll_once(second.as_mut()) {
            break response;
        }
    };
    assert_eq!(response.status(), StatusCode::OK);
    drain(response).await;
    let metrics = scrape(&server).await;
    assert!(metrics.contains(
        "kanata_requests_finished_total{endpoint=\"chat\",outcome=\"success\",status_class=\"2xx\",timeout_phase=\"none\"} 2"
    ));
    assert!(metrics.contains("kanata_requests_inflight{endpoint=\"chat\"} 0"));
}
