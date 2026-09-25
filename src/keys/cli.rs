//! `kanata key` commands. Synchronous, host-only; writes go through [`store`].

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

pub const KEY_USAGE: &str = "usage: kanata key new --config <path> --id <id> [--chat <alias>]... [--transcription <alias>]... --expires <1|3|7|13|30|60|unlimited> [--owner] [--max-in-flight N] [--rate-limit N/MS] [--key-out <path>] [--keys <path>]
       kanata key list (--config <path>|--keys <path>) [--usage-dir <path>] [--all] [--json]
       kanata key show <id> (--config <path>|--keys <path>) [--usage-dir <path>] [--json]
       kanata key edit <id> --config <path> [--add-chat A]... [--remove-chat A]... [--add-transcription A]... [--remove-transcription A]... [--expires <choice>] [--max-in-flight N] [--rate-limit N/MS] [--clear-limits] [--force] [--keys <path>]
       kanata key rm <id> --config <path> [--force] [--keys <path>]
       kanata key rotate (<id>|--owner) --config <path> --expires <choice> [--key-out <path>] [--keys <path>]
       kanata key migrate --config <path> [--keys <path>]";

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
        "edit" => edit(rest, catalog),
        "rm" => rm(rest),
        "rotate" => rotate(rest),
        "migrate" => migrate(rest),
        _ => Err(KEY_USAGE.into()),
    }
}

// ---- arguments ----

#[derive(Default)]
struct Args {
    values: Vec<(String, String)>,
    switches: BTreeSet<String>,
    positional: Vec<String>,
}

impl Args {
    fn parse(
        arguments: &[String],
        value_flags: &[&str],
        switch_flags: &[&str],
        max_positional: usize,
    ) -> Result<Self, String> {
        let mut args = Self::default();
        let mut iter = arguments.iter();
        while let Some(argument) = iter.next() {
            if value_flags.contains(&argument.as_str()) {
                let value = iter.next().ok_or_else(|| KEY_USAGE.to_owned())?;
                args.values.push((argument.clone(), value.clone()));
            } else if switch_flags.contains(&argument.as_str()) {
                if !args.switches.insert(argument.clone()) {
                    return Err(KEY_USAGE.into());
                }
            } else if !argument.starts_with("--") && args.positional.len() < max_positional {
                args.positional.push(argument.clone());
            } else {
                return Err(KEY_USAGE.into());
            }
        }
        Ok(args)
    }

    fn all(&self, flag: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(name, _)| name == flag)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    fn one(&self, flag: &str) -> Result<Option<&str>, String> {
        match self.all(flag).as_slice() {
            [] => Ok(None),
            [value] => Ok(Some(value)),
            _ => Err(format!("{flag} may be given only once")),
        }
    }

    fn has(&self, flag: &str) -> bool {
        self.switches.contains(flag)
    }
}

/// `Some(days)`, or `None` for `unlimited`.
fn parse_expires(value: &str) -> Result<Option<u64>, String> {
    match value {
        "1" | "3" | "7" | "13" | "30" | "60" => Ok(Some(value.parse().expect("digits"))),
        "unlimited" => Ok(None),
        _ => Err("--expires must be one of 1, 3, 7, 13, 30, 60 or unlimited".into()),
    }
}

fn expires_at(now: u64, days: Option<u64>) -> Option<u64> {
    days.map(|days| now + days * SECONDS_PER_DAY)
}

fn long_lived_warning(id: &str, days: Option<u64>) {
    match days {
        None => eprintln!("warning: long-lived key {id:?} never expires"),
        Some(days) if days >= 60 => {
            eprintln!("warning: long-lived key {id:?} expires in {days} days")
        }
        Some(_) => {}
    }
}

fn parse_positive(flag: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{flag} must be a positive integer"))
}

fn parse_rate_limit(value: &str) -> Result<KeyRateLimit, String> {
    let error = || "--rate-limit must be N/MS, e.g. 60/60000".to_owned();
    let (requests, per_ms) = value.split_once('/').ok_or_else(error)?;
    Ok(KeyRateLimit {
        requests: parse_positive("--rate-limit", requests).map_err(|_| error())?,
        per_ms: parse_positive("--rate-limit", per_ms).map_err(|_| error())?,
    })
}

fn parse_id(value: &str) -> Result<String, String> {
    if valid_identifier(value) {
        Ok(value.to_owned())
    } else {
        Err("key id must use only ASCII letters, digits, '-', '_' or '.'".into())
    }
}

// ---- locations ----

