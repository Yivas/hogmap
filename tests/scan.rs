use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use hogmap::index::{EntryId, FileIdentity, Index, Kind, Sizes, State};
use hogmap::scan::{
    Batch, DrainReport, MAX_BATCH_BYTES, MAX_BATCH_RECORDS, NodeKey, ScanEvent, ScanEventKind,
    ScanGeneration, ScanRecord, ScanReducer, Terminal,
};
use hogmap::synthetic::{ProducerOutcome, SYNTHETIC_ACCESS_DENIED, SyntheticSpec, run_synthetic};

fn spec() -> SyntheticSpec {
    SyntheticSpec {
        root_name: "Synthetic volume".into(),
        directories: 4,
        files: 8,
        inaccessible_directories: 0,
        hardlink_pairs: 0,
        unknown_size_files: 0,
        progress_every: 3,
    }
}

fn record(key: u64, parent: Option<u64>, name: &str, kind: Kind) -> ScanRecord {
    ScanRecord {
        key: NodeKey::new(key),
        parent: parent.map(NodeKey::new),
        name: name.into(),
        kind,
        sizes: Sizes::default(),
        identity: None,
    }
}

fn batch_of(records: impl IntoIterator<Item = ScanRecord>) -> Batch {
    let mut batch = Batch::new();
    for record in records {
        assert!(batch.push(record));
    }
    batch
}

fn event(generation: ScanGeneration, event: ScanEventKind) -> ScanEvent {
    ScanEvent { generation, event }
}

fn drain_until_end(reducer: &mut ScanReducer, receiver: &Receiver<ScanEvent>) -> DrainReport {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let report = reducer.drain(receiver, 4096);
        if report.terminal.is_some() || report.disconnected {
            return report;
        }
        assert!(Instant::now() < deadline, "scan did not terminate");
        thread::sleep(Duration::from_millis(1));
    }
}

fn run_to_end(
    spec: SyntheticSpec,
    seed: u64,
    capacity: usize,
    producer_generation: ScanGeneration,
    reducer_generation: ScanGeneration,
) -> (ScanReducer, ProducerOutcome) {
    let (sender, receiver) = sync_channel(capacity);
    let producer = thread::spawn(move || {
        let cancel = AtomicBool::new(false);
        run_synthetic(producer_generation, seed, spec, sender, &cancel)
    });
    let mut reducer = ScanReducer::new(reducer_generation);
    drain_until_end(&mut reducer, &receiver);
    let outcome = producer.join().unwrap();
    (reducer, outcome)
}

type SnapshotEntry = (
    Option<EntryId>,
    OsString,
    Kind,
    Sizes,
    Option<FileIdentity>,
    State,
);

fn snapshot(index: &Index) -> Vec<SnapshotEntry> {
    index
        .entries()
        .map(|(_, entry)| {
            (
                entry.parent,
                entry.name.clone(),
                entry.kind,
                entry.sizes,
                entry.identity,
                entry.state,
            )
        })
        .collect()
}

#[test]
fn synthetic_scan_reaches_the_index_and_finishes() {
    let generation = ScanGeneration::new(1);
    let (reducer, outcome) = run_to_end(spec(), 42, 4, generation, generation);
    assert_eq!(outcome, ProducerOutcome::Finished);
    assert_eq!(reducer.terminal(), Some(Terminal::Finished));
    let index = reducer.index().unwrap();
    assert_eq!(index.entries().count(), 1 + 4 + 8);
    assert!(
        index
            .entries()
            .all(|(_, entry)| entry.state == State::Complete)
    );
    let summary = index.summary().unwrap();
    assert_eq!(summary.files, 8);
    assert!(summary.complete);
    let diagnostics = reducer.diagnostics();
    assert_eq!(diagnostics.stale_generation_events, 0);
    assert_eq!(diagnostics.unknown_parent_records, 0);
    assert_eq!(diagnostics.duplicate_key_records, 0);
    assert_eq!(diagnostics.invalid_records, 0);
    assert_eq!(diagnostics.events_after_terminal, 0);
    assert_eq!(diagnostics.partial_directories, 0);
    assert!(!diagnostics.missing_terminal);
}

