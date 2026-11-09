//! Budgeted preparation/application around the conflict-only native metadata worker.
use super::{ConflictRereadIssue, ConflictRereadReport, ScanCancellation};
use crate::index::{EntryId, FileIdentity, Index, SizeMetric, Sizes, utf16_units};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

// Paths and identities stay private and have no Debug implementation.
pub(super) struct ConflictCandidate {
    pub(super) identity: FileIdentity,
    pub(super) path: PathBuf,
}

struct PreparingPath {
    identity: FileIdentity,
    cursor: Option<EntryId>,
    names: Vec<OsString>,
    path: PathBuf,
    units: usize,
}

/// Retains at most MAX_CONFLICT_REREADS names, never a clone of the complete index.
/// Prepare on the snapshot owner; move the prepared plan to a metadata worker.
pub struct ConflictRereadPlan {
    root: PathBuf,
    pending: Vec<(FileIdentity, EntryId)>,
    current: Option<PreparingPath>,
    candidates: Vec<ConflictCandidate>,
    report: ConflictRereadReport,
}

impl ConflictRereadPlan {
    /// Only use a finished snapshot. A representative is the name at its first disagreement,
    /// not necessarily the first listed hardlink. No other name is retried.
    pub fn new(index: &Index, root: &Path) -> Self {
        let pending: Vec<_> = index.conflict_candidates().collect();
        let count = index.conflicting_identity_count();
        let now = SystemTime::now();
        let mut report = ConflictRereadReport {
            candidate_identities: count,
            attempted_identities: 0,
            resolved_identities: 0,
            unresolved_identities: count,
            logical_delta_bytes: 0,
            allocated_delta_bytes: 0,
            started_at: now,
            finished_at: now,
            issues: Default::default(),
        };
        report.issue(ConflictRereadIssue::LimitExceeded, count - pending.len());
        Self {
            root: root.to_path_buf(),
            pending,
            current: None,
            candidates: Vec::new(),
            report,
        }
    }

    pub fn candidate_count(&self) -> usize {
        self.report.candidate_identities
    }

    /// No filesystem calls. Checks the frame budget between ancestry/path components;
    /// even a zero budget permits one step. True means preparation is complete.
    pub fn prepare(&mut self, index: &Index, cancel: &ScanCancellation, budget: Duration) -> bool {
        let started = Instant::now();
        loop {
            if cancel.is_cancelled() {
                let count = self.pending.len()
                    + usize::from(self.current.is_some())
                    + self.candidates.len();
                self.report.issue(ConflictRereadIssue::Cancelled, count);
                self.pending.clear();
                self.current = None;
                self.candidates.clear();
                return true;
            }
            if self.current.is_none() {
                let Some((identity, id)) = self.pending.pop() else {
                    return true;
                };
                self.current = Some(PreparingPath {
                    identity,
                    cursor: Some(id),
                    names: Vec::new(),
                    path: self.root.clone(),
                    units: utf16_units(self.root.as_os_str().as_encoded_bytes()),
                });
            }
            let current = self.current.as_mut().expect("preparing one name");
            if current.units >= 32767 {
                self.report.issue(ConflictRereadIssue::PathTooLong, 1);
                self.current = None;
            } else if let Some(id) = current.cursor {
                if id == index.root() {
                    current.cursor = None;
                } else if let Some(entry) = index.get(id) {
                    current.units += utf16_units(entry.name.as_encoded_bytes()) + 1;
                    current.names.push(entry.name.to_os_string());
                    current.cursor = entry.parent;
                } else {
                    self.report.issue(ConflictRereadIssue::MissingName, 1);
                    self.current = None;
                }
            } else if let Some(name) = current.names.pop() {
                current.path.push(name);
            } else {
                let current = self.current.take().expect("prepared name");
                self.candidates.push(ConflictCandidate {
                    identity: current.identity,
                    path: current.path,
                });
            }
            if started.elapsed() >= budget {
                return false;
            }
        }
    }

    /// Metadata I/O only, with the same safety/identity checks as the synchronous API.
    /// This does not borrow the index. Unprepared names remain unresolved with MissingName.
    pub fn reread(mut self, cancel: &ScanCancellation) -> ConflictRereadBatch {
        self.report.issue(
            ConflictRereadIssue::MissingName,
            self.pending.len() + usize::from(self.current.is_some()),
        );
        let mut batch = ConflictRereadBatch {
            report: self.report,
            resolutions: Vec::new(),
        };
        if !self.candidates.is_empty() {
            #[cfg(windows)]
            super::platform::reread_candidates(self.candidates, &self.root, cancel, &mut batch);
            #[cfg(not(windows))]
            {
                let _ = cancel;
                batch.report.issue(
                    ConflictRereadIssue::UnsupportedPlatform,
                    self.candidates.len(),
                );
            }
        }
        batch.report.finished_at = SystemTime::now();
        batch
    }
}

/// Verified metadata and anonymous diagnostics returned by the worker. The owner must discard
/// an obsolete generation before applying any part of this batch to its finished snapshot.
pub struct ConflictRereadBatch {
    pub(super) report: ConflictRereadReport,
    pub(super) resolutions: Vec<(FileIdentity, Sizes)>,
}

impl ConflictRereadBatch {
    /// Applies corrections on the snapshot owner with a budget checked between identities.
    pub fn apply(&mut self, index: &mut Index, budget: Duration) -> bool {
        let started = Instant::now();
        while let Some((identity, sizes)) = self.resolutions.pop() {
            let before = [
                index.summary_for(SizeMetric::Logical),
                index.summary_for(SizeMetric::Allocated),
            ];
            match index.apply_authoritative_sizes(identity, sizes) {
                Ok(result) => {
                    if result.still_conflicted {
                        self.report.issue(ConflictRereadIssue::UnknownSize, 1);
                    } else {
                        self.report.resolved_identities += 1;
                    }
                    let after = [
                        index.summary_for(SizeMetric::Logical),
                        index.summary_for(SizeMetric::Allocated),
                    ];
                    for (slot, delta) in [
                        &mut self.report.logical_delta_bytes,
                        &mut self.report.allocated_delta_bytes,
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        if let (Ok(before), Ok(after)) = (before[slot], after[slot]) {
                            *delta +=
                                i128::from(after.unique_bytes) - i128::from(before.unique_bytes);
                        }
                    }
                }
                Err(_) => self.report.issue(ConflictRereadIssue::IndexRejected, 1),
            }
            self.report.unresolved_identities =
                self.report.candidate_identities - self.report.resolved_identities;
            if started.elapsed() >= budget {
                return self.resolutions.is_empty();
            }
        }
        true
    }

    pub fn into_report(mut self) -> ConflictRereadReport {
        self.report.unresolved_identities =
            self.report.candidate_identities - self.report.resolved_identities;
        self.report.finished_at = SystemTime::now();
        self.report
    }
}
