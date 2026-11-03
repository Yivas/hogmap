use std::ffi::OsString;

use hogmap::filter::FilterError;
use hogmap::index::{State, Summary};
use hogmap::scan::ScanDiagnostics;

pub const TITLE: &str = "HogMap";
pub const FILE: &str = "File";
pub const NEW_SCAN: &str = "New scan of synthetic sample";
pub const EXIT: &str = "Exit";
pub const VIEW: &str = "View";
pub const ZOOM_IN: &str = "Zoom into selection";
pub const ZOOM_OUT: &str = "Zoom out";
pub const HELP: &str = "Help";
pub const ABOUT: &str = "About HogMap";
pub const ABOUT_BODY: &str = "Synthetic sample only. No filesystem scanner or release yet.\nMap area: known logical bytes, charged once per file identity.\nZero-size and unknown-size items have no map area.\nNo volume free-space or physical attribution data is available.\nOnly window geometry is saved; names, filters and navigation are not saved.";
pub const BACK: &str = "Back";
pub const FORWARD: &str = "Forward";
pub const UP: &str = "Up";
pub const HOME: &str = "Home";
pub const CANCEL: &str = "Cancel scan";
pub const FILTER: &str = "Highlight";
pub const FILTER_HINT: &str = "Name or logical size: *.bin;>1mb";
pub const SAMPLE: &str = "Synthetic sample";
pub const WAITING: &str = "Waiting for synthetic metadata…";
pub const EMPTY: &str = "No known logical bytes to draw. Unknown sizes are not zero.";
pub const PARTIAL: &str = "Partial";

pub fn scan_state(state: super::logic::ScanState) -> &'static str {
    match state {
        super::logic::ScanState::Scanning => "Scanning",
        super::logic::ScanState::Finished => "Finished",
        super::logic::ScanState::Cancelled => "Cancelled",
        super::logic::ScanState::Failed => "Failed",
    }
}
pub const LAYOUT_ERROR: &str = "Cannot lay out this sample";
pub const START_ERROR: &str = "HogMap could not open its window";
pub const PRODUCER_ERROR: &str = "Could not start the synthetic producer";
pub const PRODUCER_PANIC: &str = "The previous synthetic producer stopped unexpectedly";
pub const ELLIPSIS: &str = "…";

pub fn state(state: State) -> &'static str {
    match state {
        State::Pending => "Pending",
        State::Complete => "Complete",
        State::Partial => PARTIAL,
        State::Inaccessible => "Inaccessible",
        State::Stale => "Stale",
    }
}

pub fn size(bytes: u64, unknown: bool) -> String {
    let suffix = if unknown { " + unknown" } else { "" };
    let (value, unit) = if bytes >= 1024 * 1024 * 1024 {
        (bytes as f64 / (1024_f64.powi(3)), "GiB")
    } else if bytes >= 1024 * 1024 {
        (bytes as f64 / (1024_f64.powi(2)), "MiB")
    } else if bytes >= 1024 {
        (bytes as f64 / 1024.0, "KiB")
    } else {
        return format!("{bytes} B{suffix}");
    };
    format!("{value:.1} {unit}{suffix}")
}

pub fn filter_error(error: FilterError) -> &'static str {
    match error {
        FilterError::TooLong => {
            "Filter exceeds 4096 bytes. Previous valid highlight is still active."
        }
        FilterError::TooManyConditions => {
            "Filter exceeds 64 conditions. Previous valid highlight is still active."
        }
        FilterError::EmptyCondition => {
            "Empty filter condition. Previous valid highlight is still active."
        }
        FilterError::UnsupportedSyntax => {
            "Unsupported filter syntax. Previous valid highlight is still active."
        }
        FilterError::InvalidSize => {
            "Use an integer size such as >100mb. Previous valid highlight is still active."
        }
    }
}

/// The dataset root, native path components and, when the map is opened inside an aggregate's
/// small items, the member count that the map draws.
pub fn location(root: &str, components: &[OsString], members: Option<usize>) -> String {
    let mut text = String::from(root);
    for component in components {
        text.push_str(" / ");
        text.push_str(&component.to_string_lossy());
    }
    if let Some(members) = members {
        text.push_str(" · ");
        text.push_str(&aggregate_name(members));
    }
    text
}

pub fn status(
    state: &str,
    location: &str,
    summary: Summary,
    diagnostics: &ScanDiagnostics,
) -> String {
    let protocol = diagnostics.stale_generation_events
        + diagnostics.unknown_parent_records
        + diagnostics.duplicate_key_records
        + diagnostics.invalid_records
        + diagnostics.unknown_directory_events
        + diagnostics.duplicate_directory_outcomes
        + diagnostics.events_after_terminal
        + u64::from(diagnostics.missing_terminal);
    let completeness = if summary.complete {
        "Complete"
    } else {
        "Incomplete"
    };
    format!(
        "{state} | {location} | {} | {} visible logical | {} unique map | {completeness}: {} unknown, {} inaccessible, {} partial | {protocol} protocol diagnostics",
        files(summary.files),
        size(summary.known_logical_bytes, false),
        size(summary.unique_logical_bytes, false),
        summary.unknown_size_files,
        diagnostics.directory_failures,
        diagnostics.partial_directories
    )
}

pub fn files(count: usize) -> String {
    let noun = if count == 1 { "file" } else { "files" };
    format!("{count} {noun}")
}

pub fn aggregate_name(count: usize) -> String {
    let noun = if count == 1 {
        "small item"
    } else {
        "small items"
    };
    format!("{count} {noun}")
}
pub fn aggregate_tooltip(count: usize, size: &str) -> String {
    format!("{} · {size}\nDouble-click to open", aggregate_name(count))
}

/// Keeps the parent's name and marks the map as a subset of its small items.
pub fn subset_name(name: &str, members: usize) -> String {
    format!("{name} · {}", aggregate_name(members))
}
pub fn header(name: &str, size: &str) -> String {
    format!("{name} - {size}")
}
pub fn label(name: &str, size: &str) -> String {
    format!("{name}\n{size}")
}
pub fn tooltip(name: &str, size: &str, state: &str) -> String {
    format!("{name}\nKnown unique logical: {size}\nState: {state}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_text_joins_root_path_and_resolved_members() {
        let path = [OsString::from("media"), OsString::from("2024")];
        assert_eq!(location("Sample", &path, None), "Sample / media / 2024");
        assert_eq!(
            location("Sample", &path, Some(1)),
            "Sample / media / 2024 · 1 small item"
        );
        assert_eq!(
            location("Sample", &path, Some(4)),
            "Sample / media / 2024 · 4 small items"
        );
    }
}
