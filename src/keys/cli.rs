mod args;
mod display;
mod report;
use args::*;
use display::*;

// `kanata key` commands. Synchronous, host-only; writes go through store.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, IsTerminal as _, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::config::{
    self, KeyRateLimit, KeySource, ValidatedConfig, parse_model_alias, valid_identifier,
};
use crate::core::{ModelAlias, Operation, RouteSelector};
use crate::keys::file::{self, KeysFile, StoredKey};
use crate::keys::store::{self, AuditEvent, LockedKeys, SecretFile};
use crate::keys::time;
use crate::keys::usage::{self, KeyUsage};

pub const KEY_USAGE: &str = "usage: kanata key new --config <path> --id <id> [--chat <alias>]... [--transcription <alias>]... [--embeddings <alias>]... [--speech <alias>]... --expires <1|3|7|13|30|60|unlimited> [--owner] [--max-in-flight N] [--rate-limit N/MS] [--daily-requests N] [--daily-tokens N --reservation-tokens N] [--key-out <path>] [--keys <path>]
       kanata key list (--config <path>|--keys <path>) [--usage-dir <path>] [--all] [--json]
       kanata key show <id> (--config <path>|--keys <path>) [--usage-dir <path>] [--json]
       kanata key edit <id> --config <path> [--add-chat A]... [--remove-chat A]... [--add-transcription A]... [--remove-transcription A]... [--add-embeddings A]... [--remove-embeddings A]... [--add-speech A]... [--remove-speech A]... [--expires <choice>] [--max-in-flight N] [--rate-limit N/MS] [--daily-requests N] [--daily-tokens N --reservation-tokens N] [--clear-quota] [--clear-limits] [--force] [--keys <path>]
       kanata key rm <id> --config <path> [--force] [--keys <path>]
       kanata key rotate (<id>|--owner) --config <path> --expires <choice> [--key-out <path>] [--keys <path>]
       kanata key migrate --config <path> [--keys <path>]
       kanata key usage (--config <path>|--usage-dir <path>) [--plane all|private|public] [--key-id <id>] [--model <alias>] [--from YYYY-MM-DD] [--to YYYY-MM-DD] [--input-cost-per-million N --output-cost-per-million N --cost-unit <label>] [--json]";

const KEY_PREFIX: &str = "kanata_sk_";
const SECONDS_PER_DAY: u64 = 86_400;
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

/// Runs `kanata key <arguments>`; `Ok` is the stdout text.
pub fn run(
    arguments: &[String],
    catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
) -> Result<String, String> {
    let Some((command, rest)) = arguments.split_first() else {
        return Err(KEY_USAGE.into());
    };
    match command.as_str() {
        "new" => new(rest, catalog),
        "list" => list(rest),
        "show" => show(rest),
        "usage" => report::run(rest),
        "edit" => edit(rest, catalog),
        "rm" => rm(rest),
        "rotate" => rotate(rest),
        "migrate" => migrate(rest),
        _ => Err(KEY_USAGE.into()),
    }
}

// ---- arguments ----

/// Loads configuration without reading keys.
fn load_config(path: &str) -> Result<ValidatedConfig, String> {
    config::load_deferring_keys(path).map_err(|error| error.to_string())
}

