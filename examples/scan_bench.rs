//! Headless metadata scan with the desktop consumer's drain budget and cadence.
use std::env;
use std::io::{self, Write};
use std::mem::size_of;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use hogmap::index::{FileIdentity, Kind, SizeMetric, Sizes, State};
use hogmap::scan::{
    Batch, DirectoryOutcome, NodeKey, ScanEvent, ScanEventKind, ScanRecord, ScanSender,
};
#[cfg(not(feature = "desktop"))]
#[allow(dead_code)]
#[path = "../src/ui/logic.rs"]
mod logic;
#[cfg(feature = "desktop")]
use desktop_ui::{logic, paint, theme};
use hogmap::scan::{
    DEFAULT_CHANNEL_CAPACITY, ScanCancellation, ScanGeneration, ScanReducer, Terminal,
    event_channel,
};
use hogmap::scan_win::{
    ScanOptions, run_win_scan_observed_with_options, run_win_scan_with_options,
};
use hogmap::volume::{list_drives, volume_root_for};

// The example compiles the actual desktop module, not a separate drawing model.
#[cfg(feature = "desktop")]
#[path = "../src/ui/mod.rs"]
pub mod desktop_ui;

const DRAIN_BUDGET: Duration = Duration::from_millis(4);
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const SAMPLE_HOLD: Duration = Duration::from_millis(1500);

struct Arguments {
    root: PathBuf,
    options: ScanOptions,
    unthrottled: bool,
    hold_for_sample: bool,
    time_limit: Option<Duration>,
    synthetic_million: bool,
    observed: bool,
}

fn arguments() -> Result<Arguments, &'static str> {
    let mut args = env::args_os().skip(1);
    let root = args.next().ok_or("missing_root")?;
    let synthetic_million = root == "--synthetic-million";
    if root.is_empty() || (!synthetic_million && root.to_string_lossy().starts_with("--")) {
        return Err("invalid_root");
    }
    let mut parsed = Arguments {
        root: root.into(),
        options: ScanOptions::default(),
        unthrottled: true,
        hold_for_sample: false,
        time_limit: None,
        synthetic_million,
        observed: env::var_os("HOGMAP_BENCH_OBSERVED").is_some_and(|value| value == "1"),
    };
    let mut workers_seen = false;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--workers") if !workers_seen => {
                let workers = args
                    .next()
                    .and_then(|value| value.to_str().and_then(|value| value.parse::<usize>().ok()))
                    .filter(|workers| (0..=16).contains(workers))
                    .ok_or("invalid_workers")?;
                parsed.options.workers = workers;
                workers_seen = true;
            }
            Some("--unthrottled-drain" | "--unthrottled") => parsed.unthrottled = true,
            Some("--ui-cadence") => parsed.unthrottled = false,
            Some("--time-limit-seconds") if parsed.time_limit.is_none() => {
                let seconds = args
                    .next()
                    .and_then(|value| value.to_str().and_then(|value| value.parse::<u64>().ok()))
                    .filter(|seconds| (1..=590).contains(seconds))
                    .ok_or("invalid_time_limit")?;
                parsed.time_limit = Some(Duration::from_secs(seconds));
            }
            Some("--hold-for-sample") if !parsed.hold_for_sample => parsed.hold_for_sample = true,
            _ => return Err("invalid_argument"),
        }
    }
    Ok(parsed)
}

fn optional_count(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(|| "unknown".into(), |value| value.to_string())
}

fn wall_offset_ms(start: std::time::SystemTime, time: std::time::SystemTime) -> i128 {
    match time.duration_since(start) {
        Ok(duration) => duration.as_millis() as i128,
        Err(error) => -(error.duration().as_millis() as i128),
    }
}

fn difference(
    sample: Result<hogmap::volume::OriginCapacitySample, hogmap::volume::OriginIssue>,
    allocated: Option<u64>,
) -> Result<i128, String> {
    let sample = sample.map_err(|issue| format!("origin:{issue:?}"))?;
    if let Some(issue) = sample.metadata.issue {
        return Err(format!("capacity:{issue:?}"));
    }
    let (total, free) = sample
        .total_bytes
        .zip(sample.free_bytes)
        .ok_or("capacity_unknown")?;
    if free > total {
        return Err("incoherent_capacity".into());
    }
    let allocated = allocated.ok_or("allocated_overflow")?;
    // Three u64 inputs fit in i128, preserving a negative discrepancy rather than clipping it.
    Ok(i128::from(total) - i128::from(free) - i128::from(allocated))
}

fn difference_excluding_reserve(
    sample: Result<hogmap::volume::OriginCapacitySample, hogmap::volume::OriginIssue>,
    allocated: Option<u64>,
) -> Result<i128, String> {
    let sample = sample.map_err(|issue| format!("origin:{issue:?}"))?;
    let including_reserve = difference(Ok(sample), allocated)?;
    if let Some(issue) = sample.reserve_issue {
        return Err(format!("reserve:{issue:?}"));
    }
    let reserve = sample
        .storage_reserve_bytes
        .ok_or("storage_reserve_unknown")?;
    // Four u64 inputs fit in i128, even when the observed difference is negative.
    Ok(including_reserve - i128::from(reserve))
}

