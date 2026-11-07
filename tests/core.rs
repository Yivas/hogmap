use std::ffi::OsStr;

use hogmap::filter::{Filter, FilterError, MAX_CONDITIONS, MAX_EXPRESSION_BYTES, Match};
use hogmap::index::{EntryId, FileIdentity, Index, IndexError, Kind, SizeMetric, Sizes, State};

#[test]
fn filters_follow_the_documented_name_size_and_combination_rules() {
    for (expression, name, bytes, expected) in [
        ("", "notes.txt", None, Match::Yes),
        ("*.jpg", "PHOTO.JPG", None, Match::Yes),
        ("*.jpg", "notes.txt", Some(200), Match::No),
        ("|*.jpg", "PHOTO.JPG", None, Match::No),
        ("|*.jpg", "notes.txt", None, Match::Yes),
        (">100mb", "a.bin", Some(104_857_600), Match::No),
        (">100mb", "a.bin", Some(104_857_601), Match::Yes),
        ("*.jpg;>1mb", "photo.jpg", Some(1_048_577), Match::Yes),
        ("*.jpg;>1mb", "notes.txt", Some(1_048_577), Match::No),
        ("*.jpg;>1mb", "photo.jpg", Some(1_048_576), Match::No),
        (
            "*.jpg;*.png;>1mb;|draft*",
            "draft.png",
            Some(2_000_000),
            Match::No,
        ),
        (
            "*.jpg;*.png;>1mb;|draft*",
            "photo.png",
            Some(2_000_000),
            Match::Yes,
        ),
        (" >1KB ; <2kb ", "a.bin", Some(1500), Match::Yes),
        ("*.jpg;>1mb", "photo.jpg", None, Match::Unknown),
        ("*.jpg;>1mb", "notes.txt", None, Match::No),
        ("?a*ab", "éa123ab", Some(0), Match::Yes),
        ("*a*b", "aacab", Some(0), Match::Yes),
        ("é.txt", "É.txt", Some(0), Match::No),
        ("é.txt", "é.txt", Some(0), Match::Yes),
    ] {
        let filter = Filter::parse(expression).unwrap();
        assert_eq!(
            filter.evaluate(OsStr::new(name), bytes),
            expected,
            "{expression:?}, {name}"
        );
    }
}

#[test]
fn invalid_or_excessive_filters_are_rejected_before_use() {
    for expression in [
        "*.jpg;",
        ";*.jpg",
        "*.jpg;;>1mb",
        "|",
        "||*.jpg",
        "\\temp",
        "a/b",
        ":red",
        ">1year",
        ">1 mb",
        ">-1mb",
        ">1.5mb",
        ">1",
        ">=1mb",
        ">18446744073709551616b",
        ">18446744073709551615tb",
        "photo\0.jpg",
    ] {
        assert!(Filter::parse(expression).is_err(), "{expression:?}");
    }
    assert!(matches!(
        Filter::parse(&"a".repeat(MAX_EXPRESSION_BYTES + 1)),
        Err(FilterError::TooLong)
    ));
    let conditions = vec!["*"; MAX_CONDITIONS + 1].join(";");
    assert!(matches!(
        Filter::parse(&conditions),
        Err(FilterError::TooManyConditions)
    ));
}

