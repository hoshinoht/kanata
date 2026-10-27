mod args;
mod display;
mod edit;
mod new;
mod picker;
mod report;
mod rm;
mod rotate;
use args::*;
pub use args::{KeyCommand, expires_at, parse_expires};
use display::*;
pub use display::{key_json, missing_routes_json, rate_limit_json, scope_json};
use edit::edit;
use new::new;
pub use picker::pick;
use picker::run_picker;
use rm::rm;
use rotate::rotate;

// `kanata key` commands. Synchronous, host-only; writes go through store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::config::{self, KeyRateLimit, ValidatedConfig, parse_model_alias, valid_identifier};
use crate::core::{ModelAlias, Operation, RouteSelector};
use crate::file::{self, KeysFile, StoredKey};
use crate::store::{self, AuditEvent, LockedKeys, SecretFile};
use crate::time::{self, SECONDS_PER_DAY};
use crate::usage::{self, KeyUsage};

const KEY_PREFIX: &str = "kanata_sk_";
const PICKER_ATTEMPTS: usize = 3;
const APPLY_NOTE: &str = "note: the change applies within ~2 s without a restart; a rejected reload is logged as a WARN (kanata::keys)";
const ROUTES_HINT: &str = "run \"kanata routes --config <path>\" to see all";

/// How a route may be granted, as classified by the caller from the config.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exposure {
    /// Listed in `publication.public_routes`.
    Public,
    /// Private listener only.
    Private,
    /// Can never be public; a key holding it is dropped from the public plane.
    Never,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteChoice {
    pub selector: RouteSelector,
    pub exposure: Exposure,
}

/// Runs a `kanata key` command; `Ok` is the stdout text.
pub fn run(
    command: KeyCommand,
    catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
) -> Result<String, String> {
    match command {
        KeyCommand::New(args) => new(args, catalog),
        KeyCommand::List(args) => list(args),
        KeyCommand::Show(args) => show(args),
        KeyCommand::Usage(args) => report::run(args),
        KeyCommand::Edit(args) => edit(args, catalog),
        KeyCommand::Rm(args) => rm(args),
        KeyCommand::Rotate(args) => rotate(args),
    }
}

// ---- locations ----

/// Loads configuration without reading keys.
fn load_config(path: &Path) -> Result<ValidatedConfig, String> {
    config::load_deferring_keys(path).map_err(|error| error.to_string())
}

/// `--keys` resolved against the current directory.
fn keys_override(keys: Option<&Path>) -> Result<Option<PathBuf>, String> {
    keys.map(|path| {
        std::path::absolute(path).map_err(|_| format!("cannot resolve --keys {}", path.display()))
    })
    .transpose()
}

/// Active scopes whose route is no longer in the config, per key id.
fn dangling_scopes(keys: &KeysFile, config: &ValidatedConfig) -> Vec<(String, Vec<RouteSelector>)> {
    keys.active()
        .filter_map(|record| {
            let missing: Vec<RouteSelector> = record
                .permissions()
                .iter()
                .filter(|scope| !route_exists(config, scope))
                .cloned()
                .collect();
            (!missing.is_empty()).then(|| (record.id().to_owned(), missing))
        })
        .collect()
}

fn route_exists(config: &ValidatedConfig, scope: &RouteSelector) -> bool {
    config
        .routes()
        .iter()
        .any(|route| route.identity().selector == *scope)
}

/// Refuses to write while an active key references a removed route, naming the fixes;
/// then writes with full server validation.
fn write_keys(
    locked: &LockedKeys,
    keys: &KeysFile,
    config: &ValidatedConfig,
) -> Result<(), String> {
    let dangling = dangling_scopes(keys, config);
    if !dangling.is_empty() {
        let lines: Vec<String> = dangling
            .iter()
            .map(|(id, scopes)| {
                let removes: Vec<String> = scopes
                    .iter()
                    .map(|scope| {
                        format!(
                            "--remove-{} {}",
                            scope.operation.as_str(),
                            scope.model_alias.0
                        )
                    })
                    .collect();
                format!(
                    "key {id:?} references routes missing from the config: {}; re-add the route, or run `kanata key rm {id}` or `kanata key edit {id} {}`",
                    scope_list(scopes),
                    removes.join(" ")
                )
            })
            .collect();
        return Err(lines.join("\n"));
    }
    locked.write(keys, config.routes())
}

