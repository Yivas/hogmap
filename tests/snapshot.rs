use hogmap::composition::VolumeEvidence;
use hogmap::filter::{Filter, Match, Metadata};
use hogmap::index::{FileIdentity, Index, Kind, SizeMetric, Sizes, State};
use hogmap::scan::Terminal;
use hogmap::snapshot::{self, Error, Limit, Save};
use hogmap::treemap::{self, LayoutParams, Rect};
use hogmap::volume::*;
use std::io::Cursor;
use std::time::{Duration, UNIX_EPOCH};

fn save<'a>(index: &'a Index, evidence: Option<&'a VolumeEvidence>) -> Save<'a> {
    Save {
        root_path: Some(std::path::Path::new(r"X:\fixture")),
        metric: SizeMetric::Allocated,
        terminal: Terminal::Cancelled,
        index,
        evidence,
        header: None,
    }
}
fn encoded(index: &Index) -> Vec<u8> {
    let mut bytes = Vec::new();
    snapshot::encode(&mut bytes, &save(index, None)).unwrap();
    bytes
}

#[test]
fn snapshot_round_trip_preserves_metadata_hardlinks_corrections_tags_and_navigation() {
    let mut index = Index::new("Fixture".into()).unwrap();
    let root = index.root();
    let folder = index
        .insert(
            root,
            "Folder".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Partial,
        )
        .unwrap();
    let identity = FileIdentity {
        volume: 7,
        file: (1_u128 << 100) + 9,
    };
    let a = index
        .insert(
            folder,
            "a.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(100),
                allocated: Some(4096),
            },
            Some(identity),
            State::Complete,
        )
        .unwrap();
    let b = index
        .insert(
            root,
            "b.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(200),
                allocated: Some(8192),
            },
            Some(identity),
            State::Stale,
        )
        .unwrap();
    let unknown = index
        .insert(
            folder,
            "unknown.bin".into(),
            Kind::File,
            Sizes {
                logical: None,
                allocated: Some(0),
            },
            None,
            State::Pending,
        )
        .unwrap();
    index.set_state(root, State::Complete).unwrap();
    index.set_tags(folder, 1).unwrap();
    index.set_tags(a, 0).unwrap();
    index.set_tags(b, 4).unwrap();
    index
        .set_filter_metadata(
            a,
            Metadata {
                created: 123,
                modified: 456,
                accessed: 789,
                attributes: Some(2),
                ads: Some(false),
            },
        )
        .unwrap();
    index
        .apply_authoritative_sizes(
            identity,
            Sizes {
                logical: Some(300),
                allocated: Some(16384),
            },
        )
        .unwrap();
    let metadata = CapacityMetadata {
        source: CapacitySource::WindowsActualGlobalFilesystem,
        captured_at: UNIX_EPOCH + Duration::new(100, 12),
        issue: None,
    };
    let sample = OriginCapacitySample {
        total_bytes: Some(1_000_000),
        free_bytes: Some(0),
        storage_reserve_bytes: None,
        total_reserved_bytes: Some(99),
        reserve_issue: Some(CapacityIssue::SizeOverflow),
        metadata,
    };
    let evidence = VolumeEvidence {
        scope: Ok(RootScope::Folder),
        filesystem: Ok(ObservedFilesystem::Other("fixture-fs".into())),
        before: Ok(sample),
        after: Err(OriginIssue::QueryFailed {
            query: OriginQuery::Filesystem,
            code: 5,
        }),
        validation: Ok(OriginValidation {
            directory_identity: Ok(ObservedContinuity::Changed),
            normalized_path: Err(OriginIssue::NamespaceChanged),
        }),
        directories: DirectoryNamespaceObservations {
            opened_directories: 8,
            serial64_same: 2,
            serial64_different: 3,
            serial64_unavailable: 3,
            guid_on_serial_mismatch: 3,
            guid_on_serial_unavailable: 3,
            same_guid_name: 1,
            different_guid_name_unproven: 2,
            unavailable: 3,
            not_queried: 4,
            last_issue: Some(OriginIssue::Cancelled),
        },
    };
    let header = snapshot::Header {
        root: r"X:\".into(),
        label: "Fixture volume".into(),
        free_bytes: Some(0),
        total_bytes: Some(1000),
        storage_reserve_bytes: None,
        capacity: Some(metadata),
    };
    let mut input = save(&index, Some(&evidence));
    input.header = Some(&header);
    let mut bytes = Vec::new();
    snapshot::encode(&mut bytes, &input).unwrap();
    let loaded = snapshot::decode(Cursor::new(&bytes)).unwrap();
    assert_eq!(loaded.root_path, Some(r"X:\fixture".into()));
    assert_eq!(loaded.metric, SizeMetric::Allocated);
    assert_eq!(loaded.terminal, Terminal::Cancelled);
    assert_eq!(loaded.index.len(), 5);
    for ((id, expected), (actual_id, actual)) in index.entries().zip(loaded.index.entries()) {
        assert_eq!(id, actual_id);
        assert_eq!(
            (
                actual.parent,
                actual.name,
                actual.kind,
                actual.sizes,
                actual.identity,
                actual.state
            ),
            (
                expected.parent,
                expected.name,
                expected.kind,
                expected.sizes,
                expected.identity,
                expected.state
            )
        );
        assert_eq!(loaded.index.filter_metadata(id), index.filter_metadata(id));
        assert_eq!(loaded.index.tags(id), index.tags(id));
    }
    assert_eq!(loaded.index.tags(unknown), Some(1));
    assert_eq!(loaded.index.tags(a), Some(0));
    assert_eq!(
        loaded.index.filter_metadata(a),
        Metadata {
            created: 123,
            modified: 456,
            accessed: 789,
            attributes: Some(2),
            ads: Some(false)
        }
    );
    for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
        assert_eq!(loaded.index.summary_for(metric), index.summary_for(metric));
        let params = LayoutParams {
            min_pixel_area: 1.0,
            header_height: 0.0,
            max_depth: 64,
        };
        assert_eq!(
            treemap::layout_with_metric(
                &loaded.index,
                folder,
                Rect::new(0.0, 0.0, 100.0, 100.0),
                params,
                metric
            ),
            treemap::layout_with_metric(
                &index,
                folder,
                Rect::new(0.0, 0.0, 100.0, 100.0),
                params,
                metric
            )
        );
    }
    assert_eq!(
        Filter::parse(":red")
            .unwrap()
            .matches(&loaded.index)
            .matches,
        vec![Match::Yes, Match::Yes, Match::No, Match::No, Match::Yes]
    );
    assert_eq!(
        Filter::parse(":attr:hidden")
            .unwrap()
            .evaluate_entry(&loaded.index, a),
        Match::Yes
    );
    let actual = loaded.evidence.as_ref().unwrap();
    assert_eq!(actual.scope, Ok(RootScope::Folder));
    assert_eq!(
        actual.filesystem,
        Ok(ObservedFilesystem::Other("fixture-fs".into()))
    );
    let before = actual.before.unwrap();
    assert_eq!(
        (
            before.total_bytes,
            before.free_bytes,
            before.storage_reserve_bytes,
            before.total_reserved_bytes,
            before.reserve_issue,
            before.metadata
        ),
        (
            Some(1_000_000),
            Some(0),
            None,
            Some(99),
            Some(CapacityIssue::SizeOverflow),
            metadata
        )
    );
    assert_eq!(
        actual.after.unwrap_err(),
        OriginIssue::QueryFailed {
            query: OriginQuery::Filesystem,
            code: 5
        }
    );
    assert_eq!(
        actual.validation.unwrap().directory_identity,
        Ok(ObservedContinuity::Changed)
    );
    assert_eq!(
        actual.validation.unwrap().normalized_path,
        Err(OriginIssue::NamespaceChanged)
    );
    let d = actual.directories;
    assert_eq!(
        [
            d.opened_directories,
            d.serial64_same,
            d.serial64_different,
            d.serial64_unavailable,
            d.guid_on_serial_mismatch,
            d.guid_on_serial_unavailable,
            d.same_guid_name,
            d.different_guid_name_unproven,
            d.unavailable,
            d.not_queried
        ],
        [8, 2, 3, 3, 3, 3, 1, 2, 3, 4]
    );
    assert_eq!(d.last_issue, Some(OriginIssue::Cancelled));
    let h = loaded.header.as_ref().unwrap();
    assert_eq!(
        (
            &h.root,
            h.label.as_str(),
            h.free_bytes,
            h.total_bytes,
            h.storage_reserve_bytes,
            h.capacity
        ),
        (
            &header.root,
            "Fixture volume",
            Some(0),
            Some(1000),
            None,
            Some(metadata)
        )
    );
    let mut second = Vec::new();
    snapshot::encode(
        &mut second,
        &Save {
            root_path: loaded.root_path.as_deref(),
            metric: loaded.metric,
            terminal: loaded.terminal,
            index: &loaded.index,
            evidence: loaded.evidence.as_ref(),
            header: loaded.header.as_ref(),
        },
    )
    .unwrap();
    assert_eq!(
        second, bytes,
        "explicit zero tag assignments and canonical corrections survive too"
    );
}

