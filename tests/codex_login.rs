use std::future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use kanata::adapter::codex::auth::{
    AuthorizationCodeRequest, CODEX_CLIENT_ID, CODEX_DEVICE_REDIRECT_URI,
    CODEX_DEVICE_TOKEN_ENDPOINT, CODEX_DEVICE_USERCODE_ENDPOINT, CODEX_DEVICE_VERIFICATION_URL,
    CODEX_TOKEN_ENDPOINT, DeviceCodeRequest, DevicePollFailure, DevicePollRequest,
    DevicePollResponse, LoginError, LoginExchangeFailure, LoginTokenResponse,
    MAX_DEVICE_POLL_INTERVAL, MAX_LOGIN_DURATION, complete_device_login,
};
use tokio::sync::oneshot;
use tokio::time::Instant;

const DEVICE_RESPONSE: &[u8] = include_bytes!("fixtures/codex_login/device-code-response.json");
const DEVICE_SUCCESS: &[u8] = include_bytes!("fixtures/codex_login/device-token-success.json");
const DEVICE_AUTH_ID: &str = "TEST_ONLY_DEVICE_AUTH_ID_NOT_SECRET";
const USER_CODE: &str = "TEST-ONLY-7QZX";
const AUTHORIZATION_CODE: &str = "TEST_ONLY_AUTHORIZATION_CODE_NOT_SECRET";
const PKCE_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const PKCE_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

#[test]
fn device_stage_one_pins_request_and_redacts_private_response_fields() {
    let request = DeviceCodeRequest::new().expect("device request");
    assert_eq!(request.endpoint(), CODEX_DEVICE_USERCODE_ENDPOINT);
    assert_eq!(request.content_type(), "application/json");
    assert_eq!(
        request.body(),
        br#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#
    );

    let authorization = request
        .accept_response(DEVICE_RESPONSE)
        .expect("validated device response");
    assert_eq!(
        authorization.verification_url(),
        CODEX_DEVICE_VERIFICATION_URL
    );
    assert_eq!(authorization.user_code(), USER_CODE);
    assert_eq!(authorization.poll_interval(), Duration::from_secs(1));
    let debug = format!("{authorization:?}");
    assert!(!debug.contains(DEVICE_AUTH_ID));
    assert!(!debug.contains(USER_CODE));
}

