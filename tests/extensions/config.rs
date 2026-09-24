use kanata::{
    core::ExtensionKey,
    routing::{Registry, RouteEntry},
};

use super::support::{EXTENSION_KEY, config_contents, example_contents, load_contents};

#[test]
fn omitted_allowlists_default_to_deny_and_registry_retains_both_sets() {
    let config = load_contents(example_contents()).expect("example config");
    assert!(
        config
            .adapters()
            .iter()
            .all(|adapter| adapter.extension_allowlist().is_empty())
    );
    assert!(
        config
            .routes()
            .iter()
            .all(|route| route.extension_allowlist().is_empty())
    );

    let config = load_contents(config_contents(
        &[EXTENSION_KEY],
        &[EXTENSION_KEY],
        &[],
        8192,
    ))
    .expect("allowlisted config");
    let registry = Registry::from_validated(&config);
    let route: &RouteEntry = registry
        .resolve(&kanata::core::RouteSelector {
            model_alias: kanata::core::ModelAlias("private-chat".into()),
            operation: kanata::core::Operation::Chat,
        })
        .expect("route");
    let key = ExtensionKey::parse(EXTENSION_KEY).expect("key");
    assert!(route.extension_allowlist.contains(&key));
    assert!(route.adapter_extension_allowlist.contains(&key));
}

#[test]
fn invalid_and_duplicate_allowlist_entries_are_indexed_without_echoing_values() {
    let invalid_value = "wildcard.*";
    let invalid = load_contents(config_contents(&[invalid_value], &[], &[], 8192))
        .expect_err("invalid adapter extension key");
    assert_eq!(
        invalid,
        "config error at adapters[1].extension_allowlist[0]: invalid_extension_key"
    );
    assert!(!invalid.contains(invalid_value));

    let duplicate = load_contents(config_contents(
        &[EXTENSION_KEY, EXTENSION_KEY],
        &[],
        &[],
        8192,
    ))
    .expect_err("duplicate adapter extension key");
    assert_eq!(
        duplicate,
        "config error at adapters[1].extension_allowlist[1]: duplicate"
    );

    let invalid_route = load_contents(config_contents(&[], &["route"], &[], 8192))
        .expect_err("invalid route extension key");
    assert_eq!(
        invalid_route,
        "config error at routes[1].extension_allowlist[0]: invalid_extension_key"
    );

    let duplicate_route = load_contents(config_contents(
        &[],
        &[EXTENSION_KEY, EXTENSION_KEY],
        &[],
        8192,
    ))
    .expect_err("duplicate route extension key");
    assert_eq!(
        duplicate_route,
        "config error at routes[1].extension_allowlist[1]: duplicate"
    );
}
