use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use kanata::adapter::codex::auth::{
    Credential, CredentialStore, RefreshCoordinator, RefreshError, RefreshExchangeError,
    RefreshResponse, StoreError,
};
use kanata::config::CodexAuthStore;
use tokio::sync::{Barrier, oneshot};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const ACCOUNT_ID: &str = "TEST_ONLY_ACCOUNT_ID_0001";
const ACCESS_TOKEN: &str = "TEST_ONLY_ACCESS_TOKEN_DO_NOT_PERSIST";

struct TempState {
    root: PathBuf,
    state: PathBuf,
}

impl TempState {
    fn new() -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        let root = base.join(format!(
            "kanata-codex-refresh-test-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create synthetic temp directory");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("secure synthetic directory");
        let state = root.join("state");
        fs::create_dir(&state).expect("create synthetic state directory");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))
            .expect("secure synthetic state directory");
        Self { root, state }
    }

    fn store(&self) -> CredentialStore {
        CredentialStore::new(CodexAuthStore::File, &self.state)
    }

    fn coordinator(&self, skew: Duration, lock_timeout: Duration) -> RefreshCoordinator {
        RefreshCoordinator::new(self.store(), skew, lock_timeout).expect("valid refresh policy")
    }

    async fn seed(&self, refresh_token: &str) {
        let store = self.store();
        let lock = store
            .lock(Duration::from_secs(1))
            .await
            .expect("lock store for fixture seed");
        lock.save(&Credential::new(refresh_token, ACCOUNT_ID).expect("fixture credential"))
            .expect("save fixture credential");
    }

    async fn load(&self) -> Credential {
        self.store()
            .lock(Duration::from_secs(1))
            .await
            .expect("lock store to inspect fixture")
            .load()
            .expect("load fixture credential")
            .expect("fixture credential exists")
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn response(
    access_token: &str,
    account_id: &str,
    rotated_refresh_token: Option<&str>,
) -> Result<RefreshResponse, RefreshError> {
    RefreshResponse::new(
        access_token,
        SystemTime::now() + Duration::from_secs(3_600),
        account_id,
        rotated_refresh_token.map(str::to_owned),
    )
}

#[tokio::test]
async fn simultaneous_requests_share_one_refresh_exchange() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;

    let coordinator = Arc::new(temp.coordinator(Duration::from_secs(30), Duration::from_secs(1)));
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(6));
    let mut requests = Vec::new();

    for _ in 0..5 {
        let coordinator = coordinator.clone();
        let calls = calls.clone();
        let barrier = barrier.clone();
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            coordinator
                .access_token(move |credential| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    response(
                        ACCESS_TOKEN,
                        credential.account_id(),
                        Some("TEST_ONLY_REFRESH_ROTATED"),
                    )
                    .map_err(|_| RefreshExchangeError)
                })
                .await
                .expect("single-flight refresh")
                .as_str()
                .to_owned()
        }));
    }
    barrier.wait().await;

    let mut tokens = Vec::new();
    for request in requests {
        tokens.push(request.await.expect("request task"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(tokens.iter().all(|token| token == ACCESS_TOKEN));
}

#[tokio::test]
async fn rotated_refresh_is_durable_before_access_token_is_returned() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));

    let token = coordinator
        .access_token(|credential| async move {
            assert_eq!(credential.refresh_token(), "TEST_ONLY_REFRESH_INITIAL");
            response(
                ACCESS_TOKEN,
                credential.account_id(),
                Some("TEST_ONLY_REFRESH_ROTATED"),
            )
            .map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("refreshed token");

    assert_eq!(token.as_str(), ACCESS_TOKEN);
    assert!(!format!("{token:?}").contains(ACCESS_TOKEN));
    assert!(!format!("{token}").contains(ACCESS_TOKEN));

    let loaded = temp.load().await;
    assert_eq!(loaded.refresh_token(), "TEST_ONLY_REFRESH_ROTATED");
    let raw = fs::read(temp.state.join("credential-v1.json")).expect("read stored record");
    assert!(
        !raw.windows(ACCESS_TOKEN.len())
            .any(|window| window == ACCESS_TOKEN.as_bytes())
    );
}

#[tokio::test]
async fn independent_coordinators_reread_rotation_and_refresh_serially() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let first = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));
    let second = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));

    first
        .access_token(|credential| async move {
            assert_eq!(credential.refresh_token(), "TEST_ONLY_REFRESH_INITIAL");
            response(
                "TEST_ONLY_FIRST_ACCESS",
                credential.account_id(),
                Some("TEST_ONLY_REFRESH_ROTATED"),
            )
            .map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("first coordinator refresh");

    let second_token = second
        .access_token(|credential| async move {
            assert_eq!(credential.refresh_token(), "TEST_ONLY_REFRESH_ROTATED");
            response("TEST_ONLY_SECOND_ACCESS", credential.account_id(), None)
                .map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("second coordinator refresh");

    assert_eq!(second_token.as_str(), "TEST_ONLY_SECOND_ACCESS");
}

#[tokio::test]
async fn forced_refresh_bypasses_the_rejected_cached_access_token() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));
    let rejected = coordinator
        .access_token(|credential| async move {
            response("TEST_ONLY_REJECTED_ACCESS", credential.account_id(), None)
                .map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("initial access token");
    let calls = Arc::new(AtomicUsize::new(0));

    let replacement = coordinator
        .refresh_after_unauthorized(&rejected, {
            let calls = calls.clone();
            move |credential| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                response(
                    "TEST_ONLY_REPLACEMENT_ACCESS",
                    credential.account_id(),
                    Some("TEST_ONLY_REFRESH_ROTATED"),
                )
                .map_err(|_| RefreshExchangeError)
            }
        })
        .await
        .expect("forced refresh after rejected token");

    assert_eq!(replacement.as_str(), "TEST_ONLY_REPLACEMENT_ACCESS");
    assert_eq!(replacement.account_id(), ACCOUNT_ID);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        temp.load().await.refresh_token(),
        "TEST_ONLY_REFRESH_ROTATED"
    );
}

