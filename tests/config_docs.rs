use std::collections::BTreeSet;
use std::fs;

use kanata::config::ConfigErrorClass;

fn readme() -> String {
    fs::read_to_string("config/README.md").expect("config README is readable")
}

#[test]
fn every_config_error_class_is_documented() {
    let readme = readme();
    let missing: Vec<_> = ConfigErrorClass::ALL
        .iter()
        .map(|class| class.as_str())
        .filter(|class| !readme.contains(&format!("`{class}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "config/README.md lacks error classes {missing:?}"
    );
}

/// Serialized field names of every struct in `src/config/raw.rs`.
fn raw_field_names() -> BTreeSet<String> {
    let source = fs::read_to_string("src/config/raw.rs").expect("raw schema is readable");
    let mut names = BTreeSet::new();
    let mut in_struct = false;
    let mut rename = None;
    let mut skip = false;
    for line in source.lines().map(str::trim) {
        if line.contains(" struct ") && line.ends_with('{') {
            in_struct = true;
            continue;
        }
        if !in_struct {
            continue;
        }
        if line == "}" {
            in_struct = false;
            continue;
        }
        if let Some(attribute) = line.strip_prefix("#[serde(") {
            skip |= attribute
                .split([',', ')'])
                .any(|part| part.trim() == "skip");
            if let Some((_, rest)) = attribute.split_once("rename = \"") {
                rename = rest.split('"').next().map(str::to_owned);
            }
            continue;
        }
        let field = line
            .strip_prefix("pub(super) ")
            .or_else(|| line.strip_prefix("pub(crate) "))
            .or_else(|| line.strip_prefix("pub "))
            .unwrap_or(line);
        if let Some((name, _)) = field.split_once(':')
            && !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        {
            let name = rename.take().unwrap_or_else(|| name.to_owned());
            if !std::mem::take(&mut skip) {
                names.insert(name);
            }
        }
    }
    names
}

#[test]
fn every_raw_config_field_is_documented() {
    let names = raw_field_names();
    assert!(names.len() > 50, "field extraction found only {names:?}");
    // Identifier tokens inside backtick spans, e.g. `adapters[].max_in_flight` or `include`.
    let readme = readme();
    let documented: BTreeSet<_> = readme
        .split('`')
        .skip(1)
        .step_by(2)
        .flat_map(|span| span.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')))
        .collect();
    let missing: Vec<_> = names
        .iter()
        .filter(|name| !documented.contains(name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "config/README.md lacks raw config fields {missing:?}"
    );
}
