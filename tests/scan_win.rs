#![cfg(windows)]

use hogmap::scan::{ScanCancellation, event_channel as sync_channel};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use hogmap::index::{Kind, State};
use hogmap::scan::{ScanEventKind, ScanGeneration, ScanReducer, Terminal};
use hogmap::scan_win::{ScanOptions, run_win_scan, run_win_scan_with_options};
use hogmap::synthetic::ProducerOutcome;
use hogmap::volume::{list_drives, volume_root_for};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "hogmap-scan-test-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn scan(path: PathBuf, options: ScanOptions) -> (ScanReducer, hogmap::scan_win::ScanReport) {
    let generation = ScanGeneration::new(710);
    let (sender, receiver) = sync_channel(2);
    let worker = thread::spawn(move || {
        run_win_scan_with_options(generation, path, sender, &ScanCancellation::new(), options)
    });
    let mut reducer = ScanReducer::new(generation);
    let mut terminals = 0;
    // This fixture is tiny. Receiving first also exercises backpressure without a GUI.
    let mut events = Vec::new();
    while let Ok(event) = receiver.recv_timeout(std::time::Duration::from_secs(10)) {
        assert_eq!(event.generation, generation);
        if event.is_terminal() {
            terminals += 1;
        }
        events.push(event);
    }
    let (outcome, report) = worker.join().unwrap();
    assert_eq!(outcome, ProducerOutcome::Finished);
    assert_eq!(terminals, 1);
    let (sender, receiver) = sync_channel(events.len().max(1));
    for event in events {
        sender.send(event).unwrap();
    }
    drop(sender);
    while reducer.terminal().is_none() {
        reducer.drain(&receiver, 4096);
    }
    (reducer, report)
}

#[test]
fn real_fixture_sizes_states_unicode_and_volumes() {
    let fixture = Fixture::new();
    let path = fixture.0.clone();
    std::fs::create_dir(path.join("nested")).unwrap();
    std::fs::create_dir(path.join("empty")).unwrap();
    std::fs::write(path.join("nested/known.bin"), b"12345678901").unwrap();
    std::fs::write(path.join("雪-😀.txt"), b"abcd").unwrap();
    let (reducer, report) = scan(path.clone(), ScanOptions::default());
    assert_eq!(reducer.terminal(), Some(Terminal::Finished));
    assert_eq!(reducer.diagnostics().invalid_records, 0);
    assert_eq!(reducer.diagnostics().unknown_parent_records, 0);
    assert_eq!(reducer.diagnostics().directory_failures, 0);
    assert_eq!(reducer.diagnostics().partial_directories, 0);
    let index = reducer.index().unwrap();
    assert_eq!(index.entries().count(), 5);
    let summary = index.summary().unwrap();
    assert_eq!(summary.files, 2);
    assert_eq!(summary.known_logical_bytes, 15);
    assert_eq!(summary.unique_logical_bytes, 15);
    assert!(summary.complete);
    assert!(
        index
            .entries()
            .all(|(_, entry)| entry.state == State::Complete)
    );
    assert!(index.entries().any(|(_, entry)| entry.name == "雪-😀.txt"));
    for (_, entry) in index
        .entries()
        .filter(|(_, entry)| entry.kind == Kind::File)
    {
        assert!(entry.identity.is_some());
        assert!(entry.sizes.allocated.is_some());
    }
    assert_eq!(report.fallback_directories, 0);
    let drives = list_drives().unwrap();
    assert!(
        drives
            .iter()
            .any(|drive| drive.total_bytes.is_some_and(|bytes| bytes > 0))
    );
    let root = volume_root_for(&path).expect("fixture volume root");
    assert!(root.ends_with('\\'));
    assert!(
        drives
            .iter()
            .any(|drive| drive.root.eq_ignore_ascii_case(&root)
                && drive.total_bytes.is_some_and(|bytes| bytes > 0))
    );
    drop(fixture);
    assert!(!path.exists(), "fixture must be removed");
}

