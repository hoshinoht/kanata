use super::*;
use crate::config::Plane;
use crate::keys::quota::DailyLedger;

pub(super) fn run(arguments: &[String]) -> Result<String, String> {
    let args = Args::parse(
        arguments,
        &[
            "--config",
            "--usage-dir",
            "--plane",
            "--key-id",
            "--model",
            "--from",
            "--to",
            "--input-cost-per-million",
            "--output-cost-per-million",
            "--cost-unit",
        ],
        &["--json"],
        0,
    )?;
    let config = args.one("--config")?.map(load_config).transpose()?;
    let dir = args
        .one("--usage-dir")?
        .map(PathBuf::from)
        .or_else(|| match config.as_ref().map(ValidatedConfig::key_source) {
            Some(KeySource::File { usage_dir, .. }) => usage_dir.clone(),
            _ => None,
        })
        .ok_or("usage reports require --usage-dir or [keys] usage_dir")?;
    let planes = match args.one("--plane")? {
        Some(value) => vec![Plane::parse(value).ok_or("invalid --plane")?],
        None => vec![Plane::All, Plane::Private, Plane::Public],
    };
    let key = args.one("--key-id")?.map(parse_id).transpose()?;
    let model = args.one("--model")?;
    let date = |flag| -> Result<Option<u64>, String> {
        args.one(flag)?
            .map(|value| {
                if value.len() != 10 {
                    return Err(format!("{flag} requires YYYY-MM-DD"));
                }
                time::parse(&format!("{value}T00:00:00Z"))
                    .map(|seconds| seconds / SECONDS_PER_DAY)
                    .ok_or_else(|| format!("invalid {flag}"))
            })
            .transpose()
    };
    let from = date("--from")?;
    let to = date("--to")?;
    if from.zip(to).is_some_and(|(from, to)| from > to) {
        return Err("--from must not exceed --to".into());
    }
    let rate = |flag| -> Result<Option<f64>, String> {
        args.one(flag)?
            .map(|value| {
                value
                    .parse::<f64>()
                    .ok()
                    .filter(|rate| rate.is_finite() && *rate >= 0.0 && *rate <= 1_000_000_000.0)
                    .ok_or_else(|| format!("invalid {flag}"))
            })
            .transpose()
    };
    let input = rate("--input-cost-per-million")?;
    let output = rate("--output-cost-per-million")?;
    let unit = args.one("--cost-unit")?;
    let rates = match (input, output, unit) {
        (None, None, None) => None,
        (Some(input), Some(output), Some(unit)) if !unit.is_empty() && unit.len() <= 16 && unit.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') => Some((input, output, unit)),
        _ => return Err("cost estimates require both rates and --cost-unit (1–16 letters, digits or underscores)".into()),
    };
    let mut directories = vec![dir.clone()];
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|_| "usage directory cannot be listed")?;
                if entry
                    .file_type()
                    .map_err(|_| "usage directory cannot be inspected")?
                    .is_dir()
                {
                    directories.push(entry.path());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("usage directory cannot be listed".into()),
    }
    directories.sort();
    let mut rows = Vec::new();
    for directory in directories {
        for &plane in &planes {
            let source = directory
                .strip_prefix(&dir)
                .unwrap_or(&directory)
                .join(format!("daily-{}.json", plane.as_str()))
                .display()
                .to_string();
            let records = DailyLedger::new(&directory, plane).read().map_err(|_| {
                format!(
                    "daily {} usage is unreadable; report is incomplete",
                    plane.as_str()
                )
            })?;
            for row in records {
                if key.as_ref().is_some_and(|key| row.key_id != *key)
                    || model.is_some_and(|model| row.model != model)
                    || from.is_some_and(|from| row.day < from)
                    || to.is_some_and(|to| row.day > to)
                {
                    continue;
                }
                let cost = rates.map(|(input, output, _)| {
                    (row.tokens.input_tokens as f64 * input
                        + row.tokens.output_tokens as f64 * output)
                        / 1_000_000.0
                });
                rows.push(json!({
                    "plane": plane.as_str(),
                    "source": source,
                    "date": &time::format(row.day * SECONDS_PER_DAY)[..10],
                    "key_id": row.key_id,
                    "model": row.model,
                    "operation": row.operation,
                    "requests": row.requests,
                    "charged_tokens": row.charged_tokens,
                    "retained_reservation_tokens": row.retained_reservation_tokens,
                    "overrun_tokens": row.overrun_tokens,
                    "tokens": row.tokens,
                    "estimated_reported_cost": cost,
                    "cost_incomplete": rates.map(|_| row.tokens.missing > 0),
                }));
            }
        }
    }
    rows.sort_by_key(|row| {
        (
            row["date"].as_str().unwrap_or_default().to_owned(),
            row["plane"].as_str().unwrap_or_default().to_owned(),
            row["source"].as_str().unwrap_or_default().to_owned(),
            row["key_id"].as_str().unwrap_or_default().to_owned(),
            row["model"].as_str().unwrap_or_default().to_owned(),
            row["operation"].as_str().unwrap_or_default().to_owned(),
        )
    });
    if args.has("--json") {
        return serde_json::to_string_pretty(&json!({"retention_days": 90, "cost_unit": rates.map(|(_, _, unit)| unit), "cost_basis": "operator_rates_and_reported_tokens_only", "rows": rows})).map_err(|_| "usage report could not be rendered".into());
    }
    let mut lines = vec![
        "UTC DATE   PLANE (SOURCE)   KEY / MODEL / OPERATION   REQUESTS REPORTED MISSING CHARGED RETAINED"
            .into(),
    ];
    for row in rows {
        lines.push(format!(
            "{} {} ({}) {} / {} / {}   {} {} {} {} {}{}",
            row["date"].as_str().unwrap_or_default(),
            row["plane"].as_str().unwrap_or_default(),
            row["source"].as_str().unwrap_or_default(),
            row["key_id"].as_str().unwrap_or_default(),
            row["model"].as_str().unwrap_or_default(),
            row["operation"].as_str().unwrap_or_default(),
            row["requests"],
            row["tokens"]["reported"],
            row["tokens"]["missing"],
            row["charged_tokens"],
            row["retained_reservation_tokens"],
            rates.map_or_else(String::new, |(_, _, unit)| format!(
                " estimate={} {unit}{}",
                row["estimated_reported_cost"],
                if row["cost_incomplete"] == true {
                    " (incomplete)"
                } else {
                    ""
                }
            ))
        ));
    }
    lines.push("Reports cover admitted inference attempts, retain 90 UTC days, and keep process planes separate. Missing includes pending, cancelled and unreported attempts. Cost uses supplied rates; it is an estimate.".into());
    Ok(lines.join("\n"))
}