#[test]
fn index_keeps_unknowns_and_charges_hardlinks_once_without_mutating_totals_when_filtering() {
    let mut index = Index::new("Synthetic volume".into()).unwrap();
    let root = index.root();
    let folder = index
        .insert(
            root,
            "Photos".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let identity = Some(FileIdentity {
        volume: 1,
        file: 42,
    });
    for name in ["one.jpg", "link.jpg"] {
        index
            .insert(
                folder,
                name.into(),
                Kind::File,
                Sizes {
                    logical: Some(12),
                    allocated: Some(4096),
                },
                identity,
                State::Complete,
            )
            .unwrap();
    }
    index
        .insert(
            root,
            "empty.txt".into(),
            Kind::File,
            Sizes {
                logical: Some(0),
                allocated: Some(0),
            },
            None,
            State::Complete,
        )
        .unwrap();
    index
        .insert(
            root,
            "unknown.bin".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Inaccessible,
        )
        .unwrap();
    index.set_state(root, State::Complete).unwrap();
    let before = index.summary().unwrap();
    assert_eq!(before.files, 4);
    assert_eq!(before.known_logical_bytes, 24);
    assert_eq!(before.unique_logical_bytes, 12);
    assert_eq!(before.shared_logical_bytes, 12);
    assert_eq!(before.unknown_size_files, 1);
    assert!(!before.complete);
    let filter = Filter::parse("*.jpg").unwrap();
    let matched: Vec<_> = index
        .entries()
        .filter(|(_, entry)| {
            entry.kind == Kind::File
                && filter.evaluate(entry.name, entry.sizes.logical) == Match::Yes
        })
        .collect();
    assert_eq!(matched.len(), 2);
    assert_eq!(index.summary().unwrap(), before);
    assert_eq!(index.get(folder).unwrap().parent, Some(root));
}

#[test]
fn summary_does_not_deduplicate_files_without_identity() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    for name in ["a.bin", "b.bin"] {
        index
            .insert(
                root,
                name.into(),
                Kind::File,
                Sizes {
                    logical: Some(7),
                    allocated: None,
                },
                None,
                State::Complete,
            )
            .unwrap();
    }
    index.set_state(root, State::Complete).unwrap();
    let summary = index.summary().unwrap();
    assert_eq!(summary.known_logical_bytes, 14);
    assert_eq!(summary.unique_logical_bytes, 14);
    assert_eq!(summary.shared_logical_bytes, 0);
}

#[test]
fn index_rejects_invalid_hierarchy_and_overflow_without_hiding_errors() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    let file = index
        .insert(
            root,
            "large.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(u64::MAX),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    assert_eq!(
        index.insert(
            file,
            "child".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Pending
        ),
        Err(IndexError::NotDirectory)
    );
    for name in ["", ".", "..", "a/b", "a\\b", "C:", "nul\0name"] {
        assert_eq!(
            index.insert(
                root,
                name.into(),
                Kind::File,
                Sizes::default(),
                None,
                State::Pending
            ),
            Err(IndexError::InvalidName)
        );
    }
    assert_eq!(index.entries().count(), 2);
    index
        .insert(
            root,
            "another.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(1),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    assert_eq!(index.summary(), Err(IndexError::SizeOverflow));
}

#[test]
fn summary_totals_follow_inserts_and_state_changes() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    let folder = index
        .insert(
            root,
            "Folder".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let identity = Some(FileIdentity { volume: 1, file: 7 });
    for name in ["first.bin", "link.bin"] {
        index
            .insert(
                folder,
                name.into(),
                Kind::File,
                Sizes {
                    logical: Some(100),
                    allocated: None,
                },
                identity,
                State::Complete,
            )
            .unwrap();
    }
    index
        .insert(
            root,
            "unknown.bin".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    index
        .insert(
            root,
            "empty.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(0),
                allocated: Some(0),
            },
            None,
            State::Complete,
        )
        .unwrap();

    let summary = index.summary().unwrap();
    assert_eq!(summary.files, 4);
    assert_eq!(summary.known_logical_bytes, 200);
    assert_eq!(summary.unique_logical_bytes, 100);
    assert_eq!(summary.shared_logical_bytes, 100);
    assert_eq!(summary.unknown_size_files, 1);
    assert!(!summary.complete);
    assert_eq!(
        summary.known_logical_bytes,
        summary.unique_logical_bytes + summary.shared_logical_bytes
    );

    index.set_state(root, State::Complete).unwrap();
    assert!(
        !index.summary().unwrap().complete,
        "an unknown size also clears completeness"
    );

    let mut complete_index = Index::new("Synthetic".into()).unwrap();
    let root = complete_index.root();
    complete_index
        .insert(
            root,
            "only.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(1),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    assert!(!complete_index.summary().unwrap().complete);
    complete_index.set_state(root, State::Complete).unwrap();
    assert!(complete_index.summary().unwrap().complete);
    complete_index.set_state(root, State::Partial).unwrap();
    assert!(!complete_index.summary().unwrap().complete);
    complete_index.set_state(root, State::Complete).unwrap();
    assert!(complete_index.summary().unwrap().complete);
}

#[test]
fn a_later_link_is_charged_when_the_first_link_size_is_unknown() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    let identity = Some(FileIdentity { volume: 1, file: 9 });
    index
        .insert(
            root,
            "first.bin".into(),
            Kind::File,
            Sizes::default(),
            identity,
            State::Complete,
        )
        .unwrap();
    index
        .insert(
            root,
            "second.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(40),
                allocated: None,
            },
            identity,
            State::Complete,
        )
        .unwrap();
    index.set_state(root, State::Complete).unwrap();

    let summary = index.summary().unwrap();
    assert_eq!(summary.files, 2);
    assert_eq!(summary.known_logical_bytes, 40);
    assert_eq!(summary.unique_logical_bytes, 40);
    assert_eq!(summary.shared_logical_bytes, 0);
    assert_eq!(summary.unknown_size_files, 1);
    assert!(!summary.complete);
}

