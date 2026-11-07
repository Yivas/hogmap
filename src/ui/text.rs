use std::ffi::OsString;

use hogmap::filter::FilterError;
use hogmap::index::{MetricSummary, SizeMetric, State};
use hogmap::scan::ScanDiagnostics;
use hogmap::scan_win::{self, ScanReport};
use hogmap::treemap::LayoutError;
use hogmap::volume::{CapacityIssue, CapacityMetadata, CapacitySource};
use std::time::SystemTime;

use super::logic::MetricOverflow;

pub const TITLE: &str = "HogMap";
pub const FILE: &str = "File";
pub const NEW_SCAN: &str = "New scan";
pub const SAMPLE_ACTION: &str = "Explore synthetic sample";
pub const CHOOSE_SPACE: &str = "Choose your Space";
pub const DRIVES_TAB: &str = "Drives or paths";
pub const SNAPSHOTS_TAB: &str = "Snapshots";
pub const NO_SNAPSHOTS: &str = "No saved snapshots yet";
pub const SNAPSHOTS_UNAVAILABLE: &str = "Saving and opening snapshots is not implemented yet.";
pub const DRIVES_UNAVAILABLE: &str = "No drives found. Enter an existing folder or drive root.";
pub const LISTING_DRIVES: &str = "Listing drives…";
pub const REFRESH_DRIVES: &str = "Refresh drives";
pub const DRIVE_THREAD_ERROR: &str =
    "The drive-listing thread could not start or stopped unexpectedly";
pub const PREPARING_SCAN: &str = "Checking path and volume…";
pub const STOPPING: &str = "Stopping…";
pub const NOT_DIRECTORY: &str = "This path is not a directory. Choose a folder or drive root.";
pub const PARENT_PATH: &str = "Use a path without parent (..) components.";
pub const PATH: &str = "Path";
pub const PATH_HINT: &str = "Enter a folder or drive root";
pub const START: &str = "Start";
pub const CLOSE: &str = "Close";
pub const DRIVE: &str = "Drive";
pub const RESCAN: &str = "Rescan";
pub const EXPORT: &str = "Save / Export";
pub const VIEW_MODE: &str = "View mode: nested / flat";
pub const GROUP_BY: &str = "Group by";
pub const GHOSTS: &str = "Show ghosts";
pub const OPTIONS: &str = "Options";
pub const READY: &str = "Ready";
pub const NO_SPACE: &str = "No space selected";
pub const CHOOSE_HINT: &str = "Choose a drive or folder to begin, or explore the synthetic sample.";
pub const NO_SCAN: &str = "No scan started";
pub const COMPLETE_METADATA: &str = "Complete metadata";
pub const INCOMPLETE_METADATA: &str = "Incomplete metadata";
pub const NEW_SCAN_TIP: &str = "New scan · Ctrl+N";
pub const BACK_TIP: &str = "Back · Alt+Left";
pub const FORWARD_TIP: &str = "Forward · Alt+Right";
pub const UP_TIP: &str = "Up · Backspace";
pub const HOME_TIP: &str = "Home · Alt+Home";
pub const RESCAN_TIP: &str = "Rescan selected scan root or sample · F5";
pub const STOP_TIP: &str = "Stop scan · Esc";
pub const EXPORT_TIP: &str = "Save / Export — not implemented yet";
pub const VIEW_MODE_TIP: &str = "Nested / flat view — not implemented yet; current map is nested";
pub const GROUP_BY_TIP: &str = "Group by — not implemented yet";
pub const HIGHLIGHT_TIP: &str = "Focus highlight filter · Ctrl+F";
pub const GHOSTS_TIP: &str = "Ghosts — not implemented yet";
pub const OPTIONS_TIP: &str = "Options — not implemented yet";
pub const EXIT: &str = "Exit";
pub const VIEW: &str = "View";
pub const ZOOM_IN: &str = "Zoom into selection";
pub const ZOOM_OUT: &str = "Zoom out";
pub const THEME_DARK: &str = "Dark";
pub const THEME_LIGHT: &str = "Light";
pub const METRIC_LOGICAL: &str = "Logical size";
pub const METRIC_ALLOCATED: &str = "Allocated size";
pub const HELP: &str = "Help";
pub const ABOUT: &str = "About HogMap";
pub const BACK: &str = "Back";
pub const FORWARD: &str = "Forward";
pub const UP: &str = "Up";
pub const HOME: &str = "Home";
pub const CANCEL: &str = "Stop scan";
pub const FILTER: &str = "Highlight";
pub const FILTER_HINT: &str = "Name or logical size: *.bin;>1mb";
pub const SAMPLE: &str = "Synthetic sample";
pub const WAITING: &str = "Waiting for metadata…";
pub const PARTIAL: &str = "Partial";