fn print_origin_diagnostics(
    observations: &hogmap::scan_win::ScanOriginObservations,
    filesystem: Option<&str>,
    allocated: Option<u64>,
) {
    let filesystem = match filesystem.map(str::to_ascii_uppercase).as_deref() {
        Some("NTFS") => "NTFS",
        Some("REFS") => "ReFS",
        Some("FAT") => "FAT",
        Some("FAT32") => "FAT32",
        Some("EXFAT") => "exFAT",
        _ => "unknown",
    };
    println!("observed_listed_filesystem_type={filesystem}");
    println!("observed_filesystem_source=pre_scan_listing");
    match &observations.root {
        Ok(root) => {
            println!("origin_root_scope={:?}", root.scope);
            println!("origin_root_scope_issue={:?}", root.scope_issue);
            println!(
                "origin_root_capture_offset_ms={}",
                wall_offset_ms(observations.started_at, root.captured_at)
            );
        }
        Err(issue) => {
            println!("origin_root_scope=Unknown");
            println!("origin_root_scope_issue={issue:?}");
            println!("origin_root_capture_offset_ms=unknown");
        }
    }
    println!(
        "origin_enumeration_finished_offset_ms={}",
        optional_count(
            observations
                .enumeration_finished_at
                .map(|time| wall_offset_ms(observations.started_at, time))
        )
    );
    match observations.validation {
        Ok(validation) => {
            println!(
                "origin_validation_identity={:?}",
                validation.directory_identity
            );
            println!("origin_validation_path={:?}", validation.normalized_path);
        }
        Err(issue) => {
            println!("origin_validation_identity=unknown:{issue:?}");
            println!("origin_validation_path=unknown:{issue:?}");
        }
    }
    println!(
        "observed_allocated_overflow={}",
        u8::from(allocated.is_none())
    );
    for (name, sample) in [
        ("before", observations.before),
        ("after", observations.after),
    ] {
        match sample {
            Ok(sample) => {
                println!(
                    "origin_{name}_total_bytes={}",
                    optional_count(sample.total_bytes)
                );
                println!(
                    "origin_{name}_free_bytes={}",
                    optional_count(sample.free_bytes)
                );
                println!(
                    "origin_{name}_storage_reserve_bytes={}",
                    optional_count(sample.storage_reserve_bytes)
                );
                println!(
                    "origin_{name}_total_reserved_bytes={}",
                    optional_count(sample.total_reserved_bytes)
                );
                println!("origin_{name}_reserve_issue={:?}", sample.reserve_issue);
                println!("origin_{name}_capacity_issue={:?}", sample.metadata.issue);
                println!(
                    "origin_{name}_capture_offset_ms={}",
                    wall_offset_ms(observations.started_at, sample.metadata.captured_at)
                );
            }
            Err(issue) => {
                println!("origin_{name}_total_bytes=unknown");
                println!("origin_{name}_free_bytes=unknown");
                println!("origin_{name}_storage_reserve_bytes=unknown");
                println!("origin_{name}_total_reserved_bytes=unknown");
                println!("origin_{name}_reserve_issue=origin:{issue:?}");
                println!("origin_{name}_capacity_issue=origin:{issue:?}");
                println!("origin_{name}_capture_offset_ms=unknown");
            }
        }
        // Keep the earlier diagnostic keys as aliases for the including-reserve value.
        for (field, result) in [
            (format!("observed_d_{name}"), difference(sample, allocated)),
            (
                format!("observed_d_including_reserve_{name}"),
                difference(sample, allocated),
            ),
            (
                format!("observed_d_excluding_reserve_{name}"),
                difference_excluding_reserve(sample, allocated),
            ),
        ] {
            match result {
                Ok(value) => {
                    println!("{field}_bytes={value}");
                    println!("{field}_issue=none");
                }
                Err(issue) => {
                    println!("{field}_bytes=unknown");
                    println!("{field}_issue={issue}");
                }
            }
        }
    }
}

