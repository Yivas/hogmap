//! Pure accounting eligibility and independently known volume components; no drawing or I/O.
use crate::index::{IndexError, MetricSummary, SizeMetric};
use crate::scan_win::ScanOriginObservations;
use crate::volume::{
    CapacityIssue, DirectoryNamespaceObservations, ObservedContinuity, ObservedFilesystem,
    OriginCapacitySample, OriginIssue, OriginValidation, RootScope,
};

/// Normalized facts from one generation's origin sidecar, held only in RAM.
#[derive(Clone, Debug)]
pub struct VolumeEvidence {
    pub scope: Result<RootScope, OriginIssue>,
    pub filesystem: Result<ObservedFilesystem, OriginIssue>,
    pub before: Result<OriginCapacitySample, OriginIssue>,
    pub after: Result<OriginCapacitySample, OriginIssue>,
    pub validation: Result<OriginValidation, OriginIssue>,
    pub directories: DirectoryNamespaceObservations,
}

impl VolumeEvidence {
    pub fn from_observations(observations: &ScanOriginObservations) -> Self {
        let scope = observations
            .root
            .as_ref()
            .map_err(|issue| *issue)
            .and_then(|root| {
                if let Some(issue) = root.scope_issue {
                    return Err(issue);
                }
                let directory = root.directory_identity?;
                let volume = root.volume_root_identity?;
                if root.scope == RootScope::WholeVolume && directory != volume {
                    return Err(OriginIssue::UnprovenVolumeRelation);
                }
                Ok(root.scope)
            });
        Self {
            scope,
            filesystem: observations
                .root
                .as_ref()
                .map_err(|issue| *issue)
                .and_then(|root| root.filesystem.clone()),
            before: observations.before,
            after: observations.after,
            validation: observations.validation,
            directories: observations.directories,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CompositionView {
    pub metric: SizeMetric,
    pub unzoomed: bool,
    pub finished: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Before,
    After,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EligibilityIssue {
    NotObserved,
    LogicalView,
    Zoomed,
    NotFinished,
    ScopeUnavailable(OriginIssue),
    NotWholeVolume,
    ValidationUnavailable(OriginIssue),
    RootIdentityUnproven,
    RootPathUnproven,
    CapacityUnavailable(Endpoint, OriginIssue),
    CapacityFailed(Endpoint, CapacityIssue),
    UnknownCapacity(Endpoint),
    ReserveFailed(Endpoint, CapacityIssue),
    UnknownReserve(Endpoint),
    AllocatedOverflow,
    UnresolvedConflicts(usize),
    KnownSizeWithoutIdentity(usize),
    SerialMismatch(u64),
    DifferentGuidUnproven(u64),
    FilesystemUnavailable(OriginIssue),
    NotNtfs,
    NonPositiveDifference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VolumePart {
    Free,
    Reserve,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VolumeComponents {
    pub free_bytes: Option<u64>,
    pub reserve_bytes: Option<u64>,
}

/// Endpoints, not a minimum/maximum range or a bound on intermediate activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointDifferences {
    pub before: i128,
    pub after: i128,
}

#[derive(Debug)]
pub struct CompositionEvaluation {
    /// Valid final volume components do not require a positive or even calculable D.
    pub components: VolumeComponents,
    pub differences: Option<EndpointDifferences>,
    pub issues: Vec<EligibilityIssue>,
}
impl CompositionEvaluation {
    pub fn eligible(&self) -> bool {
        self.issues.is_empty()
    }
}

fn capacity(
    sample: Result<OriginCapacitySample, OriginIssue>,
    endpoint: Endpoint,
    issues: &mut Vec<EligibilityIssue>,
) -> Option<(u64, u64)> {
    match sample {
        Err(issue) => {
            issues.push(EligibilityIssue::CapacityUnavailable(endpoint, issue));
            None
        }
        Ok(sample) => {
            if let Some(issue) = sample.metadata.issue {
                issues.push(EligibilityIssue::CapacityFailed(endpoint, issue));
                return None;
            }
            match sample.total_bytes.zip(sample.free_bytes) {
                Some((total, free)) if free <= total => Some((total, free)),
                Some(_) => {
                    issues.push(EligibilityIssue::CapacityFailed(
                        endpoint,
                        CapacityIssue::IncoherentFreeSpace,
                    ));
                    None
                }
                None => {
                    issues.push(EligibilityIssue::UnknownCapacity(endpoint));
                    None
                }
            }
        }
    }
}
fn reserve(
    sample: Result<OriginCapacitySample, OriginIssue>,
    endpoint: Endpoint,
    issues: &mut Vec<EligibilityIssue>,
) -> Option<u64> {
    let sample = sample.ok()?;
    if let Some(issue) = sample.reserve_issue {
        issues.push(EligibilityIssue::ReserveFailed(endpoint, issue));
        return None;
    }
    if sample.storage_reserve_bytes.is_none() {
        issues.push(EligibilityIssue::UnknownReserve(endpoint));
    }
    sample.storage_reserve_bytes
}

/// Coverage incompleteness, unknown file sizes, skips, fallback and unavailable namespace checks
/// are diagnostics, deliberately not gates here. This function never grants an area to D.
pub fn evaluate(
    view: CompositionView,
    evidence: Option<&VolumeEvidence>,
    allocated: Result<MetricSummary, IndexError>,
    unresolved_identities: usize,
) -> CompositionEvaluation {
    use EligibilityIssue as Issue;
    let allocated = allocated.and_then(|summary| {
        summary.apparent_bytes()?;
        Ok(summary)
    });
    let mut issues = Vec::new();
    if view.metric != SizeMetric::Allocated {
        issues.push(Issue::LogicalView);
    }
    if !view.unzoomed {
        issues.push(Issue::Zoomed);
    }
    if !view.finished {
        issues.push(Issue::NotFinished);
    }
    let mut result = CompositionEvaluation {
        components: VolumeComponents::default(),
        differences: None,
        issues,
    };
    let Some(evidence) = evidence else {
        result.issues.push(Issue::NotObserved);
        return result;
    };
    match evidence.scope {
        Ok(RootScope::WholeVolume) => {}
        Ok(_) => result.issues.push(Issue::NotWholeVolume),
        Err(issue) => result.issues.push(Issue::ScopeUnavailable(issue)),
    }
    match evidence.validation {
        Err(issue) => result.issues.push(Issue::ValidationUnavailable(issue)),
        Ok(validation) => {
            if validation.directory_identity != Ok(ObservedContinuity::UnchangedAtEndpoints) {
                result.issues.push(Issue::RootIdentityUnproven);
            }
            if validation.normalized_path != Ok(ObservedContinuity::UnchangedAtEndpoints) {
                result.issues.push(Issue::RootPathUnproven);
            }
        }
    }
    if allocated.is_err() {
        result.issues.push(Issue::AllocatedOverflow);
    }
    let frame_valid = result.issues.is_empty();
    let after = capacity(evidence.after, Endpoint::After, &mut result.issues);
    // A reserve-only failure must not erase independently valid free space.
    let after_reserve = reserve(evidence.after, Endpoint::After, &mut result.issues);
    if frame_valid && let Some((_, free)) = after {
        result.components = VolumeComponents {
            free_bytes: Some(free),
            reserve_bytes: after_reserve,
        };
    }
    let before = capacity(evidence.before, Endpoint::Before, &mut result.issues);
    let before_reserve = reserve(evidence.before, Endpoint::Before, &mut result.issues);
    let unresolved = unresolved_identities.max(
        allocated
            .as_ref()
            .map_or(0, |summary| summary.conflicting_identities),
    );
    if unresolved > 0 {
        result.issues.push(Issue::UnresolvedConflicts(unresolved));
    }
    if let Ok(summary) = allocated
        && summary.identity_missing_files > 0
    {
        result.issues.push(Issue::KnownSizeWithoutIdentity(
            summary.identity_missing_files,
        ));
    }
    if evidence.directories.serial64_different > 0 {
        result.issues.push(Issue::SerialMismatch(
            evidence.directories.serial64_different,
        ));
    }
    if evidence.directories.different_guid_name_unproven > 0 {
        result.issues.push(Issue::DifferentGuidUnproven(
            evidence.directories.different_guid_name_unproven,
        ));
    }
    match &evidence.filesystem {
        Ok(ObservedFilesystem::Ntfs) => {}
        Ok(_) => result.issues.push(Issue::NotNtfs),
        Err(issue) => result.issues.push(Issue::FilesystemUnavailable(*issue)),
    }
    if result.issues.is_empty()
        && let (Some((bt, bf)), Some(br), Some((at, af)), Some(ar), Ok(summary)) =
            (before, before_reserve, after, after_reserve, allocated)
    {
        // Apparent total corrects the charged record; alias records retain listed sizes.
        // This is not full per-name authoritative correction and implies no error bound.
        let differences = summary.apparent_bytes().ok().and_then(|apparent| {
            let apparent = i128::from(apparent);
            Some(EndpointDifferences {
                before: (i128::from(bt) - i128::from(bf) - i128::from(br)).checked_sub(apparent)?,
                after: (i128::from(at) - i128::from(af) - i128::from(ar)).checked_sub(apparent)?,
            })
        });
        if let Some(differences) = differences {
            if differences.before <= 0 || differences.after <= 0 {
                result.issues.push(Issue::NonPositiveDifference);
            }
            result.differences = Some(differences);
        } else {
            result.issues.push(Issue::AllocatedOverflow);
        }
    }
    result
}
