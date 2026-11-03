//! Headless metadata scan with the desktop consumer's drain budget and cadence.
use std::env;
use std::io::{self, Write};
use std::mem::size_of;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use hogmap::index::{Entry, Kind, State};
use hogmap::scan::{
    DEFAULT_CHANNEL_CAPACITY, ScanCancellation, ScanGeneration, ScanReducer, Terminal,
    event_channel,
};
use hogmap::scan_win::{ScanOptions, run_win_scan_with_options};
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
}

fn arguments() -> Result<Arguments, &'static str> {
    let mut args = env::args_os().skip(1);
    let root = args.next().ok_or("missing_root")?;
    if root.is_empty() || root.to_string_lossy().starts_with("--") {
        return Err("invalid_root");
    }
    let mut parsed = Arguments {
        root: root.into(),
        options: ScanOptions::default(),
        unthrottled: true,
        hold_for_sample: false,
        time_limit: None,
    };
    let mut workers_seen = false;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--workers") if !workers_seen => {
                let workers = args
                    .next()
                    .and_then(|value| value.to_str().and_then(|value| value.parse::<usize>().ok()))
                    .filter(|workers| (0..=8).contains(workers))
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

fn run(args: Arguments) -> Result<bool, &'static str> {
    // Query capacity outside the scan timing; never print roots, labels or filesystem names.
    let volume_root = volume_root_for(&args.root);
    let volumes = list_drives();
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
            let result =
                run_win_scan_with_options(generation, args.root, sender, &flag, args.options);
            (result, Instant::now())
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
    let ((outcome, report), producer_finished) =
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
    println!("entry_struct_size_bytes={}", size_of::<Entry>());
    println!(
        "index_inline_memory_estimate_bytes={}",
        u128::from(entries) * size_of::<Entry>() as u128
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
    println!("scan_done=1");
    io::stdout().flush().map_err(|_| "stdout_failed")?;
    if args.hold_for_sample {
        thread::sleep(SAMPLE_HOLD);
        std::hint::black_box(&reducer);
    }
    Ok((terminal == Some(Terminal::Finished)
        || (partial_run && terminal == Some(Terminal::Cancelled)))
        && !diagnostics.missing_terminal)
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
