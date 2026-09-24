use std::{
    fs,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
static SERVE_SMOKE_SERIAL: Mutex<()> = Mutex::new(());
const OWNER_KEY: &str = "SYNTHETIC_OWNER_KEY_FOR_TESTS_0123456789";
const RESTRICTED_KEY: &str = "SYNTHETIC_RESTRICTED_KEY_FOR_TESTS_9876543210";
const CHAT_RESPONSE: &str = include_str!("fixtures/vllm/chat-text-response.json");

struct FixtureDir(PathBuf);

impl FixtureDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kanata-serve-smoke-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create fixture directory");
        set_mode(&path, 0o700);
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write_key(&self, name: &str, value: &str) -> PathBuf {
        let path = self.path().join(name);
        if path.exists() {
            fs::remove_file(&path).expect("replace synthetic key");
        }
        fs::write(&path, format!("{value}\n")).expect("write synthetic key");
        set_mode(&path, 0o400);
        path
    }

    fn write_config(
        &self,
        name: &str,
        upstream: SocketAddr,
        public_bind: Option<IpAddr>,
        public_routes: &str,
    ) -> PathBuf {
        let client_port = free_port(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let admin_port =
            free_port_distinct(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), &[client_port]);
        let public_listener = public_bind.map_or(String::new(), |bind| {
            let port = free_port(bind);
            format!("[listeners.public]\nbind = \"{bind}\"\nport = {port}\n\n")
        });
        let owner_key = self.write_key("owner.key", OWNER_KEY);
        let restricted_key = self.write_key("restricted.key", RESTRICTED_KEY);
        let codex_state = self.path().join("codex-state");
        let contents = format!(
            r#"[listeners.client]
bind = "127.0.0.1"
port = {client_port}

{public_listener}[listeners.admin]
bind = "127.0.0.1"
port = {admin_port}

[publication]
tailnet_addresses = ["100.64.0.10"]
public_routes = {public_routes}

[codex_auth]
store = "file"
state_dir = "{}"

[[adapters]]
id = "fixture-vllm"
kind = "vllm"
base_url = "http://{upstream}/v1"
trust_zone = "local"
transcription_mode = "audio_chat"
[adapters.capabilities]
operations = ["chat", "transcription"]
streaming_chat = false
function_tools = false
input_audio = true

[[adapters]]
id = "fixture-codex"
kind = "codex"
base_url = "https://chatgpt.com/backend-api/codex"
trust_zone = "external"
[adapters.capabilities]
operations = ["chat"]
streaming_chat = true
function_tools = true

[[routes]]
id = "fixture-chat-route"
model_alias = "fixture-chat"
operation = "chat"
adapter_id = "fixture-vllm"
upstream_id = "fixture-chat-upstream"
requires_streaming_chat = false
requires_function_tools = false
allows_input_audio = true

[[routes]]
id = "fixture-asr-route"
model_alias = "fixture-asr"
operation = "transcription"
adapter_id = "fixture-vllm"
upstream_id = "fixture-asr-upstream"
requires_streaming_chat = false
requires_function_tools = false

[[routes]]
id = "fixture-codex-route"
model_alias = "fixture-codex"
operation = "chat"
adapter_id = "fixture-codex"
upstream_id = "fixture-codex-upstream"
requires_streaming_chat = true
requires_function_tools = true

[[application_keys]]
id = "fixture-owner"
secret_ref = "file:{}"
owner = true
permissions = [
  {{ model_alias = "fixture-chat", operation = "chat" }},
  {{ model_alias = "fixture-asr", operation = "transcription" }},
  {{ model_alias = "fixture-codex", operation = "chat" }},
]

[[application_keys]]
id = "fixture-restricted"
secret_ref = "file:{}"
permissions = [
  {{ model_alias = "fixture-chat", operation = "chat" }},
  {{ model_alias = "fixture-asr", operation = "transcription" }},
]

[limits]
max_queue = 8
max_in_flight = 4
max_body_bytes = 1048576
max_audio_bytes = 26214400
max_extension_bytes = 8192

[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#,
            toml_path(&codex_state),
            toml_path(&owner_key),
            toml_path(&restricted_key),
        );
        let path = self.path().join(name);
        fs::write(&path, contents).expect("write fixture config");
        set_mode(&path, 0o600);
        path
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct MockUpstream {
    address: SocketAddr,
    requests: mpsc::Receiver<String>,
    held: Arc<AtomicBool>,
    released: Arc<(Mutex<bool>, Condvar)>,
    stop: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl MockUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local mock upstream");
        listener
            .set_nonblocking(true)
            .expect("nonblocking mock listener");
        let address = listener.local_addr().expect("mock upstream address");
        let (request_tx, requests) = mpsc::channel();
        let held = Arc::new(AtomicBool::new(false));
        let released = Arc::new((Mutex::new(true), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_held = held.clone();
        let thread_released = released.clone();
        let thread_stop = stop.clone();
        let accept_thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let request_tx = request_tx.clone();
                        let held = thread_held.clone();
                        let released = thread_released.clone();
                        thread::spawn(move || {
                            serve_mock_request(stream, request_tx, held, released);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            requests,
            held,
            released,
            stop,
            accept_thread: Some(accept_thread),
        }
    }

    fn next_request(&self) -> String {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .expect("mock upstream request")
    }

    fn assert_no_request(&self) {
        assert!(matches!(
            self.requests.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    fn hold_responses(&self) {
        *self.released.0.lock().expect("release lock") = false;
        self.held.store(true, Ordering::Release);
    }

    fn release_responses(&self) {
        self.held.store(false, Ordering::Release);
        *self.released.0.lock().expect("release lock") = true;
        self.released.1.notify_all();
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.release_responses();
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_mock_request(
    mut stream: TcpStream,
    requests: mpsc::Sender<String>,
    held: Arc<AtomicBool>,
    released: Arc<(Mutex<bool>, Condvar)>,
) {
    let Ok(request) = read_http_request(&mut stream) else {
        return;
    };
    let _ = requests.send(request);
    if held.load(Ordering::Acquire) {
        let mut ready = released.0.lock().expect("release lock");
        while !*ready {
            ready = released.1.wait(ready).expect("release wait");
        }
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        CHAT_RESPONSE.len(),
        CHAT_RESPONSE
    );
    let _ = stream.write_all(response.as_bytes());
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "incomplete fixture request",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
}

struct RunningServer {
    child: Child,
}

impl RunningServer {
    fn start(config: &Path, admin: SocketAddr) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_kanata"))
            .args(["serve", "--config"])
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start kanata serve fixture");
        let mut server = Self { child };
        server.wait_ready(admin);
        server
    }

    fn wait_ready(&mut self, admin: SocketAddr) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll fixture server") {
                panic!(
                    "fixture server exited during startup ({status}): {}",
                    self.stderr()
                );
            }
            if exchange_if_ready(admin) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "fixture server did not become ready"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        {
            let status = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status()
                .expect("send SIGTERM to fixture server");
            assert!(status.success(), "SIGTERM delivery succeeded");
        }
        #[cfg(not(unix))]
        self.child.kill().expect("stop fixture server");
    }

    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll fixture server exit") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "fixture server did not stop in time"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stderr(&mut self) -> String {
        let mut output = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut output);
        }
        output
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: String,
}

fn exchange(address: SocketAddr, request: &str) -> HttpResponse {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .unwrap_or_else(|error| panic!("connect to fixture listener {address}: {error}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set fixture read timeout");
    stream
        .write_all(request.as_bytes())
        .expect("write fixture request");
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .expect("read fixture response");
    parse_http_response(&bytes)
}

fn exchange_if_ready(address: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(100)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    if stream
        .write_all(b"GET /live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).is_ok() && bytes.starts_with(b"HTTP/1.1 200")
}

fn parse_http_response(bytes: &[u8]) -> HttpResponse {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response headers")
        + 4;
    let headers = std::str::from_utf8(&bytes[..header_end]).expect("response header utf8");
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("response status");
    HttpResponse {
        status,
        body: String::from_utf8(bytes[header_end..].to_vec()).expect("response body utf8"),
    }
}

fn models_request(address: SocketAddr, key: &str) -> HttpResponse {
    exchange(
        address,
        &format!(
            "GET /v1/models HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {key}\r\nConnection: close\r\n\r\n"
        ),
    )
}

fn chat_request(address: SocketAddr, key: &str, model: &str) -> HttpResponse {
    let body = format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"fixture"}}],"stream":false}}"#
    );
    exchange(
        address,
        &format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn transcription_request(address: SocketAddr, key: &str) -> HttpResponse {
    const BOUNDARY: &str = "kanata-serve-smoke-boundary";
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nfixture-asr\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fixture.wav\"\r\nContent-Type: audio/wav\r\n\r\nfixture-audio\r\n--{BOUNDARY}--\r\n"
    );
    exchange(
        address,
        &format!(
            "POST /v1/audio/transcriptions HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {key}\r\nContent-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn admin_request(address: SocketAddr, path: &str) -> HttpResponse {
    client_path_request(address, path, None)
}

fn client_path_request(address: SocketAddr, path: &str, key: Option<&str>) -> HttpResponse {
    let authorization = key
        .map(|key| format!("Authorization: Bearer {key}\r\n"))
        .unwrap_or_default();
    exchange(
        address,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\n{authorization}Connection: close\r\n\r\n"
        ),
    )
}

fn model_ids(response: &HttpResponse) -> Vec<String> {
    assert_eq!(response.status, 200, "{}", response.body);
    serde_json::from_str::<serde_json::Value>(&response.body).expect("models JSON")["data"]
        .as_array()
        .expect("models data")
        .iter()
        .map(|model| model["id"].as_str().expect("model id").to_owned())
        .collect()
}

fn run_serve_until_exit(config: &Path, timeout: Duration) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kanata"))
        .args(["serve", "--config"])
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start failing serve fixture");
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .expect("poll failing serve fixture")
            .is_some()
        {
            return child
                .wait_with_output()
                .expect("collect failing serve output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect stuck serve output");
            panic!(
                "serve did not fail closed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn start_config(
    fixture: &FixtureDir,
    name: &str,
    upstream: SocketAddr,
    public_bind: Option<IpAddr>,
    public_routes: &str,
) -> (PathBuf, SocketAddr, Option<SocketAddr>, SocketAddr) {
    let path = fixture.write_config(name, upstream, public_bind, public_routes);
    let config = kanata::config::load(&path).expect("fixture config validates");
    let plan = kanata::server::ServerPlan::from_validated(&config).expect("listener plan");
    (
        path,
        plan.client_addr(),
        plan.public_addr(),
        plan.admin_addr(),
    )
}

fn free_port(bind: IpAddr) -> u16 {
    TcpListener::bind(SocketAddr::new(bind, 0))
        .expect("reserve fixture port")
        .local_addr()
        .expect("reserved port")
        .port()
}

fn free_port_distinct(bind: IpAddr, used: &[u16]) -> u16 {
    loop {
        let port = free_port(bind);
        if !used.contains(&port) {
            return port;
        }
    }
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set fixture permissions");
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}

#[test]
fn private_serve_binds_routes_keys_and_admin_separately() {
    let _serial = SERVE_SMOKE_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = FixtureDir::new();
    let upstream = MockUpstream::start();
    let (config, client, _, admin) =
        start_config(&fixture, "private.toml", upstream.address, None, "[]");
    let state_dir = fixture.path().join("codex-state");
    let mut server = RunningServer::start(&config, admin);
    thread::sleep(Duration::from_millis(50));
    upstream.assert_no_request();
    assert!(
        !state_dir.exists(),
        "startup does not inspect or create Codex storage"
    );

    let owner_models = model_ids(&models_request(client, OWNER_KEY));
    assert_eq!(
        owner_models,
        ["fixture-asr", "fixture-chat", "fixture-codex"]
    );
    let restricted_models = model_ids(&models_request(client, RESTRICTED_KEY));
    assert_eq!(restricted_models, ["fixture-asr", "fixture-chat"]);

    assert_eq!(
        chat_request(client, RESTRICTED_KEY, "fixture-codex").status,
        403
    );
    assert_eq!(chat_request(client, OWNER_KEY, "fixture-codex").status, 503);
    upstream.assert_no_request();

    let chat = chat_request(client, RESTRICTED_KEY, "fixture-chat");
    assert_eq!(chat.status, 200, "{}", chat.body);
    assert!(chat.body.contains("fixture response"));
    assert!(upstream.next_request().contains("fixture-chat-upstream"));
    assert_eq!(admin_request(client, "/live").status, 404);
    assert_eq!(admin_request(client, "/metrics").status, 404);
    assert_eq!(admin_request(admin, "/v1/models").status, 404);
    assert_eq!(admin_request(admin, "/live").status, 200);

    server.terminate();
    assert!(server.wait_exit(Duration::from_secs(5)).success());
}

#[test]
fn startup_secret_failures_are_redacted_before_binding() {
    let _serial = SERVE_SMOKE_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let fixture = FixtureDir::new();
    let upstream = MockUpstream::start();
    let (config, client, _, admin) = start_config(
        &fixture,
        "missing-secret.toml",
        upstream.address,
        None,
        "[]",
    );
    let owner_path = fixture.path().join("owner.key");
    let missing_path = fixture.path().join("missing-owner.key");
    let contents = fs::read_to_string(&config).expect("read fixture config");
    let contents = contents.replace(&toml_path(&owner_path), &toml_path(&missing_path));
    assert_ne!(
        contents,
        fs::read_to_string(&config).expect("read original config")
    );
    fs::write(&config, contents).expect("remove configured key fixture");
    set_mode(&config, 0o600);

    let output = run_serve_until_exit(&config, Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.trim(), "server initialization failed");
    assert!(!stderr.contains(&missing_path.to_string_lossy().to_string()));
    assert!(!stderr.contains(OWNER_KEY));
    assert!(
        TcpListener::bind(client).is_ok(),
        "private bind was not opened"
    );
    assert!(
        TcpListener::bind(admin).is_ok(),
        "admin bind was not opened"
    );
    upstream.assert_no_request();
}

#[cfg(unix)]
#[test]
#[ignore = "requires KANATA_TEST_PUBLIC_BIND set to an assigned non-loopback private IP; run explicitly with --ignored"]
fn public_serve_enforces_exact_routes_and_drains_both_client_listeners() {
    let _serial = SERVE_SMOKE_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let public_bind = std::env::var("KANATA_TEST_PUBLIC_BIND")
        .expect("set KANATA_TEST_PUBLIC_BIND to an assigned private non-loopback address");
    let public_bind = public_bind.parse::<IpAddr>().expect("public fixture IP");
    assert!(!public_bind.is_loopback() && !public_bind.is_unspecified());

    let fixture = FixtureDir::new();
    let upstream = MockUpstream::start();
    let empty_routes = "[]";
    let (empty_config, _client, public, admin) = start_config(
        &fixture,
        "public-empty.toml",
        upstream.address,
        Some(public_bind),
        empty_routes,
    );
    let public = public.expect("public listener plan");
    let mut empty_server = RunningServer::start(&empty_config, admin);
    thread::sleep(Duration::from_millis(50));
    upstream.assert_no_request();
    assert_eq!(client_path_request(public, "/v1/models", None).status, 403);
    assert_eq!(
        model_ids(&models_request(public, OWNER_KEY)),
        Vec::<String>::new()
    );
    assert_eq!(chat_request(public, OWNER_KEY, "fixture-chat").status, 403);
    assert_eq!(transcription_request(public, OWNER_KEY).status, 403);
    assert_eq!(chat_request(public, OWNER_KEY, "fixture-codex").status, 403);
    assert_eq!(admin_request(public, "/live").status, 403);
    assert_eq!(
        client_path_request(public, "/live", Some(OWNER_KEY)).status,
        404
    );
    assert_eq!(
        client_path_request(public, "/metrics", Some(OWNER_KEY)).status,
        404
    );
    assert_eq!(admin_request(admin, "/v1/models").status, 404);
    upstream.assert_no_request();
    empty_server.terminate();
    assert!(empty_server.wait_exit(Duration::from_secs(5)).success());

    let routes = r#"[{ model_alias = "fixture-chat", operation = "chat" }, { model_alias = "fixture-asr", operation = "transcription" }]"#;
    let (config, client, public, admin) = start_config(
        &fixture,
        "public-allowlist.toml",
        upstream.address,
        Some(public_bind),
        routes,
    );
    let public = public.expect("public listener plan");
    let mut server = RunningServer::start(&config, admin);
    thread::sleep(Duration::from_millis(50));
    upstream.assert_no_request();
    let public_models = model_ids(&models_request(public, OWNER_KEY));
    assert_eq!(public_models, ["fixture-asr", "fixture-chat"]);
    let chat = chat_request(public, RESTRICTED_KEY, "fixture-chat");
    assert_eq!(chat.status, 200, "{}", chat.body);
    assert!(upstream.next_request().contains("fixture-chat-upstream"));
    let transcription = transcription_request(public, OWNER_KEY);
    assert_eq!(transcription.status, 200, "{}", transcription.body);
    assert!(transcription.body.contains("fixture response"));
    assert!(upstream.next_request().contains("fixture-asr-upstream"));

    assert_eq!(chat_request(public, OWNER_KEY, "fixture-codex").status, 403);
    assert_eq!(chat_request(client, OWNER_KEY, "fixture-codex").status, 503);
    upstream.assert_no_request();
    assert_eq!(admin_request(client, "/live").status, 404);
    assert_eq!(admin_request(public, "/live").status, 403);
    assert_eq!(
        client_path_request(public, "/live", Some(OWNER_KEY)).status,
        404
    );
    assert_eq!(admin_request(admin, "/live").status, 200);

    upstream.hold_responses();
    let private_addr = client;
    let public_addr = public;
    let private_request =
        thread::spawn(move || chat_request(private_addr, RESTRICTED_KEY, "fixture-chat"));
    let public_request =
        thread::spawn(move || chat_request(public_addr, OWNER_KEY, "fixture-chat"));
    assert!(upstream.next_request().contains("fixture-chat-upstream"));
    assert!(upstream.next_request().contains("fixture-chat-upstream"));

    server.terminate();
    thread::sleep(Duration::from_millis(100));
    assert!(
        server
            .child
            .try_wait()
            .expect("poll draining server")
            .is_none()
    );
    upstream.release_responses();
    assert_eq!(
        private_request
            .join()
            .expect("private request thread")
            .status,
        200
    );
    assert_eq!(
        public_request.join().expect("public request thread").status,
        200
    );
    assert!(server.wait_exit(Duration::from_secs(5)).success());
}