/// The keys file is read separately, so route drift in it never blocks the CLI.
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
    match operation {
        Operation::Chat => "chat",
        Operation::Transcription => "transcription",
    }
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
    chat_flag: &str,
    transcription_flag: &str,
    flag: fn(Operation) -> &'static str,
) -> Result<Vec<RouteSelector>, String> {
    let mut scopes = Vec::new();
    for (name, operation) in [
        (chat_flag, Operation::Chat),
        (transcription_flag, Operation::Transcription),
    ] {
        for alias in args.all(name) {
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
    }
}

fn add_flag(operation: Operation) -> &'static str {
    match operation {
        Operation::Chat => "--add-chat",
        Operation::Transcription => "--add-transcription",
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
            "--expires",
            "--max-in-flight",
            "--rate-limit",
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
    let mut scopes = scope_flags(&args, &routes, "--chat", "--transcription", new_flag)?;
    if scopes.is_empty() {
        scopes = run_picker(
            &routes,
            &[],
            "pass --chat <alias> or --transcription <alias>",
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
    keys.push(StoredKey::new(
        id.clone(),
        digest,
        owner,
        scopes,
        max_in_flight,
        rate_limit,
        now,
        expires_at,
    ));
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
            "--expires",
            "--max-in-flight",
            "--rate-limit",
        ],
        &["--clear-limits", "--force"],
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
    let mut added = scope_flags(
        &args,
        &routes,
        "--add-chat",
        "--add-transcription",
        add_flag,
    )?;
    let mut removed = Vec::new();
    for (flag, operation) in [
        ("--remove-chat", Operation::Chat),
        ("--remove-transcription", Operation::Transcription),
    ] {
        for alias in args.all(flag) {
            let scope = selector(alias, operation);
            if removed.contains(&scope) || added.contains(&scope) {
                return Err(format!("{} is given twice", scope_label(&scope)));
            }
            removed.push(scope);
        }
    }
    let limits_given = max_in_flight.is_some() || rate_limit.is_some() || clear_limits;
    if added.is_empty() && removed.is_empty() && days.is_none() && !limits_given {
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
            "nothing to change; pass --add-chat/--remove-chat/--add-transcription/--remove-transcription, --expires or limit flags",
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
    changes.insert("added".into(), scope_json(&added));
    changes.insert("removed".into(), scope_json(&removed));
    let mut lines = vec![
        format!("updated key {id}"),
        format!("scopes: {} -> {}", scope_list(&before), scope_list(&after)),
    ];
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

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serializes")
}

// ---- list / show ----

fn format_date(at: u64) -> String {
    time::format(at)[..10].to_owned()
}

fn format_optional_time(at: Option<u64>) -> String {
    at.map_or_else(|| "never".into(), time::format)
}

fn format_rate_limit(limit: Option<KeyRateLimit>) -> String {
    limit.map_or_else(
        || "-".into(),
        |limit| format!("{}/{}ms", limit.requests, limit.per_ms),
    )
}

fn rate_limit_json(limit: Option<KeyRateLimit>) -> Value {
    limit.map_or(
        Value::Null,
        |limit| json!({"requests": limit.requests, "per_ms": limit.per_ms}),
    )
}

fn scope_list(scopes: &[RouteSelector]) -> String {
    scopes.iter().map(scope_label).collect::<Vec<_>>().join(",")
}

fn usage_for(usage_dir: Option<&Path>) -> Option<BTreeMap<String, KeyUsage>> {
    usage_dir.map(usage::read_merged)
}

fn key_json(
    record: &StoredKey,
    usage: Option<&BTreeMap<String, KeyUsage>>,
    now: u64,
) -> serde_json::Map<String, Value> {
    let used = usage.and_then(|usage| usage.get(record.id()));
    let Value::Object(object) = json!({
        "id": record.id(),
        "owner": record.is_owner(),
        "scopes": record.permissions().iter().map(|scope| json!({
            "model_alias": scope.model_alias.0,
            "operation": operation_name(scope.operation),
        })).collect::<Vec<_>>(),
        "created_at": time::format(record.created_at()),
        "expires_at": record.expires_at().map(time::format),
        "expired": record.is_expired(now),
        "rotated_at": record.rotated_at().map(time::format),
        "revoked_at": record.revoked_at().map(time::format),
        "last_used_at": used.filter(|used| used.last_used_at > 0).map(|used| time::format(used.last_used_at)),
        "requests": usage.map(|_| used.map_or(0, |used| used.requests)),
    }) else {
        unreachable!("object literal")
    };
    object
}

/// Scopes whose route is not in `config`.
fn missing_routes_json(record: &StoredKey, config: &ValidatedConfig) -> Value {
    record
        .permissions()
        .iter()
        .filter(|scope| !route_exists(config, scope))
        .map(|scope| {
            json!({
                "model_alias": scope.model_alias.0,
                "operation": operation_name(scope.operation),
            })
        })
        .collect()
}

fn list(arguments: &[String]) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &["--config", "--keys", "--usage-dir"],
        &["--all", "--json"],
        0,
    )?;
    let location = read_location(&args)?;
    let usage = usage_for(location.usage_dir.as_deref());
    let now = time::now();
    let records: Vec<&StoredKey> = location
        .keys
        .records()
        .iter()
        .filter(|record| args.has("--all") || !record.is_revoked())
        .collect();
    if args.has("--json") {
        let array: Vec<Value> = records
            .iter()
            .map(|record| {
                let mut object = key_json(record, usage.as_ref(), now);
                if let Some(config) = &location.config {
                    object.insert("missing_routes".into(), missing_routes_json(record, config));
                }
                Value::Object(object)
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&array).expect("json serializes"));
    }
    let mut rows = vec![
        [
            "ID",
            "OWNER",
            "SCOPES",
            "CREATED",
            "EXPIRES",
            "REVOKED",
            "LAST USED",
            "REQUESTS",
        ]
        .map(str::to_owned),
    ];
    for record in records {
        let used = usage.as_ref().and_then(|usage| usage.get(record.id()));
        let expires = match record.expires_at() {
            None => "never".into(),
            Some(at) if record.is_expired(now) => format!("{} expired", format_date(at)),
            Some(at) => format_date(at),
        };
        rows.push([
            record.id().to_owned(),
            if record.is_owner() { "yes" } else { "no" }.into(),
            record
                .permissions()
                .iter()
                .map(|scope| match &location.config {
                    Some(config) if !route_exists(config, scope) => {
                        format!("{}(no-route)", scope_label(scope))
                    }
                    _ => scope_label(scope),
                })
                .collect::<Vec<_>>()
                .join(","),
            format_date(record.created_at()),
            expires,
            record.revoked_at().map_or("-".into(), format_date),
            used.filter(|used| used.last_used_at > 0)
                .map_or("-".into(), |used| format_date(used.last_used_at)),
            match (&usage, used) {
                (None, _) => "-".into(),
                (Some(_), used) => used.map_or(0, |used| used.requests).to_string(),
            },
        ]);
    }
    Ok(render_table(&rows))
}

fn render_table<const N: usize>(rows: &[[String; N]]) -> String {
    let mut widths = [0; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .zip(widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn show(arguments: &[String]) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &["--config", "--keys", "--usage-dir"],
        &["--json"],
        1,
    )?;
    let id = parse_id(
        args.positional
            .first()
            .ok_or_else(|| KEY_USAGE.to_owned())?,
    )?;
    let location = read_location(&args)?;
    let record = location
        .keys
        .records()
        .iter()
        .find(|record| record.id() == id)
        .ok_or_else(|| format!("no key {id:?}"))?;
    let usage = usage_for(location.usage_dir.as_deref());
    let now = time::now();
    let missing = |scope: &RouteSelector| {
        location
            .config
            .as_ref()
            .is_some_and(|config| !route_exists(config, scope))
    };
    if args.has("--json") {
        let mut object = key_json(record, usage.as_ref(), now);
        object.insert("max_in_flight".into(), json!(record.max_in_flight()));
        object.insert("rate_limit".into(), rate_limit_json(record.rate_limit()));
        if let Some(config) = &location.config {
            object.insert("missing_routes".into(), missing_routes_json(record, config));
        }
        return Ok(serde_json::to_string_pretty(&object).expect("json serializes"));
    }
    let used = usage.as_ref().and_then(|usage| usage.get(record.id()));
    let scopes = record
        .permissions()
        .iter()
        .map(|scope| {
            if missing(scope) {
                format!("{} (route no longer in config)", scope_label(scope))
            } else {
                scope_label(scope)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut expires = format_optional_time(record.expires_at());
    if record.is_expired(now) {
        expires.push_str(" (expired)");
    }
    let rows = [
        ("id", record.id().to_owned()),
        ("owner", if record.is_owner() { "yes" } else { "no" }.into()),
        ("scopes", scopes),
        (
            "max_in_flight",
            record
                .max_in_flight()
                .map_or("-".into(), |value| value.to_string()),
        ),
        ("rate_limit", format_rate_limit(record.rate_limit())),
        ("created", time::format(record.created_at())),
        ("expires", expires),
        (
            "rotated",
            record.rotated_at().map_or("-".into(), time::format),
        ),
        (
            "revoked",
            record.revoked_at().map_or("-".into(), time::format),
        ),
        (
            "last used",
            used.filter(|used| used.last_used_at > 0)
                .map_or("-".into(), |used| time::format(used.last_used_at)),
        ),
        (
            "requests",
            match (&usage, used) {
                (None, _) => "-".into(),
                (Some(_), used) => used.map_or(0, |used| used.requests).to_string(),
            },
        ),
    ]
    .map(|(label, value)| [format!("{label}:"), value]);
    Ok(render_table(&rows))
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
