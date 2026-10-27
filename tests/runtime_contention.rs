#[path = "support/scratch.rs"]
mod scratch;
#[path = "support/gateway.rs"]
mod support;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use serde_json::json;
use support::{adapter_spec, capabilities, chat_outcome, chat_request, config, server_with};

const SMALL: &str = r#"{"model":"private-chat","messages":[{"role":"user","content":"hi"}]}"#;

fn large_body(limit: usize) -> String {
    let filler = "lorem ipsum dolor sit amet ".repeat(40);
    let mut messages = Vec::new();
    let mut body = String::new();
    loop {
        messages.push(json!({"role": "user", "content": filler}));
        let next = json!({"model": "private-chat", "messages": messages}).to_string();
        if next.len() > limit {
            return body;
        }
        body = next;
    }
}

fn summary(label: &str, mut samples: Vec<Duration>) {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    eprintln!(
        "{label}: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        at(0.5),
        at(0.99),
        samples[samples.len() - 1]
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "manual runtime contention measurement"]
async fn small_request_latency_under_large_bodies() {
    measure("current_thread").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual runtime contention measurement"]
async fn small_request_latency_under_large_bodies_multi_thread() {
    measure("multi_thread(4)").await;
}

async fn measure(flavor: &str) {
    eprintln!("runtime: {flavor}");
    let config = config();
    let capabilities = capabilities(&config, "vllm-private");
    let (server, _) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities,
            chat_outcome("ok"),
        )],
    );
    let server = Arc::new(server);
    let large = large_body(config.limits().max_body_bytes() as usize);
    eprintln!("large body: {} bytes", large.len());

    let started = Instant::now();
    let response = server
        .client_oneshot(chat_request(&large))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    eprintln!("one large request alone: {:?}", started.elapsed());

    let mut idle = Vec::new();
    for _ in 0..200 {
        let at = Instant::now();
        let response = server
            .client_oneshot(chat_request(SMALL))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        idle.push(at.elapsed());
    }
    summary("small, idle", idle);

    let load = {
        let server = server.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut completed = 0usize;
            while Instant::now() < deadline {
                let batch = (0..8).map(|_| {
                    let server = server.clone();
                    let large = large.clone();
                    tokio::spawn(async move {
                        server
                            .client_oneshot(chat_request(&large))
                            .await
                            .expect("response")
                    })
                });
                for task in batch.collect::<Vec<_>>() {
                    assert_eq!(task.await.expect("task").status(), StatusCode::OK);
                    completed += 1;
                }
            }
            completed
        })
    };
    let mut loaded = Vec::new();
    while !load.is_finished() {
        let at = Instant::now();
        let response = server
            .client_oneshot(chat_request(SMALL))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        loaded.push(at.elapsed());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let completed = load.await.expect("load");
    eprintln!("large requests completed in 3s: {completed}");
    summary("small, 8 concurrent large", loaded);
}