fn run(args: Arguments) -> Result<bool, &'static str> {
    if args.synthetic_million && env::var_os("HOGMAP_BENCH_EXTRA").is_some_and(|value| value == "1")
    {
        extra_bench()?;
        return Ok(true);
    }
    #[cfg(feature = "desktop")]
    if args.synthetic_million && env::var_os("HOGMAP_BENCH_UI").is_some_and(|value| value == "1") {
        return ui_bench();
    }
    // Query capacity outside the scan timing; never print roots, labels or filesystem names.
    let volume_root = if args.synthetic_million {
        None
    } else {
        volume_root_for(&args.root)
    };
    let volumes = if args.synthetic_million {
        Ok(Vec::new())
    } else {
        list_drives()
    };
    let volume_error = volumes.as_ref().err().map(|error| error.code);
    let volume = volumes.ok().and_then(|volumes| {
        volumes.into_iter().find(|volume| {
            volume_root
                .as_ref()
                .is_some_and(|root| volume.root.eq_ignore_ascii_case(root))
        })
    });
    let generation = ScanGeneration::new(1);
    let (sender, receiver) = event_channel(DEFAULT_CHANNEL_CAPACITY);
    let mut reducer = ScanReducer::new(generation);
    let started = Instant::now();
    let cancellation = ScanCancellation::new();
    let flag = cancellation.clone();
    let (stop_timer, timer_done) = std::sync::mpsc::channel();
    let timer = args.time_limit.map(|limit| {
        let cancellation = cancellation.clone();
        thread::spawn(move || {
            if matches!(
                timer_done.recv_timeout(limit),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                cancellation.cancel();
            }
        })
    });
    let reread_root = args.observed.then(|| args.root.clone());
    let producer = thread::Builder::new()
        .name("hogmap-scan-bench".into())
        .spawn(move || {
            let (result, observations) = if args.synthetic_million {
                (
                    (
                        synthetic_million(generation, sender, &flag),
                        hogmap::scan_win::ScanReport::default(),
                    ),
                    None,
                )
            } else if args.observed {
                let result = run_win_scan_observed_with_options(
                    generation,
                    args.root,
                    sender,
                    &flag,
                    args.options,
                );
                ((result.outcome, result.report), Some(result.observations))
            } else {
                (
                    run_win_scan_with_options(generation, args.root, sender, &flag, args.options),
                    None,
                )
            };
            (result, observations, Instant::now())
        })
        .map_err(|_| "producer_start_failed")?;
    let mut first_batch = None;
    let mut drain_time = Duration::ZERO;
    let mut max_drain = Duration::ZERO;
    let mut apply_time = Duration::ZERO;
    let mut drains = 0_u64;
    let mut events = 0_u64;
    let mut time_budget_hits = 0_u64;
    let mut received_records = 0_u64;
    let mut received_directories = 0_u64;
    let mut last_progress = Instant::now();
    let mut previous_records = 0_u64;
    let mut previous_directories = 0_u64;
    loop {
        if args.unthrottled {
            receiver.wait_ready_timeout(Duration::from_secs(1));
        }
        let drain_started = Instant::now();
        let report = reducer.drain_budgeted(&receiver, usize::MAX, DRAIN_BUDGET);
        let elapsed = drain_started.elapsed();
        drain_time += elapsed;
        apply_time += report.apply_time;
        max_drain = max_drain.max(elapsed);
        drains += 1;
        events += report.events as u64;
        received_records += report.records as u64;
        received_directories += report.directories as u64;
        if last_progress.elapsed() >= Duration::from_secs(5) {
            println!(
                "window_seconds={:.3} window_entries_per_second={:.3} window_directories_per_second={:.3}",
                last_progress.elapsed().as_secs_f64(),
                (received_records - previous_records) as f64
                    / last_progress.elapsed().as_secs_f64(),
                (received_directories - previous_directories) as f64
                    / last_progress.elapsed().as_secs_f64()
            );
            previous_records = received_records;
            previous_directories = received_directories;
            println!(
                "partial_elapsed_ms={:.3} partial_entries={} partial_directories={} partial_entries_per_second={:.3}",
                started.elapsed().as_secs_f64() * 1000.0,
                received_records,
                received_directories,
                received_records as f64 / started.elapsed().as_secs_f64()
            );
            io::stdout().flush().map_err(|_| "stdout_failed")?;
            last_progress = Instant::now();
        }
        time_budget_hits += u64::from(report.events > 0 && elapsed >= DRAIN_BUDGET);
        if report.records > 0 && first_batch.is_none() {
            first_batch = Some(started.elapsed());
        }
        if report.terminal.is_some() || report.disconnected {
            break;
        }
        if !args.unthrottled {
            thread::sleep(FRAME_INTERVAL);
        }
    }
    let ingested = Instant::now();
    let ((outcome, report), observations, producer_finished) =
        producer.join().map_err(|_| "producer_panicked")?;
    if observations.is_some()
        && let Some(root) = reread_root.as_ref()
    {
        if let Some(index) = reducer.finished_index_mut() {
            for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
                println!(
                    "conflict_reread_before_{metric:?}={}",
                    optional_count(
                        index
                            .summary_for(metric)
                            .ok()
                            .map(|s| s.conflicting_identities)
                    )
                );
            }
            let reread = hogmap::scan_win::reread_identity_conflicts(index, root, &cancellation);
            println!("conflict_reread_candidates={}", reread.candidate_identities);
            println!("conflict_reread_attempted={}", reread.attempted_identities);
            println!("conflict_reread_resolved={}", reread.resolved_identities);
            println!(
                "conflict_reread_unresolved={}",
                reread.unresolved_identities
            );
            println!(
                "conflict_reread_logical_delta_bytes={}",
                reread.logical_delta_bytes
            );
            println!(
                "conflict_reread_allocated_delta_bytes={}",
                reread.allocated_delta_bytes
            );
            println!("conflict_reread_issues={:?}", reread.issues);
            println!(
                "conflict_reread_limit={}",
                hogmap::scan_win::MAX_CONFLICT_REREADS
            );
            if let Some(observed) = observations.as_ref() {
                println!(
                    "conflict_reread_started_offset_ms={}",
                    wall_offset_ms(observed.started_at, reread.started_at)
                );
                println!(
                    "conflict_reread_finished_offset_ms={}",
                    wall_offset_ms(observed.started_at, reread.finished_at)
                );
            }
            for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
                println!(
                    "conflict_reread_after_{metric:?}={}",
                    optional_count(
                        index
                            .summary_for(metric)
                            .ok()
                            .map(|s| s.conflicting_identities)
                    )
                );
            }
        } else {
            println!("conflict_reread_status=not_finished");
        }
    }
    let elapsed = started.elapsed();
    let _ = stop_timer.send(());
    if let Some(timer) = timer {
        timer.join().map_err(|_| "timer_panicked")?;
    }
    let index = reducer.index().ok_or("missing_index")?;
    let summary = index.summary().map_err(|_| "summary_overflow")?;
    let mut entries = 0_u64;
    let mut directories = 0_u64;
    let mut inaccessible = 0_u64;
    let mut partial = 0_u64;
    for (_, entry) in index.entries() {
        entries += 1;
        if entry.kind == Kind::Directory {
            directories += 1;
            inaccessible += u64::from(entry.state == State::Inaccessible);
            partial += u64::from(entry.state == State::Partial);
        }
    }
    println!("observed={}", u8::from(observations.is_some()));
    if let Some(observations) = observations.as_ref() {
        let counts = observations.directories;
        println!("origin_opened={}", counts.opened_directories);
        println!("origin_serial64_same={}", counts.serial64_same);
        println!("origin_serial64_different={}", counts.serial64_different);
        println!(
            "origin_serial64_unavailable={}",
            counts.serial64_unavailable
        );
        println!(
            "origin_guid_on_serial_mismatch={}",
            counts.guid_on_serial_mismatch
        );
        println!(
            "origin_guid_on_serial_unavailable={}",
            counts.guid_on_serial_unavailable
        );
        println!("origin_guid_same={}", counts.same_guid_name);
        println!(
            "origin_guid_different_unproven={}",
            counts.different_guid_name_unproven
        );
        println!("origin_unavailable={}", counts.unavailable);
        println!("origin_not_queried={}", counts.not_queried);
    }
    println!("elapsed_ms={:.3}", elapsed.as_secs_f64() * 1000.0);
    println!(
        "producer_wall_ms={:.3}",
        producer_finished.duration_since(started).as_secs_f64() * 1000.0
    );
    println!(
        "ingestion_wall_ms={:.3}",
        ingested.duration_since(started).as_secs_f64() * 1000.0
    );
    println!(
        "post_producer_tail_ms={:.3}",
        ingested
            .saturating_duration_since(producer_finished)
            .as_secs_f64()
            * 1000.0
    );
    println!("drain_active_ms={:.3}", drain_time.as_secs_f64() * 1000.0);
    println!("reducer_apply_ms={:.3}", apply_time.as_secs_f64() * 1000.0);
    println!(
        "entries_per_second={:.3}",
        entries as f64 / elapsed.as_secs_f64()
    );
    println!(
        "directories_per_second={:.3}",
        directories as f64 / elapsed.as_secs_f64()
    );
    for (id, worker) in report
        .workers
        .iter()
        .enumerate()
        .take(report.selected_workers)
    {
        println!("worker_{id}={worker:?}");
    }
    println!("first_open_ns={}", optional_count(report.first_open_ns));
    println!(
        "first_batch_sent_ns={}",
        optional_count(report.first_batch_sent_ns)
    );
    println!("last_outcome_ns={}", optional_count(report.last_outcome_ns));
    println!("terminal_ns={}", optional_count(report.terminal_ns));
    println!("max_drain_ms={:.3}", max_drain.as_secs_f64() * 1000.0);
    println!(
        "time_to_first_batch_ms={}",
        optional_count(first_batch.map(|time| format!("{:.3}", time.as_secs_f64() * 1000.0)))
    );
    println!("workers={}", report.selected_workers);
    println!("seek_penalty={}", optional_count(report.seek_penalty));
    println!("mft_ordered={}", u8::from(report.mft_ordered));
    println!("drain_cadence_ms={}", if args.unthrottled { 0 } else { 33 });
    println!("drain_calls={drains}");
    println!("events={events}");
    println!("time_budget_hits={time_budget_hits}");
    println!("entries={entries}");
    println!("files={}", summary.files);
    println!("directories={directories}");
    println!("known_logical_bytes={}", summary.known_logical_bytes);
    println!("unique_logical_bytes={}", summary.unique_logical_bytes);
    println!("shared_logical_bytes={}", summary.shared_logical_bytes);
    println!("unknown_size_files={}", summary.unknown_size_files);
    println!("inaccessible_directories={inaccessible}");
    println!("partial_directories={partial}");
    println!("summary_complete={}", u8::from(summary.complete));
    // Both metric summaries are constant-time; they exercise the independent accounting and
    // report its confidence indicators without adding retained data to the workload.
    let logical_metric = index
        .summary_for(SizeMetric::Logical)
        .map_err(|_| "summary_overflow")?;
    let allocated_metric = index.summary_for(SizeMetric::Allocated);
    if let Some(observations) = observations.as_ref() {
        print_origin_diagnostics(
            observations,
            volume
                .as_ref()
                .and_then(|volume| volume.filesystem.as_deref()),
            allocated_metric
                .as_ref()
                .ok()
                .map(|metric| metric.unique_bytes),
        );
    }
    if observations.is_none() && allocated_metric.is_err() {
        return Err("summary_overflow");
    }
    println!(
        "logical_identity_missing_files={}",
        logical_metric.identity_missing_files
    );
    println!(
        "logical_conflicting_identities={}",
        logical_metric.conflicting_identities
    );
    if let Ok(allocated_metric) = allocated_metric {
        println!("allocated_known_bytes={}", allocated_metric.known_bytes);
        println!("allocated_unique_bytes={}", allocated_metric.unique_bytes);
        println!(
            "allocated_unknown_size_files={}",
            allocated_metric.unknown_size_files
        );
        println!(
            "allocated_identity_missing_files={}",
            allocated_metric.identity_missing_files
        );
        println!(
            "allocated_conflicting_identities={}",
            allocated_metric.conflicting_identities
        );
    } else {
        for field in [
            "allocated_known_bytes",
            "allocated_unique_bytes",
            "allocated_unknown_size_files",
            "allocated_identity_missing_files",
            "allocated_conflicting_identities",
        ] {
            println!("{field}=unknown");
        }
    }
    println!(
        "entry_struct_size_bytes={}",
        index.memory_estimate().entry_bytes / index.len()
    );
    println!("entry_id_bytes={}", size_of::<hogmap::index::EntryId>());
    println!("index_memory={:?}", index.memory_estimate());
    println!(
        "name_boundary_capacity_bytes={}",
        index.memory_estimate().name_boundary_capacity_bytes
    );
    println!(
        "index_memory_estimate_bytes={}",
        index.memory_estimate().total_bytes()
    );
    println!(
        "consumer_memory_before_release={:?}",
        reducer.memory_before_release()
    );
    println!(
        "scan_record_struct_bytes={}",
        size_of::<hogmap::scan::ScanRecord>()
    );
    println!(
        "scan_report_struct_bytes={}",
        size_of::<hogmap::scan_win::ScanReport>()
    );
    println!(
        "in_flight_batch_payload_bound_bytes={}",
        (DEFAULT_CHANNEL_CAPACITY + 2) * hogmap::scan::MAX_BATCH_BYTES
    );
    println!(
        "index_inline_memory_estimate_bytes={}",
        index.memory_estimate().entry_bytes
    );
    println!(
        "report_fallback_directories={}",
        report.fallback_directories
    );
    println!(
        "report_last_fallback_code={}",
        optional_count(report.last_fallback_code)
    );
    println!("report_invalid_names={}", report.invalid_names);
    println!("report_reparse_skips={}", report.reparse_skips);
    println!("report_cloud_skips={}", report.cloud_skips);
    println!(
        "report_local_stack_limit_hits={}",
        report.local_stack_limit_hits
    );
    println!(
        "report_pending_memory_limit_hits={}",
        report.pending_memory_limit_hits
    );
    println!("report_peak_pending_bytes={}", report.peak_pending_bytes);
    println!("report_peak_pending_nodes={}", report.peak_pending_nodes);
    println!(
        "report_pending_node_struct_bytes={}",
        report.pending_node_struct_bytes
    );
    println!("report_max_queue_depth={}", report.max_queue_depth);
    println!(
        "report_max_local_stack_depth={}",
        report.max_local_stack_depth
    );
    println!(
        "report_cancellation_wakeups={}",
        report.cancellation_wakeups
    );
    let diagnostics = reducer.diagnostics();
    println!(
        "diagnostics_stale_generation_events={}",
        diagnostics.stale_generation_events
    );
    println!(
        "diagnostics_unknown_parent_records={}",
        diagnostics.unknown_parent_records
    );
    println!(
        "diagnostics_duplicate_key_records={}",
        diagnostics.duplicate_key_records
    );
    println!(
        "diagnostics_invalid_records={}",
        diagnostics.invalid_records
    );
    println!(
        "diagnostics_unknown_directory_events={}",
        diagnostics.unknown_directory_events
    );
    println!(
        "diagnostics_duplicate_directory_outcomes={}",
        diagnostics.duplicate_directory_outcomes
    );
    println!(
        "diagnostics_events_after_terminal={}",
        diagnostics.events_after_terminal
    );
    println!(
        "diagnostics_partial_directories={}",
        diagnostics.partial_directories
    );
    println!(
        "diagnostics_directory_failures={}",
        diagnostics.directory_failures
    );
    println!(
        "diagnostics_last_directory_failure_code={}",
        optional_count(diagnostics.last_directory_failure_code)
    );
    println!(
        "diagnostics_missing_terminal={}",
        u8::from(diagnostics.missing_terminal)
    );
    let terminal = reducer.terminal();
    println!(
        "terminal={}",
        match terminal {
            Some(Terminal::Finished) => "finished",
            Some(Terminal::Cancelled) => "cancelled",
            Some(Terminal::Failed(_)) => "failed",
            None => "missing",
        }
    );
    println!(
        "terminal_code={}",
        optional_count(match terminal {
            Some(Terminal::Failed(code)) => Some(code),
            _ => None,
        })
    );
    println!("producer_outcome={outcome:?}");
    println!("volume_query_error_code={}", optional_count(volume_error));
    println!(
        "volume_total_bytes={}",
        optional_count(volume.as_ref().and_then(|volume| volume.total_bytes))
    );
    println!(
        "volume_free_bytes={}",
        optional_count(volume.as_ref().and_then(|volume| volume.free_bytes))
    );
    println!(
        "volume_used_bytes={}",
        optional_count(volume.and_then(|volume| {
            volume
                .total_bytes
                .zip(volume.free_bytes)
                .and_then(|(total, free)| total.checked_sub(free))
        }))
    );
    println!(
        "sample_hold_ms={}",
        if args.hold_for_sample {
            SAMPLE_HOLD.as_millis()
        } else {
            0
        }
    );
    let partial_run = cancellation.is_cancelled();
    println!("run_partial={}", u8::from(partial_run));
    let mut synthetic_cache = None;
    #[cfg(feature = "desktop")]
    let mut prepared_cells = None;
    #[cfg(feature = "desktop")]
    let context = args.synthetic_million.then(eframe::egui::Context::default);
    if args.synthetic_million {
        let started = Instant::now();
        let layout = hogmap::treemap::layout(
            index,
            index.root(),
            hogmap::treemap::Rect::new(0.0, 0.0, 1920.0, 1080.0),
            hogmap::treemap::LayoutParams {
                min_pixel_area: 400.0,
                header_height: 20.0,
                max_depth: usize::MAX,
            },
        )
        .map_err(|_| "synthetic_layout_failed")?;
        let matches = logic::HighlightFilter::default()
            .match_results(index)
            .matches;
        #[cfg(feature = "desktop")]
        let _ = context.as_ref().expect("synthetic context").run_ui(
            eframe::egui::RawInput::default(),
            |ui| {
                prepared_cells = Some(paint::prepare(
                    ui,
                    index,
                    &layout,
                    None,
                    hogmap::index::SizeMetric::Logical,
                ));
            },
        );
        println!(
            "synthetic_ui_cache_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        println!("synthetic_layout_cells={}", layout.placed.len());
        println!(
            "synthetic_match_capacity_bytes={}",
            matches.capacity() * size_of::<hogmap::filter::Match>()
        );
        println!("synthetic_native_window=0");
        println!(
            "synthetic_desktop_cache={}",
            u8::from(cfg!(feature = "desktop"))
        );
        synthetic_cache = Some((layout, matches));
    }
    println!("scan_done=1");
    io::stdout().flush().map_err(|_| "stdout_failed")?;
    if args.hold_for_sample {
        thread::sleep(SAMPLE_HOLD);
        std::hint::black_box(&reducer);
        std::hint::black_box(&synthetic_cache);
        #[cfg(feature = "desktop")]
        std::hint::black_box((&context, &prepared_cells));
    }
    Ok((terminal == Some(Terminal::Finished)
        || (partial_run && terminal == Some(Terminal::Cancelled)))
        && !diagnostics.missing_terminal)
}

