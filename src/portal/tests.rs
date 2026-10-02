use super::*;
use crate::keys::cli::Exposure;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt as _;

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    directory: PathBuf,
    portal: Portal,
    code: String,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "kanata-portal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("config.toml"),
            include_str!("../../config/embeddings.example.toml"),
        )
        .unwrap();
        let (portal, code) = Portal::new(directory.join("config.toml"), catalog, 9091).unwrap();
        Self {
            directory,
            portal,
            code,
        }
    }
    async fn send(&self, path: &str, value: serde_json::Value, session: Option<&str>) -> Response {
        let mut request = http::Request::builder()
            .method("POST")
            .uri(path)
            .header("Host", "127.0.0.1:9091")
            .header("Origin", "http://127.0.0.1:9091")
            .header("Content-Type", "application/json")
            .header("X-Kanata-Portal", "1");
        if let Some(session) = session {
            request = request.header("Authorization", format!("Bearer {session}"));
        }
        self.portal
            .clone()
            .router()
            .oneshot(request.body(Body::from(value.to_string())).unwrap())
            .await
            .unwrap()
    }
    async fn unlock(&self) -> String {
        let response = self
            .send("/api/login", json!({"code": self.code}), None)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        body(response).await["session"].as_str().unwrap().to_owned()
    }
    async fn snapshot(&self, session: &str) -> serde_json::Value {
        let response = self.send("/api/snapshot", json!({}), Some(session)).await;
        assert_eq!(response.status(), StatusCode::OK);
        body(response).await
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
fn catalog(config: &ValidatedConfig) -> Vec<RouteChoice> {
    config
        .routes()
        .iter()
        .map(|route| RouteChoice {
            selector: route.identity().selector.clone(),
            exposure: Exposure::Private,
        })
        .collect()
}
async fn body(response: Response) -> serde_json::Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}
fn new_key(revision: &serde_json::Value, id: &str) -> serde_json::Value {
    json!({"action":"new","revision":revision,"id":id,"scopes":[{"model_alias":"local-embed","operation":"embeddings"}],"expires":"7","owner":false,"limits":null})
}

