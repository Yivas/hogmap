use hogmap::index::{
    ChargeResolution, FileIdentity, Index, IndexError, Kind, SizeMetric, Sizes, State,
};
use hogmap::treemap::{LayoutParams, NodeRef, Rect, layout_with_metric};

fn add(
    index: &mut Index,
    name: &str,
    sizes: Sizes,
    identity: Option<FileIdentity>,
) -> hogmap::index::EntryId {
    index
        .insert(
            index.root(),
            name.into(),
            Kind::File,
            sizes,
            identity,
            State::Complete,
        )
        .unwrap()
}
fn assert_accounting(index: &Index, metric: SizeMetric) {
    let s = index.summary_for(metric).unwrap();
    assert_eq!(
        i128::from(s.known_bytes) + s.resolution_delta_bytes,
        i128::from(s.unique_bytes) + i128::from(s.shared_bytes)
    );
}

#[test]
fn authoritative_charges_preserve_listings_and_layout_owners_with_signed_deltas() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let identity = FileIdentity {
        volume: 1,
        file: 42,
    };
    let first = add(
        &mut index,
        "first",
        Sizes {
            logical: Some(100),
            allocated: Some(20),
        },
        Some(identity),
    );
    let alias = add(
        &mut index,
        "alias",
        Sizes {
            logical: Some(200),
            allocated: Some(40),
        },
        Some(identity),
    );
    add(
        &mut index,
        "other",
        Sizes {
            logical: Some(5),
            allocated: Some(50),
        },
        None,
    );
    index.set_state(index.root(), State::Complete).unwrap();
    assert_eq!(
        index.conflicting_identities().collect::<Vec<_>>(),
        vec![identity]
    );
    assert_eq!(index.conflicting_identity_count(), 1);
    assert_eq!(
        index.conflict_candidates().collect::<Vec<_>>(),
        vec![(identity, alias)]
    );
    let result = index
        .apply_authoritative_sizes(
            identity,
            Sizes {
                logical: Some(80),
                allocated: Some(60),
            },
        )
        .unwrap();
    assert_eq!(result.logical, ChargeResolution::Applied);
    assert_eq!(result.allocated, ChargeResolution::Applied);
    assert!(!result.still_conflicted);
    assert_eq!(index.conflicting_identities().count(), 0);
    assert_eq!(index.conflicting_identity_count(), 0);
    assert_eq!(index.conflict_candidates().count(), 0);
    let logical = index.summary_for(SizeMetric::Logical).unwrap();
    assert_eq!(
        (
            logical.known_bytes,
            logical.unique_bytes,
            logical.shared_bytes,
            logical.resolution_delta_bytes
        ),
        (305, 85, 200, -20)
    );
    let allocated = index.summary_for(SizeMetric::Allocated).unwrap();
    assert_eq!(
        (
            allocated.known_bytes,
            allocated.unique_bytes,
            allocated.shared_bytes,
            allocated.resolution_delta_bytes
        ),
        (110, 110, 40, 40)
    );
    for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
        assert_accounting(&index, metric);
        let layout = layout_with_metric(
            &index,
            index.root(),
            Rect::new(0.0, 0.0, 100.0, 100.0),
            LayoutParams {
                min_pixel_area: 1.0,
                header_height: 0.0,
                max_depth: usize::MAX,
            },
            metric,
        )
        .unwrap();
        assert_eq!(
            layout.known_sum,
            if metric == SizeMetric::Logical {
                285
            } else {
                150
            }
        );
        assert_eq!(
            layout.known_sum,
            index.summary_for(metric).unwrap().apparent_bytes().unwrap()
        );
        let weight = |id| {
            layout
                .placed
                .iter()
                .find(|p| p.node == NodeRef::Entry(id))
                .unwrap()
                .known_sum
        };
        assert_eq!(
            weight(first),
            if metric == SizeMetric::Logical {
                80
            } else {
                60
            }
        );
        assert_eq!(
            weight(alias),
            if metric == SizeMetric::Logical {
                200
            } else {
                40
            }
        );
        let detail = hogmap::treemap::layout_members_with_metric(
            &index,
            index.root(),
            &[alias],
            Rect::new(0.0, 0.0, 100.0, 100.0),
            LayoutParams {
                min_pixel_area: 1.0,
                header_height: 0.0,
                max_depth: usize::MAX,
            },
            metric,
        )
        .unwrap();
        assert_eq!(
            detail.known_sum,
            if metric == SizeMetric::Logical {
                200
            } else {
                40
            }
        );
        assert!(
            layout
                .placed
                .iter()
                .any(|p| p.node == NodeRef::Entry(first))
        );
        assert!(
            layout
                .placed
                .iter()
                .any(|p| p.node == NodeRef::Entry(alias))
        );
    }
    assert_eq!(
        index.get(first).unwrap().sizes,
        Sizes {
            logical: Some(100),
            allocated: Some(20)
        }
    );
    assert_eq!(
        index.get(alias).unwrap().sizes,
        Sizes {
            logical: Some(200),
            allocated: Some(40)
        }
    );
    assert_eq!(
        index.apply_authoritative_sizes(identity, Sizes::default()),
        Err(IndexError::NotConflicted)
    );
    assert_eq!(
        index.insert(
            index.root(),
            "late".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete
        ),
        Err(IndexError::ResolvedSnapshot)
    );
    index.set_state(first, State::Stale).unwrap();
    assert!(!index.summary_for(SizeMetric::Allocated).unwrap().complete);
    let report = hogmap::scan_win::reread_identity_conflicts(
        &mut index,
        std::path::Path::new("not-opened"),
        &hogmap::scan::ScanCancellation::new(),
    );
    assert_eq!(
        (report.candidate_identities, report.attempted_identities),
        (0, 0)
    );
    assert!(report.issues.is_empty());
}