#[tokio::test]
async fn rejected_session_does_not_refresh_after_durable_account_replacement() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));
    let rejected = coordinator
        .access_token(|credential| async move {
            response("TEST_ONLY_REJECTED_ACCESS", credential.account_id(), None)
                .map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("initial access token");

    temp.store()
        .lock(Duration::from_secs(1))
        .await
        .expect("lock durable credential")
        .save(
            &Credential::new("TEST_ONLY_OTHER_REFRESH", "TEST_ONLY_OTHER_ACCOUNT")
                .expect("replacement credential"),
        )
        .expect("replace durable credential");
    let calls = Arc::new(AtomicUsize::new(0));
    let result = coordinator
        .refresh_after_unauthorized(&rejected, {
            let calls = calls.clone();
            move |credential| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                response("TEST_ONLY_OTHER_ACCESS", credential.account_id(), None)
                    .map_err(|_| RefreshExchangeError)
            }
        })
        .await;

    assert!(matches!(result, Err(RefreshError::AccountMismatch)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(rejected.account_id(), ACCOUNT_ID);
}

#[tokio::test]
async fn concurrent_forced_refreshes_reuse_the_first_replacement() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator = Arc::new(temp.coordinator(Duration::from_secs(30), Duration::from_secs(1)));
    let rejected = coordinator
        .access_token(|credential| async move {
            response("TEST_ONLY_REJECTED_ACCESS", credential.account_id(), None)
                .map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("initial access token");
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(3));
    let mut requests = Vec::new();

    for _ in 0..2 {
        let coordinator = coordinator.clone();
        let rejected = rejected.clone();
        let calls = calls.clone();
        let barrier = barrier.clone();
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            coordinator
                .refresh_after_unauthorized(&rejected, move |credential| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    response(
                        "TEST_ONLY_REPLACEMENT_ACCESS",
                        credential.account_id(),
                        Some("TEST_ONLY_REFRESH_ROTATED"),
                    )
                    .map_err(|_| RefreshExchangeError)
                })
                .await
                .expect("rejected token refresh")
                .as_str()
                .to_owned()
        }));
    }
    barrier.wait().await;

    for request in requests {
        assert_eq!(
            request.await.expect("request task"),
            "TEST_ONLY_REPLACEMENT_ACCESS"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn logout_clears_the_cached_token_and_removes_only_the_durable_credential() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));
    coordinator
        .access_token(|credential| async move {
            response(ACCESS_TOKEN, credential.account_id(), None).map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("initial access token");

    coordinator.logout().await.expect("local logout");
    let store = temp.store();
    let lock = store
        .lock(Duration::from_secs(1))
        .await
        .expect("inspect logged-out store");
    assert!(lock.load().expect("credential removed").is_none());
    drop(lock);

    assert!(matches!(
        coordinator
            .access_token(|_credential| async {
                panic!("logout must clear the cached token before reading durable state")
            })
            .await,
        Err(RefreshError::MissingCredential)
    ));
}

#[tokio::test]
async fn logout_store_error_clears_cache_before_returning() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator = temp.coordinator(Duration::from_secs(30), Duration::from_millis(50));
    coordinator
        .access_token(|credential| async move {
            response(ACCESS_TOKEN, credential.account_id(), None).map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("initial access token");

    let store = temp.store();
    let held = store
        .lock(Duration::from_secs(1))
        .await
        .expect("hold store lock during logout");
    assert!(matches!(
        coordinator.logout().await,
        Err(RefreshError::Storage(StoreError::LockTimeout))
    ));
    drop(held);

    let calls = Arc::new(AtomicUsize::new(0));
    coordinator
        .access_token({
            let calls = calls.clone();
            move |credential| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                response(
                    "TEST_ONLY_AFTER_LOGOUT_ERROR",
                    credential.account_id(),
                    None,
                )
                .map_err(|_| RefreshExchangeError)
            }
        })
        .await
        .expect("refresh after failed logout");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn logout_waits_for_an_inflight_refresh_then_clears_its_cache() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator = Arc::new(temp.coordinator(Duration::from_secs(30), Duration::from_secs(1)));
    let (exchange_started_tx, exchange_started_rx) = oneshot::channel();
    let (finish_exchange_tx, finish_exchange_rx) = oneshot::channel();
    let refreshing = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator
                .access_token(move |credential| async move {
                    let _ = exchange_started_tx.send(());
                    let _ = finish_exchange_rx.await;
                    response(
                        "TEST_ONLY_INFLIGHT_ACCESS",
                        credential.account_id(),
                        Some("TEST_ONLY_REFRESH_ROTATED"),
                    )
                    .map_err(|_| RefreshExchangeError)
                })
                .await
        })
    };
    exchange_started_rx.await.expect("refresh exchange started");

    let (logout_started_tx, logout_started_rx) = oneshot::channel();
    let logging_out = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            let _ = logout_started_tx.send(());
            coordinator.logout().await
        })
    };
    logout_started_rx.await.expect("logout task started");
    tokio::task::yield_now().await;
    finish_exchange_tx
        .send(())
        .expect("release synthetic exchange");

    assert_eq!(
        refreshing
            .await
            .expect("refresh task")
            .expect("in-flight refresh completes")
            .as_str(),
        "TEST_ONLY_INFLIGHT_ACCESS"
    );
    logging_out
        .await
        .expect("logout task")
        .expect("logout completes after refresh");

    assert!(matches!(
        coordinator
            .access_token(|_credential| async {
                panic!("logout must clear the in-flight token from cache")
            })
            .await,
        Err(RefreshError::MissingCredential)
    ));
}