#[test]
fn device_stage_one_rejects_missing_blank_unbounded_or_unpinned_interval_values() {
    let invalid = [
        br#"{"device_auth_id":"","user_code":"TEST","interval":"1"}"#.as_slice(),
        br#"{"device_auth_id":"TEST","user_code":"  ","interval":"1"}"#.as_slice(),
        br#"{"device_auth_id":"TEST","user_code":"TEST","interval":"0"}"#.as_slice(),
        br#"{"device_auth_id":"TEST","user_code":"TEST","interval":"61"}"#.as_slice(),
        br#"{"device_auth_id":"TEST","user_code":"TEST","interval":1}"#.as_slice(),
        br#"{"device_auth_id":"TEST","user_code":"TEST"}"#.as_slice(),
    ];
    for body in invalid {
        assert_eq!(
            DeviceCodeRequest::new()
                .unwrap()
                .accept_response(body)
                .unwrap_err(),
            LoginError::InvalidDeviceResponse
        );
    }
    assert!(MAX_DEVICE_POLL_INTERVAL > Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn device_403_and_404_are_bounded_pending_before_form_encoded_exchange() {
    let authorization = DeviceCodeRequest::new()
        .unwrap()
        .accept_response(DEVICE_RESPONSE)
        .unwrap();
    let poll_times = Arc::new(Mutex::new(Vec::new()));
    let polls = Arc::new(AtomicUsize::new(0));
    let poll = {
        let poll_times = poll_times.clone();
        let polls = polls.clone();
        move |request: DevicePollRequest| {
            let poll_times = poll_times.clone();
            let polls = polls.clone();
            async move {
                assert_eq!(request.endpoint(), CODEX_DEVICE_TOKEN_ENDPOINT);
                assert_eq!(request.content_type(), "application/json");
                let body: serde_json::Value =
                    serde_json::from_slice(request.body().as_bytes()).unwrap();
                assert_eq!(
                    body,
                    serde_json::json!({
                        "device_auth_id": DEVICE_AUTH_ID,
                        "user_code": USER_CODE,
                    })
                );
                assert!(!format!("{request:?}").contains(DEVICE_AUTH_ID));
                assert!(!format!("{request:?}").contains(USER_CODE));
                assert!(!format!("{:?}", request.body()).contains(DEVICE_AUTH_ID));
                poll_times.lock().unwrap().push(Instant::now());
                Ok::<_, DevicePollFailure>(match polls.fetch_add(1, Ordering::SeqCst) {
                    0 => DevicePollResponse::new(403, b"TEST_ONLY_PENDING_BODY".to_vec()),
                    1 => DevicePollResponse::new(404, b"TEST_ONLY_PENDING_BODY".to_vec()),
                    _ => DevicePollResponse::new(200, DEVICE_SUCCESS.to_vec()),
                })
            }
        }
    };
    let exchange = |request: AuthorizationCodeRequest| async move {
        assert_eq!(request.endpoint(), CODEX_TOKEN_ENDPOINT);
        assert_eq!(request.redirect_uri(), CODEX_DEVICE_REDIRECT_URI);
        let form_body = request.form_body();
        assert!(!format!("{form_body:?}").contains(AUTHORIZATION_CODE));
        assert!(!format!("{form_body:?}").contains(PKCE_VERIFIER));
        let form = url::form_urlencoded::parse(form_body.as_str().as_bytes())
            .into_owned()
            .collect::<Vec<_>>();
        assert_eq!(form.len(), 5);
        assert!(form.contains(&("grant_type".into(), "authorization_code".into())));
        assert!(form.contains(&("client_id".into(), CODEX_CLIENT_ID.into())));
        assert!(form.contains(&("code".into(), AUTHORIZATION_CODE.into())));
        assert!(form.contains(&("redirect_uri".into(), CODEX_DEVICE_REDIRECT_URI.into())));
        assert!(form.contains(&("code_verifier".into(), PKCE_VERIFIER.into())));
        assert!(!form.iter().any(|(key, _)| key == "device_code"));
        Ok::<_, LoginExchangeFailure>(
            LoginTokenResponse::new(
                "TEST_ONLY_REFRESH_TOKEN_FROM_LOGIN",
                "TEST_ONLY_ACCOUNT_ID_FROM_LOGIN",
            )
            .unwrap(),
        )
    };

    let credential = complete_device_login(authorization, poll, exchange, future::pending::<()>())
        .await
        .expect("device login");
    assert_eq!(
        credential.refresh_token(),
        "TEST_ONLY_REFRESH_TOKEN_FROM_LOGIN"
    );
    assert_eq!(credential.account_id(), "TEST_ONLY_ACCOUNT_ID_FROM_LOGIN");
    assert_eq!(polls.load(Ordering::SeqCst), 3);
    let times = poll_times.lock().unwrap();
    assert!(times[1].duration_since(times[0]) >= Duration::from_secs(1));
    assert!(times[2].duration_since(times[1]) >= Duration::from_secs(1));
}

#[tokio::test]
async fn device_poll_terminal_status_fails_without_echo_or_token_exchange() {
    let authorization = DeviceCodeRequest::new()
        .unwrap()
        .accept_response(DEVICE_RESPONSE)
        .unwrap();
    let exchange_called = Arc::new(AtomicBool::new(false));
    let exchange = {
        let exchange_called = exchange_called.clone();
        move |_request| async move {
            exchange_called.store(true, Ordering::SeqCst);
            Ok::<_, LoginExchangeFailure>(
                LoginTokenResponse::new("TEST_ONLY_REFRESH", "TEST_ONLY_ACCOUNT").unwrap(),
            )
        }
    };
    let error = complete_device_login(
        authorization,
        |_request| async {
            Ok::<_, DevicePollFailure>(DevicePollResponse::new(
                401,
                b"TEST_ONLY_DEVICE_AUTH_ID_OR_CODE".to_vec(),
            ))
        },
        exchange,
        future::pending::<()>(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, LoginError::DevicePollFailed);
    assert!(!exchange_called.load(Ordering::SeqCst));
    assert!(!format!("{error:?} {error}").contains("TEST_ONLY_DEVICE_AUTH_ID_OR_CODE"));
}

#[tokio::test]
async fn device_pkce_mismatch_rejects_before_token_exchange() {
    let authorization = DeviceCodeRequest::new()
        .unwrap()
        .accept_response(DEVICE_RESPONSE)
        .unwrap();
    let exchange_called = Arc::new(AtomicBool::new(false));
    let response = DEVICE_SUCCESS
        .windows(PKCE_CHALLENGE.len())
        .position(|window| window == PKCE_CHALLENGE.as_bytes())
        .map(|at| {
            let mut body = DEVICE_SUCCESS.to_vec();
            body[at] = b'A';
            body
        })
        .expect("fixture challenge");
    let exchange = {
        let exchange_called = exchange_called.clone();
        move |_request| async move {
            exchange_called.store(true, Ordering::SeqCst);
            Ok::<_, LoginExchangeFailure>(
                LoginTokenResponse::new("TEST_ONLY_REFRESH", "TEST_ONLY_ACCOUNT").unwrap(),
            )
        }
    };
    let error = complete_device_login(
        authorization,
        move |_request| {
            let response = response.clone();
            async move { Ok::<_, DevicePollFailure>(DevicePollResponse::new(200, response)) }
        },
        exchange,
        future::pending::<()>(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, LoginError::InvalidDeviceResponse);
    assert!(!exchange_called.load(Ordering::SeqCst));
    assert!(!format!("{error:?} {error}").contains(AUTHORIZATION_CODE));
}

#[tokio::test(start_paused = true)]
async fn device_deadline_and_cancellation_stop_polling_without_exchange() {
    let authorization = DeviceCodeRequest::new()
        .unwrap()
        .accept_response(DEVICE_RESPONSE)
        .unwrap();
    tokio::time::advance(MAX_LOGIN_DURATION).await;
    let polls = Arc::new(AtomicUsize::new(0));
    let exchange_called = Arc::new(AtomicBool::new(false));
    let result = complete_device_login(
        authorization,
        {
            let polls = polls.clone();
            move |_request| {
                polls.fetch_add(1, Ordering::SeqCst);
                async { Ok::<_, DevicePollFailure>(DevicePollResponse::new(403, Vec::new())) }
            }
        },
        {
            let exchange_called = exchange_called.clone();
            move |_request| async move {
                exchange_called.store(true, Ordering::SeqCst);
                Ok::<_, LoginExchangeFailure>(
                    LoginTokenResponse::new("TEST_ONLY_REFRESH", "TEST_ONLY_ACCOUNT").unwrap(),
                )
            }
        },
        future::pending::<()>(),
    )
    .await;
    assert_eq!(result.unwrap_err(), LoginError::DeviceLoginExpired);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(!exchange_called.load(Ordering::SeqCst));

    let authorization = DeviceCodeRequest::new()
        .unwrap()
        .accept_response(DEVICE_RESPONSE)
        .unwrap();
    let (cancel, cancelled) = oneshot::channel::<()>();
    let polls = Arc::new(AtomicUsize::new(0));
    let task = tokio::spawn({
        let polls = polls.clone();
        async move {
            complete_device_login(
                authorization,
                move |_request| {
                    polls.fetch_add(1, Ordering::SeqCst);
                    async { Ok::<_, DevicePollFailure>(DevicePollResponse::new(403, Vec::new())) }
                },
                |_request| async {
                    Ok::<_, LoginExchangeFailure>(
                        LoginTokenResponse::new("TEST_ONLY_REFRESH", "TEST_ONLY_ACCOUNT").unwrap(),
                    )
                },
                async move {
                    let _ = cancelled.await;
                },
            )
            .await
        }
    });
    tokio::task::yield_now().await;
    cancel.send(()).expect("cancel device login");
    assert_eq!(task.await.unwrap().unwrap_err(), LoginError::Cancelled);
    assert_eq!(polls.load(Ordering::SeqCst), 1);
}

#[test]
fn login_token_response_rejects_unbounded_and_blank_credential_values() {
    let long_refresh = "x".repeat(4097);
    let long_account = "a".repeat(4097);
    for (refresh, account) in [
        ("  ", "TEST_ONLY_ACCOUNT"),
        ("TEST_ONLY_REFRESH", "\n"),
        (long_refresh.as_str(), "TEST_ONLY_ACCOUNT"),
        ("TEST_ONLY_REFRESH", long_account.as_str()),
    ] {
        let error = LoginTokenResponse::new(refresh.to_owned(), account.to_owned()).unwrap_err();
        assert_eq!(error, LoginError::InvalidTokenResponse);
        assert!(!format!("{error:?} {error}").contains("TEST_ONLY"));
    }
}