#[test]
fn a_stale_generation_never_touches_the_index() {
    let producer_generation = ScanGeneration::new(1);
    let reducer_generation = ScanGeneration::new(2);
    let (reducer, outcome) = run_to_end(spec(), 7, 4, producer_generation, reducer_generation);
    assert_eq!(outcome, ProducerOutcome::Finished);
    assert!(reducer.index().is_none());
    assert_eq!(reducer.terminal(), None);
    assert!(reducer.diagnostics().stale_generation_events > 0);
    assert!(reducer.diagnostics().missing_terminal);
}

#[test]
fn a_child_that_arrives_before_its_parent_is_reported_not_buffered() {
    let generation = ScanGeneration::new(9);
    let (sender, receiver) = sync_channel(8);
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([
                record(0, None, "Volume", Kind::Directory),
                record(1, Some(2), "early.bin", Kind::File),
                record(2, Some(0), "Parent", Kind::Directory),
            ])),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::DirectoryDone(NodeKey::new(2)),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::DirectoryDone(NodeKey::new(0)),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::Terminal(Terminal::Finished),
        ))
        .unwrap();
    drop(sender);

    let mut reducer = ScanReducer::new(generation);
    let report = drain_until_end(&mut reducer, &receiver);
    assert_eq!(report.terminal, Some(Terminal::Finished));
    assert_eq!(reducer.diagnostics().unknown_parent_records, 1);
    assert_eq!(reducer.diagnostics().partial_directories, 0);
    let index = reducer.index().unwrap();
    assert_eq!(index.entries().count(), 2);
    assert!(index.entries().all(|(_, entry)| entry.name != "early.bin"));
}

