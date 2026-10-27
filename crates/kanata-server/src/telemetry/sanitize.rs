pub(crate) use crate::core::sanitize::path;

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