/// Config plus keys path for a mutating command.
fn mutating_location(location: &WriteLocation) -> Result<(ValidatedConfig, PathBuf), String> {
    let config = load_config(&location.config)?;
    let keys_path = keys_override(location.keys.as_deref())?
        .unwrap_or_else(|| config.key_source().path.clone());
    Ok((config, keys_path))
}

struct ReadSource {
    config: Option<ValidatedConfig>,
    keys: KeysFile,
    usage_dir: Option<PathBuf>,
}

fn read_location(location: &ReadLocation, usage_dir: Option<&Path>) -> Result<ReadSource, String> {
    let config = location.config.as_deref().map(load_config).transpose()?;
    let (derived_path, derived_usage) = config.as_ref().map_or((None, None), |config| {
        let source = config.key_source();
        (Some(source.path.clone()), source.usage_dir.clone())
    });
    let keys_path = keys_override(location.keys.as_deref())?
        .or(derived_path)
        .ok_or_else(|| "pass --config <path> or --keys <path>".to_owned())?;
    let usage_dir = usage_dir.map(Path::to_path_buf).or(derived_usage);
    let keys = file::load(&keys_path)?;
    Ok(ReadSource {
        config,
        keys,
        usage_dir,
    })
}

// ---- scopes ----

fn scope_label(selector: &RouteSelector) -> String {
    format!("{}:{}", selector.operation.as_str(), selector.model_alias.0)
}

/// The configured `[keys] usage_dir`, required for daily quotas.
pub fn usage_dir(config: &ValidatedConfig) -> Option<&Path> {
    config.key_source().usage_dir.as_deref()
}

/// Whether a key with these scopes is loaded by the public listener.
pub fn publicly_usable(owner: bool, scopes: &[RouteSelector], routes: &[RouteChoice]) -> bool {
    let exposure = |scope: &RouteSelector| {
        routes
            .iter()
            .find(|route| route.selector == *scope)
            .map(|route| route.exposure)
    };
    !owner
        && scopes
            .iter()
            .all(|scope| exposure(scope) != Some(Exposure::Never))
        && scopes
            .iter()
            .any(|scope| exposure(scope) == Some(Exposure::Public))
}

fn selector(alias: &str, operation: Operation) -> RouteSelector {
    RouteSelector {
        model_alias: ModelAlias(alias.to_owned()),
        operation,
    }
}

/// Resolves a scope flag against the config's routes with a helpful error.
/// `flag` names the flag for each operation (e.g. `--add-chat`).
fn resolve_scope(
    routes: &[RouteChoice],
    alias: &str,
    operation: Operation,
    flag: fn(Operation) -> &'static str,
) -> Result<RouteSelector, String> {
    let wanted = selector(alias, operation);
    if routes.iter().any(|route| route.selector == wanted) {
        return Ok(wanted);
    }
    let name = operation.as_str();
    if let Some(other) = routes
        .iter()
        .find(|route| route.selector.model_alias.0 == alias)
    {
        let other = other.selector.operation;
        return Err(format!(
            "no {name} route \"{alias}\"; \"{alias}\" is a {} route, use {} {alias}",
            other.as_str(),
            flag(other)
        ));
    }
    let candidates = routes
        .iter()
        .filter(|route| route.selector.operation == operation)
        .map(|route| route.selector.model_alias.0.as_str());
    Err(unknown_alias_message(
        &format!("no {name} route \"{alias}\""),
        alias,
        candidates,
    ))
}

