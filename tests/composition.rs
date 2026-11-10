use hogmap::composition::*;
use hogmap::index::{
    FileIdentity, Index, IndexError, Kind, MetricSummary, SizeMetric, Sizes, State,
};
use hogmap::volume::*;
use std::time::SystemTime;

fn sample(total: u64, free: u64, reserve: Option<u64>) -> OriginCapacitySample {
    OriginCapacitySample {
        total_bytes: Some(total),
        free_bytes: Some(free),
        storage_reserve_bytes: reserve,
        total_reserved_bytes: Some(900),
        reserve_issue: None,
        metadata: CapacityMetadata {
            source: CapacitySource::WindowsActualGlobalFilesystem,
            captured_at: SystemTime::UNIX_EPOCH,
            issue: None,
        },
    }
}
fn evidence() -> VolumeEvidence {
    // Normalized synthetic facts; no native identities or paths are invented.
    VolumeEvidence {
        scope: Ok(RootScope::WholeVolume),
        filesystem: Ok(ObservedFilesystem::Ntfs),
        before: Ok(sample(1000, 200, Some(100))),
        after: Ok(sample(1000, 250, Some(100))),
        validation: Ok(OriginValidation {
            directory_identity: Ok(ObservedContinuity::UnchangedAtEndpoints),
            normalized_path: Ok(ObservedContinuity::UnchangedAtEndpoints),
        }),
        directories: DirectoryNamespaceObservations::default(),
    }
}
fn summary(unique: u64) -> MetricSummary {
    MetricSummary {
        metric: SizeMetric::Allocated,
        files: 2,
        known_bytes: unique,
        unique_bytes: unique,
        shared_bytes: 0,
        unknown_size_files: 0,
        identity_missing_files: 0,
        conflicting_identities: 0,
        resolution_delta_bytes: 0,
        complete: true,
    }
}
fn view() -> CompositionView {
    CompositionView {
        metric: SizeMetric::Allocated,
        unzoomed: true,
        finished: true,
    }
}

#[test]
fn apparent_total_corrects_only_the_charged_record_and_retains_listed_alias_sizes() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let identity = FileIdentity { volume: 1, file: 7 };
    let mut entries = Vec::new();
    for (name, bytes) in [("charged", 100), ("alias", 200), ("other", 50)] {
        entries.push(
            index
                .insert(
                    index.root(),
                    name.into(),
                    Kind::File,
                    Sizes {
                        logical: Some(bytes),
                        allocated: Some(bytes),
                    },
                    Some(if name == "other" {
                        FileIdentity { volume: 1, file: 8 }
                    } else {
                        identity
                    }),
                    State::Complete,
                )
                .unwrap(),
        );
    }
    index.set_state(index.root(), State::Complete).unwrap();
    index
        .apply_authoritative_sizes(
            identity,
            Sizes {
                logical: Some(300),
                allocated: Some(300),
            },
        )
        .unwrap();
    let totals = index.summary_for(SizeMetric::Allocated).unwrap();
    // Apparent 550 = authoritative charged 300 + listed alias 200 + other 50, not 650.
    assert_eq!(
        (
            totals.known_bytes,
            totals.resolution_delta_bytes,
            totals.unique_bytes,
            totals.shared_bytes
        ),
        (350, 200, 350, 200)
    );
    assert_eq!(index.get(entries[0]).unwrap().sizes.allocated, Some(100));
    assert_eq!(index.get(entries[1]).unwrap().sizes.allocated, Some(200));
    let result = evaluate(
        view(),
        Some(&evidence()),
        Ok(totals),
        index.conflicting_identity_count(),
    );
    assert!(result.eligible());
    let layout = hogmap::treemap::layout_with_metric(
        &index,
        index.root(),
        hogmap::treemap::Rect::new(0.0, 0.0, 100.0, 100.0),
        hogmap::treemap::LayoutParams {
            min_pixel_area: 1.0,
            header_height: 0.0,
            max_depth: usize::MAX,
        },
        SizeMetric::Allocated,
    )
    .unwrap();
    assert_eq!(layout.known_sum, 550);
    assert_eq!(totals.apparent_bytes(), Ok(550));
    assert_eq!(
        result.differences,
        Some(EndpointDifferences {
            before: 350,
            after: 300
        })
    );

    // Two listed names of a 300-byte file plus a separate 50-byte file: apparent 650,
    // unique 350. More repeated names may exceed capacity without changing D.
    for apparent in [650, 1100] {
        let totals = MetricSummary {
            known_bytes: apparent,
            unique_bytes: 350,
            shared_bytes: apparent - 350,
            resolution_delta_bytes: 0,
            ..totals
        };
        assert_eq!(totals.apparent_bytes(), Ok(apparent));
        let result = evaluate(view(), Some(&evidence()), Ok(totals), 0);
        assert_eq!(
            result.differences,
            Some(EndpointDifferences {
                before: 350,
                after: 300
            })
        );
        assert!(result.eligible());
        assert_eq!(result.components.free_bytes, Some(250));
    }
}

