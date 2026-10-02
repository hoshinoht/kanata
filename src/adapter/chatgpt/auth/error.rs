use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthError {
    Storage,
    LockTimeout,
    InvalidProfile,
    ProfileLimit,
    NotSignedIn,
    PermissionDenied,
    InvalidCallback,
    ConsentDenied,
    Cancelled,
    LoginTimeout,
    InvalidIdentity,
    InvalidResponse,
    Discovery,
    Network,
    InvalidGrant,
    InvalidClient,
    RefreshTooEarly,
    StateChanged,
    Clock,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Storage => "ChatGPT credential storage failed; check the directory and file owner-only permissions",
            Self::LockTimeout => "timed out waiting for ChatGPT credential storage",
            Self::InvalidProfile => "profile must contain 1-64 ASCII letters, digits, underscores or hyphens",
            Self::ProfileLimit => "the ChatGPT profile limit was reached",
            Self::NotSignedIn => "sign in with ChatGPT before using this profile",
            Self::PermissionDenied => "this ChatGPT profile has not granted permission to use its plan",
            Self::InvalidCallback => "invalid ChatGPT authorization callback",
            Self::ConsentDenied => "ChatGPT authorization was declined",
            Self::Cancelled => "ChatGPT sign-in cancelled",
            Self::LoginTimeout => "ChatGPT sign-in timed out; start a new sign-in",
            Self::InvalidIdentity => "ChatGPT identity verification failed",
            Self::InvalidResponse => "invalid ChatGPT token response",
            Self::Discovery => "invalid ChatGPT identity provider metadata",
            Self::Network => "ChatGPT authentication service is unavailable",
            Self::InvalidGrant => "ChatGPT authorization expired or was revoked; sign in again",
            Self::InvalidClient => "ChatGPT rejected the saved client registration",
            Self::RefreshTooEarly => "ChatGPT token refresh is not yet available",
            Self::StateChanged => "the selected ChatGPT profile changed during sign-in; start again",
            Self::Clock => "the host clock is invalid for ChatGPT authentication",
        })
    }
}
impl std::error::Error for AuthError {}