/// `--keys` resolved against the current directory.
fn keys_override(args: &Args) -> Result<Option<PathBuf>, String> {
    args.one("--keys")?
        .map(|path| std::path::absolute(path).map_err(|_| format!("cannot resolve --keys {path}")))
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
                            operation_name(scope.operation),
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

/// Config plus keys path for a mutating command: `--config` is required.
fn mutating_location(args: &Args, command: &str) -> Result<(ValidatedConfig, PathBuf), String> {
    let config_path = args
        .one("--config")?
        .ok_or_else(|| format!("kanata key {command} requires --config <path>"))?;
    let config = load_config(config_path)?;
    let derived = match config.key_source() {
        KeySource::File { path, .. } => path.clone(),
        KeySource::Inline => {
            return Err(format!(
                "{config_path} uses inline [[application_keys]]; run `kanata key migrate --config {config_path}` first"
            ));
        }
    };
    let keys_path = keys_override(args)?.unwrap_or(derived);
    Ok((config, keys_path))
}

struct ReadLocation {
    config: Option<ValidatedConfig>,
    keys: KeysFile,
    usage_dir: Option<PathBuf>,
}

fn read_location(args: &Args) -> Result<ReadLocation, String> {
    let config = args.one("--config")?.map(load_config).transpose()?;
    let (derived_path, derived_usage) = match config.as_ref().map(ValidatedConfig::key_source) {
        Some(KeySource::File {
            path, usage_dir, ..
        }) => (Some(path.clone()), usage_dir.clone()),
        Some(KeySource::Inline) if args.one("--keys")?.is_none() => {
            return Err(
                "config uses inline [[application_keys]]; run `kanata key migrate` first".into(),
            );
        }
        _ => (None, None),
    };
    let keys_path = keys_override(args)?
        .or(derived_path)
        .ok_or_else(|| "pass --config <path> or --keys <path>".to_owned())?;
    let usage_dir = args
        .one("--usage-dir")?
        .map(PathBuf::from)
        .or(derived_usage);
    let keys = match file::read(&keys_path).map_err(|error| error.to_string())? {
        Some(bytes) => file::parse_without_routes(&bytes).map_err(|error| error.to_string())?,
        None => KeysFile::default(),
    };
    Ok(ReadLocation {
        config,
        keys,
        usage_dir,
    })
}

// ---- scopes ----

fn operation_name(operation: Operation) -> &'static str {
    operation.as_str()
}