#[test]
fn cancellation_stops_a_producer_while_the_queue_is_full() {
    let generation = ScanGeneration::new(3);
    let (sender, receiver) = sync_channel(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let producer_cancel = Arc::clone(&cancel);
    let heavy = SyntheticSpec {
        root_name: "Synthetic volume".into(),
        directories: 5_000,
        files: 5_000,
        inaccessible_directories: 0,
        hardlink_pairs: 0,
        unknown_size_files: 0,
        progress_every: 100_000,
    };
    let producer =
        thread::spawn(move || run_synthetic(generation, 99, heavy, sender, &producer_cancel));

    let consumer_cancel = Arc::clone(&cancel);
    let (result_sender, result_receiver) = channel();
    let consumer = thread::spawn(move || {
        let mut reducer = ScanReducer::new(generation);
        let mut applied_batch = false;
        loop {
            let report = reducer.drain(&receiver, 4096);
            if report.terminal.is_some() || report.disconnected {
                break;
            }
            if !applied_batch && report.records > 0 {
                applied_batch = true;
                // Stop draining so the producer fills the bounded queue.
                thread::sleep(Duration::from_millis(50));
                consumer_cancel.store(true, Ordering::Relaxed);
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        result_sender.send(reducer).unwrap();
    });

    let reducer = result_receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("producer did not stop after cancellation");
    consumer.join().unwrap();
    assert_eq!(producer.join().unwrap(), ProducerOutcome::Cancelled);
    assert_eq!(reducer.terminal(), Some(Terminal::Cancelled));
    let diagnostics = reducer.diagnostics();
    assert_eq!(diagnostics.unknown_parent_records, 0);
    assert_eq!(diagnostics.duplicate_key_records, 0);
    assert_eq!(diagnostics.invalid_records, 0);
    assert!(diagnostics.partial_directories > 0);
    let index = reducer.index().expect("the first batch created the index");
    assert!(!index.summary().unwrap().complete);
}

#[test]
fn inaccessible_directories_are_reported_without_children() {
    let generation = ScanGeneration::new(4);
    let mut heavy = spec();
    heavy.directories = 4;
    heavy.files = 0;
    heavy.inaccessible_directories = 2;
    heavy.progress_every = 1;
    let (reducer, outcome) = run_to_end(heavy, 5, 4, generation, generation);
    assert_eq!(outcome, ProducerOutcome::Finished);
    let index = reducer.index().unwrap();
    let blocked: Vec<_> = index
        .entries()
        .filter(|(_, entry)| entry.kind == Kind::Directory && entry.state == State::Inaccessible)
        .map(|(id, _)| id)
        .collect();
    assert_eq!(blocked.len(), 2);
    for id in blocked {
        assert!(!index.entries().any(|(_, entry)| entry.parent == Some(id)));
    }
    assert_eq!(reducer.diagnostics().directory_failures, 2);
    assert_eq!(
        reducer.diagnostics().last_directory_failure_code,
        Some(SYNTHETIC_ACCESS_DENIED)
    );
    assert!(!index.summary().unwrap().complete);
}

#[test]
fn synthetic_hardlinks_share_one_logical_charge() {
    let generation = ScanGeneration::new(5);
    let mut heavy = spec();
    heavy.directories = 2;
    heavy.files = 1;
    heavy.hardlink_pairs = 1;
    heavy.progress_every = 1;
    let (reducer, _) = run_to_end(heavy, 11, 2, generation, generation);
    let index = reducer.index().unwrap();
    let mut groups: HashMap<FileIdentity, Vec<&hogmap::index::Entry>> = HashMap::new();
    for (_, entry) in index.entries() {
        if entry.kind == Kind::File {
            groups
                .entry(entry.identity.expect("synthetic files carry identity"))
                .or_default()
                .push(entry);
        }
    }
    assert_eq!(groups.len(), 1);
    let linked = groups.values().next().unwrap();
    assert_eq!(linked.len(), 2);
    assert_eq!(linked[0].sizes.logical, linked[1].sizes.logical);
    let size = linked[0].sizes.logical.unwrap();
    let summary = index.summary().unwrap();
    assert_eq!(summary.files, 2);
    assert_eq!(summary.known_logical_bytes, size * 2);
    assert_eq!(summary.unique_logical_bytes, size);
    assert_eq!(summary.shared_logical_bytes, size);
}

#[test]
fn synthetic_unknown_sizes_are_not_zero() {
    let generation = ScanGeneration::new(6);
    let mut heavy = spec();
    heavy.files = 6;
    heavy.unknown_size_files = 3;
    heavy.progress_every = 1;
    let (reducer, _) = run_to_end(heavy, 12, 2, generation, generation);
    let index = reducer.index().unwrap();
    let unknown = index
        .entries()
        .filter(|(_, entry)| entry.kind == Kind::File && entry.sizes.logical.is_none())
        .count();
    assert_eq!(unknown, 3);
    let summary = index.summary().unwrap();
    assert_eq!(summary.unknown_size_files, 3);
    assert!(!summary.complete);
}

#[test]
fn synthetic_scan_is_deterministic_for_a_seed() {
    let generation = ScanGeneration::new(7);
    let (first, _) = run_to_end(spec(), 123, 3, generation, generation);
    let (second, _) = run_to_end(spec(), 123, 3, generation, generation);
    let first_index = first.index().unwrap();
    let second_index = second.index().unwrap();
    assert_eq!(snapshot(first_index), snapshot(second_index));
    assert_eq!(first_index.summary(), second_index.summary());
}

#[test]
fn budgeted_drain_consumes_multiple_batches_but_bounds_events_and_time() {
    let generation = ScanGeneration::new(32);
    let (sender, receiver) = sync_channel(80);
    for key in 0..70 {
        sender
            .send(event(
                generation,
                ScanEventKind::Batch(batch_of([record(
                    key,
                    (key != 0).then_some(0),
                    &format!("directory-{key}"),
                    Kind::Directory,
                )])),
            ))
            .unwrap();
    }
    let mut reducer = ScanReducer::new(generation);
    let first = reducer.drain_budgeted(&receiver, 64, Duration::ZERO);
    assert_eq!((first.events, first.records), (1, 1));
    assert_eq!(reducer.index().unwrap().entries().count(), 1);
    // A generous duration isolates the deterministic event-count bound, not machine speed.
    let next = reducer.drain_budgeted(&receiver, 64, Duration::from_secs(60));
    assert_eq!((next.events, next.records), (64, 64));
    assert_eq!(reducer.index().unwrap().entries().count(), 65);
    let last = reducer.drain_budgeted(&receiver, 64, Duration::from_secs(60));
    assert_eq!((last.events, last.records), (5, 5));
    assert_eq!(reducer.diagnostics().unknown_parent_records, 0);
}

#[test]
fn batches_stop_at_the_record_and_byte_caps() {
    let mut narrow = Batch::new();
    let mut count = 0_usize;
    loop {
        let record = ScanRecord {
            key: NodeKey::new(count as u64),
            parent: None,
            name: "x".repeat(64).into(),
            kind: Kind::File,
            sizes: Sizes::default(),
            identity: None,
        };
        if !narrow.push(record) {
            break;
        }
        count += 1;
    }
    assert_eq!(count, MAX_BATCH_RECORDS);
    assert!(narrow.estimated_bytes() <= MAX_BATCH_BYTES);

    let mut wide = Batch::new();
    let mut wide_count = 0_usize;
    loop {
        let record = ScanRecord {
            key: NodeKey::new(wide_count as u64),
            parent: None,
            name: "x".repeat(1024).into(),
            kind: Kind::File,
            sizes: Sizes::default(),
            identity: None,
        };
        if !wide.push(record) {
            break;
        }
        wide_count += 1;
    }
    assert!(wide_count < MAX_BATCH_RECORDS);
    assert!(wide.estimated_bytes() <= MAX_BATCH_BYTES);
}

#[test]
fn events_after_the_terminal_are_counted() {
    let generation = ScanGeneration::new(8);
    let (sender, receiver) = sync_channel(4);
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([record(0, None, "Volume", Kind::Directory)])),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::Terminal(Terminal::Finished),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([record(1, Some(0), "late.bin", Kind::File)])),
        ))
        .unwrap();
    drop(sender);

    let mut reducer = ScanReducer::new(generation);
    let first = reducer.drain(&receiver, 4096);
    assert_eq!(first.terminal, Some(Terminal::Finished));
    let second = reducer.drain(&receiver, 4096);
    assert!(second.disconnected);
    assert_eq!(reducer.diagnostics().events_after_terminal, 1);
    assert_eq!(reducer.index().unwrap().entries().count(), 1);
}