#[tokio::test]
async fn browser_boundary_and_one_use_login_protect_every_private_route() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .send("/api/snapshot", json!({}), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for (host, origin, content, marker) in [
        (
            "attacker.test:9091",
            "http://127.0.0.1:9091",
            "application/json",
            "1",
        ),
        (
            "127.0.0.1:9091",
            "http://attacker.test",
            "application/json",
            "1",
        ),
        ("127.0.0.1:9091", "null", "application/json", "1"),
        ("127.0.0.1:9091", "http://127.0.0.1:9091", "text/plain", "1"),
        (
            "127.0.0.1:9091",
            "http://127.0.0.1:9091",
            "application/json",
            "0",
        ),
    ] {
        let request = http::Request::builder()
            .method("POST")
            .uri("/api/login")
            .header("Host", host)
            .header("Origin", origin)
            .header("Content-Type", content)
            .header("X-Kanata-Portal", marker)
            .body(Body::from(json!({"code":fixture.code}).to_string()))
            .unwrap();
        assert_eq!(
            fixture
                .portal
                .clone()
                .router()
                .oneshot(request)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        fixture
            .send("/api/login?code=secret", json!({"code":fixture.code}), None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let session = fixture.unlock().await;
    assert_eq!(
        fixture
            .send("/api/login", json!({"code":fixture.code}), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(fixture.snapshot(&session).await["keys"], json!([]));
    let request = http::Request::builder()
        .uri("/")
        .header("Host", "127.0.0.1:9091")
        .body(Body::empty())
        .unwrap();
    let response = fixture
        .portal
        .clone()
        .router()
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    let page = to_bytes(response.into_body(), 65536).await.unwrap();
    assert!(!String::from_utf8_lossy(&page).contains(&fixture.code));
    assert_eq!(
        fixture
            .send("/api/logout", json!({}), Some(&session))
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .send("/api/snapshot", json!({}), Some(&session))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn key_lifecycle_is_atomic_scoped_audited_and_reveals_secrets_once() {
    let fixture = Fixture::new();
    let session = fixture.unlock().await;
    let initial = fixture.snapshot(&session).await;
    let created = fixture
        .send(
            "/api/change",
            new_key(&initial["revision"], "search"),
            Some(&session),
        )
        .await;
    assert_eq!(created.status(), StatusCode::OK);
    let created = body(created).await;
    let first_secret = created["secret"].as_str().unwrap();
    assert!(first_secret.starts_with("kanata_sk_"));
    let state = fixture.snapshot(&session).await;
    assert!(!state.to_string().contains(first_secret));
    assert!(!state.to_string().contains("sha256:"));
    assert_eq!(state["keys"][0]["scopes"][0]["operation"], "embeddings");
    let stale = fixture
        .send(
            "/api/change",
            new_key(&initial["revision"], "stale"),
            Some(&session),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::BAD_REQUEST);
    let edit = fixture.send("/api/change", json!({"action":"edit","revision":state["revision"],"id":"search","scopes":[{"model_alias":"local-embed","operation":"embeddings"}],"limits":{"max_in_flight":2,"rate_limit":{"requests":3,"per_ms":1000}},"expires":"3"}), Some(&session)).await;
    assert_eq!(edit.status(), StatusCode::OK);
    let state = fixture.snapshot(&session).await;
    assert_eq!(state["keys"][0]["max_in_flight"], 2);
    let rotated = fixture.send("/api/change", json!({"action":"rotate","revision":state["revision"],"id":"search","confirm":"search","expires":"7"}), Some(&session)).await;
    assert_eq!(rotated.status(), StatusCode::OK);
    let rotated = body(rotated).await;
    assert_ne!(rotated["secret"], first_secret);
    let state = fixture.snapshot(&session).await;
    let denied = fixture
        .send(
            "/api/change",
            json!({"action":"revoke","revision":state["revision"],"id":"search","confirm":"wrong"}),
            Some(&session),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    let revoked = fixture.send("/api/change", json!({"action":"revoke","revision":state["revision"],"id":"search","confirm":"search"}), Some(&session)).await;
    assert_eq!(revoked.status(), StatusCode::OK);
    let state = fixture.snapshot(&session).await;
    assert!(state["keys"][0]["revoked_at"].is_string());
    assert_eq!(
        fixture
            .send(
                "/api/change",
                new_key(&state["revision"], "search"),
                Some(&session)
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let stored = std::fs::read_to_string(fixture.directory.join("keys/keys.toml")).unwrap();
    let audit = std::fs::read_to_string(fixture.directory.join("keys/audit.jsonl")).unwrap();
    for secret in [first_secret, rotated["secret"].as_str().unwrap()] {
        assert!(!stored.contains(secret));
        assert!(!audit.contains(secret));
    }
    assert_eq!(audit.lines().count(), 4);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(fixture.directory.join("keys/keys.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn expired_sessions_invalid_scopes_and_unconfirmed_private_grants_cannot_write() {
    let mut fixture = Fixture::new();
    let session = fixture.unlock().await;
    let initial = fixture.snapshot(&session).await;
    let mut request = new_key(&initial["revision"], "search");
    request["scopes"][0]["operation"] = json!("chat");
    assert_eq!(
        fixture
            .send("/api/change", request, Some(&session))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    Arc::get_mut(&mut fixture.portal.service).unwrap().catalog = |config| {
        config
            .routes()
            .iter()
            .map(|route| RouteChoice {
                selector: route.identity().selector.clone(),
                exposure: Exposure::Never,
            })
            .collect()
    };
    let denied = fixture
        .send(
            "/api/change",
            new_key(&initial["revision"], "search"),
            Some(&session),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    assert!(!fixture.directory.join("keys/keys.toml").exists());
    fixture
        .portal
        .login
        .lock()
        .unwrap()
        .session
        .as_mut()
        .unwrap()
        .1 = Instant::now() - Duration::from_secs(1);
    assert_eq!(
        fixture
            .send("/api/snapshot", json!({}), Some(&session))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn daily_quotas_are_validated_preserved_changed_and_explicitly_cleared() {
    let fixture = Fixture::new();
    let session = fixture.unlock().await;
    let initial = fixture.snapshot(&session).await;
    let quota = json!({"requests":10,"tokens":1000,"reservation_tokens":100});
    let mut create = new_key(&initial["revision"], "metered");
    create["daily_quota"] = quota.clone();
    assert_eq!(
        fixture
            .send("/api/change", create, Some(&session))
            .await
            .status(),
        StatusCode::OK
    );
    let state = fixture.snapshot(&session).await;
    assert_eq!(state["keys"][0]["daily_quota"], quota);
    let edit = |state: &serde_json::Value| json!({"action":"edit","revision":state["revision"],"id":"metered","scopes":[{"model_alias":"local-embed","operation":"embeddings"}],"limits":{"max_in_flight":2}});
    assert_eq!(
        fixture
            .send("/api/change", edit(&state), Some(&session))
            .await
            .status(),
        StatusCode::OK
    );
    let state = fixture.snapshot(&session).await;
    assert_eq!(state["keys"][0]["daily_quota"], quota);
    for quota in [
        json!({}),
        json!({"requests":0}),
        json!({"tokens":100}),
        json!({"reservation_tokens":1}),
        json!({"tokens":100,"reservation_tokens":101}),
        json!({"requests":1.5}),
        json!({"requests":-1}),
        json!({"requests":"18446744073709551616"}),
    ] {
        let mut change = edit(&state);
        change["daily_quota"] = quota;
        assert_eq!(
            fixture
                .send("/api/change", change, Some(&session))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            fixture.snapshot(&session).await["revision"],
            state["revision"]
        );
    }
    let mut change = edit(&state);
    change["daily_quota"] = json!({"requests":u64::MAX});
    assert_eq!(
        fixture
            .send("/api/change", change, Some(&session))
            .await
            .status(),
        StatusCode::OK
    );
    let state = fixture.snapshot(&session).await;
    assert_eq!(state["keys"][0]["daily_quota"]["requests"], u64::MAX);
    assert!(state["keys"][0]["daily_quota"].get("tokens").is_none());
    let mut conflict = edit(&state);
    conflict["daily_quota"] = quota;
    conflict["clear_quota"] = json!(true);
    assert_eq!(
        fixture
            .send("/api/change", conflict, Some(&session))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mut clear = edit(&state);
    clear["clear_quota"] = json!(true);
    assert_eq!(
        fixture
            .send("/api/change", clear, Some(&session))
            .await
            .status(),
        StatusCode::OK
    );
    let state = fixture.snapshot(&session).await;
    assert_eq!(state["keys"][0]["daily_quota"], serde_json::Value::Null);
    let audit = std::fs::read_to_string(fixture.directory.join("keys/audit.jsonl")).unwrap();
    assert!(audit.contains("daily_quota"));

    let path = fixture.directory.join("config.toml");
    let config = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, config.replace("usage_dir = \"state\"\n", "")).unwrap();
    let state = fixture.snapshot(&session).await;
    assert_eq!(state["usage_configured"], false);
    let mut create = new_key(&state["revision"], "unsupported");
    create["daily_quota"] = json!({"requests":10});
    let response = fixture.send("/api/change", create, Some(&session)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        body(response).await["error"]
            .as_str()
            .unwrap()
            .contains("usage_dir")
    );
    assert_eq!(
        fixture.snapshot(&session).await["revision"],
        state["revision"]
    );
}

#[tokio::test]
async fn duplicate_security_headers_and_expired_or_locked_bootstrap_are_denied() {
    let fixture = Fixture::new();
    let session = fixture.unlock().await;
    for (duplicate, value, status) in [
        ("Host", "127.0.0.1:9091", StatusCode::FORBIDDEN),
        ("Origin", "http://127.0.0.1:9091", StatusCode::FORBIDDEN),
        ("X-Kanata-Portal", "1", StatusCode::FORBIDDEN),
        ("Content-Type", "application/json", StatusCode::FORBIDDEN),
        ("Authorization", "Bearer other", StatusCode::UNAUTHORIZED),
    ] {
        let request = http::Request::builder()
            .method("POST")
            .uri("/api/snapshot")
            .header("Host", "127.0.0.1:9091")
            .header("Origin", "http://127.0.0.1:9091")
            .header("Content-Type", "application/json")
            .header("X-Kanata-Portal", "1")
            .header("Authorization", format!("Bearer {session}"))
            .header(duplicate, value)
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            fixture
                .portal
                .clone()
                .router()
                .oneshot(request)
                .await
                .unwrap()
                .status(),
            status
        );
    }
    let fixture = Fixture::new();
    for _ in 0..10 {
        assert_eq!(
            fixture
                .send("/api/login", json!({"code":"wrong"}), None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        fixture
            .send("/api/login", json!({"code":fixture.code}), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(fixture.portal.login.lock().unwrap().session.is_none());
    let fixture = Fixture::new();
    fixture.portal.login.lock().unwrap().deadline = Instant::now() - Duration::from_secs(1);
    assert_eq!(
        fixture
            .send("/api/login", json!({"code":fixture.code}), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(fixture.portal.login.lock().unwrap().session.is_none());
}