/// The metric inside a sentence. Allocated bytes are what Windows reports per file, so the map
/// calls them observed: they are not a physical attribution or recoverable space.
fn metric_bytes(metric: SizeMetric) -> &'static str {
    match metric {
        SizeMetric::Logical => "logical",
        SizeMetric::Allocated => "observed allocated",
    }
}

fn metric_title(metric: SizeMetric) -> &'static str {
    match metric {
        SizeMetric::Logical => "Logical",
        SizeMetric::Allocated => "Allocated",
    }
}

/// The line under the highlight field. Highlight size conditions stay logical in either view.
pub fn map_metric(metric: SizeMetric) -> &'static str {
    match metric {
        SizeMetric::Logical => "Map: known unique logical bytes",
        SizeMetric::Allocated => {
            "Map: known unique observed allocated bytes · Highlight uses logical size"
        }
    }
}

pub fn empty_map(metric: SizeMetric) -> String {
    format!(
        "No known {} bytes to draw. Unknown sizes are not zero.",
        metric_bytes(metric)
    )
}

pub fn about(metric: SizeMetric) -> String {
    format!(
        "Read-only Windows folder and drive scanning. No release yet.\nMap area now: known unique {} bytes. View switches between logical size (the default) and observed allocated size; each is charged once per available file identity.\nAllocated size is what Windows reports for each file, not physical attribution or recoverable space.\nHighlight size conditions always use logical size.\nZero-size and unknown-size items have no map area; unknown is not zero.\nVolume free/total are global Windows filesystem values, not limited by per-user quotas, and separate from map bytes.\nThe synthetic sample is generated in memory, not scanned.\nOnly window geometry and the chosen theme are saved; names, filters, navigation and the size view are not saved.",
        metric_bytes(metric)
    )
}

pub fn scan_state(state: super::logic::ScanState) -> &'static str {
    match state {
        super::logic::ScanState::Scanning => "Scanning",
        super::logic::ScanState::Finished => "Finished",
        super::logic::ScanState::Cancelled => "Cancelled",
        super::logic::ScanState::Failed => "Failed",
    }
}
pub const LAYOUT_ERROR: &str = "Cannot lay out this map";

/// `other_available` is true only when the other metric's own summary query succeeded.
pub fn layout_error(metric: SizeMetric, error: LayoutError, other_available: bool) -> String {
    let reason = match error {
        LayoutError::SizeOverflow => "the byte total overflowed",
        LayoutError::UnknownRoot => "the location is not in this scan",
        LayoutError::InvalidRect => "the map area is invalid",
        LayoutError::InvalidParams => "the layout parameters are invalid",
    };
    let mut text = format!("{LAYOUT_ERROR} by {} size: {reason}.", metric_bytes(metric));
    if other_available {
        text.push_str(&format!(
            " {} size is available in View.",
            metric_title(super::logic::other_metric(metric))
        ));
    }
    text
}

/// The strong label beside the location: coverage of the shown metric, or its overflow.
pub fn completeness(totals: &Result<MetricSummary, MetricOverflow>) -> String {
    match totals {
        Ok(summary) if summary.complete => COMPLETE_METADATA.into(),
        Ok(_) => INCOMPLETE_METADATA.into(),
        Err(overflow) => format!("{} totals overflowed", metric_title(overflow.metric)),
    }
}