#[test]
fn unique_difference_is_independent_of_apparent_overflow_but_rejects_allocated_overflow() {
    let totals = MetricSummary {
        known_bytes: u64::MAX,
        unique_bytes: 2,
        shared_bytes: u64::MAX - 1,
        resolution_delta_bytes: 1,
        ..summary(2)
    };
    assert_eq!(totals.apparent_bytes(), Err(IndexError::SizeOverflow));
    let result = evaluate(view(), Some(&evidence()), Ok(totals), 0);
    assert!(result.eligible());
    assert_eq!(
        result.differences,
        Some(EndpointDifferences {
            before: 698,
            after: 648
        })
    );

    let mut index = Index::new("Overflow".into()).unwrap();
    for (name, bytes) in [("max", u64::MAX), ("one", 1)] {
        index
            .insert(
                index.root(),
                name.into(),
                Kind::File,
                Sizes {
                    logical: Some(0),
                    allocated: Some(bytes),
                },
                None,
                State::Complete,
            )
            .unwrap();
    }
    let totals = index.summary_for(SizeMetric::Allocated);
    assert_eq!(totals, Err(IndexError::SizeOverflow));
    let result = evaluate(view(), Some(&evidence()), totals, 0);
    assert_eq!(result.differences, None);
    assert_eq!(result.issues, vec![EligibilityIssue::AllocatedOverflow]);
}

#[test]
fn measured_reserve_is_subtracted_once_and_endpoints_are_signed_not_clipped() {
    let facts = evidence();
    let result = evaluate(view(), Some(&facts), Ok(summary(400)), 0);
    assert!(result.eligible());
    assert_eq!(
        result.differences,
        Some(EndpointDifferences {
            before: 300,
            after: 250
        })
    );
    assert_eq!(
        result.components,
        VolumeComponents {
            free_bytes: Some(250),
            reserve_bytes: Some(100)
        }
    );
    // TotalReserved=900 is deliberately different: it is not an additional subtraction.
    for (unique, before, after) in [(650, 50, 0), (700, 0, -50), (800, -100, -150)] {
        let result = evaluate(view(), Some(&facts), Ok(summary(unique)), 0);
        assert_eq!(
            result.differences,
            Some(EndpointDifferences { before, after })
        );
        assert_eq!(result.issues, vec![EligibilityIssue::NonPositiveDifference]);
        assert!(!result.eligible());
        assert_eq!(
            result.components.free_bytes,
            Some(250),
            "valid free is not hidden by D"
        );
    }
    let mut zero = evidence();
    zero.before = Ok(sample(1000, 0, Some(0)));
    zero.after = zero.before;
    let result = evaluate(view(), Some(&zero), Ok(summary(0)), 0);
    assert!(result.eligible());
    assert_eq!(
        result.differences,
        Some(EndpointDifferences {
            before: 1000,
            after: 1000
        })
    );
    assert_eq!(
        result.components,
        VolumeComponents {
            free_bytes: Some(0),
            reserve_bytes: Some(0)
        }
    );
    let mut wide = evidence();
    wide.before = Ok(sample(0, 0, Some(u64::MAX)));
    wide.after = wide.before;
    let result = evaluate(view(), Some(&wide), Ok(summary(u64::MAX)), 0);
    assert_eq!(
        result.differences,
        Some(EndpointDifferences {
            before: -36893488147419103230,
            after: -36893488147419103230
        })
    );
}

