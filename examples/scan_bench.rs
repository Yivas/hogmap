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
#[allow(dead_code)]
#[path = "../src/ui/logic.rs"]
mod logic;
#[cfg(feature = "desktop")]
#[allow(dead_code)]
#[path = "../src/ui/paint.rs"]
mod paint;
#[cfg(feature = "desktop")]
#[allow(dead_code)]
#[path = "../src/ui/text.rs"]
mod text;
#[cfg(feature = "desktop")]
#[allow(dead_code)]
#[path = "../src/ui/theme.rs"]
mod theme;
use hogmap::scan::{
    DEFAULT_CHANNEL_CAPACITY, ScanCancellation, ScanGeneration, ScanReducer, Terminal,
    event_channel,
};
use hogmap::scan_win::{
    ScanOptions, run_win_scan_observed_with_options, run_win_scan_with_options,
};
use hogmap::volume::{list_drives, volume_root_for};

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
                println!("origin_{name}_capacity_issue={:?}", sample.metadata.issue);
                println!(
                    "origin_{name}_capture_offset_ms={}",
                    wall_offset_ms(observations.started_at, sample.metadata.captured_at)
                );
            }
            Err(issue) => {
                println!("origin_{name}_total_bytes=unknown");
                println!("origin_{name}_free_bytes=unknown");
                println!("origin_{name}_capacity_issue=origin:{issue:?}");
                println!("origin_{name}_capture_offset_ms=unknown");
            }
        }
        match difference(sample, allocated) {
            Ok(value) => {
                println!("observed_d_{name}_bytes={value}");
                println!("observed_d_{name}_issue=none");
            }
            Err(issue) => {
                println!("observed_d_{name}_bytes=unknown");
                println!("observed_d_{name}_issue={issue}");
            }
        }
    }
}

fn run(args: Arguments) -> Result<bool, &'static str> {
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
            },
        )
        .map_err(|_| "synthetic_layout_failed")?;
        let matches = logic::HighlightFilter::default().matches(index);
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
    for key in 0..1_000_000_u64 {
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
    for key in 0..=1000 {
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