// Independent hand-built v1 bytes: no optional origin/path/header, one Complete folder R.
fn minimal(count: u32) -> Vec<u8> {
    let mut bytes = b"HOGMAP\0\0\x01\0\0\0\0\0\0\0\0".to_vec();
    bytes.extend(count.to_le_bytes());
    for position in 0..count {
        bytes.extend(
            if position == 0 {
                u32::MAX
            } else {
                position - 1
            }
            .to_le_bytes(),
        );
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend([b'R', 0, 0, 0, 0, 1, 0]);
        bytes.extend([0; 24]);
        bytes.extend([0; 3]);
    }
    bytes.extend(0_u32.to_le_bytes());
    bytes
}
fn error(bytes: Vec<u8>) -> Error {
    std::panic::catch_unwind(|| snapshot::decode(Cursor::new(bytes)))
        .expect("untrusted input must not panic")
        .unwrap_err()
}
#[test]
fn untrusted_snapshot_rejects_wrong_format_versions_counts_names_parents_and_every_truncation() {
    let valid = minimal(1);
    assert_eq!(valid.len(), 67);
    assert_eq!(
        snapshot::decode(Cursor::new(&valid)).unwrap().index.len(),
        1
    );
    let mut bad = valid.clone();
    bad[0] = b'X';
    assert!(matches!(error(bad), Error::WrongFormat));
    let mut bad = valid.clone();
    bad[8..12].copy_from_slice(&2_u32.to_le_bytes());
    assert!(matches!(error(bad), Error::Version(2)));
    let mut bad = valid.clone();
    bad[17..21].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(error(bad), Error::TooLarge(Limit::Entries)));
    let mut bad = valid.clone();
    bad[25..29].copy_from_slice(&256_u32.to_le_bytes());
    assert!(matches!(error(bad), Error::TooLarge(Limit::Component)));
    let mut bad = minimal(2);
    bad[63..67].copy_from_slice(&1_u32.to_le_bytes());
    assert!(matches!(error(bad), Error::InvalidParent), "self-cycle");
    let mut bad = minimal(2);
    bad[63..67].copy_from_slice(&9_u32.to_le_bytes());
    assert!(
        matches!(error(bad), Error::InvalidParent),
        "dangling parent"
    );
    let mut bad = minimal(3);
    bad[63..67].copy_from_slice(&2_u32.to_le_bytes());
    assert!(matches!(error(bad), Error::InvalidParent), "forward parent");
    let mut bad = minimal(2);
    bad[31] = 1;
    assert!(matches!(error(bad), Error::InvalidRecord), "file root");
    let mut bad = valid.clone();
    bad[34] = 255;
    assert!(matches!(error(bad), Error::InvalidRecord));
    let mut bad = valid.clone();
    bad[32] = 2;
    assert!(
        matches!(error(bad), Error::InvalidRecord),
        "noncanonical option"
    );
    let mut bad = valid.clone();
    bad.push(0);
    assert!(matches!(error(bad), Error::TrailingData));
    for end in 0..valid.len() {
        assert!(
            matches!(error(valid[..end].to_vec()), Error::Truncated),
            "prefix {end}"
        );
    }
    assert!(matches!(error(minimal(514)), Error::TooLarge(Limit::Depth)));
    // A small deterministic malformed corpus exercises the public parser without external fuzzers.
    for position in 0..valid.len() {
        for value in [0, 1, 127, 255] {
            let mut bytes = valid.clone();
            bytes[position] = value;
            let _ = std::panic::catch_unwind(|| snapshot::decode(Cursor::new(bytes)))
                .expect("mutated input must not panic");
        }
    }
}