#[test]
fn frame_scope_completion_and_root_continuity_are_required() {
    let facts = evidence();
    let mut logical = view();
    logical.metric = SizeMetric::Logical;
    let mut zoomed = view();
    zoomed.unzoomed = false;
    let mut unfinished = view();
    unfinished.finished = false;
    for (view, issue) in [
        (logical, EligibilityIssue::LogicalView),
        (zoomed, EligibilityIssue::Zoomed),
        (unfinished, EligibilityIssue::NotFinished),
    ] {
        let result = evaluate(view, Some(&facts), Ok(summary(400)), 0);
        assert_eq!(result.issues, vec![issue]);
        assert_eq!(result.components, VolumeComponents::default());
        assert_eq!(result.differences, None);
    }
    let result = evaluate(view(), None, Ok(summary(400)), 0);
    assert_eq!(result.issues, vec![EligibilityIssue::NotObserved]);
    for scope in [RootScope::Folder, RootScope::Unknown] {
        let mut facts = evidence();
        facts.scope = Ok(scope);
        assert_eq!(
            evaluate(view(), Some(&facts), Ok(summary(400)), 0).issues,
            vec![EligibilityIssue::NotWholeVolume]
        );
    }
    let mut facts = evidence();
    facts.scope = Err(OriginIssue::UnprovenVolumeRelation);
    assert_eq!(
        evaluate(view(), Some(&facts), Ok(summary(400)), 0).issues,
        vec![EligibilityIssue::ScopeUnavailable(
            OriginIssue::UnprovenVolumeRelation
        )]
    );
    for field in 0..3 {
        let mut facts = evidence();
        let expected = match field {
            0 => {
                facts.validation.as_mut().unwrap().directory_identity =
                    Ok(ObservedContinuity::Changed);
                EligibilityIssue::RootIdentityUnproven
            }
            1 => {
                facts.validation.as_mut().unwrap().normalized_path =
                    Err(OriginIssue::NamespaceChanged);
                EligibilityIssue::RootPathUnproven
            }
            _ => {
                facts.validation = Err(OriginIssue::IdentityChanged);
                EligibilityIssue::ValidationUnavailable(OriginIssue::IdentityChanged)
            }
        };
        let result = evaluate(view(), Some(&facts), Ok(summary(400)), 0);
        assert_eq!(result.issues, vec![expected]);
        assert_eq!(result.components, VolumeComponents::default());
    }
}

