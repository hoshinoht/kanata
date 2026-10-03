#[path = "support/gateway.rs"]
mod support;
#[path = "adapter_ollama/support.rs"]
mod upstream;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use kanata::{
    adapter::speech::SpeechAdapter,
    config,
    server::{Readiness, TwoPlaneServer},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{
    adapter_spec, capabilities, chat_outcome, models_request, recorded_len, response_json,
    server_with,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
fn config_text(address: &str) -> String {
    format!(
        r#"
[listeners.client]
bind = "127.0.0.1"
port = 18080
[listeners.public]
bind = "172.30.0.3"
port = 18081
[listeners.admin]
bind = "127.0.0.1"
port = 19090
[publication]
tailnet_addresses = ["100.64.0.10"]
public_routes = [{{ model_alias = "speaker", operation = "speech" }}]
[[adapters]]
id = "speaker-adapter"
kind = "speech"
base_url = "http://{address}/v1"
trust_zone = "local"
[adapters.capabilities]
operations = ["speech"]
streaming_chat = false
function_tools = false
[[routes]]
id = "speaker-route"
model_alias = "speaker"
operation = "speech"
adapter_id = "speaker-adapter"
upstream_id = "kokoro-fixture"
requires_streaming_chat = false
requires_function_tools = false
speech_voices = ["af_heart", "af_sky"]
speech_formats = ["mp3", "wav"]
[[application_keys]]
id = "speaker-client"
secret_ref = "env:SPEECH_CLIENT_KEY"
permissions = [{{ model_alias = "speaker", operation = "speech" }}]
[limits]
max_queue = 4
max_in_flight = 2
max_body_bytes = 1048576
max_audio_bytes = 1048576
max_extension_bytes = 1024
[timeouts]
queue_ms = 1000
connect_ms = 1000
headers_ms = 1000
first_byte_ms = 1000
idle_ms = 1000
overall_ms = 5000
"#
    )
}

fn load(text: &str) -> Result<config::ValidatedConfig, config::ConfigError> {
    let path = std::env::temp_dir().join(format!(
        "kanata-speech-{}-{}.toml",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, text).unwrap();
    let result = config::load(&path);
    std::fs::remove_file(path).unwrap();
    result
}

fn request(body: Value) -> Request<Body> {
    Request::post("/v1/audio/speech")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer test-key")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn body(format: &str) -> Value {
    json!({"model":"speaker","input":"Hello from the fixture.","voice":"af_heart","response_format":format,"speed":1.25})
}

fn wav() -> Vec<u8> {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&24000u32.to_le_bytes());
    bytes.extend_from_slice(&48000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes
}
fn mp3() -> Vec<u8> {
    let mut bytes = vec![0xff, 0xfb, 0x90, 0x64];
    bytes.resize(417, 0);
    bytes
}
fn audio(format: &str) -> upstream::ResponseSpec {
    let mut spec = upstream::ResponseSpec::json("");
    spec.body = if format == "mp3" { mp3() } else { wav() };
    spec.content_type = Some(if format == "mp3" {
        "audio/mpeg"
    } else {
        "audio/wav"
    });
    spec
}
fn actual(config: &config::ValidatedConfig) -> TwoPlaneServer {
    let adapter =
        SpeechAdapter::from_config(config, "speaker-adapter", &support::Resolver).unwrap();
    TwoPlaneServer::from_validated_with_adapters(
        config,
        &support::Resolver,
        Readiness::new(true),
        vec![Arc::new(adapter)],
    )
    .unwrap()
}
fn recording(config: &config::ValidatedConfig) -> support::ServerWithRequests {
    server_with(
        config,
        vec![adapter_spec(
            "speaker-adapter",
            capabilities(config, "speaker-adapter"),
            chat_outcome("unused"),
        )],
    )
}

fn synthetic(config: &config::ValidatedConfig) -> (TwoPlaneServer, Arc<AtomicUsize>) {
    struct SpeechReply {
        capabilities: kanata::core::Capabilities,
        calls: Arc<AtomicUsize>,
    }
    impl kanata::adapter::Adapter for SpeechReply {
        fn id(&self) -> &str {
            "speaker-adapter"
        }
        fn capabilities(&self) -> &kanata::core::Capabilities {
            &self.capabilities
        }
        fn execute(&self, request: kanata::core::RoutedRequest) -> kanata::adapter::AdapterFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let model = request.request().model_alias().clone();
            Box::pin(async move {
                Ok(kanata::adapter::AdapterOutput::Complete(
                    kanata::core::Response::Speech(kanata::core::SpeechResponse {
                        model,
                        format: kanata::core::SpeechFormat::Wav,
                        bytes: wav(),
                    }),
                ))
            })
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let adapter = SpeechReply {
        capabilities: capabilities(config, "speaker-adapter"),
        calls: calls.clone(),
    };
    (
        TwoPlaneServer::from_validated_with_adapters(
            config,
            &support::Resolver,
            Readiness::new(true),
            vec![Arc::new(adapter)],
        )
        .unwrap(),
        calls,
    )
}

#[tokio::test]
async fn speech_admission_stays_with_the_output_body_and_releases_on_drop() {
    let config = load(
        &config_text("127.0.0.1:9")
            .replace("max_in_flight = 2", "max_in_flight = 1")
            .replace("queue_ms = 1000", "queue_ms = 5"),
    )
    .unwrap();
    let (server, calls) = synthetic(&config);
    let held = server.client_oneshot(request(body("wav"))).await.unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    let refused = server.client_oneshot(request(body("wav"))).await.unwrap();
    assert_eq!(
        response_json(refused).await["error"]["code"],
        "gateway_busy"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(held);
    let response = server.client_oneshot(request(body("wav"))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(support::response_body(response).await.as_ref(), wav());
}

#[tokio::test]
async fn speech_usage_retains_estimates_when_no_token_metadata_is_returned() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "kanata-speech-usage-{:016x}",
        getrandom::u64().unwrap()
    ));
    std::fs::create_dir(&dir).unwrap();
    std::fs::create_dir(dir.join("keys")).unwrap();
    std::fs::create_dir(dir.join("state")).unwrap();
    let hex: String = Sha256::digest(b"test-key")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let key = format!(
        "version = 1\n[[keys]]\nid = \"reader\"\ndigest = \"sha256:{hex}\"\ncreated_at = \"2026-01-01T00:00:00Z\"\ndaily_quota = {{ requests = 2, tokens = 100, reservation_tokens = 40 }}\npermissions = [{{ model_alias = \"local-speech\", operation = \"speech\" }}]\n"
    );
    let path = dir.join("keys/keys.toml");
    std::fs::write(&path, key).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let text =
        include_str!("../config/speech.example.toml").replace("speech-local", "speaker-adapter");
    std::fs::write(dir.join("config.toml"), text).unwrap();
    let config = config::load(dir.join("config.toml")).unwrap();
    let (server, calls) = synthetic(&config);
    let usage = server.open_usage(config::Plane::Private).unwrap();
    let mut body = body("wav");
    body["model"] = json!("local-speech");
    for _ in 0..2 {
        let response = server.client_oneshot(request(body.clone())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(support::response_body(response).await.as_ref(), wav());
    }
    let refused = server.client_oneshot(request(body)).await.unwrap();
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        usage.flush_now(),
        kanata::keys::usage::FlushOutcome::Written
    );
    let recorded = kanata::keys::usage::read_merged(&dir.join("state"));
    assert_eq!(recorded["reader"].tokens.reported, 0);
    assert_eq!(recorded["reader"].tokens.missing, 2);
    let daily = kanata::keys::quota::DailyLedger::new(&dir.join("state"), config::Plane::Private)
        .read()
        .unwrap();
    assert_eq!(daily[0].operation, "speech");
    assert_eq!(daily[0].requests, 2);
    assert_eq!(daily[0].charged_tokens, 80);
    assert_eq!(daily[0].retained_reservation_tokens, 80);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn binary_speech_uses_exact_upstream_and_declared_voice_and_format_on_both_planes() {
    for format in ["wav", "mp3"] {
        for public in [false, true] {
            let mut mock = upstream::MockServer::once(audio(format)).await;
            let config = load(&config_text(&mock.address)).unwrap();
            let server = actual(&config);
            let response = if public {
                server.public_oneshot(request(body(format))).await.unwrap()
            } else {
                server.client_oneshot(request(body(format))).await.unwrap()
            };
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                if format == "mp3" {
                    "audio/mpeg"
                } else {
                    "audio/wav"
                }
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(
                support::response_body(response).await.as_ref(),
                if format == "mp3" { mp3() } else { wav() }
            );
            mock.finish().await;
            let records = mock.requests.lock().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].path, "/v1/audio/speech");
            let sent: Value = serde_json::from_slice(&records[0].body).unwrap();
            assert_eq!(
                sent,
                json!({"model":"kokoro-fixture","input":"Hello from the fixture.","voice":"af_heart","response_format":format,"speed":1.25,"stream":false,"allow_voice_tags":false})
            );
        }
    }
}

#[tokio::test]
async fn speech_discovery_is_scoped_and_unknown_options_or_voice_never_dispatch() {
    let config = load(&config_text("127.0.0.1:9")).unwrap();
    let (server, requests) = recording(&config);
    let models = response_json(server.public_oneshot(models_request()).await.unwrap()).await;
    assert_eq!(models["data"][0]["kanata"]["operations"], json!(["speech"]));
    assert_eq!(
        models["data"][0]["kanata"]["speech"]["voices"],
        json!(["af_heart", "af_sky"])
    );
    assert_eq!(
        models["data"][0]["kanata"]["speech"]["formats"],
        json!(["mp3", "wav"])
    );
    assert!(models["data"][0]["kanata"].get("responses").is_none());
    for change in [
        json!({"input":""}),
        json!({"input":"[pause:9999999999s]"}),
        json!({"input":"[VOICE:other]"}),
        json!({"input":"a".repeat(4097)}),
        json!({"voice":"other"}),
        json!({"voice":"af_heart+af_sky"}),
        json!({"response_format":"flac"}),
        json!({"speed":0.2}),
        json!({"speed":4.1}),
        json!({"speed":null}),
        json!({"stream":true}),
        json!({"instructions":"Change speaker"}),
        json!({"return_download_link":true}),
    ] {
        let mut body = body("wav");
        body.as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        assert_eq!(
            server.client_oneshot(request(body)).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    let mut unauth = request(body("wav"));
    unauth.headers_mut().remove(header::AUTHORIZATION);
    assert_eq!(
        server.client_oneshot(unauth).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let mut other = body("wav");
    other["model"] = json!("unlisted");
    assert_eq!(
        server
            .public_oneshot(request(other))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(recorded_len(&requests), 0);
    let denied = config_text("127.0.0.1:9").replace(
        "public_routes = [{ model_alias = \"speaker\", operation = \"speech\" }]",
        "public_routes = []",
    );
    let config = load(&denied).unwrap();
    let (server, requests) = recording(&config);
    assert_eq!(
        server
            .public_oneshot(request(body("wav")))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        response_json(server.public_oneshot(models_request()).await.unwrap()).await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(recorded_len(&requests), 0);
}

#[tokio::test]
async fn speech_rejects_malformed_mismatched_and_oversized_upstream_audio() {
    let mut variants = Vec::new();
    let mut bad = audio("wav");
    bad.content_type = Some("application/json");
    variants.push(bad);
    let mut bad = audio("wav");
    bad.body = b"{\"error\":\"secret fixture\"}".to_vec();
    variants.push(bad);
    let mut bad = audio("wav");
    bad.body.pop();
    variants.push(bad);
    let mut bad = audio("wav");
    bad.body = vec![0; kanata::core::MAX_SPEECH_RESPONSE_BYTES + 1];
    variants.push(bad);
    for spec in variants {
        let mock = upstream::MockServer::once(spec).await;
        let server = actual(&load(&config_text(&mock.address)).unwrap());
        let response = server.client_oneshot(request(body("wav"))).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(
            !response_json(response)
                .await
                .to_string()
                .contains("secret fixture")
        );
    }
}

#[tokio::test]
async fn speech_cancellation_closes_upstream_and_releases_input_and_admission() {
    let mut spec = audio("wav");
    spec.wait_for_close = true;
    let mut mock = upstream::MockServer::once(spec).await;
    let server = Arc::new(actual(&load(&config_text(&mock.address)).unwrap()));
    let worker = server.clone();
    let task = tokio::spawn(async move { worker.client_oneshot(request(body("wav"))).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        mock.wait_for_response_headers(),
    )
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(3), mock.wait_for_close())
        .await
        .unwrap();
    mock.finish().await;
    let metrics = server
        .admin_oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let metrics = String::from_utf8(support::response_body(metrics).await.to_vec()).unwrap();
    for expected in [
        "kanata_requests_inflight{endpoint=\"speech\"} 0",
        "kanata_reserved_request_bytes 0",
        "kanata_buffered_requests 0",
        "kanata_requests_queued 0",
    ] {
        assert!(metrics.contains(expected), "missing {expected}");
    }
}

#[test]
fn speech_configuration_requires_allowlists_and_secure_credentials() {
    let text = config_text("127.0.0.1:9");
    for changed in [
        text.replace(
            "speech_voices = [\"af_heart\", \"af_sky\"]",
            "speech_voices = []",
        ),
        text.replace("speech_formats = [\"mp3\", \"wav\"]", "speech_formats = []"),
        text.replace(
            "speech_formats = [\"mp3\", \"wav\"]",
            "speech_formats = [\"mp3\", \"mp3\"]",
        ),
        text.replace("kind = \"speech\"", "kind = \"ollama\""),
        text.replace("trust_zone = \"local\"", "trust_zone = \"external\""),
        text.replace(
            "[adapters.capabilities]",
            "secret_ref = \"env:TTS_KEY\"\n[adapters.capabilities]",
        ),
    ] {
        assert_ne!(changed, text);
        assert!(load(&changed).is_err());
    }
    let external = text
        .replace("http://", "https://")
        .replace("trust_zone = \"local\"", "trust_zone = \"external\"");
    assert!(load(&external).is_err());
    assert!(
        load(&external.replace(
            "[adapters.capabilities]",
            "secret_ref = \"env:TTS_KEY\"\n[adapters.capabilities]"
        ))
        .is_ok()
    );
}