/// Streaming fixture: exactly 1M entries, every non-root identity present, no disk/plan allocation.
fn synthetic_million(
    generation: ScanGeneration,
    sender: ScanSender,
    cancel: &ScanCancellation,
) -> hogmap::synthetic::ProducerOutcome {
    synthetic_entries(generation, sender, cancel, 1_000_000, true)
}

fn synthetic_entries(
    generation: ScanGeneration,
    sender: ScanSender,
    cancel: &ScanCancellation,
    entries: u64,
    directory_outcomes: bool,
) -> hogmap::synthetic::ProducerOutcome {
    use hogmap::synthetic::ProducerOutcome;
    let flush = |batch: &mut Batch| -> Result<(), ProducerOutcome> {
        match hogmap::scan::send_event(
            &sender,
            ScanEvent {
                generation,
                event: ScanEventKind::Batch(std::mem::take(batch)),
            },
            cancel,
        ) {
            hogmap::scan::SendOutcome::Sent => Ok(()),
            hogmap::scan::SendOutcome::Disconnected => Err(ProducerOutcome::Disconnected),
            hogmap::scan::SendOutcome::Cancelled => Err(
                if hogmap::scan::send_terminal(&sender, generation, Terminal::Cancelled) {
                    ProducerOutcome::Cancelled
                } else {
                    ProducerOutcome::Disconnected
                },
            ),
        }
    };
    let mut batch = Batch::new();
    for key in 0..entries {
        let directory = key <= 1000;
        let record = ScanRecord {
            key: NodeKey::new(key),
            parent: if key == 0 {
                None
            } else if directory {
                Some(NodeKey::new(0))
            } else {
                Some(NodeKey::new(1 + (key - 1001) % 1000))
            },
            name: if key == 0 {
                "Synthetic million".into()
            } else {
                format!("entry-{key:07}.bin").into()
            },
            kind: if directory {
                Kind::Directory
            } else {
                Kind::File
            },
            sizes: if directory || key % 10000 == 0 {
                Sizes::default()
            } else {
                Sizes {
                    logical: Some(key % 65536),
                    allocated: Some(65536),
                }
            },
            identity: (key > 0).then_some(FileIdentity {
                volume: 1,
                file: u128::from(key),
            }),
        };
        if !batch.push(record.clone()) {
            if let Err(outcome) = flush(&mut batch) {
                return outcome;
            }
            assert!(batch.push(record));
        }
    }
    for key in (0..=1000).filter(|_| directory_outcomes) {
        let outcome = DirectoryOutcome {
            key: NodeKey::new(key),
            code: None,
        };
        if !batch.push_outcome(outcome) {
            if let Err(outcome) = flush(&mut batch) {
                return outcome;
            }
            assert!(batch.push_outcome(outcome));
        }
    }
    if !batch.is_empty()
        && let Err(outcome) = flush(&mut batch)
    {
        return outcome;
    }
    if hogmap::scan::send_terminal(&sender, generation, Terminal::Finished) {
        ProducerOutcome::Finished
    } else {
        ProducerOutcome::Disconnected
    }
}