#[test]
fn directory_batches_publish_every_parent_before_descendants() {
    use std::collections::{HashMap, HashSet};
    let fixture = Fixture::new();
    for position in 0..256 {
        let parent = fixture.0.join(format!("directory-{position}"));
        std::fs::create_dir_all(parent.join("nested")).unwrap();
        std::fs::write(parent.join("nested/leaf"), b"x").unwrap();
    }
    for workers in [1, 8] {
        let generation = ScanGeneration::new(720 + workers as u64);
        let (sender, receiver) = sync_channel(2);
        let path = fixture.0.clone();
        let worker = thread::spawn(move || {
            run_win_scan_with_options(
                generation,
                path,
                sender,
                &ScanCancellation::new(),
                ScanOptions {
                    workers,
                    ..ScanOptions::default()
                },
            )
        });
        let mut seen = HashSet::new();
        let mut children: HashMap<hogmap::scan::NodeKey, usize> = HashMap::new();
        let mut outcomes = HashSet::new();
        let mut largest_directory_batch = 0;
        let mut terminals = 0;
        while let Ok(event) = receiver.recv_timeout(std::time::Duration::from_secs(10)) {
            match event.event {
                ScanEventKind::Batch(batch) => {
                    largest_directory_batch = largest_directory_batch.max(
                        batch
                            .records()
                            .iter()
                            .filter(|record| record.kind == Kind::Directory)
                            .count(),
                    );
                    for record in batch.records() {
                        if let Some(parent) = record.parent {
                            assert!(seen.contains(&parent), "descendant preceded its parent");
                            assert!(
                                !outcomes.contains(&parent),
                                "record arrived after its directory's outcome"
                            );
                            *children.entry(parent).or_insert(0) += 1;
                        }
                        assert!(seen.insert(record.key));
                    }
                    for outcome in batch.outcomes() {
                        assert!(
                            seen.contains(&outcome.key),
                            "outcome preceded its own record"
                        );
                        assert!(outcomes.insert(outcome.key), "duplicate directory outcome");
                    }
                }
                ScanEventKind::DirectoryDone(key) | ScanEventKind::DirectoryFailed { key, .. } => {
                    assert!(seen.contains(&key), "outcome preceded its own record");
                    assert!(outcomes.insert(key), "duplicate directory outcome");
                }
                ScanEventKind::Terminal(terminal) => {
                    assert_eq!(terminal, Terminal::Finished);
                    terminals += 1;
                }
                ScanEventKind::Progress(_) => {}
            }
        }
        assert_eq!(worker.join().unwrap().0, ProducerOutcome::Finished);
        assert_eq!(terminals, 1);
        assert_eq!(seen.len(), 769);
        // Every directory outcome follows all records of that directory, including its children.
        for key in children.keys() {
            assert!(
                outcomes.contains(key),
                "directory outcome missing for {key:?}"
            );
        }
        assert_eq!(outcomes.len(), 513);
        if workers == 1 {
            assert!(largest_directory_batch > 1, "directory records must batch");
        }
    }
}

#[test]
fn real_hardlinks_charge_once_and_local_overflow_does_not_truncate() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("first"), b"1234567").unwrap();
    std::fs::hard_link(fixture.0.join("first"), fixture.0.join("second")).unwrap();
    for directory in ["a", "b", "c", "d", "e", "f"] {
        std::fs::create_dir(fixture.0.join(directory)).unwrap();
    }
    let (reducer, report) = scan(
        fixture.0.clone(),
        ScanOptions {
            workers: 1,
            queue_capacity: 1,
            local_stack_capacity: 16,
            ..ScanOptions::default()
        },
    );
    let summary = reducer.index().unwrap().summary().unwrap();
    assert_eq!(summary.files, 2);
    assert_eq!(summary.known_logical_bytes, 14);
    assert_eq!(summary.unique_logical_bytes, 7);
    assert_eq!(summary.shared_logical_bytes, 7);
    assert!(summary.complete);
    assert_eq!(report.local_stack_limit_hits, 0);
    assert_eq!(report.max_queue_depth, 1);
    assert!(report.max_local_stack_depth > 0);
}

