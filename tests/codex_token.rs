use std::future;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kanata::adapter::codex::auth::{
    DeviceCodeRequest, DevicePollFailure, DevicePollResponse, LoginError, LoginExchangeFailure,
    RefreshError, complete_device_login, normalize_authorization_code_response,
    normalize_refresh_response,
};
use serde_json::{Value, json};

const AUTHORIZATION_RESPONSE: &[u8] =
    include_bytes!("fixtures/codex_token/authorization-code-response.json");
const REFRESH_RESPONSE: &[u8] = include_bytes!("fixtures/codex_token/refresh-response.json");
const DEVICE_CODE_RESPONSE: &[u8] =
    include_bytes!("fixtures/codex_login/device-code-response.json");
const DEVICE_TOKEN_RESPONSE: &[u8] =
    include_bytes!("fixtures/codex_login/device-token-success.json");
const ID_TOKEN_ACCOUNT: &str = "TEST_ONLY_ID_TOKEN_ACCOUNT_ID_NOT_SECRET_0001";
const ACCESS_TOKEN_ACCOUNT: &str = "TEST_ONLY_ACCESS_TOKEN_ACCOUNT_ID_MUST_NOT_WIN";
const STORED_ACCOUNT: &str = "TEST_ONLY_STORED_ACCOUNT_ID_NOT_SECRET";
const MAX_RESPONSE_BYTES: usize = 16 * 1024;

fn authorization_parts() -> (String, String, String) {
    let value: Value = serde_json::from_slice(AUTHORIZATION_RESPONSE).expect("fixture JSON");
    (
        value["id_token"].as_str().expect("ID token").to_owned(),
        value["access_token"]
            .as_str()
            .expect("access token")
            .to_owned(),
        value["refresh_token"]
            .as_str()
            .expect("refresh token")
            .to_owned(),
    )
}

fn jwt_payload(token: &str) -> Value {
    let encoded = token.split('.').nth(1).expect("JWT payload");
    let decoded = URL_SAFE_NO_PAD.decode(encoded).expect("base64url payload");
    serde_json::from_slice(&decoded).expect("JWT payload JSON")
}

fn authorization_body(id_token: &str, access_token: &str, refresh_token: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id_token": id_token,
        "access_token": access_token,
        "refresh_token": refresh_token,
    }))
    .expect("synthetic token response JSON")
}

fn synthetic_jwt(payload: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(b"TEST_ONLY_SIGNATURE_NOT_VALID");
    format!("{header}.{payload}.{signature}")
}

fn claims_jwt(account_id: &str) -> String {
    synthetic_jwt(
        &json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
            }
        })
        .to_string(),
    )
}

fn refresh_body(
    access_token: &str,
    expires_in: Option<Value>,
    refresh_token: Option<Value>,
) -> Vec<u8> {
    let mut value = json!({"access_token": access_token});
    let object = value.as_object_mut().expect("object JSON");
    if let Some(expires_in) = expires_in {
        object.insert("expires_in".into(), expires_in);
    }
    if let Some(refresh_token) = refresh_token {
        object.insert("refresh_token".into(), refresh_token);
    }
    serde_json::to_vec(&value).expect("synthetic refresh JSON")
}

#[tokio::test]
async fn device_login_normalizes_tokens_and_takes_account_from_the_id_token() {
    let (_, access_token, _) = authorization_parts();
    assert_eq!(
        jwt_payload(&access_token)["https://api.openai.com/auth"]["chatgpt_account_id"],
        ACCESS_TOKEN_ACCOUNT
    );

    let normalized = normalize_authorization_code_response(AUTHORIZATION_RESPONSE)
        .expect("normalized token response");
    assert!(!format!("{normalized:?}").contains("TEST_ONLY"));
    drop(normalized);

    let authorization = DeviceCodeRequest::new()
        .expect("device request")
        .accept_response(DEVICE_CODE_RESPONSE)
        .expect("device response");
    let credential = complete_device_login(
        authorization,
        |_request| async {
            Ok::<_, DevicePollFailure>(DevicePollResponse::new(200, DEVICE_TOKEN_RESPONSE))
        },
        |_request| async {
            normalize_authorization_code_response(AUTHORIZATION_RESPONSE)
                .map_err(|_| LoginExchangeFailure)
        },
        future::pending::<()>(),
    )
    .await
    .expect("normalized device login");

    assert_eq!(
        credential.refresh_token(),
        "TEST_ONLY_REFRESH_TOKEN_NOT_SECRET_0001"
    );
    assert_eq!(credential.account_id(), ID_TOKEN_ACCOUNT);
    assert_ne!(credential.account_id(), ACCESS_TOKEN_ACCOUNT);
    assert!(!format!("{credential:?}").contains("TEST_ONLY"));
}

