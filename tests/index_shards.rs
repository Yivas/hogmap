use hogmap::index::{FileIdentity, Index, IndexError, Kind, SizeMetric, Sizes, State};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn identity_charge_growth_matches_independent_accounting_and_sorted_conflicts() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    let folder = index
        .insert(
            root,
            "folder".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let mut listed = Vec::new();
    let rows = [
        (Some((1, 1)), Some(10), Some(16)),
        (Some((1, 1)), Some(10), Some(16)),
        (None, Some(3), None),
        (Some((1, 2)), Some(20), Some(32)),
        (Some((1, 2)), Some(21), Some(40)),
        (Some((1, 3)), None, Some(8)),
        (Some((1, 3)), Some(5), Some(8)),
        (Some((2, 9)), Some(2), Some(2)),
        (Some((2, 9)), Some(4), Some(4)),
    ];
    for (number, (identity, logical, allocated)) in rows.into_iter().enumerate() {
        let identity = identity.map(|(volume, file)| FileIdentity { volume, file });
        index
            .insert(
                folder,
                format!("listed-{number}").into(),
                Kind::File,
                Sizes { logical, allocated },
                identity,
                State::Complete,
            )
            .unwrap();
        listed.push((identity, logical, allocated));
    }
    assert_eq!(index.summary().unwrap().known_logical_bytes, 75);
    assert_eq!(index.summary().unwrap().unique_logical_bytes, 40);
    assert_eq!(index.summary().unwrap().shared_logical_bytes, 35);
    assert_eq!(
        index
            .summary_for(SizeMetric::Allocated)
            .unwrap()
            .known_bytes,
        126
    );
    assert_eq!(
        index
            .conflicting_identities()
            .map(|id| (id.volume, id.file))
            .collect::<Vec<_>>(),
        [(1, 2), (2, 9)]
    );
    assert_eq!(
        index
            .conflict_candidates()
            .map(|(id, entry)| (id.volume, id.file, entry.position()))
            .collect::<Vec<_>>(),
        [(1, 2, 6), (2, 9, 10)]
    );

    for number in 0..16_384 {
        let identity = FileIdentity {
            volume: 3,
            file: number,
        };
        index
            .insert(
                folder,
                format!("growth-{number}").into(),
                Kind::File,
                Sizes {
                    logical: Some(1),
                    allocated: Some(2),
                },
                Some(identity),
                State::Complete,
            )
            .unwrap();
        listed.push((Some(identity), Some(1), Some(2)));
    }
    for (metric, expected) in [
        (SizeMetric::Logical, (16_459, 16_424, 35)),
        (SizeMetric::Allocated, (32_894, 32_826, 68)),
    ] {
        // Independent first-known accounting, not Index summaries or the sharding algorithm.
        let mut first = BTreeMap::new();
        let mut conflicts = BTreeSet::new();
        let (mut known, mut unique, mut shared, mut unknown, mut missing) = (0, 0, 0, 0, 0);
        for (identity, logical, allocated) in &listed {
            let size = if metric == SizeMetric::Logical {
                logical
            } else {
                allocated
            };
            let Some(bytes) = *size else {
                unknown += 1;
                continue;
            };
            known += bytes;
            let Some(identity) = identity else {
                unique += bytes;
                missing += 1;
                continue;
            };
            let key = (identity.volume, identity.file);
            if let Some(previous) = first.get(&key) {
                shared += bytes;
                if *previous != bytes {
                    conflicts.insert(key);
                }
            } else {
                first.insert(key, bytes);
                unique += bytes;
            }
        }
        let actual = index.summary_for(metric).unwrap();
        assert_eq!((known, unique, shared), expected);
        assert_eq!(
            (actual.known_bytes, actual.unique_bytes, actual.shared_bytes),
            expected
        );
        assert_eq!(
            (
                actual.unknown_size_files,
                actual.identity_missing_files,
                actual.conflicting_identities
            ),
            (unknown, missing, conflicts.len())
        );
        assert_eq!(actual.files, 16_393);
    }
    assert_eq!(index.conflicting_identity_count(), 2);
    assert_eq!(
        index
            .conflicting_identities()
            .map(|id| (id.volume, id.file))
            .collect::<Vec<_>>(),
        [(1, 2), (2, 9)]
    );
    index
        .apply_authoritative_sizes(
            FileIdentity { volume: 1, file: 2 },
            Sizes {
                logical: Some(22),
                allocated: Some(48),
            },
        )
        .unwrap();
    assert_eq!(
        index
            .summary_for(SizeMetric::Logical)
            .unwrap()
            .resolution_delta_bytes,
        2
    );
    assert_eq!(
        index
            .summary_for(SizeMetric::Allocated)
            .unwrap()
            .resolution_delta_bytes,
        16
    );
    assert_eq!(
        index.insert(
            root,
            "sealed".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete
        ),
        Err(IndexError::ResolvedSnapshot)
    );
}
