#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use kanata::core::{ModelAlias, Operation, RouteSelector};
use kanata::keys::file::{KeysFile, StoredKey};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

struct Fixture {
    directory: PathBuf,
    child: Child,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}
fn keys(alias: &str, secret: &str) -> String {
    let mut file = KeysFile::default();
    file.push(StoredKey::new(
        "client".into(),
        Sha256::digest(secret.as_bytes()).into(),
        false,
        vec![RouteSelector {
            model_alias: ModelAlias(alias.into()),
            operation: Operation::Embeddings,
        }],
        None,
        None,
        kanata::keys::time::now(),
        None,
    ));
    file.render()
}
fn connect(port: u16) -> BufReader<TcpStream> {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    BufReader::new(stream)
}
fn get(stream: &mut BufReader<TcpStream>, path: &str, secret: Option<&str>) -> (u16, Value) {
    write!(
        stream.get_mut(),
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n"
    )
    .unwrap();
    if let Some(secret) = secret {
        write!(stream.get_mut(), "Authorization: Bearer {secret}\r\n").unwrap();
    }
    write!(stream.get_mut(), "\r\n").unwrap();
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut length = None;
    loop {
        line.clear();
        stream.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut bytes = vec![0; length.expect("bounded JSON response")];
    stream.read_exact(&mut bytes).unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn signal(child: &Child, name: &str) {
    assert!(
        Command::new("kill")
            .args([name, &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}
fn wait_status(port: u16, attempts: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let (status, value) = get(&mut BufReader::new(stream), "/status", None);
            if status == 200
                && value["configuration_reload"]["attempts"]
                    .as_u64()
                    .unwrap_or(0)
                    >= attempts
            {
                return value;
            }
        }
        assert!(Instant::now() < deadline, "reload did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn signal_reload_updates_keepalive_requests_rejects_invalid_changes_and_retargets_key_polls() {
    let directory = std::env::temp_dir().join(format!(
        "kanata-config-reload-process-{}",
        std::process::id()
    ));
    fs::create_dir_all(directory.join("keys")).unwrap();
    fs::create_dir_all(directory.join("state")).unwrap();
    let client = TcpListener::bind("127.0.0.1:0").unwrap();
    let client_port = client.local_addr().unwrap().port();
    let admin = TcpListener::bind("127.0.0.1:0").unwrap();
    let admin_port = admin.local_addr().unwrap().port();
    let initial = include_str!("../config/embeddings.example.toml")
        .replace("port = 8080", &format!("port = {client_port}"))
        .replace("port = 9090", &format!("port = {admin_port}"))
        .replace("local-embed", "before");
    fs::write(directory.join("config.toml"), &initial).unwrap();
    fs::write(directory.join("keys/keys.toml"), keys("before", "test-key")).unwrap();
    drop(client);
    drop(admin);
    let child = Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["serve", "--config"])
        .arg(directory.join("config.toml"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut fixture = Fixture { directory, child };
    let status = wait_status(admin_port, 0);
    assert_eq!(status["pid"], fixture.child.id());
    assert_eq!(status["configuration_reload"]["enabled"], true);
    let mut session = connect(client_port);
    let (code, before) = get(&mut session, "/v1/models", Some("test-key"));
    assert_eq!(code, 200);
    assert_eq!(before["data"][0]["id"], "before");
    let updated = initial.replace("before", "after");
    fs::write(
        fixture.directory.join("keys/keys.toml"),
        keys("after", "test-key"),
    )
    .unwrap();
    fs::write(fixture.directory.join("config.toml"), &updated).unwrap();
    signal(&fixture.child, "-HUP");
    let status = wait_status(admin_port, 1);
    assert_eq!(status["configuration_reload"]["generation"], 2);
    assert_eq!(status["configuration_reload"]["healthy"], true);
    let (code, after) = get(&mut session, "/v1/models", Some("test-key"));
    assert_eq!(code, 200);
    assert_eq!(after["data"][0]["id"], "after");
    fs::write(fixture.directory.join("config.toml"), "invalid TOML [").unwrap();
    signal(&fixture.child, "-HUP");
    let status = wait_status(admin_port, 2);
    assert_eq!(status["configuration_reload"]["generation"], 2);
    assert_eq!(status["configuration_reload"]["healthy"], false);
    assert_eq!(status["ready"], true);
    assert_eq!(
        get(&mut session, "/v1/models", Some("test-key")).1["data"][0]["id"],
        "after"
    );
    fs::write(
        fixture.directory.join("config.toml"),
        updated.replace("max_in_flight = 8", "max_in_flight = 9"),
    )
    .unwrap();
    signal(&fixture.child, "-HUP");
    let status = wait_status(admin_port, 3);
    assert_eq!(status["configuration_reload"]["generation"], 2);
    assert!(
        status["configuration_reload"]["last_error"]
            .as_str()
            .unwrap()
            .contains("restart_required")
    );
    fs::write(fixture.directory.join("config.toml"), &updated).unwrap();
    fs::write(
        fixture.directory.join("keys/keys.toml"),
        keys("after", "rotated-key"),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let (code, value) = get(&mut session, "/v1/models", Some("rotated-key"));
        if code == 200 {
            assert_eq!(value["data"][0]["id"], "after");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "key reload still validates the old routes"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(get(&mut session, "/v1/models", Some("test-key")).0, 401);
    signal(&fixture.child, "-TERM");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = fixture.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "shutdown did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
    let usage = kanata::keys::usage::read_merged(&fixture.directory.join("state"));
    assert!(usage["client"].requests >= 4);
}