#[test]
fn resolution_is_per_metric_and_does_not_fill_unknown_listed_charges() {
    let identity = FileIdentity { volume: 1, file: 7 };
    let mut index = Index::new("Synthetic".into()).unwrap();
    add(
        &mut index,
        "one",
        Sizes {
            logical: None,
            allocated: Some(10),
        },
        Some(identity),
    );
    add(
        &mut index,
        "two",
        Sizes {
            logical: None,
            allocated: Some(20),
        },
        Some(identity),
    );
    let result = index
        .apply_authoritative_sizes(
            identity,
            Sizes {
                logical: Some(99),
                allocated: Some(0),
            },
        )
        .unwrap();
    assert_eq!(result.logical, ChargeResolution::MissingCharge);
    assert_eq!(result.allocated, ChargeResolution::Applied);
    assert!(!result.still_conflicted);
    let logical = index.summary_for(SizeMetric::Logical).unwrap();
    assert_eq!(
        (
            logical.known_bytes,
            logical.unique_bytes,
            logical.unknown_size_files,
            logical.resolution_delta_bytes
        ),
        (0, 0, 2, 0)
    );
    assert_eq!(
        index
            .summary_for(SizeMetric::Allocated)
            .unwrap()
            .unique_bytes,
        0
    );
    assert_accounting(&index, SizeMetric::Logical);
    assert_accounting(&index, SizeMetric::Allocated);

    let mut partial = Index::new("Synthetic".into()).unwrap();
    add(
        &mut partial,
        "one",
        Sizes {
            logical: Some(1),
            allocated: Some(10),
        },
        Some(identity),
    );
    add(
        &mut partial,
        "two",
        Sizes {
            logical: Some(2),
            allocated: Some(20),
        },
        Some(identity),
    );
    let result = partial
        .apply_authoritative_sizes(
            identity,
            Sizes {
                logical: None,
                allocated: Some(15),
            },
        )
        .unwrap();
    assert_eq!(result.logical, ChargeResolution::UnknownSize);
    assert!(result.still_conflicted);
    assert_eq!(
        partial
            .summary_for(SizeMetric::Logical)
            .unwrap()
            .conflicting_identities,
        1
    );
    assert_eq!(
        partial
            .summary_for(SizeMetric::Allocated)
            .unwrap()
            .conflicting_identities,
        0
    );
    let result = partial
        .apply_authoritative_sizes(
            identity,
            Sizes {
                logical: Some(0),
                allocated: None,
            },
        )
        .unwrap();
    assert!(!result.still_conflicted);
    assert_eq!(
        partial
            .summary_for(SizeMetric::Logical)
            .unwrap()
            .resolution_delta_bytes,
        -1
    );
    assert_eq!(
        partial
            .summary_for(SizeMetric::Allocated)
            .unwrap()
            .unique_bytes,
        15
    );
    assert_accounting(&partial, SizeMetric::Logical);
    assert_accounting(&partial, SizeMetric::Allocated);
}

