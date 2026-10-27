use std::{collections::BTreeMap, future::Future, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use url::Url;

use super::{
    AuthError, AuthManager, ProfileStatus, State, jwt,
    net::RESOURCE,
    profile_status, random_secret,
    record::{MAX_PROFILES, Registration, valid_client_id, valid_value, validate_profile},
    unix_now,
};

const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
const CALLBACK_BYTES: usize = 16 * 1024;

pub struct LoginFlow {
    manager: AuthManager,
    listener: TcpListener,
    profile: String,
    registration: Option<Registration>,
    revision: u64,
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
    url: String,
    deadline: tokio::time::Instant,
}
struct Callback {
    code: String,
    client_id: String,
}

impl LoginFlow {
    pub(super) async fn begin(manager: AuthManager, profile: &str) -> Result<Self, AuthError> {
        validate_profile(profile)?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| AuthError::Network)?;
        let redirect = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener
                .local_addr()
                .map_err(|_| AuthError::Network)?
                .port()
        );
        let locked = manager.store.lock().await?;
        let stored = locked.load()?;
        let state = match stored {
            Some(state) => state,
            None => {
                let state = State::new()?;
                locked.save(&state)?;
                state
            }
        };
        let registration = state.profiles.get(profile).cloned();
        if registration.is_none() && state.profiles.len() >= MAX_PROFILES {
            return Err(AuthError::ProfileLimit);
        }
        let nonce = random_secret()?;
        let verifier = random_secret()?;
        let csrf = random_secret()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = Url::parse("https://auth.openai.com/api/accounts/authorize")
            .map_err(|_| AuthError::Network)?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair(
                    "client_id",
                    registration
                        .as_ref()
                        .map_or("dynamic_agent_client", |r| &r.client_id),
                )
                .append_pair("ext_agent_host_id", &state.host_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", &redirect)
                .append_pair("scope", SCOPES)
                .append_pair("resource", RESOURCE)
                .append_pair("state", &csrf)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge_method", "S256")
                .append_pair("code_challenge", &challenge);
            if registration.is_none() {
                query.append_pair("agent_name_hint", "Kanata");
            }
        }
        drop(locked);
        Ok(Self {
            manager,
            listener,
            profile: profile.into(),
            registration,
            revision: state.revision,
            state: csrf,
            nonce,
            verifier,
            redirect,
            url: url.into(),
            deadline: tokio::time::Instant::now() + LOGIN_TIMEOUT,
        })
    }
    pub fn authorization_url(&self) -> &str {
        &self.url
    }
    pub async fn finish<C: Future<Output = ()>>(
        self,
        cancellation: C,
    ) -> Result<ProfileStatus, AuthError> {
        tokio::select! {
            biased;
            _ = cancellation => Err(AuthError::Cancelled),
            result = tokio::time::timeout_at(self.deadline, self.complete()) => result.map_err(|_| AuthError::LoginTimeout)?,
        }
    }
    async fn complete(mut self) -> Result<ProfileStatus, AuthError> {
        let callback = self.callback().await?;
        let locked = self.manager.store.lock().await?;
        let mut state = locked.load()?.ok_or(AuthError::StateChanged)?;
        if state.revision != self.revision
            || state.profiles.get(&self.profile) != self.registration.as_ref()
        {
            return Err(AuthError::StateChanged);
        }
        if self.registration.is_none() {
            let registration = Registration {
                client_id: callback.client_id.clone(),
                identity: None,
                session: None,
            };
            state
                .profiles
                .insert(self.profile.clone(), registration.clone());
            state.changed()?;
            locked.save(&state)?;
            self.registration = Some(registration);
            self.revision = state.revision;
        }
        drop(locked);
        let metadata = self.manager.client.discovery().await?;
        let response = self
            .manager
            .client
            .token(&[
                ("grant_type", "authorization_code"),
                ("client_id", &callback.client_id),
                ("code", &callback.code),
                ("code_verifier", &self.verifier),
                ("redirect_uri", &self.redirect),
                ("resource", RESOURCE),
            ])
            .await?;
        let jwks = self.manager.client.jwks(&metadata).await?;
        let identity = jwt::verify(
            response
                .id_token
                .as_deref()
                .ok_or(AuthError::InvalidIdentity)?,
            &jwks,
            &callback.client_id,
            Some(&self.nonce),
            &response.access_token,
            unix_now()?,
        )?;
        if self
            .registration
            .as_ref()
            .and_then(|r| r.identity.as_ref())
            .is_some_and(|previous| {
                previous.issuer != identity.issuer || previous.subject != identity.subject
            })
        {
            return Err(AuthError::InvalidIdentity);
        }
        let session = response.session(unix_now()?, None, false)?;
        let locked = self.manager.store.lock().await?;
        let mut state = locked.load()?.ok_or(AuthError::StateChanged)?;
        if state.revision != self.revision
            || state.profiles.get(&self.profile) != self.registration.as_ref()
        {
            return Err(AuthError::StateChanged);
        }
        if state
            .profiles
            .iter()
            .any(|(name, profile)| name != &self.profile && profile.client_id == callback.client_id)
        {
            return Err(AuthError::StateChanged);
        }
        let registration = Registration {
            client_id: callback.client_id,
            identity: Some(identity),
            session: Some(session),
        };
        let status = profile_status(&self.profile, &registration);
        state.profiles.insert(self.profile, registration);
        state.active_profile = Some(status.profile.clone());
        state.changed()?;
        locked.save(&state)?;
        Ok(status)
    }
    async fn callback(&self) -> Result<Callback, AuthError> {
        for _ in 0..32 {
            let (mut stream, peer) = self
                .listener
                .accept()
                .await
                .map_err(|_| AuthError::Network)?;
            if !peer.ip().is_loopback() {
                continue;
            }
            let parsed =
                tokio::time::timeout(Duration::from_secs(5), self.read_callback(&mut stream)).await;
            let result = parsed.unwrap_or(Err(AuthError::InvalidCallback));
            let success = result.is_ok();
            let body = if success {
                "Authorization received. Return to the Kanata terminal for the sign-in result."
            } else {
                "Authorization callback rejected. Return to the Kanata terminal."
            };
            let response = format!(
                "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{}",
                if success { "200 OK" } else { "400 Bad Request" },
                body.len(),
                body
            );
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                stream.write_all(response.as_bytes()),
            )
            .await;
            match result {
                Ok(callback) => return Ok(callback),
                Err(AuthError::ConsentDenied) => return Err(AuthError::ConsentDenied),
                _ => continue,
            }
        }
        Err(AuthError::InvalidCallback)
    }
    async fn read_callback(&self, stream: &mut TcpStream) -> Result<Callback, AuthError> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream
                .read(&mut buffer)
                .await
                .map_err(|_| AuthError::InvalidCallback)?;
            if read == 0 || bytes.len() + read > CALLBACK_BYTES {
                return Err(AuthError::InvalidCallback);
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        let request = std::str::from_utf8(&bytes).map_err(|_| AuthError::InvalidCallback)?;
        self.parse_callback(request)
    }
    fn parse_callback(&self, request: &str) -> Result<Callback, AuthError> {
        let (head, body) = request
            .split_once("\r\n\r\n")
            .ok_or(AuthError::InvalidCallback)?;
        if !body.is_empty() {
            return Err(AuthError::InvalidCallback);
        }
        let mut lines = head.split("\r\n");
        let parts = lines
            .next()
            .ok_or(AuthError::InvalidCallback)?
            .split(' ')
            .collect::<Vec<_>>();
        if parts.len() != 3 || parts[0] != "GET" || parts[2] != "HTTP/1.1" {
            return Err(AuthError::InvalidCallback);
        }
        let uri: http::Uri = parts[1].parse().map_err(|_| AuthError::InvalidCallback)?;
        if uri.authority().is_some() || uri.scheme().is_some() || uri.path() != "/auth/callback" {
            return Err(AuthError::InvalidCallback);
        }
        let mut hosts = Vec::new();
        for line in lines {
            let (name, value) = line.split_once(':').ok_or(AuthError::InvalidCallback)?;
            if name.eq_ignore_ascii_case("host") {
                hosts.push(value.trim());
            }
            if name.eq_ignore_ascii_case("transfer-encoding")
                || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
            {
                return Err(AuthError::InvalidCallback);
            }
        }
        let host = self
            .redirect
            .strip_prefix("http://")
            .and_then(|s| s.split_once('/'))
            .ok_or(AuthError::InvalidCallback)?
            .0;
        if hosts != [host] {
            return Err(AuthError::InvalidCallback);
        }
        let mut params = BTreeMap::new();
        for (key, value) in
            url::form_urlencoded::parse(uri.query().ok_or(AuthError::InvalidCallback)?.as_bytes())
        {
            if !valid_value(&key, 64)
                || !valid_value(&value, 4096)
                || params.len() >= 16
                || params
                    .insert(key.into_owned(), value.into_owned())
                    .is_some()
            {
                return Err(AuthError::InvalidCallback);
            }
        }
        let state = params.get("state").ok_or(AuthError::InvalidCallback)?;
        if !bool::from(state.as_bytes().ct_eq(self.state.as_bytes())) {
            return Err(AuthError::InvalidCallback);
        }
        if params.contains_key("error") {
            return Err(AuthError::ConsentDenied);
        }
        let client_id = match &self.registration {
            Some(registration) => {
                if params
                    .get("client_id")
                    .is_some_and(|id| id != &registration.client_id)
                {
                    return Err(AuthError::InvalidCallback);
                }
                registration.client_id.clone()
            }
            None => params
                .get("client_id")
                .filter(|id| valid_client_id(id))
                .cloned()
                .ok_or(AuthError::InvalidCallback)?,
        };
        Ok(Callback {
            code: params
                .get("code")
                .cloned()
                .ok_or(AuthError::InvalidCallback)?,
            client_id,
        })
    }
}
