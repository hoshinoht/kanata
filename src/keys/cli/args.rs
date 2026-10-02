use super::*;

#[derive(Default)]
pub(super) struct Args {
    pub(super) values: Vec<(String, String)>,
    pub(super) switches: BTreeSet<String>,
    pub(super) positional: Vec<String>,
}

impl Args {
    pub(super) fn parse(
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

    pub(super) fn all(&self, flag: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(name, _)| name == flag)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    pub(super) fn one(&self, flag: &str) -> Result<Option<&str>, String> {
        match self.all(flag).as_slice() {
            [] => Ok(None),
            [value] => Ok(Some(value)),
            _ => Err(format!("{flag} may be given only once")),
        }
    }

    pub(super) fn has(&self, flag: &str) -> bool {
        self.switches.contains(flag)
    }
}

/// `Some(days)`, or `None` for `unlimited`.
pub(super) fn parse_expires(value: &str) -> Result<Option<u64>, String> {
    match value {
        "1" | "3" | "7" | "13" | "30" | "60" => Ok(Some(value.parse().expect("digits"))),
        "unlimited" => Ok(None),
        _ => Err("--expires must be one of 1, 3, 7, 13, 30, 60 or unlimited".into()),
    }
}

pub(super) fn expires_at(now: u64, days: Option<u64>) -> Option<u64> {
    days.map(|days| now + days * SECONDS_PER_DAY)
}

pub(super) fn long_lived_warning(id: &str, days: Option<u64>) {
    match days {
        None => eprintln!("warning: long-lived key {id:?} never expires"),
        Some(days) if days >= 60 => {
            eprintln!("warning: long-lived key {id:?} expires in {days} days")
        }
        Some(_) => {}
    }
}

pub(super) fn parse_positive(flag: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{flag} must be a positive integer"))
}

pub(super) fn parse_rate_limit(value: &str) -> Result<KeyRateLimit, String> {
    let error = || "--rate-limit must be N/MS, e.g. 60/60000".to_owned();
    let (requests, per_ms) = value.split_once('/').ok_or_else(error)?;
    Ok(KeyRateLimit {
        requests: parse_positive("--rate-limit", requests).map_err(|_| error())?,
        per_ms: parse_positive("--rate-limit", per_ms).map_err(|_| error())?,
    })
}

pub(super) fn parse_id(value: &str) -> Result<String, String> {
    if valid_identifier(value) {
        Ok(value.to_owned())
    } else {
        Err("key id must use only ASCII letters, digits, '-', '_' or '.'".into())
    }
}

// ---- locations ----