fn unknown_alias_message<'a>(
    prefix: &str,
    alias: &str,
    candidates: impl Iterator<Item = &'a str>,
) -> String {
    let suggestion = candidates
        .map(|candidate| (edit_distance(alias, candidate), candidate))
        .filter(|(distance, _)| *distance <= 2)
        .min_by_key(|(distance, _)| *distance);
    match suggestion {
        Some((_, candidate)) => {
            format!("{prefix}; did you mean \"{candidate}\"? {ROUTES_HINT}")
        }
        None => format!("{prefix}; {ROUTES_HINT}"),
    }
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, l) in left.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, r) in right.iter().enumerate() {
            let substitute = previous[j] + usize::from(l != *r);
            current.push(substitute.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[right.len()]
}

fn scope_flags(
    given: [(Operation, &[String]); 4],
    routes: &[RouteChoice],
    flag: fn(Operation) -> &'static str,
) -> Result<Vec<RouteSelector>, String> {
    let mut scopes = Vec::new();
    for (operation, aliases) in given {
        for alias in aliases {
            if parse_model_alias(alias).is_none() {
                return Err(format!("model alias \"{alias}\" is invalid"));
            }
            let scope = resolve_scope(routes, alias, operation, flag)?;
            if scopes.contains(&scope) {
                return Err(format!("duplicate scope {}", scope_label(&scope)));
            }
            scopes.push(scope);
        }
    }
    Ok(scopes)
}

fn new_flag(operation: Operation) -> &'static str {
    match operation {
        Operation::Chat => "--chat",
        Operation::Transcription => "--transcription",
        Operation::Embeddings => "--embeddings",
        Operation::Speech => "--speech",
    }
}

fn add_flag(operation: Operation) -> &'static str {
    match operation {
        Operation::Chat => "--add-chat",
        Operation::Transcription => "--add-transcription",
        Operation::Embeddings => "--add-embeddings",
        Operation::Speech => "--add-speech",
    }
}

// ---- secrets ----

pub fn generate_key() -> Result<(String, [u8; 32]), String> {
    use base64::Engine as _;
    use sha2::Digest as _;

    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|_| "operating system randomness is unavailable")?;
    let key = format!(
        "{KEY_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
    );
    let digest = sha2::Sha256::digest(key.as_bytes()).into();
    Ok((key, digest))
}

fn secret_notice(id: &str, expires_at: Option<u64>, key_out: Option<&Path>) {
    match key_out {
        Some(path) => eprintln!(
            "The key for {id:?} was written to {}; that file is its only copy.",
            path.display()
        ),
        None => eprintln!("This is the only time the key for {id:?} is shown."),
    }
    eprintln!(
        "Kanata stores only its hash; if the key is lost, replace it with `kanata key rotate {id}`."
    );
    match expires_at {
        Some(at) => eprintln!("Expires: {}", time::format(at)),
        None => eprintln!("Expires: never"),
    }
    eprintln!("{APPLY_NOTE}");
}