fn scope_label(selector: &RouteSelector) -> String {
    format!(
        "{}:{}",
        operation_name(selector.operation),
        selector.model_alias.0
    )
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
    let name = operation_name(operation);
    if let Some(other) = routes
        .iter()
        .find(|route| route.selector.model_alias.0 == alias)
    {
        let other = other.selector.operation;
        return Err(format!(
            "no {name} route \"{alias}\"; \"{alias}\" is a {} route, use {} {alias}",
            operation_name(other),
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
    args: &Args,
    routes: &[RouteChoice],
    flag: fn(Operation) -> &'static str,
) -> Result<Vec<RouteSelector>, String> {
    let mut scopes = Vec::new();
    for operation in [
        Operation::Chat,
        Operation::Transcription,
        Operation::Embeddings,
        Operation::Speech,
    ] {
        for alias in args.all(flag(operation)) {
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

// ---- picker ----

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn exposure_note(exposure: Exposure) -> &'static str {
    match exposure {
        Exposure::Public => "public",
        Exposure::Private => "private only",
        Exposure::Never => "private only, never public",
    }
}

/// Numbered route menu on `output`, comma-separated numbers or aliases from `input`.
/// `current` scopes are pre-marked; the selection replaces them.
pub fn pick(
    routes: &[RouteChoice],
    current: &[RouteSelector],
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<Vec<RouteSelector>, String> {
    if routes.is_empty() {
        return Err("the config has no routes to grant".into());
    }
    let io_error = |_| "could not write the route menu".to_owned();
    let width = routes
        .iter()
        .map(|route| route.selector.model_alias.0.len())
        .max()
        .unwrap_or(0);
    writeln!(output, "Routes (* = current scope):").map_err(io_error)?;
    for (index, route) in routes.iter().enumerate() {
        let mark = if current.contains(&route.selector) {
            '*'
        } else {
            ' '
        };
        writeln!(
            output,
            "{:>3} {mark} {:<width$}  {:<13}  {}",
            index + 1,
            route.selector.model_alias.0,
            operation_name(route.selector.operation),
            exposure_note(route.exposure),
        )
        .map_err(io_error)?;
    }
    for _ in 0..PICKER_ATTEMPTS {
        write!(
            output,
            "Select routes (comma-separated numbers or aliases): "
        )
        .map_err(io_error)?;
        output.flush().map_err(io_error)?;
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .map_err(|_| "could not read the selection".to_owned())?
            == 0
        {
            return Err("no selection".into());
        }
        match parse_selection(routes, &line) {
            Ok(selected) => return Ok(selected),
            Err(message) => writeln!(output, "{message}").map_err(io_error)?,
        }
    }
    Err(format!(
        "no valid selection after {PICKER_ATTEMPTS} attempts"
    ))
}

fn parse_selection(routes: &[RouteChoice], line: &str) -> Result<Vec<RouteSelector>, String> {
    let mut selected: Vec<RouteSelector> = Vec::new();
    for token in line
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        let choice = if let Ok(number) = token.parse::<usize>() {
            routes
                .get(number.wrapping_sub(1))
                .ok_or_else(|| format!("no route number {number}"))?
        } else {
            let matches: Vec<_> = routes
                .iter()
                .filter(|route| route.selector.model_alias.0 == token)
                .collect();
            match matches.as_slice() {
                [only] => *only,
                [] => {
                    return Err(unknown_alias_message(
                        &format!("no route \"{token}\""),
                        token,
                        routes
                            .iter()
                            .map(|route| route.selector.model_alias.0.as_str()),
                    ));
                }
                _ => {
                    return Err(format!(
                        "\"{token}\" has several operations; use its number"
                    ));
                }
            }
        };
        if !selected.contains(&choice.selector) {
            selected.push(choice.selector.clone());
        }
    }
    if selected.is_empty() {
        return Err("select at least one route".into());
    }
    Ok(selected)
}

fn run_picker(
    routes: &[RouteChoice],
    current: &[RouteSelector],
    hint: &str,
) -> Result<Vec<RouteSelector>, String> {
    if !interactive() {
        return Err(format!("{hint} (see `kanata routes --config <path>`)"));
    }
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = std::io::stderr();
    pick(routes, current, &mut input, &mut output)
}

// ---- secrets ----

fn generate_key() -> Result<(String, [u8; 32]), String> {
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

// ---- commands ----

fn new(
    arguments: &[String],
    catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &[
            "--config",
            "--keys",
            "--id",
            "--chat",
            "--transcription",
            "--embeddings",
            "--speech",
            "--expires",
            "--max-in-flight",
            "--rate-limit",
            "--daily-requests",
            "--daily-tokens",
            "--reservation-tokens",
            "--key-out",
        ],
        &["--owner"],
        0,
    )?;
    let id = parse_id(args.one("--id")?.ok_or_else(|| KEY_USAGE.to_owned())?)?;
    let days = parse_expires(
        args.one("--expires")?
            .ok_or("--expires <1|3|7|13|30|60|unlimited> is required")?,
    )?;
    let max_in_flight = args
        .one("--max-in-flight")?
        .map(|value| parse_positive("--max-in-flight", value))
        .transpose()?;
    let rate_limit = args
        .one("--rate-limit")?
        .map(parse_rate_limit)
        .transpose()?;
    let key_out = args.one("--key-out")?.map(PathBuf::from);
    let owner = args.has("--owner");
    let (config, keys_path) = mutating_location(&args, "new")?;
    let routes = catalog(&config);
    let mut scopes = scope_flags(&args, &routes, new_flag)?;
    if scopes.is_empty() {
        scopes = run_picker(
            &routes,
            &[],
            "pass --chat <alias>, --transcription <alias> --embeddings <alias> or --speech <alias>",
        )?;
    }

    let (key, digest) = generate_key()?;
    let pending = key_out
        .as_deref()
        .map(|path| SecretFile::prepare(path, &key, false))
        .transpose()?;
    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let now = time::now();
    let expires_at = expires_at(now, days);
    let mut keys = locked.read()?;
    if keys.records().iter().any(|record| record.id() == id) {
        return Err(format!(
            "key id {id:?} already exists (ids are never reused, even after rm)"
        ));
    }
    if owner && keys.active().any(StoredKey::is_owner) {
        return Err(
            "an owner key already exists; use `kanata key rotate --owner` to replace it".to_owned(),
        );
    }
    let mut record = StoredKey::new(
        id.clone(),
        digest,
        owner,
        scopes,
        max_in_flight,
        rate_limit,
        now,
        expires_at,
    );
    record.set_daily_quota(quota_flags(&args, None, &config)?);
    keys.push(record);
    let event = AuditEvent {
        action: "new",
        key_id: id.clone(),
        owner,
        changes: None,
    };
    commit_secret(&locked, &keys, &config, event, &key, pending)?;
    drop(locked);
    long_lived_warning(&id, days);
    Ok(reveal(&id, &key, expires_at, key_out.as_deref(), "created"))
}

fn rotate(arguments: &[String]) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &["--config", "--keys", "--expires", "--key-out"],
        &["--owner"],
        1,
    )?;
    let target = match (args.positional.first(), args.has("--owner")) {
        (Some(id), false) => Some(parse_id(id)?),
        (None, true) => None,
        _ => return Err(KEY_USAGE.into()),
    };
    let days = parse_expires(
        args.one("--expires")?
            .ok_or("--expires <1|3|7|13|30|60|unlimited> is required")?,
    )?;
    let key_out = args.one("--key-out")?.map(PathBuf::from);
    let (config, keys_path) = mutating_location(&args, "rotate")?;

    let (key, digest) = generate_key()?;
    let pending = key_out
        .as_deref()
        .map(|path| SecretFile::prepare(path, &key, true))
        .transpose()?;
    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let now = time::now();
    let expires_at = expires_at(now, days);
    let mut keys = locked.read()?;
    let record = match &target {
        Some(id) => keys
            .records_mut()
            .iter_mut()
            .find(|record| record.id() == id)
            .ok_or_else(|| format!("no key {id:?}"))?,
        None => keys
            .records_mut()
            .iter_mut()
            .find(|record| record.is_owner() && !record.is_revoked())
            .ok_or("no active owner key; create one with `kanata key new --owner`")?,
    };
    if record.is_revoked() {
        return Err(format!(
            "key {:?} is revoked; create a new key instead",
            record.id()
        ));
    }
    record.rotate(digest, now, expires_at);
    let id = record.id().to_owned();
    let owner = record.is_owner();
    let event = AuditEvent {
        action: "rotate",
        key_id: id.clone(),
        owner,
        changes: None,
    };
    commit_secret(&locked, &keys, &config, event, &key, pending)?;
    drop(locked);
    long_lived_warning(&id, days);
    Ok(reveal(&id, &key, expires_at, key_out.as_deref(), "rotated"))
}

fn rm(arguments: &[String]) -> Result<String, String> {
    let args = Args::parse(arguments, &["--config", "--keys"], &["--force"], 1)?;
    let id = parse_id(
        args.positional
            .first()
            .ok_or_else(|| KEY_USAGE.to_owned())?,
    )?;
    let (config, keys_path) = mutating_location(&args, "rm")?;
    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let mut keys = locked.read()?;
    let record = keys
        .records_mut()
        .iter_mut()
        .find(|record| record.id() == id)
        .ok_or_else(|| format!("no key {id:?}"))?;
    if record.is_revoked() {
        return Ok(format!("key {id} is already revoked"));
    }
    if record.is_owner() && !args.has("--force") {
        return Err(format!(
            "key {id:?} is the owner key; pass --force to revoke it"
        ));
    }
    record.revoke(time::now());
    let owner = record.is_owner();
    write_keys(&locked, &keys, &config)?;
    locked.audit(&[AuditEvent {
        action: "rm",
        key_id: id.clone(),
        owner,
        changes: None,
    }])?;
    eprintln!("{APPLY_NOTE}");
    Ok(format!("revoked key {id}"))
}

fn edit(
    arguments: &[String],
    catalog: fn(&ValidatedConfig) -> Vec<RouteChoice>,
) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &[
            "--config",
            "--keys",
            "--add-chat",
            "--remove-chat",
            "--add-transcription",
            "--remove-transcription",
            "--add-embeddings",
            "--remove-embeddings",
            "--add-speech",
            "--remove-speech",
            "--expires",
            "--max-in-flight",
            "--rate-limit",
            "--daily-requests",
            "--daily-tokens",
            "--reservation-tokens",
        ],
        &["--clear-limits", "--clear-quota", "--force"],
        1,
    )?;
    let id = parse_id(
        args.positional
            .first()
            .ok_or_else(|| KEY_USAGE.to_owned())?,
    )?;
    let days = args.one("--expires")?.map(parse_expires).transpose()?;
    let max_in_flight = args
        .one("--max-in-flight")?
        .map(|value| parse_positive("--max-in-flight", value))
        .transpose()?;
    let rate_limit = args
        .one("--rate-limit")?
        .map(parse_rate_limit)
        .transpose()?;
    let clear_limits = args.has("--clear-limits");
    if clear_limits && (max_in_flight.is_some() || rate_limit.is_some()) {
        return Err(
            "--clear-limits cannot be combined with --max-in-flight or --rate-limit".into(),
        );
    }
    let (config, keys_path) = mutating_location(&args, "edit")?;
    let routes = catalog(&config);
    let mut added = scope_flags(&args, &routes, add_flag)?;
    let mut removed = Vec::new();
    for (flag, operation) in [
        ("--remove-chat", Operation::Chat),
        ("--remove-transcription", Operation::Transcription),
        ("--remove-embeddings", Operation::Embeddings),
        ("--remove-speech", Operation::Speech),
    ] {
        for alias in args.all(flag) {
            let scope = selector(alias, operation);
            if removed.contains(&scope) || added.contains(&scope) {
                return Err(format!("{} is given twice", scope_label(&scope)));
            }
            removed.push(scope);
        }
    }
    let quota_given = args.has("--clear-quota")
        || ["--daily-requests", "--daily-tokens", "--reservation-tokens"]
            .iter()
            .any(|flag| !args.all(flag).is_empty());
    let limits_given = max_in_flight.is_some() || rate_limit.is_some() || clear_limits;
    if added.is_empty() && removed.is_empty() && days.is_none() && !limits_given && !quota_given {
        let current = match file::read(&keys_path).map_err(|error| error.to_string())? {
            Some(bytes) => file::parse_without_routes(&bytes).map_err(|error| error.to_string())?,
            None => KeysFile::default(),
        };
        let record = current
            .records()
            .iter()
            .find(|record| record.id() == id)
            .ok_or_else(|| format!("no key {id:?}"))?;
        let selection = run_picker(
            &routes,
            record.permissions(),
            "nothing to change; pass --add-chat/--remove-chat/--add-transcription/--remove-transcription/--add-embeddings/--remove-embeddings/--add-speech/--remove-speech, --expires or limit flags",
        )?;
        added = selection
            .iter()
            .filter(|scope| !record.permissions().contains(scope))
            .cloned()
            .collect();
        removed = record
            .permissions()
            .iter()
            .filter(|scope| !selection.contains(scope))
            .cloned()
            .collect();
        if added.is_empty() && removed.is_empty() {
            return Ok(format!("key {id} unchanged"));
        }
    }

    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let mut keys = locked.read()?;
    let record = keys
        .records_mut()
        .iter_mut()
        .find(|record| record.id() == id)
        .ok_or_else(|| format!("no key {id:?}"))?;
    if record.is_revoked() {
        return Err(format!("key {id:?} is revoked and cannot be edited"));
    }
    let before = record.permissions().to_vec();
    for scope in &added {
        if before.contains(scope) {
            return Err(format!("key {id:?} already has {}", scope_label(scope)));
        }
    }
    for scope in &removed {
        if !before.contains(scope) {
            return Err(format!("key {id:?} has no scope {}", scope_label(scope)));
        }
    }
    let after: Vec<RouteSelector> = before
        .iter()
        .filter(|scope| !removed.contains(scope))
        .chain(added.iter())
        .cloned()
        .collect();
    if after.is_empty() {
        return Err(format!(
            "key {id:?} must keep at least one scope; use `kanata key rm {id}` to remove its access"
        ));
    }
    let exposure = |scope: &RouteSelector| {
        routes
            .iter()
            .find(|route| route.selector == *scope)
            .map(|route| route.exposure)
    };
    let publicly_usable = !record.is_owner()
        && before
            .iter()
            .all(|scope| exposure(scope) != Some(Exposure::Never))
        && before
            .iter()
            .any(|scope| exposure(scope) == Some(Exposure::Public));
    let mut notes = Vec::new();
    for scope in &added {
        match exposure(scope) {
            Some(Exposure::Never) if publicly_usable && !args.has("--force") => {
                return Err(format!(
                    "adding {} removes key {id:?} from the public listener entirely (that route is never public); pass --force to proceed",
                    scope_label(scope)
                ));
            }
            Some(Exposure::Private) if config.listeners().public().is_some() => {
                notes.push(format!(
                    "note: {} is private-only; it works on the private listener only",
                    scope_label(scope)
                ))
            }
            _ => {}
        }
    }

    let quota = quota_flags(&args, record.daily_quota(), &config)?;
    record.set_daily_quota(quota);
    record.set_permissions(after.clone());
    let mut changes = serde_json::Map::new();
    let scope_json = |scopes: &[RouteSelector]| -> Value {
        scopes
            .iter()
            .map(|scope| {
                json!({
                    "model_alias": scope.model_alias.0,
                    "operation": operation_name(scope.operation),
                })
            })
            .collect()
    };
    if quota_given {
        changes.insert("daily_quota".into(), json!(quota));
    }
    changes.insert("added".into(), scope_json(&added));
    changes.insert("removed".into(), scope_json(&removed));
    let mut lines = vec![
        format!("updated key {id}"),
        format!("scopes: {} -> {}", scope_list(&before), scope_list(&after)),
    ];
    if quota_given {
        lines.push(format!("daily quota: {}", format_daily_quota(quota)));
    }
    if let Some(days) = days {
        let expires_at = expires_at(time::now(), days);
        record.set_expires_at(expires_at);
        changes.insert("expires_at".into(), json!(expires_at.map(time::format)));
        lines.push(format!("expires: {}", format_optional_time(expires_at)));
    }
    if limits_given {
        let max_in_flight = max_in_flight.or(if clear_limits {
            None
        } else {
            record.max_in_flight()
        });
        let rate_limit = rate_limit.or(if clear_limits {
            None
        } else {
            record.rate_limit()
        });
        record.set_limits(max_in_flight, rate_limit);
        changes.insert("max_in_flight".into(), json!(max_in_flight));
        changes.insert("rate_limit".into(), rate_limit_json(rate_limit));
        lines.push(format!(
            "limits: max_in_flight {}, rate_limit {}",
            max_in_flight.map_or("-".into(), |value| value.to_string()),
            format_rate_limit(rate_limit)
        ));
    }
    let owner = record.is_owner();
    write_keys(&locked, &keys, &config)?;
    locked.audit(&[AuditEvent {
        action: "edit",
        key_id: id.clone(),
        owner,
        changes: Some(Value::Object(changes)),
    }])?;
    drop(locked);
    if let Some(days) = days {
        long_lived_warning(&id, days);
    }
    for note in notes {
        eprintln!("{note}");
    }
    eprintln!("{APPLY_NOTE}");
    Ok(lines.join("\n"))
}

fn migrate(arguments: &[String]) -> Result<String, String> {
    let args = Args::parse(arguments, &["--config", "--keys"], &[], 0)?;
    let config_path = args
        .one("--config")?
        .ok_or("kanata key migrate requires --config <path>")?;
    let config = load_config(config_path)?;
    if !matches!(config.key_source(), KeySource::Inline) {
        return Err(format!(
            "{config_path} already uses a [keys] file; nothing to migrate"
        ));
    }
    let config_dir = std::path::absolute(config_path)
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .ok_or_else(|| format!("cannot resolve --config {config_path}"))?;
    let keys_path = keys_override(&args)?.unwrap_or_else(|| config_dir.join("keys/keys.toml"));
    let refused: Vec<&str> = config
        .application_keys()
        .iter()
        .filter(|key| key.secret_ref().sha256_digest().is_none())
        .map(|key| key.id())
        .collect();
    if !refused.is_empty() {
        return Err(format!(
            "keys using env: or file: references cannot be migrated: {}; re-issue them with `kanata key new`",
            refused.join(", ")
        ));
    }

    let locked = store::lock(&keys_path, store::LOCK_TIMEOUT)?;
    let mut keys = locked.read()?;
    if !keys.records().is_empty() {
        return Err(format!(
            "{} already has keys; refusing to migrate into it",
            keys_path.display()
        ));
    }
    let now = time::now();
    let mut events = Vec::new();
    for key in config.application_keys() {
        let digest = *key.secret_ref().sha256_digest().expect("checked above");
        keys.push(StoredKey::new(
            key.id().to_owned(),
            digest,
            key.is_owner(),
            key.permissions().to_vec(),
            key.max_in_flight(),
            key.rate_limit(),
            now,
            None,
        ));
        events.push(AuditEvent {
            action: "migrate",
            key_id: key.id().to_owned(),
            owner: key.is_owner(),
            changes: None,
        });
    }
    write_keys(&locked, &keys, &config)?;
    locked.audit(&events)?;
    drop(locked);
    eprintln!(
        "warning: migrated keys never expire; set an expiry with `kanata key edit <id> --expires <days>`"
    );
    let file_value = keys_path
        .strip_prefix(&config_dir)
        .unwrap_or(&keys_path)
        .display()
        .to_string();
    Ok(format!(
        "migrated {} keys to {}\nadd this to {config_path}, delete its [[application_keys]] blocks, then run `kanata check --config {config_path}`:\n\n[keys]\nfile = {}\nusage_dir = \"state\"",
        events.len(),
        keys_path.display(),
        toml_string(&file_value)
    ))
}

fn quota_flags(
    args: &Args,
    existing: Option<crate::keys::quota::DailyQuota>,
    config: &ValidatedConfig,
) -> Result<Option<crate::keys::quota::DailyQuota>, String> {
    let number = |flag| {
        args.one(flag)?
            .map(|value| parse_positive(flag, value))
            .transpose()
    };
    let requests = number("--daily-requests")?;
    let tokens = number("--daily-tokens")?;
    let reservation_tokens = number("--reservation-tokens")?;
    if args.has("--clear-quota") {
        if requests.is_some() || tokens.is_some() || reservation_tokens.is_some() {
            return Err("--clear-quota cannot be combined with quota flags".into());
        }
        return Ok(None);
    }
    if requests.is_none() && tokens.is_none() && reservation_tokens.is_none() {
        return Ok(existing);
    }
    if !matches!(
        config.key_source(),
        KeySource::File {
            usage_dir: Some(_),
            ..
        }
    ) {
        return Err("daily quotas require [keys] usage_dir".into());
    }
    let quota = crate::keys::quota::DailyQuota {
        requests: requests.or(existing.and_then(|quota| quota.requests)),
        tokens: tokens.or(existing.and_then(|quota| quota.tokens)),
        reservation_tokens: reservation_tokens
            .or(existing.and_then(|quota| quota.reservation_tokens)),
    };
    quota
        .validate()
        .map_err(|class| format!("invalid daily quota: {class}"))?;
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