fn overflow_detail(overflow: MetricOverflow) -> String {
    let other = metric_title(super::logic::other_metric(overflow.metric));
    let other = if overflow.other_available {
        format!("{other} size is available in View")
    } else {
        format!("{other} totals overflowed too")
    };
    format!(
        "{} byte totals overflowed and are not shown | {other}",
        metric_title(overflow.metric)
    )
}
pub const START_ERROR: &str = "HogMap could not open its window";
pub const PRODUCER_ERROR: &str = "Could not start the scan thread";
pub const PRODUCER_PANIC: &str = "The scan thread stopped unexpectedly";
pub const MISSING_TERMINAL: &str =
    "The scan stopped without a final status; results are incomplete";
pub const ELLIPSIS: &str = "…";

/// The documented meaning of a producer-defined code; Win32 errors have none here.
fn scan_code_text(code: u32) -> Option<&'static str> {
    match code {
        scan_win::SKIP_REPARSE_CODE => {
            Some("Not followed: symbolic link, junction or other reparse point")
        }
        scan_win::SKIP_CLOUD_CODE => Some("Not opened: cloud tag or recall attributes"),
        scan_win::UNSUPPORTED_PLATFORM_CODE => {
            Some("Windows scanning is unavailable on this platform")
        }
        scan_win::WORKER_FAILED_CODE => {
            Some("A directory worker could not start or stopped unexpectedly")
        }
        scan_win::PENDING_MEMORY_LIMIT_CODE => {
            Some("Scan incomplete: pending-directory memory budget reached")
        }
        _ => None,
    }
}

pub fn scan_code(code: u32) -> String {
    scan_code_text(code).map_or_else(|| format!("error {code}"), str::to_owned)
}

pub fn drive_error(code: u32) -> String {
    // A producer code reads as its meaning; a Win32 error is printed once, as `code N`.
    let detail = scan_code_text(code).map_or_else(|| format!("code {code}"), str::to_owned);
    format!("Cannot list drives: {detail}. Enter an existing folder or drive root.")
}

pub fn path_error(error: &std::io::Error) -> String {
    let detail = error
        .raw_os_error()
        .map_or_else(|| error.kind().to_string(), |code| scan_code(code as u32));
    format!("Cannot access this path: {detail}. Choose an existing directory.")
}

pub fn scan_failure(code: u32) -> String {
    format!("Scan failed: {}", scan_code(code))
}

pub fn directory_failure(code: u32) -> String {
    format!("Incomplete metadata: {}", scan_code(code))
}

pub fn scan_report(report: &ScanReport) -> String {
    let fallback = report
        .last_fallback_code
        .map_or_else(String::new, |code| format!(" (last: {})", scan_code(code)));
    format!(
        "Fallback directories: {}{fallback} | Reparse skips: {} | Cloud/recall skips: {} | Invalid names: {} | Pending-memory limit hits: {} | Max queue depth: {} | Max local stack depth: {} | Cancellation wakeups: {}",
        report.fallback_directories,
        report.reparse_skips,
        report.cloud_skips,
        report.invalid_names,
        report.pending_memory_limit_hits,
        report.max_queue_depth,
        report.max_local_stack_depth,
        report.cancellation_wakeups
    )
}

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

/// An aggregate's members opened as the map. `small` holds while the map uses the metric that
/// grouped them; after a metric switch the same members are only known to be grouped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subset {
    pub members: usize,
    pub small: bool,
}

fn subset_count(subset: Subset) -> String {
    if subset.small {
        aggregate_name(subset.members)
    } else {
        let noun = if subset.members == 1 {
            "grouped item"
        } else {
            "grouped items"
        };
        format!("{} {noun}", subset.members)
    }
}