/// Prints the key (unless written to a file), then the one-time notice.
/// Returns stdout text still to print; empty when the key was printed.
fn reveal(
    id: &str,
    key: &str,
    expires_at: Option<u64>,
    key_out: Option<&Path>,
    verb: &str,
) -> String {
    if key_out.is_none() {
        println!("{key}");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    secret_notice(id, expires_at, key_out);
    match key_out {
        Some(_) => format!("{verb} key {id}"),
        None => String::new(),
    }
}

/// Writes under the lock, audits, and on success publishes the key output file.
/// An audit failure after the write still reveals a new secret so it is not lost.
fn commit_secret(
    locked: &LockedKeys,
    keys: &KeysFile,
    config: &ValidatedConfig,
    event: AuditEvent,
    key: &str,
    key_out: Option<SecretFile>,
) -> Result<(), String> {
    write_keys(locked, keys, config)?;
    let published = key_out.map(SecretFile::publish).transpose();
    let audited = locked.audit(&[event]);
    match (published, audited) {
        (Ok(_), Ok(())) => Ok(()),
        (Err(error), _) => Err(format!(
            "{error}; the key change was applied, replace the key with `kanata key rotate`"
        )),
        (Ok(published), Err(error)) => {
            if published.is_none() {
                println!("{key}");
            }
            Err(error)
        }
    }
}

/// The quota after `flags`; the clear flags are exclusive with their quota flags at parse time.
/// `period` is `daily` or `monthly`.
fn quota_flags(
    flags: QuotaFlags,
    clear: bool,
    existing: Option<crate::quota::DailyQuota>,
    config: &ValidatedConfig,
    period: &str,
) -> Result<Option<crate::quota::DailyQuota>, String> {
    let QuotaFlags([requests, tokens, reservation_tokens]) = flags;
    if clear {
        return Ok(None);
    }
    if !flags.given() {
        return Ok(existing);
    }
    if usage_dir(config).is_none() {
        return Err(format!("{period} quotas require [keys] usage_dir"));
    }
    let quota = crate::quota::DailyQuota {
        requests: requests.or(existing.and_then(|quota| quota.requests)),
        tokens: tokens.or(existing.and_then(|quota| quota.tokens)),
        reservation_tokens: reservation_tokens
            .or(existing.and_then(|quota| quota.reservation_tokens)),
    };
    quota
        .validate()
        .map_err(|class| format!("invalid {period} quota: {class}"))?;
    Ok(Some(quota))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(alias: &str, operation: Operation, exposure: Exposure) -> RouteChoice {
        RouteChoice {
            selector: selector(alias, operation),
            exposure,
        }
    }

    fn routes() -> Vec<RouteChoice> {
        vec![
            choice("local-chat", Operation::Chat, Exposure::Public),
            choice("private-chat", Operation::Chat, Exposure::Private),
            choice(
                "private-transcribe",
                Operation::Transcription,
                Exposure::Private,
            ),
        ]
    }

    fn run_pick(
        input: &str,
        current: &[RouteSelector],
    ) -> (Result<Vec<RouteSelector>, String>, String) {
        let mut output = Vec::new();
        let result = pick(&routes(), current, &mut input.as_bytes(), &mut output);
        (result, String::from_utf8(output).expect("utf8"))
    }

    #[test]
    fn picker_grants_selected_numbers_and_marks_current() {
        let current = [selector("private-chat", Operation::Chat)];
        let (result, output) = run_pick("1, 3\n", &current);
        assert_eq!(
            result,
            Ok(vec![
                selector("local-chat", Operation::Chat),
                selector("private-transcribe", Operation::Transcription),
            ])
        );
        assert!(output.contains("  2 * private-chat"), "{output}");
        assert!(output.contains("  1   local-chat"), "{output}");
    }

    #[test]
    fn picker_reprompts_then_gives_up_after_three_invalid_answers() {
        let (result, output) = run_pick("\n9\nlocl-chat\n1\n", &[]);
        assert_eq!(result, Err("no valid selection after 3 attempts".into()));
        assert!(output.contains("no route number 9"));
        assert!(output.contains("did you mean \"local-chat\"?"));
        assert_eq!(output.matches("Select routes").count(), 3);
    }

    #[test]
    fn scope_errors_suggest_near_aliases_and_the_right_flag() {
        let routes = routes();
        let typo = resolve_scope(&routes, "local-chta", Operation::Chat, new_flag).unwrap_err();
        assert!(
            typo.starts_with("no chat route \"local-chta\"; did you mean \"local-chat\"?"),
            "{typo}"
        );
        assert!(typo.contains("kanata routes"));
        let far = resolve_scope(&routes, "unrelated", Operation::Chat, new_flag).unwrap_err();
        assert!(!far.contains("did you mean"));
        let other =
            resolve_scope(&routes, "private-transcribe", Operation::Chat, add_flag).unwrap_err();
        assert!(
            other.contains("is a transcription route, use --add-transcription private-transcribe"),
            "{other}"
        );
    }
}
