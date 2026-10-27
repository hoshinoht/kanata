use std::collections::BTreeSet;

use kanata::core::{ErrorKind, TimeoutPhase};
use serde_json::Value;

const PATHS: [&str; 8] = [
    "/v1/audio/speech",
    "/v1/audio/transcriptions",
    "/v1/chat/completions",
    "/v1/embeddings",
    "/v1/messages",
    "/v1/models",
    "/v1/models/{id}",
    "/v1/responses",
];

fn document() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/openapi.json"))
        .expect("read docs/openapi.json");
    serde_json::from_str(&text).expect("docs/openapi.json is JSON")
}

/// Every `ErrorKind`; the match fails to compile when a variant is added.
fn error_kinds() -> Vec<ErrorKind> {
    let phases = [
        TimeoutPhase::Upload,
        TimeoutPhase::Queue,
        TimeoutPhase::Connect,
        TimeoutPhase::Headers,
        TimeoutPhase::FirstByte,
        TimeoutPhase::Idle,
        TimeoutPhase::Overall,
    ];
    for phase in phases {
        match phase {
            TimeoutPhase::Upload
            | TimeoutPhase::Queue
            | TimeoutPhase::Connect
            | TimeoutPhase::Headers
            | TimeoutPhase::FirstByte
            | TimeoutPhase::Idle
            | TimeoutPhase::Overall => {}
        }
    }
    let mut kinds = vec![
        ErrorKind::InvalidRequest,
        ErrorKind::Unauthorized,
        ErrorKind::Forbidden,
        ErrorKind::NotFound,
        ErrorKind::Conflict,
        ErrorKind::RateLimited,
        ErrorKind::Cancelled,
        ErrorKind::UpstreamUnavailable,
        ErrorKind::UpstreamFailure,
        ErrorKind::UnsupportedOperation,
        ErrorKind::Internal,
    ];
    kinds.extend(phases.map(|phase| ErrorKind::Timeout { phase }));
    for kind in &kinds {
        match kind {
            ErrorKind::InvalidRequest
            | ErrorKind::Unauthorized
            | ErrorKind::Forbidden
            | ErrorKind::NotFound
            | ErrorKind::Conflict
            | ErrorKind::RateLimited
            | ErrorKind::Timeout { .. }
            | ErrorKind::Cancelled
            | ErrorKind::UpstreamUnavailable
            | ErrorKind::UpstreamFailure
            | ErrorKind::UnsupportedOperation
            | ErrorKind::Internal => {}
        }
    }
    kinds
}

fn string_set(value: &Value) -> BTreeSet<&str> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|item| item.as_str().expect("string"))
        .collect()
}

#[test]
fn version_matches_the_crate() {
    assert_eq!(
        document()["info"]["version"],
        env!("CARGO_PKG_VERSION"),
        "bump docs/openapi.json info.version with Cargo.toml"
    );
}

#[test]
fn paths_and_responses_are_complete() {
    let document = document();
    let paths = document["paths"].as_object().expect("paths");
    assert_eq!(
        paths.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(PATHS)
    );
    for (path, item) in paths {
        for (method, operation) in item.as_object().expect("path item") {
            let responses = operation["responses"].as_object().expect("responses");
            assert!(
                responses.keys().any(|status| status.starts_with('2')),
                "{method} {path} has no 2xx response"
            );
            let mut expected = vec!["401", "403"];
            if method == "post" {
                expected.push("400");
            }
            if path.contains('{') {
                expected.push("404");
            }
            for status in expected {
                assert!(
                    responses.contains_key(status),
                    "{method} {path} lacks {status}"
                );
            }
        }
    }
}

#[test]
fn every_contract_error_is_documented_with_its_status() {
    let document = document();
    let schemas = &document["components"]["schemas"];
    let codes = string_set(&schemas["ErrorCode"]["enum"]);
    let types = string_set(&schemas["ErrorType"]["enum"]);
    for kind in error_kinds() {
        let mapping = kind.mapping();
        assert!(
            codes.contains(mapping.code),
            "{} undocumented",
            mapping.code
        );
        assert!(types.contains(mapping.error_type), "{}", mapping.error_type);
        // `conflict` is not emitted by any client path, so no response lists it.
        if kind == ErrorKind::Conflict {
            continue;
        }
        let response = &document["components"]["responses"][format!("E{}", mapping.status)];
        let description = response["description"].as_str().unwrap_or_default();
        assert!(
            description.contains(&format!("`{}`", mapping.code)),
            "E{} does not list {}",
            mapping.status,
            mapping.code
        );
    }
}

#[test]
fn every_quickstart_error_code_is_documented() {
    let guide = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/guides/public-api-quickstart.md"
    ))
    .expect("read quickstart");
    let document = document();
    let codes = string_set(&document["components"]["schemas"]["ErrorCode"]["enum"]);
    let mut seen = 0;
    for row in guide
        .lines()
        .filter(|line| line.starts_with("| 4") || line.starts_with("| 5"))
    {
        let Some(cell) = row.split('|').nth(2).map(str::trim) else {
            continue;
        };
        if !cell.starts_with('`') {
            continue;
        }
        for code in cell.split('`').skip(1).step_by(2) {
            assert!(codes.contains(code), "quickstart code {code} undocumented");
            seen += 1;
        }
    }
    assert!(seen >= 10, "quickstart error table not found");
}
