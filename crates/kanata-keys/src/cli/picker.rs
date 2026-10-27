use super::*;
use std::io::{BufRead, IsTerminal as _, Write};

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
            route.selector.operation.as_str(),
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

pub(super) fn run_picker(
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