#[test]
fn an_overflowing_resolution_is_atomic_and_does_not_seal_or_clear_conflicts() {
    let identity = FileIdentity { volume: 1, file: 8 };
    let mut index = Index::new("Synthetic".into()).unwrap();
    add(
        &mut index,
        "one",
        Sizes {
            logical: Some(1),
            allocated: Some(1),
        },
        Some(identity),
    );
    add(
        &mut index,
        "two",
        Sizes {
            logical: Some(2),
            allocated: Some(2),
        },
        Some(identity),
    );
    add(
        &mut index,
        "large",
        Sizes {
            logical: Some(u64::MAX - 3),
            allocated: Some(0),
        },
        None,
    );
    let before = index.summary_for(SizeMetric::Allocated).unwrap();
    assert_eq!(
        index.apply_authoritative_sizes(
            identity,
            Sizes {
                logical: Some(10),
                allocated: Some(50)
            }
        ),
        Err(IndexError::SizeOverflow)
    );
    assert_eq!(index.summary_for(SizeMetric::Allocated).unwrap(), before);
    assert_eq!(index.conflicting_identities().count(), 1);
    add(
        &mut index,
        "still-appendable",
        Sizes {
            logical: Some(0),
            allocated: Some(0),
        },
        None,
    );
    assert_accounting(&index, SizeMetric::Logical);
}

#[cfg(windows)]
#[test]
fn a_cancelled_capped_postpass_opens_no_files_and_reports_every_unresolved_identity() {
    use hogmap::scan::ScanCancellation;
    use hogmap::scan_win::{ConflictRereadIssue, MAX_CONFLICT_REREADS, reread_identity_conflicts};
    let mut index = Index::new("Synthetic".into()).unwrap();
    for n in 0..=MAX_CONFLICT_REREADS {
        let identity = Some(FileIdentity {
            volume: 1,
            file: n as u128 + 1,
        });
        add(
            &mut index,
            &format!("a{n}"),
            Sizes {
                logical: Some(1),
                allocated: Some(1),
            },
            identity,
        );
        add(
            &mut index,
            &format!("b{n}"),
            Sizes {
                logical: Some(2),
                allocated: Some(2),
            },
            identity,
        );
    }
    assert_eq!(index.conflicting_identity_count(), MAX_CONFLICT_REREADS + 1);
    assert_eq!(index.conflict_candidates().count(), MAX_CONFLICT_REREADS);
    let cancel = ScanCancellation::new();
    cancel.cancel();
    let report = reread_identity_conflicts(&mut index, std::path::Path::new("not-opened"), &cancel);
    assert_eq!(report.candidate_identities, MAX_CONFLICT_REREADS + 1);
    assert_eq!(report.attempted_identities, 0);
    assert_eq!(report.resolved_identities, 0);
    assert_eq!(report.unresolved_identities, MAX_CONFLICT_REREADS + 1);
    assert_eq!(
        report.issues.get(&ConflictRereadIssue::LimitExceeded),
        Some(&1)
    );
    assert_eq!(
        report.issues.get(&ConflictRereadIssue::Cancelled),
        Some(&MAX_CONFLICT_REREADS)
    );
    assert_eq!(
        index.conflicting_identities().count(),
        MAX_CONFLICT_REREADS + 1
    );
}
