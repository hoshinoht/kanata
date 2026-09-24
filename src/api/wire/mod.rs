pub(super) mod chat;
mod messages;
mod options;
mod tools;
pub(super) use chat::ChatWire;
pub(super) use messages::ChatWireError;

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
