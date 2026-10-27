use std::borrow::Cow;

pub(super) mod chat;
mod messages;
mod options;
mod tools;
pub(super) use chat::{ChatWire, parse as parse_chat};

/// Longest `param` echoed to clients; longer paths are cut at a segment boundary.
const MAX_PARAM_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::api) enum ChatWireError {
    /// Not a JSON object, or the offending member cannot be named safely.
    Malformed,
    /// JSON path of the offending member, e.g. `messages[2].refusal`.
    InvalidParam(Cow<'static, str>),
    TooLarge,
}

impl ChatWireError {
    pub(in crate::api) fn param(param: impl Into<Cow<'static, str>>) -> Self {
        Self::InvalidParam(param.into())
    }

    /// Appends `member` to `parent`, or blames `parent` when `member` is not a safe key.
    pub(in crate::api) fn member(parent: &str, member: &str) -> Self {
        if !safe_key(member) {
            return if parent.is_empty() {
                Self::Malformed
            } else {
                Self::param(parent.to_owned())
            };
        }
        if parent.is_empty() {
            Self::param(member.to_owned())
        } else {
            Self::param(bounded(format!("{parent}.{member}")))
        }
    }
}

/// Client keys are echoed only when they cannot carry markup or secrets of any length.
fn safe_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn bounded(mut path: String) -> String {
    if path.len() > MAX_PARAM_BYTES {
        let cut = path[..=MAX_PARAM_BYTES]
            .rfind(['.', '['])
            .unwrap_or(MAX_PARAM_BYTES);
        path.truncate(cut);
    }
    path
}

/// Maps a typed-parse failure to the JSON path of the member that caused it.
pub(in crate::api) fn serde_error(
    error: serde_path_to_error::Error<serde_json::Error>,
) -> ChatWireError {
    use std::fmt::Write as _;

    if !error.inner().is_data() {
        return ChatWireError::Malformed;
    }
    let mut path = String::new();
    let mut last_key = None;
    let mut complete = true;
    for segment in error.path().iter() {
        match segment {
            serde_path_to_error::Segment::Seq { index } if !path.is_empty() => {
                let _ = write!(path, "[{index}]");
            }
            serde_path_to_error::Segment::Map { key }
            | serde_path_to_error::Segment::Enum { variant: key }
                if safe_key(key) =>
            {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(key);
                last_key = Some(key.as_str());
            }
            _ => {
                complete = false;
                break;
            }
        }
    }
    // Missing fields are reported at their parent; unknown ones may be too.
    let message = error.inner().to_string();
    if complete
        && let Some(field) = ["unknown field `", "missing field `"]
            .iter()
            .find_map(|prefix| message.strip_prefix(prefix))
            .and_then(|rest| rest.split('`').next())
        && last_key != Some(field)
    {
        return ChatWireError::member(&path, field);
    }
    if path.is_empty() {
        ChatWireError::Malformed
    } else {
        ChatWireError::param(bounded(path))
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