#[test]
fn global_pending_memory_budget_fails_explicitly_without_losing_records() {
    let fixture = Fixture::new();
    for number in 0..64 {
        std::fs::create_dir(fixture.0.join(format!("directory-{number:04}"))).unwrap();
    }
    let (reducer, report) = scan(
        fixture.0.clone(),
        ScanOptions {
            workers: 1,
            queue_capacity: 1,
            local_stack_capacity: 1,
            max_pending_bytes: 1024,
        },
    );
    assert_eq!(reducer.index().unwrap().entries().count(), 65);
    assert_eq!(report.local_stack_limit_hits, 0);
    assert!(report.pending_memory_limit_hits > 0);
    assert!(report.peak_pending_bytes <= 1024);
    assert_eq!(
        reducer.terminal(),
        Some(Terminal::Failed(
            hogmap::scan_win::PENDING_MEMORY_LIMIT_CODE
        ))
    );
    assert!(!reducer.index().unwrap().summary().unwrap().complete);
}

#[test]
fn wide_tree_exceeds_ten_thousand_siblings_without_traversal_truncation() {
    let fixture = Fixture::new();
    for number in 0..10_001 {
        std::fs::create_dir(fixture.0.join(format!("directory-{number:05}"))).unwrap();
    }
    let (reducer, report) = scan(
        fixture.0.clone(),
        ScanOptions {
            workers: 2,
            ..ScanOptions::default()
        },
    );
    assert_eq!(reducer.index().unwrap().entries().count(), 10_002);
    assert!(reducer.index().unwrap().summary().unwrap().complete);
    assert_eq!(report.local_stack_limit_hits, 0);
    assert_eq!(report.pending_memory_limit_hits, 0);
    assert_eq!(reducer.diagnostics().unknown_parent_records, 0);
}

#[test]
fn cancellation_after_root_publication_emits_one_terminal() {
    let fixture = Fixture::new();
    for name in ["a", "b", "c"] {
        std::fs::create_dir(fixture.0.join(name)).unwrap();
    }
    let cancel = ScanCancellation::new();
    let generation = ScanGeneration::new(712);
    let (sender, receiver) = sync_channel(1);
    thread::scope(|scope| {
        let path = fixture.0.clone();
        let worker = scope.spawn(|| run_win_scan(generation, path, sender, &cancel));
        let root = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(root.event, ScanEventKind::Batch(_)));
        cancel.cancel();
        let mut terminals = Vec::new();
        while let Ok(event) = receiver.recv_timeout(std::time::Duration::from_secs(5)) {
            assert_eq!(event.generation, generation);
            if let ScanEventKind::Terminal(terminal) = event.event {
                terminals.push(terminal);
            }
        }
        assert_eq!(worker.join().unwrap(), ProducerOutcome::Cancelled);
        assert_eq!(terminals, vec![Terminal::Cancelled]);
    });
}

#[test]
fn missing_root_is_a_directory_failure_not_an_empty_success() {
    let fixture = Fixture::new();
    let (reducer, _) = scan(fixture.0.join("missing"), ScanOptions::default());
    assert_eq!(reducer.diagnostics().directory_failures, 1);
    assert!(matches!(
        reducer.diagnostics().last_directory_failure_code,
        Some(2 | 3)
    ));
    assert_eq!(
        reducer
            .index()
            .unwrap()
            .get(reducer.index().unwrap().root())
            .unwrap()
            .state,
        State::Inaccessible
    );
    assert!(!reducer.index().unwrap().summary().unwrap().complete);
}

#[test]
fn cancellation_and_disconnect_finish_without_gui() {
    let fixture = Fixture::new();
    let generation = ScanGeneration::new(711);
    let (sender, receiver) = sync_channel(1);
    let cancelled = ScanCancellation::new();
    cancelled.cancel();
    assert_eq!(
        run_win_scan(generation, fixture.0.clone(), sender, &cancelled),
        ProducerOutcome::Cancelled
    );
    assert!(matches!(
        receiver.recv().unwrap().event,
        ScanEventKind::Terminal(Terminal::Cancelled)
    ));
    assert!(receiver.recv().is_err());
    let (sender, receiver) = sync_channel(1);
    drop(receiver);
    assert_eq!(
        run_win_scan(
            generation,
            fixture.0.clone(),
            sender,
            &ScanCancellation::new()
        ),
        ProducerOutcome::Disconnected
    );
}