#[test]
fn a_duplicate_key_is_counted_and_ignored() {
    let generation = ScanGeneration::new(20);
    let (sender, receiver) = sync_channel(4);
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([
                record(0, None, "Volume", Kind::Directory),
                record(1, Some(0), "first.bin", Kind::File),
                record(1, Some(0), "second.bin", Kind::File),
            ])),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::Terminal(Terminal::Finished),
        ))
        .unwrap();
    drop(sender);

    let mut reducer = ScanReducer::new(generation);
    drain_until_end(&mut reducer, &receiver);
    assert_eq!(reducer.diagnostics().duplicate_key_records, 1);
    assert_eq!(reducer.diagnostics().invalid_records, 0);
    let index = reducer.index().unwrap();
    assert_eq!(index.entries().count(), 2);
    assert!(index.entries().all(|(_, entry)| entry.name != "second.bin"));
}

#[test]
fn invalid_records_are_counted_and_not_applied() {
    let generation = ScanGeneration::new(21);
    let (sender, receiver) = sync_channel(4);
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([
                record(0, None, "Volume", Kind::Directory),
                record(1, Some(0), "file.bin", Kind::File),
                record(2, Some(1), "child.bin", Kind::File),
                record(3, Some(0), "bad/name.bin", Kind::File),
            ])),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::Terminal(Terminal::Finished),
        ))
        .unwrap();
    drop(sender);

    let mut reducer = ScanReducer::new(generation);
    drain_until_end(&mut reducer, &receiver);
    assert_eq!(reducer.diagnostics().invalid_records, 2);
    assert_eq!(reducer.diagnostics().unknown_parent_records, 0);
    assert_eq!(reducer.diagnostics().duplicate_key_records, 0);
    let index = reducer.index().unwrap();
    assert_eq!(index.entries().count(), 2);
    assert!(
        index
            .entries()
            .all(|(_, entry)| entry.name != "child.bin" && entry.name != "bad/name.bin")
    );
}

