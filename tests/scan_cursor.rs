use hogmap::index::{Kind, Sizes, State};
use hogmap::scan::{
    Batch, DirectoryOutcome, NodeKey, ScanEvent, ScanEventKind, ScanGeneration, ScanRecord,
    ScanReducer, Terminal, event_channel,
};
use std::time::Duration;

#[test]
fn budgeted_batch_resumes_before_outcomes_and_terminal_without_reordering() {
    let generation = ScanGeneration::new(68);
    let (sender, receiver) = event_channel(8);
    let mut batch = Batch::new();
    for (key, parent, name, kind) in [
        (1, None, "Synthetic", Kind::Directory),
        (2, Some(1), "folder", Kind::Directory),
    ] {
        assert!(batch.push(ScanRecord {
            key: NodeKey::new(key),
            parent: parent.map(NodeKey::new),
            name: name.into(),
            kind,
            sizes: Sizes::default(),
            identity: None
        }));
    }
    for number in 0..40 {
        assert!(batch.push(ScanRecord {
            key: NodeKey::new(number + 3),
            parent: Some(NodeKey::new(2)),
            name: format!("file-{number}").into(),
            kind: Kind::File,
            sizes: Sizes {
                logical: Some(1),
                allocated: Some(2)
            },
            identity: None
        }));
    }
    assert!(batch.push_outcome(DirectoryOutcome {
        key: NodeKey::new(2),
        code: Some(5)
    }));
    assert!(batch.push_outcome(DirectoryOutcome {
        key: NodeKey::new(1),
        code: None
    }));
    for (generation, event) in [
        (generation, ScanEventKind::Batch(batch)),
        (
            ScanGeneration::new(67),
            ScanEventKind::Terminal(Terminal::Failed(7)),
        ),
        (generation, ScanEventKind::Terminal(Terminal::Finished)),
        (generation, ScanEventKind::Terminal(Terminal::Failed(9))),
    ] {
        sender.send(ScanEvent { generation, event }).unwrap();
    }
    let mut reducer = ScanReducer::new(generation);
    let first = reducer.drain_budgeted(&receiver, usize::MAX, Duration::from_nanos(1));
    assert_eq!(
        (first.records, first.directories, first.terminal),
        (32, 2, None)
    );
    let index = reducer.index().unwrap();
    assert_eq!(index.len(), 32);
    assert_eq!(index.summary().unwrap().known_logical_bytes, 30);
    assert_eq!(index.get(index.root()).unwrap().state, State::Pending);
    assert_eq!(reducer.diagnostics().directory_failures, 0);
    for _ in 0..8 {
        if reducer.terminal().is_some() {
            break;
        }
        reducer.drain_budgeted(&receiver, usize::MAX, Duration::from_nanos(1));
    }
    assert_eq!(reducer.terminal(), Some(Terminal::Finished));
    let index = reducer.index().unwrap();
    assert_eq!(
        (
            index.len(),
            index.summary().unwrap().files,
            index.summary().unwrap().known_logical_bytes
        ),
        (42, 40, 40)
    );
    assert!(!index.summary().unwrap().complete);
    assert_eq!(index.get(index.root()).unwrap().state, State::Complete);
    assert_eq!(reducer.diagnostics().directory_failures, 1);
    assert_eq!(reducer.diagnostics().last_directory_failure_code, Some(5));
    assert_eq!(reducer.diagnostics().partial_directories, 0);
    assert_eq!(reducer.diagnostics().unknown_parent_records, 0);
    assert_eq!(reducer.diagnostics().stale_generation_events, 1);
    reducer.drain(&receiver, 4096);
    assert_eq!(reducer.terminal(), Some(Terminal::Finished));
    assert_eq!(reducer.diagnostics().events_after_terminal, 1);
}
