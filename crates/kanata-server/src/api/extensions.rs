use std::collections::BTreeSet;

use crate::core::{ExtensionKey, Extensions};

pub(crate) fn validate_extensions(
    extensions: &Extensions,
    route_allowlist: &BTreeSet<ExtensionKey>,
    adapter_allowlist: &BTreeSet<ExtensionKey>,
    max_bytes: usize,
) -> bool {
    serde_json::to_vec(extensions).is_ok_and(|bytes| bytes.len() <= max_bytes)
        && extensions
            .iter()
            .all(|(key, _)| route_allowlist.contains(key) && adapter_allowlist.contains(key))
}
