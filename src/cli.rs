//! Startup arguments are data, never commands. Native paths stay native; expressions use the
//! same bounded parser as the expression field, without shell or environment expansion.
use crate::filter::Filter;
use crate::index::SizeMetric;
use std::ffi::OsString;
use std::path::PathBuf;

pub const USAGE: &str = "Usage: hogmap.exe [--scan <path> | --open <snapshot>] [--filter <expression>] [--metric logical|allocated] [--theme dark|light]\n       hogmap.exe --help\nPaths on the command line are visible to other processes. No shell expansion is performed.";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Startup {
    pub scan: Option<PathBuf>,
    pub open: Option<PathBuf>,
    pub filter: Option<String>,
    pub metric: Option<SizeMetric>,
    pub theme: Option<bool>, // true is light; independent of the desktop renderer.
}
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Start(Startup),
    Exit { code: u8, message: String },
}

fn error(message: &str) -> Parsed {
    Parsed::Exit {
        code: 2,
        message: format!("{message}\n\n{USAGE}"),
    }
}

/// Pure parser; filesystem eligibility is checked separately before creating a window and again
/// by the existing scan preparation worker, since paths can change after validation.
pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Parsed {
    let mut arguments = arguments.into_iter();
    let mut startup = Startup::default();
    let mut seen = std::collections::HashSet::new();
    while let Some(flag) = arguments.next() {
        let Some(flag) = flag.to_str() else {
            return error("Invalid non-Unicode flag.");
        };
        if flag == "--help" {
            return Parsed::Exit {
                code: 0,
                message: USAGE.into(),
            };
        }
        if !matches!(
            flag,
            "--scan" | "--open" | "--filter" | "--metric" | "--theme"
        ) {
            return error("Unknown argument. Use --help for supported flags.");
        }
        if !seen.insert(flag.to_owned()) {
            return error("A flag may only be supplied once.");
        }
        let Some(value) = arguments.next() else {
            return error(&format!("Missing value for {flag}."));
        };
        if flag != "--filter"
            && (value.is_empty() || value.to_str().is_some_and(|s| s.starts_with("--")))
        {
            return error(&format!("Missing value for {flag}."));
        }
        match flag {
            "--scan" => startup.scan = Some(value.into()),
            "--open" => startup.open = Some(value.into()),
            "--filter" => {
                let Some(expression) = value.to_str() else {
                    return error("Filter expression must be Unicode.");
                };
                if Filter::parse(expression).is_err() {
                    return error("Invalid filter expression. Use the expression-field grammar.");
                }
                startup.filter = Some(expression.into());
            }
            "--metric" => {
                startup.metric = Some(match value.to_str() {
                    Some("logical") => SizeMetric::Logical,
                    Some("allocated") => SizeMetric::Allocated,
                    _ => return error("Invalid metric. Expected logical or allocated."),
                })
            }
            "--theme" => {
                startup.theme = Some(match value.to_str() {
                    Some("dark") => false,
                    Some("light") => true,
                    _ => return error("Invalid theme. Expected dark or light."),
                })
            }
            _ => unreachable!(),
        }
    }
    if startup.scan.is_some() && startup.open.is_some() {
        return error("--scan and --open cannot be used together.");
    }
    Parsed::Start(startup)
}

pub fn validate_scan_path(startup: &Startup) -> Result<(), String> {
    if startup.scan.as_ref().is_some_and(|path| !path.is_dir()) {
        return Err(format!(
            "Scan path must exist and be a directory.\n\n{USAGE}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_flags_are_parsed_without_opening_a_window_or_executing_input() {
        let parse = |args: &[&str]| super::parse(args.iter().map(OsString::from));
        assert_eq!(parse(&[]), Parsed::Start(Startup::default()));
        assert_eq!(
            parse(&[
                "--scan",
                "fixture & not-a-command",
                "--filter",
                "*.jpg;s>1kb",
                "--metric",
                "allocated",
                "--theme",
                "light"
            ]),
            Parsed::Start(Startup {
                scan: Some("fixture & not-a-command".into()),
                filter: Some("*.jpg;s>1kb".into()),
                metric: Some(SizeMetric::Allocated),
                theme: Some(true),
                ..Startup::default()
            })
        );
        assert_eq!(
            parse(&[
                "--open",
                "fixture.hogmap",
                "--metric",
                "logical",
                "--theme",
                "dark"
            ]),
            Parsed::Start(Startup {
                open: Some("fixture.hogmap".into()),
                metric: Some(SizeMetric::Logical),
                theme: Some(false),
                ..Startup::default()
            })
        );
        for (args, message) in [
            (
                vec!["--exec", "anything"],
                "Unknown argument. Use --help for supported flags.",
            ),
            (vec!["--scan"], "Missing value for --scan."),
            (vec!["--open", "--filter"], "Missing value for --open."),
            (
                vec!["--scan", "fixture", "--open", "fixture.hogmap"],
                "--scan and --open cannot be used together.",
            ),
            (
                vec!["--filter", "s>bad"],
                "Invalid filter expression. Use the expression-field grammar.",
            ),
            (
                vec!["--metric", "physical"],
                "Invalid metric. Expected logical or allocated.",
            ),
            (
                vec!["--theme", "system"],
                "Invalid theme. Expected dark or light.",
            ),
            (
                vec!["--theme", "dark", "--theme", "light"],
                "A flag may only be supplied once.",
            ),
        ] {
            assert_eq!(
                parse(&args),
                Parsed::Exit {
                    code: 2,
                    message: format!("{message}\n\n{USAGE}")
                }
            );
        }
        assert_eq!(
            parse(&["--help"]),
            Parsed::Exit {
                code: 0,
                message: USAGE.into()
            }
        );
        assert!(
            validate_scan_path(&Startup {
                scan: Some("\0".into()),
                ..Startup::default()
            })
            .unwrap_err()
            .starts_with("Scan path must exist and be a directory.")
        );
    }
}