/// Opt-in release comparison: the existing million-entry stream, no filesystem or native UI.
#[cfg(feature = "desktop")]
fn ui_bench() -> Result<bool, &'static str> {
    use hogmap::treemap::{Layout, LayoutParams, NodeRef, Rect, layout};
    use std::hash::{Hash, Hasher};
    if cfg!(debug_assertions) {
        return Err("ui_bench_requires_release");
    }
    let generation = ScanGeneration::new(660);
    let (sender, receiver) = event_channel(16);
    let worker =
        thread::spawn(move || synthetic_million(generation, sender, &ScanCancellation::new()));
    let mut reducer = ScanReducer::new(generation);
    while reducer.terminal().is_none() {
        receiver.wait_ready();
        reducer.drain(&receiver, 65536);
    }
    assert_eq!(
        worker.join().unwrap(),
        hogmap::synthetic::ProducerOutcome::Finished
    );
    if env::var_os("HOGMAP_BENCH_LIVE_FRAME").is_some_and(|value| value == "1") {
        live_frame_bench(reducer)?;
        return Ok(true);
    }
    let index = reducer
        .finished_index_mut()
        .ok_or("ui_fixture_not_finished")?;
    assert_eq!(index.len(), 1_000_000);
    let area = Rect::new(0.0, 0.0, 1920.0, 1000.0);
    let params = LayoutParams {
        min_pixel_area: 400.0,
        header_height: 20.0,
        max_depth: 2,
    };
    let digest = |layout: &Layout| {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        layout.known_sum.hash(&mut hash);
        layout.has_unknown.hash(&mut hash);
        for cell in &layout.placed {
            match cell.node {
                NodeRef::Entry(id) => (0, id.position()).hash(&mut hash),
                NodeRef::Aggregate { parent } => (1, parent.position()).hash(&mut hash),
            }
            for value in [
                cell.rect.x,
                cell.rect.y,
                cell.rect.width,
                cell.rect.height,
                cell.content.x,
                cell.content.y,
                cell.content.width,
                cell.content.height,
            ] {
                value.to_bits().hash(&mut hash);
            }
            cell.known_sum.hash(&mut hash);
            cell.has_unknown.hash(&mut hash);
            cell.members.hash(&mut hash);
        }
        hash.finish()
    };
    println!(
        "ui_bench_dataset=synthetic-million-v1 entries={} width=1920 height=1000 depth=2 native_window=0",
        index.len()
    );
    let start = Instant::now();
    let first = layout(index, index.root(), area, params).map_err(|_| "ui_layout_failed")?;
    println!(
        "ui_layout_cold_ms={:.3}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    println!(
        "ui_layout_cells={} ui_layout_digest={}",
        first.placed.len(),
        digest(&first)
    );
    let mut warm = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let repeated = layout(index, index.root(), area, params).map_err(|_| "ui_layout_failed")?;
        warm.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(digest(&first), digest(&repeated));
        std::hint::black_box(repeated);
    }
    warm.sort_by(f64::total_cmp);
    println!("ui_layout_warm_median_ms={:.3}", warm[2]);
    let context = eframe::egui::Context::default();
    let input = || eframe::egui::RawInput {
        screen_rect: Some(eframe::egui::Rect::from_min_size(
            eframe::egui::Pos2::ZERO,
            eframe::egui::vec2(1920.0, 1000.0),
        )),
        ..Default::default()
    };
    let mut cells = Vec::new();
    let start = Instant::now();
    let _ = context.run_ui(input(), |ui| {
        cells = paint::prepare(ui, index, &first, None, SizeMetric::Logical);
    });
    println!(
        "ui_labels_cold_ms={:.3}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    let mut label_hash = std::collections::hash_map::DefaultHasher::new();
    for cell in &cells {
        if let Some((galley, position)) = &cell.label {
            galley.job.text.hash(&mut label_hash);
            position.x.to_bits().hash(&mut label_hash);
            position.y.to_bits().hash(&mut label_hash);
        }
    }
    println!(
        "ui_labels_count={} ui_labels_digest={}",
        cells.iter().filter(|cell| cell.label.is_some()).count(),
        label_hash.finish()
    );
    let mut labels = Vec::new();
    let mut draws = Vec::new();
    let mut draw_scratch = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let _ = context.run_ui(input(), |ui| {
            cells = paint::prepare(ui, index, &first, None, SizeMetric::Logical);
        });
        labels.push(start.elapsed().as_secs_f64() * 1000.0);
        let start = Instant::now();
        let _ = context.run_ui(input(), |ui| {
            paint::draw(
                ui.painter(),
                theme::Theme::Dark.palette(),
                &first,
                &cells,
                &[],
                (None, None),
                &mut draw_scratch,
            );
        });
        draws.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    labels.sort_by(f64::total_cmp);
    draws.sort_by(f64::total_cmp);
    println!(
        "ui_labels_warm_median_ms={:.3} ui_draw_median_ms={:.3}",
        labels[2], draws[2]
    );
    for (tag, expression) in [("empty", ""), ("real", "*.bin;>32kb")] {
        let mut filter = logic::HighlightFilter::default();
        assert!(filter.set(expression));
        let start = Instant::now();
        let results = filter.match_results(index);
        let counts = (results.matched_files, results.unknown_files);
        let values = results.matches;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for value in &values {
            (match value {
                hogmap::filter::Match::Yes => 1_u8,
                hogmap::filter::Match::No => 0,
                hogmap::filter::Match::Unknown => 2,
            })
            .hash(&mut hash);
        }
        println!(
            "ui_filter_{tag}_first_ms={elapsed:.3} matched={} unknown={} digest={}",
            counts.0,
            counts.1,
            hash.finish()
        );
        let mut frames = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            let _ = context.run_ui(input(), |ui| {
                let map = layout(index, index.root(), area, params).unwrap();
                let results = filter.match_results(index);
                let counts = (results.matched_files, results.unknown_files);
                let matches = results.matches;
                let prepared = paint::prepare(ui, index, &map, None, SizeMetric::Logical);
                paint::draw(
                    ui.painter(),
                    theme::Theme::Dark.palette(),
                    &map,
                    &prepared,
                    &matches,
                    (None, None),
                    &mut draw_scratch,
                );
                std::hint::black_box((map, matches, counts, prepared));
            });
            frames.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        frames.sort_by(f64::total_cmp);
        println!("ui_rebuild_full_{tag}_median_ms={:.3}", frames[2]);
    }
    let parent = index
        .entries()
        .find(|(id, entry)| *id != index.root() && entry.kind == Kind::Directory)
        .unwrap()
        .0;
    let start = Instant::now();
    for number in 0..64 {
        index
            .insert(
                parent,
                format!("growth-{number:03}").into(),
                Kind::File,
                Sizes {
                    logical: Some(17),
                    allocated: Some(32),
                },
                None,
                State::Complete,
            )
            .map_err(|_| "ui_append_failed")?;
    }
    println!(
        "ui_append_64_ms={:.3}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    let start = Instant::now();
    let grown = layout(index, index.root(), area, params).map_err(|_| "ui_growth_layout_failed")?;
    println!(
        "ui_layout_after_append_ms={:.3} ui_growth_digest={} ui_growth_cells={}",
        start.elapsed().as_secs_f64() * 1000.0,
        digest(&grown),
        grown.placed.len()
    );
    let mut filter = logic::HighlightFilter::default();
    assert!(filter.set("*.bin;>32kb"));
    let start = Instant::now();
    let growth_matches = filter.match_results(index);
    println!(
        "ui_filter_after_append_ms={:.3} matched={} unknown={}",
        start.elapsed().as_secs_f64() * 1000.0,
        growth_matches.matched_files,
        growth_matches.unknown_files
    );
    let memory = index.memory_estimate();
    println!(
        "ui_index_estimate_bytes={} ui_layout_cache_bytes={} ui_filter_cache_bytes={} ui_draw_scratch_capacity={}",
        memory.total_bytes(),
        memory.layout_cache_bytes,
        memory.filter_cache_bytes,
        draw_scratch.capacity()
    );
    println!("scan_done=1");
    println!("entries={}", index.len());
    println!("sample_hold_ms=1500");
    io::stdout().flush().map_err(|_| "stdout_failed")?;
    thread::sleep(SAMPLE_HOLD);
    std::hint::black_box((&reducer, first, grown, cells, context));
    Ok(true)
}

#[cfg(feature = "desktop")]
struct FrameSample {
    ms: f64,
    shapes: usize,
    vertices: usize,
    repaint_ms: f64,
    ui_ms: f64,
    tess_ms: f64,
}

#[cfg(feature = "desktop")]
fn rendered_frame(
    context: &eframe::egui::Context,
    app: &mut desktop_ui::HogMap,
    events: Vec<eframe::egui::Event>,
    time: f64,
) -> FrameSample {
    use eframe::egui;
    let start = Instant::now();
    let output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1920.0, 1000.0),
            )),
            time: Some(time),
            events,
            ..Default::default()
        },
        |ui| app.render(ui),
    );
    let shapes = output.shapes.len();
    let repaint_ms = output
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .map_or(f64::INFINITY, |viewport| {
            viewport.repaint_delay.as_secs_f64() * 1000.0
        });
    let ui_ms = start.elapsed().as_secs_f64() * 1000.0;
    let primitives = context.tessellate(output.shapes, output.pixels_per_point);
    let vertices = primitives
        .iter()
        .map(|primitive| match &primitive.primitive {
            egui::epaint::Primitive::Mesh(mesh) => mesh.vertices.len(),
            egui::epaint::Primitive::Callback(_) => 0,
        })
        .sum();
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    std::hint::black_box(primitives);
    FrameSample {
        ms,
        shapes,
        vertices,
        repaint_ms,
        ui_ms,
        tess_ms: ms - ui_ms,
    }
}

