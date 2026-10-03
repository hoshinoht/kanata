#[path = "support/template.rs"]
mod template;
#[path = "adapter_ollama/support.rs"]
mod upstream;
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
};

fn run(command: &str, status: u16, body: &'static str) -> std::process::Output {
    let fixture = template::Template::new("personal.example.toml");
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("address").port();
    let config = std::fs::read_to_string(&fixture.0)
        .expect("config")
        .replace("port = 9090", &format!("port = {port}"));
    std::fs::write(&fixture.0, config).expect("config");
    let expected = if command == "health" {
        "/ready"
    } else {
        "/status"
    };
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("timeout");
        let mut request = [0; 1024];
        let n = socket.read(&mut request).expect("request");
        assert!(String::from_utf8_lossy(&request[..n]).starts_with(&format!("GET {expected} ")));
        write!(
            socket,
            "HTTP/1.0 {status} fixture\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .expect("response");
    });
    let output = Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args([command, "--config"])
        .arg(&fixture.0)
        .output()
        .expect("CLI");
    server.join().expect("server");
    output
}

#[test]
fn native_health_checks_http_readiness_and_doctor_reads_reload_status() {
    assert!(run("health", 200, "ready\n").status.success());
    assert!(!run("health", 503, "not_ready\n").status.success());
    let report = run(
        "doctor",
        200,
        r#"{"ready":true,"key_reload":{"enabled":true,"healthy":false,"failures":2}}"#,
    );
    assert!(report.status.success());
    let text = String::from_utf8(report.stdout).expect("text");
    assert!(text.contains("gateway ready: true"));
    assert!(text.contains("\"healthy\":false"));
    assert!(!text.contains("TCP"));
}

fn probe_config(address: &str, chat: bool) -> template::Template {
    let fixture = template::Template::new("embeddings.example.toml");
    let unused = TcpListener::bind("127.0.0.1:0").expect("unused admin port");
    let port = unused.local_addr().expect("admin address").port();
    let mut config = std::fs::read_to_string(&fixture.0)
        .expect("config")
        .replace("http://127.0.0.1:11434/v1", &format!("http://{address}/v1"))
        .replace("port = 9090", &format!("port = {port}"));
    if chat {
        config = config
            .replace("operations = [\"embeddings\"]", "operations = [\"chat\"]")
            .replace("operation = \"embeddings\"", "operation = \"chat\"")
            .replace("streaming_chat = false", "streaming_chat = true")
            .replace(
                "function_tools = false\n\n[[routes]]",
                "function_tools = false\nsampling_controls = true\n\n[[routes]]",
            );
    }
    std::fs::write(&fixture.0, config).expect("write config");
    fixture
}

async fn probe(fixture: &template::Template, flags: &[&str]) -> Result<String, String> {
    let mut args = vec![
        "doctor".into(),
        "--config".into(),
        fixture.0.to_string_lossy().into_owned(),
    ];
    args.extend(flags.iter().map(|flag| (*flag).to_owned()));
    kanata::cli::run_async(args)
        .await
        .map(|output| output.expect("doctor report"))
}

#[tokio::test]
async fn model_discovery_reports_only_configured_aliases_and_rejects_invalid_catalogs() {
    for (body, expected) in [
        (
            r#"{"data":[{"id":"replace-with-installed-embedding-model"},{"id":"private-inventory-marker"}]}"#,
            "listed",
        ),
        (
            r#"{"data":[{"id":"private-inventory-marker"}]}"#,
            "not_listed",
        ),
        (
            r#"{"data":[{"id":"duplicate"},{"id":"duplicate"}]}"#,
            "invalid_response",
        ),
        (r#"{"error":"secret-provider-message"}"#, "invalid_response"),
    ] {
        let server = upstream::MockServer::once(upstream::ResponseSpec::json(body)).await;
        let fixture = probe_config(&server.address, false);
        let report = probe(&fixture, &["--probe-models"]).await.expect("report");
        assert!(
            report.contains(&format!("model local-embed/embeddings: {expected}")),
            "{report}"
        );
        for hidden in [
            "private-inventory-marker",
            "secret-provider-message",
            "replace-with-installed-embedding-model",
            &server.address,
        ] {
            assert!(!report.contains(hidden));
        }
        let requests = server.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/v1/models");
        assert!(requests[0].body.is_empty());
        assert!(!requests[0].headers.contains_key("authorization"));
    }
}

#[tokio::test]
async fn inference_probe_requires_exact_selector_and_a_valid_terminal_response() {
    let embedding =
        r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.5,0.25]}]}"#;
    let server = upstream::MockServer::once(upstream::ResponseSpec::json(embedding)).await;
    let fixture = probe_config(&server.address, false);
    assert!(
        probe(&fixture, &["--probe-inference", "local-embed"])
            .await
            .is_err()
    );
    assert!(
        probe(
            &fixture,
            &["--probe-inference", "local-embed", "--operation", "chat"]
        )
        .await
        .is_err()
    );
    assert_eq!(server.once.load(std::sync::atomic::Ordering::SeqCst), 0);
    let report = probe(
        &fixture,
        &[
            "--probe-inference",
            "local-embed",
            "--operation",
            "embeddings",
        ],
    )
    .await
    .expect("report");
    assert!(report.contains("verified embeddings_completed"), "{report}");
    {
        let requests = server.requests.lock().expect("requests");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json");
        assert_eq!(body["input"], serde_json::json!(["Kanata diagnostic"]));
    }

    for (stream, success) in [
        (upstream::TEXT_STREAM, true),
        (
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"unverified-provider-text\"},\"finish_reason\":null}]}\n\n",
            false,
        ),
    ] {
        let server =
            upstream::MockServer::once(upstream::ResponseSpec::event_stream(stream, 1024)).await;
        let fixture = probe_config(&server.address, true);
        let report = probe(
            &fixture,
            &["--probe-inference", "local-embed", "--operation", "chat"],
        )
        .await
        .expect("report");
        assert_eq!(
            report.contains("verified chat_stream_completed"),
            success,
            "{report}"
        );
        assert!(!report.contains("unverified-provider-text"));
        let requests = server.requests.lock().expect("requests");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json");
        assert_eq!(body["max_tokens"], 16);
    }
}