#[test]
fn a_non_directory_root_is_rejected() {
    let generation = ScanGeneration::new(22);
    let (sender, receiver) = sync_channel(4);
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([record(0, None, "Volume", Kind::File)])),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::Terminal(Terminal::Finished),
        ))
        .unwrap();
    drop(sender);

    let mut reducer = ScanReducer::new(generation);
    drain_until_end(&mut reducer, &receiver);
    assert_eq!(reducer.diagnostics().invalid_records, 1);
    assert!(reducer.index().is_none());
}

#[test]
fn directory_outcomes_for_unknown_keys_are_counted() {
    let generation = ScanGeneration::new(23);
    let (sender, receiver) = sync_channel(4);
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([record(0, None, "Volume", Kind::Directory)])),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::DirectoryDone(NodeKey::new(99)),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::DirectoryFailed {
                key: NodeKey::new(100),
                code: 5,
            },
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::Terminal(Terminal::Finished),
        ))
        .unwrap();
    drop(sender);

    let mut reducer = ScanReducer::new(generation);
    drain_until_end(&mut reducer, &receiver);
    let diagnostics = reducer.diagnostics();
    assert_eq!(diagnostics.unknown_directory_events, 2);
    assert_eq!(diagnostics.directory_failures, 0);
    assert_eq!(diagnostics.last_directory_failure_code, None);
    assert_eq!(diagnostics.invalid_records, 0);
    assert_eq!(reducer.index().unwrap().entries().count(), 1);
}

#[test]
fn a_repeated_directory_outcome_is_counted_once() {
    let generation = ScanGeneration::new(26);
    let (sender, receiver) = sync_channel(8);
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([
                record(0, None, "Volume", Kind::Directory),
                record(1, Some(0), "file.bin", Kind::File),
            ])),
        ))
        .unwrap();
    for outcome in [
        ScanEventKind::DirectoryDone(NodeKey::new(0)),
        ScanEventKind::DirectoryFailed {
            key: NodeKey::new(0),
            code: 5,
        },
        ScanEventKind::DirectoryDone(NodeKey::new(1)),
        ScanEventKind::Terminal(Terminal::Finished),
    ] {
        sender.send(event(generation, outcome)).unwrap();
    }
    drop(sender);

    let mut reducer = ScanReducer::new(generation);
    drain_until_end(&mut reducer, &receiver);
    let diagnostics = reducer.diagnostics();
    assert_eq!(diagnostics.duplicate_directory_outcomes, 1);
    assert_eq!(diagnostics.unknown_directory_events, 1);
    assert_eq!(diagnostics.directory_failures, 0);
    let index = reducer.index().unwrap();
    assert_eq!(index.get(index.root()).unwrap().state, State::Complete);
}

