use std::sync::mpsc::{TrySendError, channel};
use std::thread;
use std::time::Duration;

use hogmap::index::{Kind, Sizes, State};
use hogmap::scan::{
    Batch, DirectoryOutcome, NodeKey, ScanCancellation, ScanEvent, ScanEventKind, ScanGeneration,
    ScanRecord, ScanReducer, SendOutcome, Terminal, event_channel, send_event,
};
use hogmap::scan_win::worker_count_for;

fn event() -> ScanEvent {
    ScanEvent {
        generation: ScanGeneration::new(1),
        event: ScanEventKind::Terminal(Terminal::Cancelled),
    }
}

#[test]
fn cancellation_wakes_a_sender_without_consumer_progress() {
    let (sender, receiver) = event_channel(1);
    sender.try_send(event()).unwrap();
    assert!(matches!(
        sender.try_send(event()),
        Err(TrySendError::Full(_))
    ));
    let cancel = ScanCancellation::new();
    let flag = cancel.clone();
    let (ready, started) = channel();
    let (done, outcome) = channel();
    let worker = thread::spawn(move || {
        ready.send(()).unwrap();
        done.send(send_event(&sender, event(), &flag)).unwrap();
    });
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    // No drain occurs: cancellation must release a full-channel send itself.
    assert!(outcome.recv_timeout(Duration::from_millis(30)).is_err());
    cancel.cancel();
    assert_eq!(
        outcome.recv_timeout(Duration::from_secs(1)).unwrap(),
        SendOutcome::Cancelled
    );
    worker.join().unwrap();
    assert!(receiver.recv().unwrap().is_terminal());
    assert!(receiver.try_recv().is_err());
}

#[test]
fn receiver_disconnect_wakes_a_blocked_sender() {
    let (sender, receiver) = event_channel(1);
    sender.try_send(event()).unwrap();
    let (done, outcome) = channel();
    let worker = thread::spawn(move || {
        done.send(send_event(&sender, event(), &ScanCancellation::new()))
            .unwrap()
    });
    assert!(outcome.recv_timeout(Duration::from_millis(30)).is_err());
    drop(receiver);
    assert_eq!(
        outcome.recv_timeout(Duration::from_secs(1)).unwrap(),
        SendOutcome::Disconnected
    );
    worker.join().unwrap();
}

#[test]
fn batched_outcomes_apply_after_records_and_keep_generation_checks() {
    let generation = ScanGeneration::new(1);
    let mut batch = Batch::new();
    for (key, parent, name, kind) in [
        (0, None, "Root", Kind::Directory),
        (1, Some(0), "child", Kind::Directory),
        (2, Some(1), "file", Kind::File),
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
    assert!(batch.push_outcome(DirectoryOutcome {
        key: NodeKey::new(1),
        code: Some(5)
    }));
    assert!(batch.push_outcome(DirectoryOutcome {
        key: NodeKey::new(0),
        code: None
    }));
    let (sender, receiver) = event_channel(3);
    sender
        .send(ScanEvent {
            generation: ScanGeneration::new(2),
            event: ScanEventKind::Batch(batch.clone()),
        })
        .unwrap();
    sender
        .send(ScanEvent {
            generation,
            event: ScanEventKind::Batch(batch),
        })
        .unwrap();
    sender
        .send(ScanEvent {
            generation,
            event: ScanEventKind::Terminal(Terminal::Finished),
        })
        .unwrap();
    let mut reducer = ScanReducer::new(generation);
    reducer.drain(&receiver, 4096);
    assert_eq!(reducer.diagnostics().stale_generation_events, 1);
    assert_eq!(reducer.diagnostics().unknown_directory_events, 0);
    assert_eq!(reducer.diagnostics().partial_directories, 0);
    assert_eq!(reducer.diagnostics().directory_failures, 1);
    let index = reducer.index().unwrap();
    assert_eq!(index.entries().count(), 3);
    assert_eq!(index.get(index.root()).unwrap().state, State::Complete);
}

#[test]
fn storage_policy_is_conservative_when_unknown_and_respects_available_workers() {
    assert_eq!(worker_count_for(None, 8), 1);
    assert_eq!(worker_count_for(Some(true), 8), 1);
    assert_eq!(worker_count_for(Some(false), 8), 4);
    assert_eq!(worker_count_for(Some(false), 2), 2);
    assert_eq!(worker_count_for(Some(false), 0), 1);
}