#[cfg(feature = "desktop")]
fn report_frames(tag: &str, samples: &[FrameSample]) {
    let mut times: Vec<_> = samples.iter().map(|sample| sample.ms).collect();
    times.sort_by(f64::total_cmp);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let p95 = times[(times.len() * 95).div_ceil(100).saturating_sub(1)];
    let sample = &samples[samples.len() - 1];
    println!(
        "live_frame_{tag}_components ui_mean_ms={:.3} tess_mean_ms={:.3}",
        samples.iter().map(|sample| sample.ui_ms).sum::<f64>() / samples.len() as f64,
        samples.iter().map(|sample| sample.tess_ms).sum::<f64>() / samples.len() as f64
    );
    println!(
        "live_frame_{tag} n={} mean_ms={mean:.3} median_ms={:.3} p95_ms={p95:.3} max_ms={:.3} shapes={} vertices={} repaint_ms={:.3}",
        times.len(),
        times[times.len() / 2],
        times[times.len() - 1],
        sample.shapes,
        sample.vertices,
        sample.repaint_ms
    );
}

#[cfg(feature = "desktop")]
fn live_frame_bench(reducer: ScanReducer) -> Result<(), &'static str> {
    use eframe::egui;
    let context = egui::Context::default();
    theme::apply(&context, theme::Theme::Dark);
    let mut app = desktop_ui::HogMap::with_reducer(context.clone(), reducer);
    let mut time = 0.0;
    for _ in 0..8 {
        rendered_frame(&context, &mut app, Vec::new(), time);
        time += 0.25;
    }
    println!(
        "live_frame_fixture entries={} width=1920 height=1000 native_window=0 accesskit_active=0 includes=real_chrome_status_map_and_tessellation",
        app.reducer.index().unwrap().len()
    );
    let mut stable = Vec::new();
    for _ in 0..20 {
        stable.push(rendered_frame(&context, &mut app, Vec::new(), time));
        time += 0.25;
    }
    report_frames("stable", &stable);
    println!(
        "live_frame_complete_rebuild_cost_ms={:.3}",
        app.last_layout_cost.as_secs_f64() * 1000.0
    );
    let mut hover = Vec::new();
    for number in 0..20 {
        hover.push(rendered_frame(
            &context,
            &mut app,
            vec![egui::Event::PointerMoved(egui::pos2(
                100.0 + (number % 10) as f32 * 96.0,
                300.0,
            ))],
            time,
        ));
        time += 0.25;
    }
    report_frames("hover", &hover);
    let parent = app
        .reducer
        .index()
        .unwrap()
        .entries()
        .find(|(id, entry)| {
            *id != app.reducer.index().unwrap().root() && entry.kind == Kind::Directory
        })
        .unwrap()
        .0;
    let (hold_sender, receiver) = event_channel(1);
    app.receiver = Some(receiver);
    let mut growth = Vec::new();
    for number in 0..10 {
        for offset in 0..64 {
            app.reducer
                .finished_index_mut()
                .unwrap()
                .insert(
                    parent,
                    format!("live-growth-{number}-{offset}").into(),
                    Kind::File,
                    Sizes {
                        logical: Some(17),
                        allocated: Some(32),
                    },
                    None,
                    State::Complete,
                )
                .map_err(|_| "live_growth_failed")?;
        }
        app.version += 1;
        // Measure an eligible growth rebuild separately from throttled intermediate frames.
        app.last_layout = None;
        let previous = app.last_layout;
        growth.push(rendered_frame(&context, &mut app, Vec::new(), time));
        time += 0.26;
        assert_ne!(
            app.last_layout, previous,
            "growth sample must actually rebuild"
        );
    }
    std::hint::black_box(hold_sender);
    report_frames("growth_rebuild", &growth);
    drop(app);

    // A separate real-channel synthetic scan measures ingestion and UI backpressure together.
    let context = egui::Context::default();
    theme::apply(&context, theme::Theme::Dark);
    let generation = ScanGeneration::new(680);
    let (sender, receiver) = event_channel(DEFAULT_CHANNEL_CAPACITY);
    let wake_context = context.clone();
    sender.set_waker(move || wake_context.request_repaint_after(desktop_ui::FRAME_INTERVAL));
    let producer =
        thread::spawn(move || synthetic_million(generation, sender, &ScanCancellation::new()));
    let mut app = desktop_ui::HogMap::with_reducer(context.clone(), ScanReducer::new(generation));
    app.scan_started = Some(Instant::now());
    app.receiver = Some(receiver);
    let start = Instant::now();
    let mut next = start;
    let mut ordinary = Vec::new();
    let mut rebuilds = Vec::new();
    let mut ordinary_drains = Vec::new();
    let mut rebuild_drains = Vec::new();
    let mut drain_over8 = 0;
    let mut drain_over12 = 0;
    let mut over8 = 0;
    let mut over12 = 0;
    while app.receiver.is_some() {
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        }
        let tick = Instant::now();
        let previous = app.last_layout;
        app.drain(&context);
        let drain_ms = tick.elapsed().as_secs_f64() * 1000.0;
        drain_over8 += usize::from(drain_ms > 8.0);
        drain_over12 += usize::from(drain_ms > 12.0);
        let drain_sample = FrameSample {
            ms: drain_ms,
            shapes: 0,
            vertices: 0,
            repaint_ms: 0.0,
            ui_ms: 0.0,
            tess_ms: 0.0,
        };
        let mut sample = rendered_frame(
            &context,
            &mut app,
            Vec::new(),
            start.elapsed().as_secs_f64(),
        );
        sample.ms = tick.elapsed().as_secs_f64() * 1000.0;
        over8 += usize::from(sample.ms > 8.0);
        over12 += usize::from(sample.ms > 12.0);
        if app.last_layout != previous {
            rebuilds.push(sample);
            rebuild_drains.push(drain_sample);
        } else {
            ordinary.push(sample);
            ordinary_drains.push(drain_sample);
        }
        next = tick + desktop_ui::FRAME_INTERVAL;
        if start.elapsed() > Duration::from_secs(120) {
            return Err("live_stream_timeout");
        }
    }
    let elapsed = start.elapsed();
    assert_eq!(
        producer.join().unwrap(),
        hogmap::synthetic::ProducerOutcome::Finished
    );
    assert_eq!(app.reducer.index().unwrap().len(), 1_000_000);
    if !ordinary.is_empty() {
        report_frames("stream_ordinary_drain_plus_frame", &ordinary);
    }
    if !rebuilds.is_empty() {
        report_frames("stream_rebuild_drain_plus_frame", &rebuilds);
    }
    println!(
        "live_stream wall_ms={:.3} ticks={} rebuilds={} ticks_over8={} ticks_over12={} nominal_tick_hz={:.3} entries=1000000",
        elapsed.as_secs_f64() * 1000.0,
        ordinary.len() + rebuilds.len(),
        rebuilds.len(),
        over8,
        over12,
        1.0 / desktop_ui::FRAME_INTERVAL.as_secs_f64()
    );
    if !ordinary_drains.is_empty() {
        report_frames("stream_ordinary_drain_only", &ordinary_drains);
    }
    if !rebuild_drains.is_empty() {
        report_frames("stream_rebuild_drain_only", &rebuild_drains);
    }
    println!(
        "live_stream_rates entries_per_second={:.3} map_layout_hz={:.3} mesh_builds={} drain_over8={} drain_over12={}",
        1_000_000.0 / elapsed.as_secs_f64(),
        rebuilds.len() as f64 / elapsed.as_secs_f64(),
        app.paint_rebuilds,
        drain_over8,
        drain_over12
    );
    println!("scan_done=1\nentries=1000000\nsample_hold_ms=1500");
    io::stdout().flush().map_err(|_| "stdout_failed")?;
    thread::sleep(SAMPLE_HOLD);
    std::hint::black_box((&app, context));
    Ok(())
}