#[tokio::test]
async fn omitted_refresh_rotation_preserves_the_durable_refresh_token() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_UNCHANGED").await;
    let coordinator = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));

    let token = coordinator
        .access_token(|credential| async move {
            response(ACCESS_TOKEN, credential.account_id(), None).map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("refreshed token");

    assert_eq!(token.as_str(), ACCESS_TOKEN);
    assert_eq!(
        temp.load().await.refresh_token(),
        "TEST_ONLY_REFRESH_UNCHANGED"
    );
}

#[tokio::test]
async fn exchange_failure_and_account_mismatch_do_not_cache_or_persist_access_tokens() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));

    assert!(matches!(
        coordinator
            .access_token(|_credential| async { Err(RefreshExchangeError) })
            .await,
        Err(RefreshError::ExchangeFailed)
    ));
    assert_eq!(
        temp.load().await.refresh_token(),
        "TEST_ONLY_REFRESH_INITIAL"
    );

    assert!(matches!(
        coordinator
            .access_token(|_credential| async {
                response(
                    ACCESS_TOKEN,
                    "TEST_ONLY_DIFFERENT_ACCOUNT",
                    Some("TEST_ONLY_REFRESH_ROTATED"),
                )
                .map_err(|_| RefreshExchangeError)
            })
            .await,
        Err(RefreshError::AccountMismatch)
    ));
    assert_eq!(
        temp.load().await.refresh_token(),
        "TEST_ONLY_REFRESH_INITIAL"
    );
    let raw = fs::read(temp.state.join("credential-v1.json")).expect("read stored record");
    assert!(
        !raw.windows(ACCESS_TOKEN.len())
            .any(|window| window == ACCESS_TOKEN.as_bytes())
    );
}

