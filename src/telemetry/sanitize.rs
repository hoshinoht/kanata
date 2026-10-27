const MAX_PATH_CHARS: usize = 64;
const MAX_TOKEN_CHARS: usize = 64;
const MAX_MESSAGE_CHARS: usize = 200;

/// Printable ASCII only, truncated.
pub(crate) fn path(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_graphic)
        .take(MAX_PATH_CHARS)
        .collect()
}

/// `[A-Za-z0-9_.:-]` only, truncated; `None` when nothing survives.
pub(crate) fn token(value: &str) -> Option<String> {
    let token: String = value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | ':' | '-')
        })
        .take(MAX_TOKEN_CHARS)
        .collect();
    (!token.is_empty()).then_some(token)
}

/// Control characters become spaces; truncated.
pub(crate) fn message(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_MESSAGE_CHARS)
        .collect()
}

/// Caller `x-request-id`: `Ok(None)` when absent, `Err(())` when repeated or unsafe.
pub(crate) fn request_id(headers: &http::HeaderMap) -> Result<Option<&str>, ()> {
    let mut values = headers.get_all("x-request-id").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?;
    (value.len() <= 128
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then_some(Some(value))
    .ok_or(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn sanitizers_bound_and_filter() {
        assert_eq!(
            super::path(&format!("/a\u{7}b{}", "x".repeat(100))).len(),
            64
        );
        assert_eq!(super::token("bad code\"}\n").as_deref(), Some("badcode"));
        assert_eq!(super::token("\"\" "), None);
        assert_eq!(super::message("a\nb\u{1b}[31m"), "a b [31m");
    }
}
