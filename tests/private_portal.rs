use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Fixture {
    dir: PathBuf,
    child: Child,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn host_portal_binds_loopback_and_requires_its_browser_session() {
    let dir = std::env::temp_dir().join(format!("kanata-portal-socket-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        include_str!("../config/embeddings.example.toml"),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["portal", "--config"])
        .arg(dir.join("config.toml"))
        .args(["--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut fixture = Fixture { dir, child };
    let stdout = fixture.child.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        let first = lines.next().unwrap().unwrap();
        let second = lines.next().unwrap().unwrap();
        send.send((first, second)).unwrap();
    });
    let (address, code) = receive
        .recv_timeout(Duration::from_secs(10))
        .expect("portal startup");
    let origin = address.strip_prefix("Private key portal: ").unwrap();
    assert!(origin.starts_with("http://127.0.0.1:"));
    let authority = origin.strip_prefix("http://").unwrap();
    let code = code.split(": ").nth(1).unwrap();
    let page = request(authority, "GET / HTTP/1.1", &[], "");
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("Your keys."));
    assert!(!page.contains(code));
    let headers = [
        ("Origin", origin),
        ("X-Kanata-Portal", "1"),
        ("Content-Type", "application/json"),
    ];
    let denied = request(authority, "POST /api/snapshot HTTP/1.1", &headers, "{}");
    assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
    let login = request(
        authority,
        "POST /api/login HTTP/1.1",
        &headers,
        &serde_json::json!({"code":code}).to_string(),
    );
    assert!(login.starts_with("HTTP/1.1 200"), "{login}");
    let body = login.split("\r\n\r\n").nth(1).unwrap();
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    let authorization = format!("Bearer {}", value["session"].as_str().unwrap());
    let mut headers = headers.to_vec();
    headers.push(("Authorization", &authorization));
    let snapshot = request(authority, "POST /api/snapshot HTTP/1.1", &headers, "{}");
    assert!(snapshot.starts_with("HTTP/1.1 200"), "{snapshot}");
    assert!(snapshot.contains("local-embed"));
    headers[0] = ("Origin", "http://attacker.test");
    let denied = request(authority, "POST /api/snapshot HTTP/1.1", &headers, "{}");
    assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");
    assert!(!fixture.dir.join("keys/keys.toml").exists());
}

fn request(authority: &str, start: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut socket = TcpStream::connect(authority).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        socket,
        "{start}\r\nHost: {authority}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    )
    .unwrap();
    for (key, value) in headers {
        write!(socket, "{key}: {value}\r\n").unwrap();
    }
    write!(socket, "\r\n{body}").unwrap();
    let mut reader = BufReader::new(socket);
    let mut response = String::new();
    let mut length = None;
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "incomplete response head"
        );
        response.push_str(&line);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; length.expect("bounded portal response")];
    reader.read_exact(&mut body).unwrap();
    response.push_str(std::str::from_utf8(&body).unwrap());
    response
}