#[test]
fn capacity_and_reserve_fail_independently_without_fabricating_zero() {
    for endpoint in [Endpoint::Before, Endpoint::After] {
        let mut facts = evidence();
        let target = if endpoint == Endpoint::Before {
            &mut facts.before
        } else {
            &mut facts.after
        };
        *target = Err(OriginIssue::UnprovenVolumeRelation);
        let result = evaluate(view(), Some(&facts), Ok(summary(400)), 0);
        assert_eq!(
            result.issues,
            vec![EligibilityIssue::CapacityUnavailable(
                endpoint,
                OriginIssue::UnprovenVolumeRelation
            )]
        );
        assert_eq!(result.differences, None);
        assert_eq!(
            result.components.free_bytes,
            if endpoint == Endpoint::Before {
                Some(250)
            } else {
                None
            }
        );

        let mut facts = evidence();
        let target = if endpoint == Endpoint::Before {
            &mut facts.before
        } else {
            &mut facts.after
        };
        target.as_mut().unwrap().storage_reserve_bytes = None;
        let result = evaluate(view(), Some(&facts), Ok(summary(400)), 0);
        assert_eq!(
            result.issues,
            vec![EligibilityIssue::UnknownReserve(endpoint)]
        );
        assert_eq!(result.components.free_bytes, Some(250));
        assert_eq!(
            result.components.reserve_bytes,
            if endpoint == Endpoint::After {
                None
            } else {
                Some(100)
            }
        );

        let target = if endpoint == Endpoint::Before {
            &mut facts.before
        } else {
            &mut facts.after
        };
        target.as_mut().unwrap().reserve_issue = Some(CapacityIssue::SizeOverflow);
        assert_eq!(
            evaluate(view(), Some(&facts), Ok(summary(400)), 0).issues,
            vec![EligibilityIssue::ReserveFailed(
                endpoint,
                CapacityIssue::SizeOverflow
            )]
        );
    }
    let mut facts = evidence();
    facts.after.as_mut().unwrap().total_bytes = None;
    let result = evaluate(view(), Some(&facts), Ok(summary(400)), 0);
    assert_eq!(
        result.issues,
        vec![EligibilityIssue::UnknownCapacity(Endpoint::After)]
    );
    assert_eq!(result.components, VolumeComponents::default());
    facts.after = Ok(sample(100, 200, Some(100)));
    assert_eq!(
        evaluate(view(), Some(&facts), Ok(summary(400)), 0).issues,
        vec![EligibilityIssue::CapacityFailed(
            Endpoint::After,
            CapacityIssue::IncoherentFreeSpace
        )]
    );
    facts.after.as_mut().unwrap().metadata.issue = Some(CapacityIssue::QueryFailed(-1));
    assert_eq!(
        evaluate(view(), Some(&facts), Ok(summary(400)), 0).issues,
        vec![EligibilityIssue::CapacityFailed(
            Endpoint::After,
            CapacityIssue::QueryFailed(-1)
        )]
    );
}

#[test]
fn overcount_risks_and_non_ntfs_block_difference_but_coverage_counts_do_not() {
    let facts = evidence();
    let result = evaluate(view(), Some(&facts), Err(IndexError::SizeOverflow), 0);
    assert_eq!(result.issues, vec![EligibilityIssue::AllocatedOverflow]);
    assert_eq!(result.components, VolumeComponents::default());
    let mut counts = summary(400);
    counts.conflicting_identities = 2;
    counts.identity_missing_files = 3;
    assert_eq!(
        evaluate(view(), Some(&facts), Ok(counts), 2).issues,
        vec![
            EligibilityIssue::UnresolvedConflicts(2),
            EligibilityIssue::KnownSizeWithoutIdentity(3)
        ]
    );
    assert_eq!(
        evaluate(view(), Some(&facts), Ok(summary(400)), 1).issues,
        vec![EligibilityIssue::UnresolvedConflicts(1)],
        "a logical-only unresolved identity still blocks eligibility"
    );
    let mut facts = evidence();
    facts.directories.serial64_different = 2;
    facts.directories.different_guid_name_unproven = 3;
    assert_eq!(
        evaluate(view(), Some(&facts), Ok(summary(400)), 0).issues,
        vec![
            EligibilityIssue::SerialMismatch(2),
            EligibilityIssue::DifferentGuidUnproven(3)
        ]
    );
    for filesystem in [
        Ok(ObservedFilesystem::Other("ReFS".into())),
        Err(OriginIssue::MissingFilesystem),
    ] {
        let mut facts = evidence();
        facts.filesystem = filesystem;
        let expected = if facts.filesystem.is_ok() {
            EligibilityIssue::NotNtfs
        } else {
            EligibilityIssue::FilesystemUnavailable(OriginIssue::MissingFilesystem)
        };
        assert_eq!(
            evaluate(view(), Some(&facts), Ok(summary(400)), 0).issues,
            vec![expected]
        );
    }
    let mut facts = evidence();
    facts.directories.not_queried = 7;
    facts.directories.unavailable = 8;
    facts.directories.serial64_unavailable = 9;
    let mut counts = summary(400);
    counts.unknown_size_files = 99;
    counts.complete = false;
    let result = evaluate(view(), Some(&facts), Ok(counts), 0);
    assert!(
        result.eligible(),
        "coverage warnings do not decide a drawing policy"
    );
    assert_eq!(
        result.differences,
        Some(EndpointDifferences {
            before: 300,
            after: 250
        })
    );
}