#[test]
fn a_disconnect_without_terminal_partializes_the_index_it_created() {
    let generation = ScanGeneration::new(24);
    let (sender, receiver) = sync_channel(4);
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([
                record(0, None, "Volume", Kind::Directory),
                record(1, Some(0), "Folder", Kind::Directory),
            ])),
        ))
        .unwrap();
    drop(sender);

    let mut reducer = ScanReducer::new(generation);
    let report = reducer.drain(&receiver, 4096);
    assert!(report.disconnected);
    assert_eq!(report.terminal, None);
    assert!(reducer.diagnostics().missing_terminal);
    assert_eq!(reducer.diagnostics().partial_directories, 2);
    let index = reducer.index().expect("the batch created the index");
    assert!(
        index
            .entries()
            .all(|(_, entry)| entry.state == State::Partial)
    );
    assert!(!index.summary().unwrap().complete);
}

#[test]
fn a_producer_terminates_when_the_receiver_is_dropped_after_cancellation() {
    let generation = ScanGeneration::new(25);
    let (sender, receiver) = sync_channel(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let producer_cancel = Arc::clone(&cancel);
    let heavy = SyntheticSpec {
        root_name: "Synthetic volume".into(),
        directories: 5_000,
        files: 5_000,
        inaccessible_directories: 0,
        hardlink_pairs: 0,
        unknown_size_files: 0,
        progress_every: 100_000,
    };
    let producer =
        thread::spawn(move || run_synthetic(generation, 31, heavy, sender, &producer_cancel));

    let mut reducer = ScanReducer::new(generation);
    let deadline = Instant::now() + Duration::from_secs(20);
    while reducer.drain(&receiver, 4096).records == 0 {
        assert!(Instant::now() < deadline, "the producer sent no records");
        thread::sleep(Duration::from_millis(1));
    }
    cancel.store(true, Ordering::Relaxed);
    drop(receiver);

    let (outcome_sender, outcome_receiver) = channel();
    let joiner = thread::spawn(move || {
        let outcome = producer.join().expect("producer thread panicked");
        outcome_sender
            .send(outcome)
            .expect("outcome channel closed");
    });
    // The producer may queue its terminal before the receiver is dropped; either end is a stop.
    let outcome = outcome_receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("producer did not stop after the receiver was dropped");
    assert!(matches!(
        outcome,
        ProducerOutcome::Cancelled | ProducerOutcome::Disconnected
    ));
    joiner.join().expect("joiner thread panicked");
}

#[test]
fn a_failed_terminal_marks_unfinished_directories_partial() {
    let generation = ScanGeneration::new(10);
    let (sender, receiver) = sync_channel(8);
    sender
        .send(event(
            generation,
            ScanEventKind::Batch(batch_of([
                record(0, None, "Volume", Kind::Directory),
                record(1, Some(0), "Blocked", Kind::Directory),
            ])),
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::DirectoryFailed {
                key: NodeKey::new(1),
                code: 77,
            },
        ))
        .unwrap();
    sender
        .send(event(
            generation,
            ScanEventKind::Terminal(Terminal::Failed(9)),
        ))
        .unwrap();
    drop(sender);

    let mut reducer = ScanReducer::new(generation);
    let report = drain_until_end(&mut reducer, &receiver);
    assert_eq!(report.terminal, Some(Terminal::Failed(9)));
    let index = reducer.index().unwrap();
    let root = index.root();
    let blocked = index
        .entries()
        .find(|(_, entry)| entry.name == "Blocked")
        .map(|(id, _)| id)
        .unwrap();
    assert_eq!(index.get(root).unwrap().state, State::Partial);
    assert_eq!(index.get(blocked).unwrap().state, State::Inaccessible);
    assert_eq!(reducer.diagnostics().partial_directories, 1);
    assert_eq!(reducer.diagnostics().directory_failures, 1);
    assert_eq!(reducer.diagnostics().last_directory_failure_code, Some(77));
    assert!(!index.summary().unwrap().complete);
}
