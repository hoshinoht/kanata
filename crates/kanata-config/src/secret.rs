//! Secret resolution and bearer-token canonicalisation.

use std::fmt;
use std::fs::File;
use std::io::Read;

use crate::SecretReference;

pub const MAX_BEARER_TOKEN_BYTES: usize = 4096;
const MAX_FILE_SECRET_BYTES: usize = MAX_BEARER_TOKEN_BYTES + 2;

pub trait SecretResolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError>;
}

pub struct EnvironmentSecretResolver;

impl SecretResolver for EnvironmentSecretResolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        match reference {
            SecretReference::Env(name) => std::env::var(name)
                .map(String::into_bytes)
                .map_err(|_| SecretResolutionError),
            SecretReference::File(path) => read_bounded_file(path),
        }
    }
}

fn read_bounded_file(path: &std::path::Path) -> Result<Vec<u8>, SecretResolutionError> {
    let mut file = File::open(path).map_err(|_| SecretResolutionError)?;
    let mut secret = Vec::with_capacity(MAX_FILE_SECRET_BYTES);
    file.by_ref()
        .take((MAX_FILE_SECRET_BYTES + 1) as u64)
        .read_to_end(&mut secret)
        .map_err(|_| SecretResolutionError)?;
    if secret.len() > MAX_FILE_SECRET_BYTES {
        return Err(SecretResolutionError);
    }
    Ok(secret)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretResolutionError;

impl fmt::Display for SecretResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("secret resolution failed")
    }
}

impl std::error::Error for SecretResolutionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthBuildError {
    InvalidSecret,
    DuplicateSecret,
}

impl fmt::Display for AuthBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSecret => "application key is invalid",
            Self::DuplicateSecret => "application keys contain duplicate material",
        })
    }
}

impl std::error::Error for AuthBuildError {}

pub fn canonical_bearer_token(
    reference: &SecretReference,
    mut secret: Vec<u8>,
) -> Result<String, AuthBuildError> {
    if matches!(reference, SecretReference::File(_)) {
        strip_one_terminal_newline(&mut secret);
    }
    let token = std::str::from_utf8(&secret).map_err(|_| AuthBuildError::InvalidSecret)?;
    if !valid_b64token(token.as_bytes()) {
        return Err(AuthBuildError::InvalidSecret);
    }
    Ok(token.into())
}

fn strip_one_terminal_newline(secret: &mut Vec<u8>) {
    if secret.last() == Some(&b'\n') {
        secret.pop();
        if secret.last() == Some(&b'\r') {
            secret.pop();
        }
    }
}

pub fn valid_b64token(token: &[u8]) -> bool {
    if token.is_empty() || token.len() > MAX_BEARER_TOKEN_BYTES {
        return false;
    }
    let mut padding = false;
    for byte in token {
        match byte {
            b'=' if !padding => padding = true,
            b'=' => {}
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'+' | b'/'
                if !padding => {}
            _ => return false,
        }
    }
    !matches!(token[0], b'=')
}
