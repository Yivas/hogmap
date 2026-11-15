use hogmap::index::{Kind, Sizes};
use hogmap::scan::*;
use hogmap::{bookmarks, notices::Notices};

#[test]
fn accepted_batch_progress_and_failure_codes_reach_the_notice_console() {
    let generation = ScanGeneration::new(80);
    let (sender, receiver) = event_channel(4);
    let mut batch = Batch::new();
    assert!(batch.push(ScanRecord {
        key: NodeKey::new(0),
        parent: None,
        name: "Fixture".into(),
        kind: Kind::Directory,
        sizes: Sizes::default(),
        identity: None
    }));
    assert!(batch.push(ScanRecord {
        key: NodeKey::new(1),
        parent: Some(NodeKey::new(0)),
        name: "denied".into(),
        kind: Kind::Directory,
        sizes: Sizes::default(),
        identity: None
    }));
    assert!(batch.push_outcome(DirectoryOutcome {
        key: NodeKey::new(0),
        code: None
    }));
    assert!(batch.push_outcome(DirectoryOutcome {
        key: NodeKey::new(1),
        code: Some(5)
    }));
    sender
        .send(ScanEvent {
            generation,
            event: ScanEventKind::Batch(batch),
        })
        .unwrap();
    assert!(send_terminal(&sender, generation, Terminal::Finished));
    let mut reducer = ScanReducer::new(generation);
    reducer.drain(&receiver, 100);
    assert_eq!(
        reducer.observed_progress(),
        Progress {
            records: 2,
            directories_done: 1,
            directories_failed: 1
        }
    );
    let mut notices = Notices::default();
    for code in reducer.take_directory_failures() {
        notices.directory_failure(code);
    }
    assert!(notices.entries().next().unwrap().detail.contains("code 5"));
    assert_eq!(reducer.take_directory_failures().count(), 0);
}

#[test]
fn bookmark_file_round_trip_corruption_future_version_and_no_unrelated_settings() {
    let directory =
        std::env::temp_dir().join(format!("hogmap-bookmark-fixture-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join(bookmarks::FILE_NAME);
    let entries = vec![bookmarks::Bookmark {
        name: "Chosen folder".into(),
        root: Some(directory.join("synthetic-root")),
        path: vec!["folder".into()],
        members: Some(vec!["chosen.bin".into()]),
    }];
    assert!(bookmarks::load(&path).unwrap().is_empty());
    bookmarks::save(&path, &entries).unwrap();
    assert_eq!(bookmarks::load(&path).unwrap(), entries);
    bookmarks::save(&path, &[]).unwrap();
    assert!(bookmarks::load(&path).unwrap().is_empty());
    std::fs::write(&path, b"corrupt").unwrap();
    assert_eq!(bookmarks::load(&path), Err(bookmarks::Error::Invalid));
    std::fs::write(&path, b"{\"version\":99,\"bookmarks\":[]}").unwrap();
    assert_eq!(bookmarks::load(&path), Err(bookmarks::Error::FutureVersion));
    assert_eq!(
        std::fs::read_dir(&directory).unwrap().count(),
        1,
        "no preference, log or other setting file is written"
    );
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