/// Separate diagnostics: no filesystem query, and no change to the million-entry sentinel.
fn extra_bench() -> Result<(), &'static str> {
    use hogmap::index::Index;
    use hogmap::treemap::{LayoutParams, Rect};
    for complete in [true, false] {
        let generation = ScanGeneration::new(682);
        let (sender, receiver) = event_channel(DEFAULT_CHANNEL_CAPACITY);
        let worker = thread::spawn(move || {
            synthetic_entries(
                generation,
                sender,
                &ScanCancellation::new(),
                1_000_000,
                complete,
            )
        });
        let mut reducer = ScanReducer::new(generation);
        loop {
            receiver.wait_ready();
            let start = Instant::now();
            let report = reducer.drain(&receiver, 1);
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            if report.terminal.is_some() {
                println!(
                    "terminal_fixture complete={complete} terminal_records={} terminal_ms={ms:.3} partial_directories={}",
                    report.records,
                    reducer.diagnostics().partial_directories
                );
                assert_eq!(report.records, 0);
                assert_eq!(
                    reducer.diagnostics().partial_directories,
                    if complete { 0 } else { 1001 }
                );
                #[cfg(feature = "desktop")]
                {
                    let context = eframe::egui::Context::default();
                    theme::apply(&context, theme::Theme::Dark);
                    let mut app = desktop_ui::HogMap::with_reducer(context.clone(), reducer);
                    let sample = rendered_frame(&context, &mut app, Vec::new(), 0.0);
                    report_frames(
                        if complete {
                            "terminal_complete_rebuild"
                        } else {
                            "terminal_incomplete_rebuild"
                        },
                        &[sample],
                    );
                }
                break;
            }
            if reducer
                .index()
                .is_some_and(|index| index.len() == 1_000_000)
            {
                let index = reducer.index().unwrap();
                let _ = hogmap::treemap::layout(
                    index,
                    index.root(),
                    Rect::new(0.0, 0.0, 1920.0, 1000.0),
                    LayoutParams {
                        min_pixel_area: 1024.0,
                        header_height: 20.0,
                        max_depth: 64,
                    },
                )
                .map_err(|_| "extra_layout")?;
                let _ = logic::HighlightFilter::default().match_results(index);
            }
        }
        assert_eq!(
            worker.join().unwrap(),
            hogmap::synthetic::ProducerOutcome::Finished
        );
    }
    let mut index = Index::new("Synthetic million".into()).map_err(|_| "extra_index")?;
    let mut parents = vec![index.root()];
    let mut previous = index.memory_estimate().charged_identity_bytes;
    let mut growths = [(0_usize, Duration::ZERO); 64];
    for key in 1..3_800_000_u64 {
        let directory = key <= 1000;
        let parent = if directory {
            index.root()
        } else {
            parents[(1 + (key - 1001) % 1000) as usize]
        };
        let sizes = if directory || key % 10000 == 0 {
            Sizes::default()
        } else {
            Sizes {
                logical: Some(key % 65536),
                allocated: Some(65536),
            }
        };
        let name = format!("entry-{key:07}.bin").into();
        let start = Instant::now();
        let id = index
            .insert(
                parent,
                name,
                if directory {
                    Kind::Directory
                } else {
                    Kind::File
                },
                sizes,
                Some(FileIdentity {
                    volume: 1,
                    file: u128::from(key),
                }),
                if directory {
                    State::Pending
                } else {
                    State::Complete
                },
            )
            .map_err(|_| "extra_insert")?;
        let elapsed = start.elapsed();
        if directory {
            parents.push(id);
        }
        let memory = index.memory_estimate();
        if memory.charged_identity_bytes > previous {
            // Mirrors the private routing solely to label measured capacity changes. The time is
            // an upper bound on rehash cost: the whole insertion, not a claimed allocator timer.
            let shard =
                ((key ^ 1_u64.rotate_left(11)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 58) as usize;
            growths[shard].0 += 1;
            growths[shard].1 = growths[shard].1.max(elapsed);
        }
        previous = memory.charged_identity_bytes;
        if key + 1 == 1_000_000 || key + 1 == 3_800_000 {
            let map = hogmap::treemap::layout(
                &index,
                index.root(),
                Rect::new(0.0, 0.0, 1920.0, 1000.0),
                LayoutParams {
                    min_pixel_area: 1024.0,
                    header_height: 20.0,
                    max_depth: 64,
                },
            )
            .map_err(|_| "extra_layout")?;
            let matches = logic::HighlightFilter::default().match_results(&index);
            let memory = index.memory_estimate();
            println!(
                "memory_fixture entries={} index_bytes={} charge_bytes={} layout_bytes={} filter_bytes={} drawn_cells={} output_matches_capacity={}",
                index.len(),
                memory.total_bytes(),
                memory.charged_identity_bytes,
                memory.layout_cache_bytes,
                memory.filter_cache_bytes,
                map.placed.len(),
                matches.matches.capacity()
            );
        }
    }
    for (shard, (count, max)) in growths.into_iter().enumerate() {
        println!(
            "identity_shard={shard} growths={count} max_growing_insert_ms={:.3}",
            max.as_secs_f64() * 1000.0
        );
    }
    println!("scan_done=1\nentries=3800000\nsample_hold_ms=1500");
    io::stdout().flush().map_err(|_| "stdout_failed")?;
    thread::sleep(SAMPLE_HOLD);
    std::hint::black_box(index);
    Ok(())
}

fn main() -> ExitCode {
    match arguments().and_then(run) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(code) => {
            println!("benchmark_error={code}");
            ExitCode::FAILURE
        }
    }
}