/// The dataset root, native path components and, when the map is opened inside an aggregate,
/// the member count that the map draws.
pub fn location(root: &str, components: &[OsString], subset: Option<Subset>) -> String {
    let mut text = String::from(root);
    for component in components {
        text.push_str(" / ");
        text.push_str(&component.to_string_lossy());
    }
    if let Some(subset) = subset {
        text.push_str(" · ");
        text.push_str(&subset_count(subset));
    }
    text
}

pub fn status(
    state: &str,
    location: &str,
    totals: &Result<MetricSummary, MetricOverflow>,
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
    let failure = diagnostics
        .last_directory_failure_code
        .map_or_else(String::new, |code| {
            format!(" | {}", directory_failure(code))
        });
    let totals = match totals {
        Ok(summary) => {
            let metric = metric_bytes(summary.metric);
            let completeness = if summary.complete {
                "Complete"
            } else {
                "Incomplete"
            };
            let conflicts = if summary.conflicting_identities == 1 {
                "ID"
            } else {
                "IDs"
            };
            // Identity gaps are confidence indicators, reported apart from coverage.
            format!(
                "{} | {} visible {metric} | {} unique map | {completeness}: {} unknown {metric} sizes, {} inaccessible, {} partial | Identity: {} without ID, {} conflicting {conflicts}",
                files(summary.files),
                size(summary.known_bytes, false),
                size(summary.unique_bytes, false),
                summary.unknown_size_files,
                diagnostics.directory_failures,
                diagnostics.partial_directories,
                files(summary.identity_missing_files),
                summary.conflicting_identities
            )
        }
        Err(overflow) => overflow_detail(*overflow),
    };
    format!("{state} | {location} | {totals} | {protocol} protocol diagnostics{failure}")
}

pub fn master_status(
    state: &str,
    entries: usize,
    totals: &Result<MetricSummary, MetricOverflow>,
    elapsed: std::time::Duration,
) -> String {
    let seconds = elapsed.as_secs();
    let bytes = match totals {
        Ok(summary) => format!(
            "{} known {}",
            size(summary.known_bytes, false),
            metric_bytes(summary.metric)
        ),
        Err(overflow) => format!("{} total overflowed", metric_title(overflow.metric)),
    };
    format!(
        "{state}  ·  {entries} entries seen  ·  {bytes}  ·  {}:{:02} elapsed",
        seconds / 60,
        seconds % 60
    )
}

pub fn match_count(matches: usize, unknown: usize) -> String {
    format!("{matches} matches · {unknown} unknown")
}

pub fn drive_capacity(free: Option<u64>, total: Option<u64>) -> String {
    let known =
        |value: Option<u64>| value.map_or_else(|| "unknown".into(), |bytes| size(bytes, false));
    format!("{} free / {} total", known(free), known(total))
}