#[test]
fn authorization_exchange_rejects_missing_blank_and_malformed_fields() {
    let (id_token, access_token, refresh_token) = authorization_parts();
    let cases = [
        serde_json::to_vec(&json!({
            "access_token": access_token,
            "refresh_token": refresh_token,
        }))
        .expect("missing ID-token response"),
        serde_json::to_vec(&json!({
            "id_token": id_token,
            "refresh_token": refresh_token,
        }))
        .expect("missing access-token response"),
        serde_json::to_vec(&json!({
            "id_token": id_token,
            "access_token": access_token,
        }))
        .expect("missing refresh-token response"),
        serde_json::to_vec(&json!({
            "id_token": 7,
            "access_token": access_token,
            "refresh_token": refresh_token,
        }))
        .expect("non-string ID token response"),
        serde_json::to_vec(&json!({
            "id_token": id_token,
            "access_token": 7,
            "refresh_token": refresh_token,
        }))
        .expect("non-string access token response"),
        serde_json::to_vec(&json!({
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": 7,
        }))
        .expect("non-string refresh token response"),
        authorization_body(" ", &access_token, &refresh_token),
        authorization_body(&id_token, "\t", &refresh_token),
        authorization_body(&id_token, &access_token, "\n"),
        authorization_body("not.a.valid.jwt", &access_token, &refresh_token),
        authorization_body(&claims_jwt("TEST_ONLY_ACCOUNT"), "", &refresh_token),
        authorization_body(&claims_jwt("  "), &access_token, &refresh_token),
        authorization_body(
            &synthetic_jwt(r#"{"sub":"TEST_ONLY_NO_ACCOUNT_CLAIM"}"#),
            &access_token,
            &refresh_token,
        ),
        authorization_body(
            &synthetic_jwt(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":7}}"#),
            &access_token,
            &refresh_token,
        ),
        authorization_body(&id_token, &"x".repeat(4 * 1024 + 1), &refresh_token),
    ];

    for body in cases {
        let error = normalize_authorization_code_response(&body).unwrap_err();
        assert_eq!(error, LoginError::InvalidTokenResponse);
        assert!(!format!("{error:?} {error}").contains("TEST_ONLY"));
    }
}

#[test]
fn base64url_valid_non_json_jwt_headers_are_rejected() {
    let (id_token, access_token, refresh_token) = authorization_parts();
    let (_, id_remainder) = id_token.split_once('.').expect("synthetic JWT");
    let malformed_id = format!("QQ.{id_remainder}");
    assert_eq!(
        normalize_authorization_code_response(&authorization_body(
            &malformed_id,
            &access_token,
            &refresh_token,
        ))
        .unwrap_err(),
        LoginError::InvalidTokenResponse
    );

    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
        + 60 * 60;
    let access = synthetic_jwt(&format!(r#"{{"exp":{expiry}}}"#));
    let (_, access_remainder) = access.split_once('.').expect("synthetic JWT");
    let malformed_access = format!("QQ.{access_remainder}");
    assert_eq!(
        normalize_refresh_response(&refresh_body(&malformed_access, None, None), STORED_ACCOUNT)
            .unwrap_err(),
        RefreshError::InvalidResponse
    );
}

#[test]
fn authorization_exchange_rejects_duplicate_claims_and_excessive_json_depth() {
    let (id_token, access_token, refresh_token) = authorization_parts();
    let duplicate_claim = synthetic_jwt(
        r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"TEST_ONLY_A","chatgpt_account_id":"TEST_ONLY_B"}}"#,
    );
    let duplicate_claim_body = authorization_body(&duplicate_claim, &access_token, &refresh_token);
    let duplicate_field = format!(
        r#"{{"id_token":{},"access_token":"TEST_ONLY_A","access_token":"TEST_ONLY_B","refresh_token":"TEST_ONLY_REFRESH"}}"#,
        serde_json::to_string(&id_token).expect("ID token JSON")
    );
    let depth = "[".repeat(17);
    let close_depth = "]".repeat(17);
    let nested = format!(
        r#"{{"id_token":{},"access_token":{},"refresh_token":{},"extra":{depth}0{close_depth}}}"#,
        serde_json::to_string(&id_token).expect("ID token JSON"),
        serde_json::to_string(&access_token).expect("access token JSON"),
        serde_json::to_string(&refresh_token).expect("refresh token JSON"),
    );

    for body in [
        duplicate_claim_body,
        duplicate_field.into_bytes(),
        nested.into_bytes(),
    ] {
        let error = normalize_authorization_code_response(&body).unwrap_err();
        assert_eq!(error, LoginError::InvalidTokenResponse);
        assert!(!format!("{error:?} {error}").contains("TEST_ONLY"));
    }

    let oversized = vec![b' '; MAX_RESPONSE_BYTES + 1];
    let error = normalize_authorization_code_response(&oversized).unwrap_err();
    assert_eq!(error, LoginError::InvalidTokenResponse);
}

#[test]
fn refresh_response_accepts_rotation_and_uses_only_the_stored_account_id() {
    let response = normalize_refresh_response(REFRESH_RESPONSE, STORED_ACCOUNT)
        .expect("synthetic refresh response");
    let debug = format!("{response:?}");
    for secret in [
        "TEST_ONLY_ACCESS_TOKEN_NOT_SECRET_0001",
        "TEST_ONLY_ROTATED_REFRESH_TOKEN_NOT_SECRET_0001",
        "TEST_ONLY_RESPONSE_ACCOUNT_ID_MUST_NOT_BE_USED",
        STORED_ACCOUNT,
    ] {
        assert!(!debug.contains(secret));
    }

    assert_eq!(
        normalize_refresh_response(REFRESH_RESPONSE, " ").unwrap_err(),
        RefreshError::InvalidResponse
    );
}

#[test]
fn refresh_expiry_uses_positive_expires_in_or_source_backed_access_jwt_exp() {
    let explicit =
        normalize_refresh_response(REFRESH_RESPONSE, STORED_ACCOUNT).expect("positive expires_in");
    assert!(!format!("{explicit:?}").contains("TEST_ONLY"));

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    let exp = i64::try_from(now + 60 * 60).expect("synthetic exp");
    let access_token = synthetic_jwt(&format!(r#"{{"exp":{exp}}}"#));
    let fallback_body = refresh_body(&access_token, None, None);
    let fallback = normalize_refresh_response(&fallback_body, STORED_ACCOUNT)
        .expect("access-token JWT expiry fallback");
    assert!(!format!("{fallback:?}").contains(&access_token));

    let unusable = [
        refresh_body("TEST_ONLY_ACCESS", Some(json!(0)), None),
        refresh_body("TEST_ONLY_ACCESS", Some(Value::Null), None),
        refresh_body("TEST_ONLY_ACCESS", Some(json!("3600")), None),
        refresh_body("TEST_ONLY_ACCESS", Some(json!(u64::MAX)), None),
        refresh_body("TEST_ONLY_ACCESS", Some(json!(60)), None),
        refresh_body("TEST_ONLY_ACCESS", None, None),
        refresh_body(&synthetic_jwt(r#"{"exp":1}"#), None, None),
        refresh_body(
            &synthetic_jwt(r#"{"exp":"TEST_ONLY_NOT_A_NUMBER"}"#),
            None,
            None,
        ),
        refresh_body(&synthetic_jwt("{}"), None, None),
        refresh_body(
            &synthetic_jwt(&format!(r#"{{"exp":{}}}"#, i64::MAX)),
            None,
            None,
        ),
    ];
    for (index, body) in unusable.into_iter().enumerate() {
        assert!(
            matches!(
                normalize_refresh_response(&body, STORED_ACCOUNT),
                Err(RefreshError::InvalidResponse)
            ),
            "accepted unusable expiry case {index}"
        );
    }
}

#[test]
fn refresh_response_rejects_malformed_optional_rotation_and_oversized_input() {
    let access_token = "TEST_ONLY_ACCESS_TOKEN";
    let bad_rotation = [
        refresh_body(access_token, Some(json!(3600)), Some(Value::Null)),
        refresh_body(access_token, Some(json!(3600)), Some(json!(" "))),
        refresh_body(
            access_token,
            Some(json!(3600)),
            Some(json!("x".repeat(4 * 1024 + 1))),
        ),
    ];
    for body in bad_rotation {
        assert_eq!(
            normalize_refresh_response(&body, STORED_ACCOUNT).unwrap_err(),
            RefreshError::InvalidResponse
        );
    }

    let oversized = vec![b' '; MAX_RESPONSE_BYTES + 1];
    assert_eq!(
        normalize_refresh_response(&oversized, STORED_ACCOUNT).unwrap_err(),
        RefreshError::InvalidResponse
    );
}

#[test]
fn refresh_response_requires_a_bounded_nonblank_access_token() {
    let oversized = "x".repeat(4 * 1024 + 1);
    let cases = [
        b"{}".to_vec(),
        refresh_body(" ", Some(json!(3600)), None),
        refresh_body(&oversized, Some(json!(3600)), None),
        serde_json::to_vec(&json!({"access_token": 7, "expires_in": 3600}))
            .expect("non-string refresh access token"),
    ];

    for body in cases {
        assert_eq!(
            normalize_refresh_response(&body, STORED_ACCOUNT).unwrap_err(),
            RefreshError::InvalidResponse
        );
    }
}

#[test]
fn malformed_refresh_json_and_duplicate_expiry_are_redacted() {
    let malformed = b"{\"access_token\":\"TEST_ONLY_PRIVATE_VALUE\"";
    let error = normalize_refresh_response(malformed, STORED_ACCOUNT).unwrap_err();
    assert_eq!(error, RefreshError::InvalidResponse);
    assert!(!format!("{error:?} {error}").contains("TEST_ONLY_PRIVATE_VALUE"));

    let duplicate = synthetic_jwt(r#"{"exp":1893456000,"exp":1893459600}"#);
    let body = refresh_body(&duplicate, None, None);
    let error = normalize_refresh_response(&body, STORED_ACCOUNT).unwrap_err();
    assert_eq!(error, RefreshError::InvalidResponse);
}

#[test]
fn normalized_exchange_errors_never_echo_token_bodies() {
    let raw_token = "TEST_ONLY_RAW_TOKEN_NOT_SECRET";
    let body = authorization_body(
        "malformed.TEST_ONLY_PAYLOAD.signature",
        raw_token,
        raw_token,
    );
    let error = normalize_authorization_code_response(&body).unwrap_err();
    let rendered = format!("{error:?} {error}");
    let body_text = String::from_utf8_lossy(&body);
    assert!(!rendered.contains(raw_token));
    assert!(!rendered.contains(body_text.as_ref()));

    let body = refresh_body(raw_token, Some(json!(0)), Some(json!(raw_token)));
    let error = normalize_refresh_response(&body, STORED_ACCOUNT).unwrap_err();
    let rendered = format!("{error:?} {error}");
    let body_text = String::from_utf8_lossy(&body);
    assert!(!rendered.contains(raw_token));
    assert!(!rendered.contains(body_text.as_ref()));
}
