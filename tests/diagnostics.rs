#[path = "support/template.rs"]
mod template;
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