#[test]
fn names_are_limited_to_255_utf16_units() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    let ascii_at_limit = index
        .insert(
            root,
            "a".repeat(255).into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete,
        )
        .is_ok();
    assert!(ascii_at_limit);
    for name in ["a".repeat(256), "😀".repeat(128)] {
        assert_eq!(
            index.insert(
                root,
                name.into(),
                Kind::File,
                Sizes::default(),
                None,
                State::Complete
            ),
            Err(IndexError::InvalidName)
        );
    }
    let astral_at_limit = index
        .insert(
            root,
            format!("{}a", "😀".repeat(127)).into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete,
        )
        .is_ok();
    assert!(astral_at_limit);
}

#[test]
fn compact_metadata_preserves_max_sizes_identity_bits_and_all_states() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let identity = FileIdentity {
        volume: u64::MAX,
        file: u128::MAX,
    };
    for (number, state) in [
        State::Pending,
        State::Complete,
        State::Partial,
        State::Inaccessible,
        State::Stale,
    ]
    .into_iter()
    .enumerate()
    {
        let sizes = Sizes {
            logical: Some(0),
            allocated: Some(u64::MAX),
        };
        let id = index
            .insert(
                index.root(),
                format!("file-{number}").into(),
                Kind::File,
                sizes,
                Some(identity),
                state,
            )
            .unwrap();
        let entry = index.get(id).unwrap();
        assert_eq!(entry.sizes, sizes);
        assert_eq!(entry.identity, Some(identity));
        assert_eq!(entry.kind, Kind::File);
        assert_eq!(entry.state, state);
        index.set_state(id, State::Complete).unwrap();
        assert_eq!(index.get(id).unwrap().state, State::Complete);
        assert_eq!(index.get(id).unwrap().sizes.allocated, Some(u64::MAX));
    }
    let id = index
        .insert(
            index.root(),
            "max-logical".into(),
            Kind::File,
            Sizes {
                logical: Some(u64::MAX),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    assert_eq!(index.get(id).unwrap().sizes.logical, Some(u64::MAX));
    assert_eq!(index.get(id).unwrap().sizes.allocated, None);
    assert_eq!(index.summary().unwrap().known_logical_bytes, u64::MAX);
}

#[cfg(windows)]
#[test]
fn non_unicode_windows_names_are_preserved_and_not_silently_excluded() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let name = OsString::from_wide(&[0xd800, b'.' as u16, b'j' as u16, b'p' as u16, b'g' as u16]);
    let mut index = Index::new("Synthetic".into()).unwrap();
    let id = index
        .insert(
            index.root(),
            name.clone(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    assert_eq!(index.get(id).unwrap().name, name);
    let filter = Filter::parse("*.jpg;>1mb").unwrap();
    assert_eq!(filter.evaluate(&name, None), Match::Unknown);
    assert_eq!(filter.evaluate(&name, Some(0)), Match::No);
}

fn insert_file(
    index: &mut Index,
    parent: EntryId,
    name: &str,
    sizes: Sizes,
    identity: Option<FileIdentity>,
) -> EntryId {
    index
        .insert(
            parent,
            name.into(),
            Kind::File,
            sizes,
            identity,
            State::Complete,
        )
        .unwrap()
}

#[test]
fn each_metric_charges_the_first_known_size_of_a_shared_identity() {
    let identity = Some(FileIdentity {
        volume: 1,
        file: 501,
    });
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    insert_file(
        &mut index,
        root,
        "first.bin",
        Sizes {
            logical: Some(1000),
            allocated: None,
        },
        identity,
    );
    insert_file(
        &mut index,
        root,
        "second.bin",
        Sizes {
            logical: Some(1000),
            allocated: Some(4096),
        },
        identity,
    );
    index.set_state(root, State::Complete).unwrap();

    let logical = index.summary_for(SizeMetric::Logical).unwrap();
    assert_eq!(logical.metric, SizeMetric::Logical);
    assert_eq!(logical.files, 2);
    assert_eq!(logical.known_bytes, 2000);
    assert_eq!(logical.unique_bytes, 1000);
    assert_eq!(logical.shared_bytes, 1000);
    assert_eq!(logical.unknown_size_files, 0);
    assert_eq!(logical.conflicting_identities, 0);
    assert!(logical.complete);

    // The allocated charge waits for its own first known sample, on the second entry.
    let allocated = index.summary_for(SizeMetric::Allocated).unwrap();
    assert_eq!(allocated.metric, SizeMetric::Allocated);
    assert_eq!(allocated.known_bytes, 4096);
    assert_eq!(allocated.unique_bytes, 4096);
    assert_eq!(allocated.shared_bytes, 0);
    assert_eq!(allocated.unknown_size_files, 1);
    assert_eq!(allocated.conflicting_identities, 0);
    assert!(!allocated.complete);
}

#[test]
fn an_unknown_logical_size_does_not_skip_the_allocated_charge() {
    let identity = Some(FileIdentity {
        volume: 1,
        file: 502,
    });
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    insert_file(
        &mut index,
        root,
        "first.bin",
        Sizes {
            logical: None,
            allocated: Some(8192),
        },
        identity,
    );
    insert_file(
        &mut index,
        root,
        "second.bin",
        Sizes {
            logical: Some(500),
            allocated: Some(8192),
        },
        identity,
    );
    index.set_state(root, State::Complete).unwrap();

    let logical = index.summary_for(SizeMetric::Logical).unwrap();
    assert_eq!(logical.known_bytes, 500);
    assert_eq!(logical.unique_bytes, 500);
    assert_eq!(logical.shared_bytes, 0);
    assert_eq!(logical.unknown_size_files, 1);
    assert!(!logical.complete);

    let allocated = index.summary_for(SizeMetric::Allocated).unwrap();
    assert_eq!(allocated.known_bytes, 16_384);
    assert_eq!(allocated.unique_bytes, 8192);
    assert_eq!(allocated.shared_bytes, 8192);
    assert_eq!(allocated.unknown_size_files, 0);
    assert_eq!(allocated.conflicting_identities, 0);
    assert!(allocated.complete);

    // `summary()` keeps the legacy logical answer for the same index.
    let legacy = index.summary().unwrap();
    assert_eq!(legacy.known_logical_bytes, logical.known_bytes);
    assert_eq!(legacy.unique_logical_bytes, logical.unique_bytes);
    assert_eq!(legacy.shared_logical_bytes, logical.shared_bytes);
    assert_eq!(legacy.complete, logical.complete);
}

#[test]
fn a_known_zero_size_consumes_the_identity_charge_unlike_an_unknown_size() {
    let identity = Some(FileIdentity {
        volume: 1,
        file: 503,
    });
    let mut zero_first = Index::new("Synthetic".into()).unwrap();
    let root = zero_first.root();
    insert_file(
        &mut zero_first,
        root,
        "zero.bin",
        Sizes {
            logical: Some(0),
            allocated: None,
        },
        identity,
    );
    insert_file(
        &mut zero_first,
        root,
        "later.bin",
        Sizes {
            logical: Some(500),
            allocated: None,
        },
        identity,
    );
    zero_first.set_state(root, State::Complete).unwrap();
    let logical = zero_first.summary_for(SizeMetric::Logical).unwrap();
    assert_eq!(logical.known_bytes, 500);
    assert_eq!(
        logical.unique_bytes, 0,
        "the known zero was the first charge"
    );
    assert_eq!(logical.shared_bytes, 500);
    assert_eq!(logical.unknown_size_files, 0);
    assert_eq!(
        logical.conflicting_identities, 1,
        "zero and 500 are unequal known samples of one identity"
    );

    let mut unknown_first = Index::new("Synthetic".into()).unwrap();
    let root = unknown_first.root();
    insert_file(
        &mut unknown_first,
        root,
        "unknown.bin",
        Sizes::default(),
        identity,
    );
    insert_file(
        &mut unknown_first,
        root,
        "later.bin",
        Sizes {
            logical: Some(500),
            allocated: None,
        },
        identity,
    );
    unknown_first.set_state(root, State::Complete).unwrap();
    let logical = unknown_first.summary_for(SizeMetric::Logical).unwrap();
    assert_eq!(logical.unique_bytes, 500);
    assert_eq!(logical.shared_bytes, 0);
    assert_eq!(logical.unknown_size_files, 1);
    assert_eq!(logical.conflicting_identities, 0);
}

#[test]
fn conflicting_samples_of_one_identity_keep_the_first_charge_and_count_once() {
    let repeated = FileIdentity {
        volume: 1,
        file: 504,
    };
    let other = FileIdentity {
        volume: 1,
        file: 505,
    };
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    for (name, bytes, identity) in [
        ("first.bin", 100, repeated),
        ("second.bin", 300, repeated),
        ("third.bin", 300, repeated),
        ("fourth.bin", 7, other),
        ("fifth.bin", 9, other),
    ] {
        insert_file(
            &mut index,
            root,
            name,
            Sizes {
                logical: Some(bytes),
                allocated: None,
            },
            Some(identity),
        );
    }
    index.set_state(root, State::Complete).unwrap();

    let logical = index.summary_for(SizeMetric::Logical).unwrap();
    assert_eq!(logical.known_bytes, 100 + 300 + 300 + 7 + 9);
    assert_eq!(logical.unique_bytes, 100 + 7, "the first charge is kept");
    assert_eq!(logical.shared_bytes, 300 + 300 + 9);
    assert_eq!(
        logical.conflicting_identities, 2,
        "one per identity, not one per disagreeing sample"
    );
    assert!(
        logical.complete,
        "a conflict is not an unknown size or an incomplete entry state"
    );
    assert_eq!(index.len(), 6, "no record is dropped");
}

#[test]
fn an_allocated_conflict_leaves_consistent_logical_samples_unmarked() {
    let identity = Some(FileIdentity {
        volume: 1,
        file: 507,
    });
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    for (name, allocated) in [("first.bin", 4096), ("second.bin", 8192)] {
        insert_file(
            &mut index,
            root,
            name,
            Sizes {
                logical: Some(100),
                allocated: Some(allocated),
            },
            identity,
        );
    }
    index.set_state(root, State::Complete).unwrap();

    let logical = index.summary_for(SizeMetric::Logical).unwrap();
    assert_eq!(logical.conflicting_identities, 0);
    assert_eq!((logical.unique_bytes, logical.shared_bytes), (100, 100));
    let allocated = index.summary_for(SizeMetric::Allocated).unwrap();
    assert_eq!(allocated.conflicting_identities, 1);
    assert_eq!(
        (allocated.unique_bytes, allocated.shared_bytes),
        (4096, 8192),
        "the first allocated charge is kept, not the larger sample"
    );
    assert!(allocated.complete);
}

#[test]
fn unequal_known_samples_are_not_permutation_invariant() {
    let identity = Some(FileIdentity {
        volume: 1,
        file: 506,
    });
    for (first, second, unique, shared) in [(100, 300, 100, 300), (300, 100, 300, 100)] {
        let mut index = Index::new("Synthetic".into()).unwrap();
        let root = index.root();
        for (name, bytes) in [("first.bin", first), ("second.bin", second)] {
            insert_file(
                &mut index,
                root,
                name,
                Sizes {
                    logical: Some(bytes),
                    allocated: None,
                },
                identity,
            );
        }
        index.set_state(root, State::Complete).unwrap();
        let logical = index.summary_for(SizeMetric::Logical).unwrap();
        assert_eq!(logical.unique_bytes, unique, "order {first} then {second}");
        assert_eq!(logical.shared_bytes, shared, "order {first} then {second}");
        assert_eq!(logical.conflicting_identities, 1);
    }
}

#[test]
fn known_sizes_without_identity_are_charged_individually_and_reported() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    insert_file(
        &mut index,
        root,
        "a.bin",
        Sizes {
            logical: Some(7),
            allocated: Some(4096),
        },
        None,
    );
    insert_file(
        &mut index,
        root,
        "b.bin",
        Sizes {
            logical: Some(7),
            allocated: None,
        },
        None,
    );
    insert_file(&mut index, root, "c.bin", Sizes::default(), None);
    index.set_state(root, State::Complete).unwrap();

    let logical = index.summary_for(SizeMetric::Logical).unwrap();
    assert_eq!(logical.unique_bytes, 14);
    assert_eq!(logical.shared_bytes, 0);
    assert_eq!(logical.identity_missing_files, 2);
    assert_eq!(logical.unknown_size_files, 1);

    let allocated = index.summary_for(SizeMetric::Allocated).unwrap();
    assert_eq!(allocated.unique_bytes, 4096);
    assert_eq!(allocated.shared_bytes, 0);
    assert_eq!(allocated.identity_missing_files, 1);
    assert_eq!(allocated.unknown_size_files, 2);
}

#[test]
fn overflow_of_one_metric_does_not_poison_the_other() {
    let mut logical_overflow = Index::new("Synthetic".into()).unwrap();
    let root = logical_overflow.root();
    insert_file(
        &mut logical_overflow,
        root,
        "max.bin",
        Sizes {
            logical: Some(u64::MAX),
            allocated: Some(10),
        },
        None,
    );
    insert_file(
        &mut logical_overflow,
        root,
        "one.bin",
        Sizes {
            logical: Some(1),
            allocated: Some(20),
        },
        None,
    );
    logical_overflow.set_state(root, State::Complete).unwrap();
    assert_eq!(logical_overflow.summary(), Err(IndexError::SizeOverflow));
    assert_eq!(
        logical_overflow.summary_for(SizeMetric::Logical),
        Err(IndexError::SizeOverflow)
    );
    let allocated = logical_overflow.summary_for(SizeMetric::Allocated).unwrap();
    assert_eq!(allocated.known_bytes, 30);
    assert_eq!(allocated.unique_bytes, 30);
    assert!(allocated.complete);
    assert_eq!(logical_overflow.len(), 3, "inserted records are kept");

    let mut allocated_overflow = Index::new("Synthetic".into()).unwrap();
    let root = allocated_overflow.root();
    insert_file(
        &mut allocated_overflow,
        root,
        "max.bin",
        Sizes {
            logical: Some(10),
            allocated: Some(u64::MAX),
        },
        None,
    );
    insert_file(
        &mut allocated_overflow,
        root,
        "one.bin",
        Sizes {
            logical: Some(20),
            allocated: Some(1),
        },
        None,
    );
    allocated_overflow.set_state(root, State::Complete).unwrap();
    let logical = allocated_overflow.summary().unwrap();
    assert_eq!(logical.known_logical_bytes, 30);
    assert!(logical.complete);
    assert_eq!(
        allocated_overflow.summary_for(SizeMetric::Allocated),
        Err(IndexError::SizeOverflow)
    );
    assert_eq!(allocated_overflow.len(), 3);
}