/// Detail shared by the chooser and header; wall-clock changes never invent a capture age.
pub fn capacity_detail(metadata: Option<&CapacityMetadata>, now: SystemTime) -> String {
    let Some(metadata) = metadata else {
        return "No global filesystem capacity observation is available.".into();
    };
    let source = match metadata.source {
        CapacitySource::WindowsActualGlobalFilesystem => {
            "Windows GetDiskSpaceInformationW: global filesystem values, not limited by per-user quotas"
        }
    };
    let age = match now.duration_since(metadata.captured_at) {
        Ok(age) => {
            let seconds = age.as_secs();
            if seconds < 60 {
                format!("Queried {seconds} s ago")
            } else if seconds < 3600 {
                format!("Queried {} min ago", seconds / 60)
            } else if seconds < 86400 {
                format!("Queried {} h ago", seconds / 3600)
            } else if seconds < 2 * 86400 {
                "Queried 1 day ago".into()
            } else {
                format!("Queried {} days ago", seconds / 86400)
            }
        }
        Err(_) => "Query age unavailable: system clock changed".into(),
    };
    let issue = match metadata.issue {
        None => "Capacity observed".into(),
        Some(CapacityIssue::QueryFailed(code)) => format!(
            "Capacity unknown: Windows query failed (HRESULT 0x{:08X})",
            code as u32
        ),
        Some(CapacityIssue::InvalidAllocationUnit) => {
            "Capacity unknown: invalid filesystem allocation unit".into()
        }
        Some(CapacityIssue::SizeOverflow) => {
            "Capacity unknown: filesystem byte count overflowed".into()
        }
        Some(CapacityIssue::IncoherentFreeSpace) => {
            "Capacity unknown: reported free space exceeds total capacity".into()
        }
    };
    format!("{source}. {age}. {issue}.")
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
pub fn aggregate_tooltip(count: usize, size: &str, metric: SizeMetric) -> String {
    format!(
        "{} · {size} known unique {}\nDouble-click to open",
        aggregate_name(count),
        metric_bytes(metric)
    )
}

/// Keeps the parent's name and marks the map as a subset of its members.
pub fn subset_name(name: &str, subset: Subset) -> String {
    format!("{name} · {}", subset_count(subset))
}
pub fn header(name: &str, size: &str) -> String {
    format!("{name} - {size}")
}
pub fn label(name: &str, size: &str) -> String {
    format!("{name}\n{size}")
}
pub fn tooltip(name: &str, size: &str, state: &str, metric: SizeMetric) -> String {
    format!(
        "{name}\nKnown unique {}: {size}\nState: {state}",
        metric_bytes(metric)
    )
}

#[cfg(test)]
mod tests {
    use super::super::logic::metric_totals;
    use super::*;
    use hogmap::index::{FileIdentity, Index, Kind, Sizes};

    #[test]
    fn scan_codes_distinguish_safety_skips_from_windows_errors() {
        assert_eq!(
            scan_code(scan_win::SKIP_REPARSE_CODE),
            "Not followed: symbolic link, junction or other reparse point"
        );
        assert_eq!(
            scan_code(scan_win::SKIP_CLOUD_CODE),
            "Not opened: cloud tag or recall attributes"
        );
        assert_eq!(
            scan_code(scan_win::UNSUPPORTED_PLATFORM_CODE),
            "Windows scanning is unavailable on this platform"
        );
        assert_eq!(
            scan_code(scan_win::WORKER_FAILED_CODE),
            "A directory worker could not start or stopped unexpectedly"
        );
        assert_eq!(scan_code(5), "error 5");
    }

    #[test]
    fn error_messages_keep_code_meanings_at_the_ui_boundary() {
        assert_eq!(
            scan_failure(scan_win::WORKER_FAILED_CODE),
            "Scan failed: A directory worker could not start or stopped unexpectedly"
        );
        assert_eq!(directory_failure(5), "Incomplete metadata: error 5");
        assert_eq!(scan_code(9876), "error 9876");
        assert_eq!(
            drive_error(5),
            "Cannot list drives: code 5. Enter an existing folder or drive root."
        );
        assert_eq!(
            drive_error(scan_win::UNSUPPORTED_PLATFORM_CODE),
            "Cannot list drives: Windows scanning is unavailable on this platform. Enter an existing folder or drive root."
        );
        let message = path_error(&std::io::Error::from_raw_os_error(5));
        assert_eq!(
            message,
            "Cannot access this path: error 5. Choose an existing directory."
        );
        let detail = status(
            "Finished",
            "Fixture",
            &Ok(hogmap::index::Index::new("Fixture".into())
                .unwrap()
                .summary_for(SizeMetric::Logical)
                .unwrap()),
            &ScanDiagnostics {
                last_directory_failure_code: Some(scan_win::SKIP_CLOUD_CODE),
                directory_failures: 1,
                ..Default::default()
            },
        );
        assert!(detail.contains("Not opened: cloud tag or recall attributes"));
    }

    fn fixture(files: &[(&str, Sizes, Option<u128>)]) -> Index {
        let mut index = Index::new("Fixture".into()).unwrap();
        let root = index.root();
        for (name, sizes, identity) in files {
            index
                .insert(
                    root,
                    (*name).into(),
                    Kind::File,
                    *sizes,
                    identity.map(|file| FileIdentity { volume: 1, file }),
                    State::Complete,
                )
                .unwrap();
        }
        index.set_state(root, State::Complete).unwrap();
        index
    }

    fn sizes(logical: Option<u64>, allocated: Option<u64>) -> Sizes {
        Sizes { logical, allocated }
    }

    #[test]
    fn status_describes_the_selected_metric_and_never_hides_its_overflow() {
        let quiet = ScanDiagnostics::default();
        let index = fixture(&[
            ("zero.bin", sizes(Some(0), Some(0)), Some(1)),
            ("first.bin", sizes(Some(100), Some(4096)), Some(2)),
            ("link.bin", sizes(Some(100), Some(8192)), Some(2)),
            ("loose.bin", sizes(Some(10), Some(4096)), None),
            ("fallback.bin", sizes(Some(50), None), None),
        ]);
        let allocated = metric_totals(&index, SizeMetric::Allocated);
        assert_eq!(
            status("Finished", "Fixture", &allocated, &quiet),
            "Finished | Fixture | 5 files | 16.0 KiB visible observed allocated | 8.0 KiB unique map | Incomplete: 1 unknown observed allocated sizes, 0 inaccessible, 0 partial | Identity: 1 file without ID, 1 conflicting ID | 0 protocol diagnostics"
        );
        assert_eq!(completeness(&allocated), INCOMPLETE_METADATA);
        let logical = metric_totals(&index, SizeMetric::Logical);
        assert_eq!(
            status("Finished", "Fixture", &logical, &quiet),
            "Finished | Fixture | 5 files | 260 B visible logical | 160 B unique map | Complete: 0 unknown logical sizes, 0 inaccessible, 0 partial | Identity: 2 files without ID, 0 conflicting IDs | 0 protocol diagnostics"
        );
        assert_eq!(completeness(&logical), COMPLETE_METADATA);

        let allocated_overflow = fixture(&[
            ("max.bin", sizes(Some(10), Some(u64::MAX)), None),
            ("one.bin", sizes(Some(20), Some(1)), None),
        ]);
        let totals = metric_totals(&allocated_overflow, SizeMetric::Allocated);
        assert_eq!(
            status("Finished", "Fixture", &totals, &quiet),
            "Finished | Fixture | Allocated byte totals overflowed and are not shown | Logical size is available in View | 0 protocol diagnostics"
        );
        assert_eq!(
            master_status("Ready", 3, &totals, std::time::Duration::from_secs(61)),
            "Ready  ·  3 entries seen  ·  Allocated total overflowed  ·  1:01 elapsed"
        );

        let both_overflow = fixture(&[
            ("max.bin", sizes(Some(u64::MAX), Some(u64::MAX)), None),
            ("one.bin", sizes(Some(1), Some(1)), None),
        ]);
        for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
            let detail = status(
                "Finished",
                "Fixture",
                &metric_totals(&both_overflow, metric),
                &quiet,
            );
            assert!(detail.contains("totals overflowed too"), "{detail}");
            assert!(!detail.contains("available"), "{detail}");
        }
    }

    #[test]
    fn location_text_joins_root_path_and_resolved_members() {
        let path = [OsString::from("media"), OsString::from("2024")];
        assert_eq!(location("Sample", &path, None), "Sample / media / 2024");
        let small = |members| Subset {
            members,
            small: true,
        };
        assert_eq!(
            location("Sample", &path, Some(small(1))),
            "Sample / media / 2024 · 1 small item"
        );
        assert_eq!(
            location("Sample", &path, Some(small(4))),
            "Sample / media / 2024 · 4 small items"
        );
        // After a metric switch the same members are no longer known to be the small ones.
        assert_eq!(
            location(
                "Sample",
                &path,
                Some(Subset {
                    members: 4,
                    small: false
                })
            ),
            "Sample / media / 2024 · 4 grouped items"
        );
    }
}