#[test]
fn failed_save_keeps_the_existing_destination_and_cancel_has_no_writer() {
    let path =
        std::env::temp_dir().join(format!("hogmap-snapshot-write-{}.txt", std::process::id()));
    let owned = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    drop(owned);
    std::fs::write(&path, b"original").unwrap();
    let result = snapshot::write_file(&path, |_| {
        Err(Error::Write(std::io::ErrorKind::StorageFull))
    });
    assert!(matches!(
        result,
        Err(Error::Write(std::io::ErrorKind::StorageFull))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    snapshot::write_file(&path, |w| {
        use std::io::Write;
        w.write_all(b"complete").map_err(|e| Error::Write(e.kind()))
    })
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"complete");
    std::fs::remove_file(path).unwrap();
    let index = Index::new("Fixture".into()).unwrap();
    struct Fails;
    impl std::io::Write for Fails {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(matches!(
        snapshot::encode(Fails, &save(&index, None)),
        Err(Error::Write(std::io::ErrorKind::PermissionDenied))
    ));
}

#[cfg(windows)]
#[test]
fn snapshot_preserves_lone_utf16_surrogates_without_lossy_path_conversion() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let mut index = Index::new("Fixture".into()).unwrap();
    let id = index
        .insert(
            index.root(),
            std::ffi::OsString::from_wide(&[65, 0xD800, 66]),
            Kind::File,
            Sizes::default(),
            None,
            State::Inaccessible,
        )
        .unwrap();
    let loaded = snapshot::decode(Cursor::new(encoded(&index))).unwrap();
    assert_eq!(
        loaded
            .index
            .get(id)
            .unwrap()
            .name
            .encode_wide()
            .collect::<Vec<_>>(),
        vec![65, 0xD800, 66]
    );
}
