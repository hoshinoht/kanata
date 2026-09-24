#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Codex credential storage is supported only on Linux and macOS");

mod credential;
mod error;
mod file;
mod keyring;
mod login;
#[allow(dead_code)]
mod net;
mod record;
mod refresh;
mod store;
mod token;

pub use credential::Credential;
pub use error::StoreError;
pub use login::{
    AuthorizationCodeForm, AuthorizationCodeRequest, CODEX_CLIENT_ID, CODEX_DEVICE_REDIRECT_URI,
    CODEX_DEVICE_TOKEN_ENDPOINT, CODEX_DEVICE_USERCODE_ENDPOINT, CODEX_DEVICE_VERIFICATION_URL,
    CODEX_ISSUER, CODEX_TOKEN_ENDPOINT, DeviceAuthorization, DeviceCodeRequest, DevicePollBody,
    DevicePollFailure, DevicePollRequest, DevicePollResponse, LoginError, LoginExchangeFailure,
    LoginTokenResponse, MAX_DEVICE_POLL_INTERVAL, MAX_LOGIN_DURATION, complete_device_login,
};
#[allow(dead_code, unused_imports)]
pub(crate) use net::CodexAuthClient;
pub use refresh::{
    AccessToken, MAX_REFRESH_LOCK_WAIT, MAX_REFRESH_SKEW, RefreshCoordinator, RefreshError,
    RefreshExchangeError, RefreshResponse,
};
pub use store::{CredentialStore, LockedStore};
pub use token::{normalize_authorization_code_response, normalize_refresh_response};