#[tokio::test]
async fn a_failed_rotation_write_never_exposes_the_new_access_token() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let sentinel = temp.root.join("sentinel");
    fs::write(&sentinel, b"TEST_ONLY_SENTINEL").expect("write sentinel");
    let record = temp.state.join("credential-v1.json");
    let sentinel_for_link = sentinel.clone();
    let coordinator = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));

    let result = coordinator
        .access_token(|credential| async move {
            fs::remove_file(&record).expect("remove fixture record");
            symlink(&sentinel_for_link, &record).expect("replace record with symlink");
            response(
                ACCESS_TOKEN,
                credential.account_id(),
                Some("TEST_ONLY_REFRESH_ROTATED"),
            )
            .map_err(|_| RefreshExchangeError)
        })
        .await;

    assert!(matches!(
        result,
        Err(RefreshError::Storage(StoreError::UnsafeStoragePath))
    ));
    assert_eq!(
        fs::read(&sentinel).expect("sentinel unchanged"),
        b"TEST_ONLY_SENTINEL"
    );
}

#[tokio::test]
async fn cancellation_during_exchange_releases_single_flight_and_storage_locks() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator = Arc::new(temp.coordinator(Duration::from_secs(30), Duration::from_secs(1)));
    let (started_tx, started_rx) = oneshot::channel();
    let task = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator
                .access_token(move |_credential| async move {
                    let _ = started_tx.send(());
                    std::future::pending::<Result<RefreshResponse, RefreshExchangeError>>().await
                })
                .await
        })
    };
    started_rx.await.expect("fixture exchange started");
    task.abort();
    assert!(
        task.await
            .expect_err("refresh task cancelled")
            .is_cancelled()
    );

    let store = temp.store();
    let lock = store
        .lock(Duration::from_millis(100))
        .await
        .expect("cancelled refresh released storage lock");
    drop(lock);
    let token = coordinator
        .access_token(|credential| async move {
            response(ACCESS_TOKEN, credential.account_id(), None).map_err(|_| RefreshExchangeError)
        })
        .await
        .expect("retry after cancellation");
    assert_eq!(token.as_str(), ACCESS_TOKEN);
}

#[tokio::test]
async fn cancellation_while_waiting_for_store_lock_is_safe_and_bounded() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    let coordinator =
        Arc::new(temp.coordinator(Duration::from_secs(30), Duration::from_millis(80)));
    let store = temp.store();
    let held = store
        .lock(Duration::from_secs(1))
        .await
        .expect("hold store lock");

    let started = std::time::Instant::now();
    assert!(matches!(
        coordinator
            .access_token(|_credential| async {
                panic!("exchange must not run while the store lock is held")
            })
            .await,
        Err(RefreshError::Storage(StoreError::LockTimeout))
    ));
    assert!(started.elapsed() < Duration::from_secs(2));

    let waiting = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator
                .access_token(|_credential| async {
                    panic!("exchange must not run while the store lock is held")
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    waiting.abort();
    assert!(waiting.await.expect_err("waiter cancelled").is_cancelled());
    drop(held);

    let started = std::time::Instant::now();
    let result = coordinator
        .access_token(|credential| async move {
            response(ACCESS_TOKEN, credential.account_id(), None).map_err(|_| RefreshExchangeError)
        })
        .await;
    assert!(result.is_ok());
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn refresh_policy_and_access_token_expiry_are_validated() {
    let temp = TempState::new();
    temp.seed("TEST_ONLY_REFRESH_INITIAL").await;
    assert!(matches!(
        RefreshCoordinator::new(
            temp.store(),
            kanata::adapter::codex::auth::MAX_REFRESH_SKEW + Duration::from_nanos(1),
            Duration::ZERO,
        ),
        Err(RefreshError::InvalidPolicy)
    ));

    assert!(matches!(
        RefreshResponse::new(
            " \n",
            SystemTime::now() + Duration::from_secs(60),
            ACCOUNT_ID,
            None,
        ),
        Err(RefreshError::InvalidResponse)
    ));

    let coordinator = temp.coordinator(Duration::from_secs(30), Duration::from_secs(1));
    let result = coordinator
        .access_token(|credential| async move {
            RefreshResponse::new(
                ACCESS_TOKEN,
                SystemTime::now() + Duration::from_secs(10),
                credential.account_id(),
                Some("TEST_ONLY_REFRESH_ROTATED".to_owned()),
            )
            .map_err(|_| RefreshExchangeError)
        })
        .await;
    assert!(matches!(result, Err(RefreshError::InvalidResponse)));
    assert_eq!(
        temp.load().await.refresh_token(),
        "TEST_ONLY_REFRESH_INITIAL"
    );
}
