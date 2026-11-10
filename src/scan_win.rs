//! Read-only Windows directory enumeration and an explicit conflict-only metadata post-pass.
//! Enumeration never opens individual files; neither path reads file contents.
#![allow(unsafe_code)]

use crate::scan::{ScanCancellation, ScanGeneration, ScanSender};
use crate::synthetic::ProducerOutcome;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

mod conflicts;
#[cfg(windows)]
use conflicts::ConflictCandidate;
pub use conflicts::{ConflictRereadBatch, ConflictRereadPlan};

/// Producer-defined codes, deliberately outside the Win32 error range.
/// Skipped entries are not followed, not necessarily denied by permissions.
pub const SKIP_REPARSE_CODE: u32 = 0xFFFF_FE01;
pub const SKIP_CLOUD_CODE: u32 = 0xFFFF_FE02;
pub const LOCAL_STACK_LIMIT_CODE: u32 = 0xFFFF_FE03;
pub const UNSUPPORTED_PLATFORM_CODE: u32 = 0xFFFF_FE04;
pub const WORKER_FAILED_CODE: u32 = 0xFFFF_FE05;
pub const PENDING_MEMORY_LIMIT_CODE: u32 = 0xFFFF_FE06;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanError {
    pub code: u32,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Windows metadata operation failed (code {})",
            self.code
        )
    }
}
impl std::error::Error for ScanError {}

impl ScanError {
    #[cfg(windows)]
    pub(crate) fn last() -> Self {
        // SAFETY: GetLastError has no pointer or handle preconditions.
        Self {
            code: unsafe { windows_sys::Win32::Foundation::GetLastError() },
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ScanOptions {
    /// Zero selects the volume policy; explicit values are clamped to 1..=16.
    pub workers: usize,
    /// Shared queue bound, clamped to 1..=1024. Overflow stays local to a worker.
    pub queue_capacity: usize,
    /// Initial local stack reservation hint, not a correctness limit.
    pub local_stack_capacity: usize,
    /// Global live parent-chain storage budget, clamped to 1 KiB..=1 GiB.
    pub max_pending_bytes: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            workers: 0,
            queue_capacity: 1024,
            local_stack_capacity: 4096,
            max_pending_bytes: 256 << 20,
        }
    }
}

/// Local worker curves currently select four for both media, with different confidence:
/// the no-seek choice is measured on a repeated tree; the HDD choice remains provisional.
/// Unknown storage (UNC, mapped or USB drives without the query) uses the HDD choice.
/// The two constants are equal today but stay separate so each medium can be retuned.
/// This is not a portable performance guarantee.
pub fn worker_count_for(seek_penalty: Option<bool>, available: usize) -> usize {
    const SSD_WORKERS: usize = 4;
    const HDD_WORKERS: usize = 4;
    let desired = match seek_penalty {
        Some(false) => SSD_WORKERS,
        Some(true) | None => HDD_WORKERS,
    };
    desired.min(available.max(1))
}

/// Per-worker accumulated timings overlap across threads; they are not scan wall time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerMetrics {
    pub open_ns: u64,
    pub open_calls: u64,
    pub enumerate_ns: u64,
    pub enumerate_calls: u64,
    pub ancestry_ns: u64,
    pub ancestry_calls: u64,
    pub send_blocked_ns: u64,
    pub channel_full_count: u64,
    pub idle_wait_ns: u64,
    pub idle_wait_count: u64,
    pub directories: u64,
    pub records: u64,
    pub batches_sent: u64,
}

/// Counts and timings only: no scanned paths or names are retained in diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub fallback_directories: u64,
    pub last_fallback_code: Option<u32>,
    pub invalid_names: u64,
    pub reparse_skips: u64,
    pub cloud_skips: u64,
    pub local_stack_limit_hits: u64,
    pub max_queue_depth: usize,
    pub max_local_stack_depth: usize,
    pub cancellation_wakeups: u64,
    pub pending_memory_limit_hits: u64,
    pub peak_pending_bytes: usize,
    pub peak_pending_nodes: usize,
    pub pending_node_struct_bytes: usize,
    /// Directory-worker counters only: the coordinating thread's root record, final flush and
    /// progress are not included, so summing these does not give the scan's totals.
    pub workers: [WorkerMetrics; 16],
    pub selected_workers: usize,
    pub seek_penalty: Option<bool>,
    pub mft_ordered: bool,
    pub first_open_ns: Option<u64>,
    pub first_batch_sent_ns: Option<u64>,
    pub last_outcome_ns: Option<u64>,
    pub terminal_ns: Option<u64>,
}

/// Owned sidecar facts. Diagnostics remain counts/timings only; this contains no paths.
#[derive(Debug)]
pub struct ObservedScanResult {
    pub outcome: ProducerOutcome,
    pub report: ScanReport,
    pub observations: ScanOriginObservations,
}

#[derive(Debug)]
pub struct ScanOriginObservations {
    pub generation: ScanGeneration,
    pub started_at: std::time::SystemTime,
    /// Workers have joined; supplementary completion queries happen after this boundary.
    pub enumeration_finished_at: Option<std::time::SystemTime>,
    pub root: Result<crate::volume::RootObservation, crate::volume::OriginIssue>,
    pub directories: crate::volume::DirectoryNamespaceObservations,
    /// Bound to the actual root handle's candidate volume, before root enumeration.
    pub before: Result<crate::volume::OriginCapacitySample, crate::volume::OriginIssue>,
    pub after: Result<crate::volume::OriginCapacitySample, crate::volume::OriginIssue>,
    pub validation: Result<crate::volume::OriginValidation, crate::volume::OriginIssue>,
}
impl ScanOriginObservations {
    fn new(generation: ScanGeneration) -> Self {
        use crate::volume::OriginIssue;
        Self {
            generation,
            started_at: std::time::SystemTime::now(),
            enumeration_finished_at: None,
            root: Err(OriginIssue::RootNotCaptured),
            directories: Default::default(),
            before: Err(OriginIssue::RootNotCaptured),
            after: Err(OriginIssue::ScanNotFinished),
            validation: Err(OriginIssue::ScanNotFinished),
        }
    }
}

#[cfg(not(windows))]
pub fn run_win_scan_observed_with_options(
    generation: ScanGeneration,
    root: PathBuf,
    sender: ScanSender,
    cancel: &ScanCancellation,
    options: ScanOptions,
) -> ObservedScanResult {
    let (outcome, report) = run_win_scan_with_options(generation, root, sender, cancel, options);
    let mut observations = ScanOriginObservations::new(generation);
    observations.root = Err(crate::volume::OriginIssue::UnsupportedPlatform);
    ObservedScanResult {
        outcome,
        report,
        observations,
    }
}

pub fn run_win_scan(
    generation: ScanGeneration,
    root: PathBuf,
    sender: ScanSender,
    cancel: &ScanCancellation,
) -> ProducerOutcome {
    run_win_scan_with_options(generation, root, sender, cancel, ScanOptions::default()).0
}

#[cfg(not(windows))]
pub fn run_win_scan_with_options(
    generation: ScanGeneration,
    _root: PathBuf,
    sender: ScanSender,
    _cancel: &ScanCancellation,
    _options: ScanOptions,
) -> (ProducerOutcome, ScanReport) {
    let sent = crate::scan::send_terminal(
        &sender,
        generation,
        crate::scan::Terminal::Failed(UNSUPPORTED_PLATFORM_CODE),
    );
    (
        if sent {
            ProducerOutcome::Finished
        } else {
            ProducerOutcome::Disconnected
        },
        ScanReport::default(),
    )
}

/// Maximum identities selected for the explicit post-pass; excess conflicts remain unresolved.
pub use crate::index::MAX_CONFLICT_REREADS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictRereadStage {
    Path,
    Open,
    Attributes,
    Identity,
    StandardInfo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictRereadIssue {
    QueryFailed {
        stage: ConflictRereadStage,
        code: u32,
    },
    UnsafePath,
    UnsafeFile,
    IdentityMismatch,
    VolumeMismatch,
    UnknownSize,
    IndexRejected,
    MissingName,
    Cancelled,
    LimitExceeded,
    PathTooLong,
    UnsupportedPlatform,
}

/// Anonymous results of a separate post-Finished pass, never part of ScanReport.
#[derive(Debug)]
pub struct ConflictRereadReport {
    pub candidate_identities: usize,
    pub attempted_identities: usize,
    pub resolved_identities: usize,
    pub unresolved_identities: usize,
    pub logical_delta_bytes: i128,
    pub allocated_delta_bytes: i128,
    pub started_at: std::time::SystemTime,
    pub finished_at: std::time::SystemTime,
    pub issues: std::collections::BTreeMap<ConflictRereadIssue, usize>,
}
impl ConflictRereadReport {
    fn issue(&mut self, issue: ConflictRereadIssue, count: usize) {
        if count > 0 {
            *self.issues.entry(issue).or_default() += count;
        }
    }
}

/// Reopen at most one listed name per conflicting identity after Finished. Callers must not
/// run this against an index still receiving records. The legacy producer never calls it.
pub fn reread_identity_conflicts(
    index: &mut crate::index::Index,
    root: &std::path::Path,
    cancel: &ScanCancellation,
) -> ConflictRereadReport {
    let mut plan = ConflictRereadPlan::new(index, root);
    while !plan.prepare(index, cancel, std::time::Duration::MAX) {}
    let mut batch = plan.reread(cancel);
    while !batch.apply(index, std::time::Duration::MAX) {}
    batch.into_report()
}

#[cfg(windows)]
pub(crate) use platform::extended_path;
#[cfg(windows)]
pub use platform::{run_win_scan_observed_with_options, run_win_scan_with_options};

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::index::{FileIdentity, Kind, Sizes};
    use crate::scan::{
        Batch, DirectoryOutcome, NodeKey, Progress, ScanEvent, ScanEventKind, ScanRecord, ScanWake,
        SendOutcome, Terminal, send_event, send_terminal,
    };
    use std::cmp::{Ordering as CompareOrdering, Reverse};
    use std::collections::{BinaryHeap, VecDeque};
    use std::ffi::{OsStr, OsString};
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Path, Prefix};
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc::TrySendError;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_DIRECTORY, ERROR_FILE_NOT_FOUND, ERROR_FILENAME_EXCED_RANGE,
        ERROR_INVALID_DATA, ERROR_INVALID_FUNCTION, ERROR_INVALID_NAME, ERROR_INVALID_PARAMETER,
        ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::*;

    fn reread_error(stage: ConflictRereadStage, code: u32) -> ConflictRereadIssue {
        ConflictRereadIssue::QueryFailed { stage, code }
    }
    fn reread_running(cancel: &ScanCancellation) -> Result<(), ConflictRereadIssue> {
        if cancel.is_cancelled() {
            Err(ConflictRereadIssue::Cancelled)
        } else {
            Ok(())
        }
    }
    fn reread_safe_path(path: &Path, cancel: &ScanCancellation) -> Result<(), ConflictRereadIssue> {
        let absolute = std::path::absolute(path).map_err(|_| ConflictRereadIssue::UnsafePath)?;
        let mut prefix = PathBuf::new();
        for component in absolute.components() {
            reread_running(cancel)?;
            if matches!(component, Component::ParentDir) {
                return Err(ConflictRereadIssue::UnsafePath);
            }
            prefix.push(component);
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            let attrs = attributes(&prefix)
                .map_err(|error| reread_error(ConflictRereadStage::Path, error.code))?;
            if skip_code(attrs, 0).is_some() {
                return Err(ConflictRereadIssue::UnsafePath);
            }
        }
        Ok(())
    }
    fn reread_open(
        path: &Path,
        directory: bool,
        cancel: &ScanCancellation,
    ) -> Result<std::fs::File, ConflictRereadIssue> {
        use std::os::windows::fs::OpenOptionsExt;
        reread_safe_path(path, cancel)?;
        let path = extended_path(path)
            .map_err(|error| reread_error(ConflictRereadStage::Path, error.code))?;
        reread_running(cancel)?;
        std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT
                    | FILE_FLAG_OPEN_NO_RECALL
                    | if directory {
                        FILE_FLAG_BACKUP_SEMANTICS
                    } else {
                        0
                    },
            )
            .open(OsString::from_wide(&path[..path.len() - 1]))
            .map_err(|error| {
                reread_error(
                    ConflictRereadStage::Open,
                    error.raw_os_error().unwrap_or(-1) as u32,
                )
            })
    }
    fn reread_binding(
        file: &std::fs::File,
        directory: bool,
        cancel: &ScanCancellation,
    ) -> Result<(u32, FILE_ID_INFO), ConflictRereadIssue> {
        use std::os::windows::io::AsRawHandle;
        reread_running(cancel)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: live owned metadata-only handle and exact binding-defined output.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(reread_error(
                ConflictRereadStage::Attributes,
                ScanError::last().code,
            ));
        }
        if skip_code(info.dwFileAttributes, 0).is_some()
            || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        {
            return Err(ConflictRereadIssue::UnsafeFile);
        }
        reread_running(cancel)?;
        let mut identity = FILE_ID_INFO::default();
        // SAFETY: same live handle and the exact FILE_ID_INFO layout and size.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut identity as *mut FILE_ID_INFO).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        {
            return Err(reread_error(
                ConflictRereadStage::Identity,
                ScanError::last().code,
            ));
        }
        if identity.FileId.Identifier == [0; 16] {
            return Err(ConflictRereadIssue::IdentityMismatch);
        }
        Ok((info.dwVolumeSerialNumber, identity))
    }
    fn reread_file(
        path: &Path,
        expected: FileIdentity,
        root_serial: u64,
        cancel: &ScanCancellation,
    ) -> Result<Sizes, ConflictRereadIssue> {
        use std::os::windows::io::AsRawHandle;
        let file = reread_open(path, false, cancel)?;
        let (serial32, identity) = reread_binding(&file, false, cancel)?;
        if u64::from(serial32) != expected.volume || identity.VolumeSerialNumber != root_serial {
            return Err(ConflictRereadIssue::VolumeMismatch);
        }
        if u128::from_le_bytes(identity.FileId.Identifier) != expected.file {
            return Err(ConflictRereadIssue::IdentityMismatch);
        }
        reread_running(cancel)?;
        let mut standard = FILE_STANDARD_INFO::default();
        // SAFETY: verified live file handle and exact binding-defined output; no data read.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileStandardInfo,
                (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                size_of::<FILE_STANDARD_INFO>() as u32,
            )
        } == 0
        {
            return Err(reread_error(
                ConflictRereadStage::StandardInfo,
                ScanError::last().code,
            ));
        }
        if standard.Directory {
            return Err(ConflictRereadIssue::UnsafeFile);
        }
        Ok(Sizes {
            logical: u64::try_from(standard.EndOfFile).ok(),
            allocated: u64::try_from(standard.AllocationSize).ok(),
        })
    }
    pub(super) fn reread_candidates(
        candidates: Vec<ConflictCandidate>,
        root: &Path,
        cancel: &ScanCancellation,
        batch: &mut ConflictRereadBatch,
    ) {
        let report = &mut batch.report;
        let binding = (|| {
            let _mode = crate::volume::ErrorModeGuard::new()
                .map_err(|error| reread_error(ConflictRereadStage::Open, error.code))?;
            let file = reread_open(root, true, cancel)?;
            let (_, identity) = reread_binding(&file, true, cancel)?;
            Ok::<_, ConflictRereadIssue>((_mode, file, identity.VolumeSerialNumber))
        })();
        let (_mode, _root_handle, serial64) = match binding {
            Ok(binding) => binding,
            Err(issue) => {
                report.issue(issue, candidates.len());
                return;
            }
        };
        for ConflictCandidate { identity, path } in candidates {
            if cancel.is_cancelled() {
                report.issue(ConflictRereadIssue::Cancelled, 1);
                continue;
            }
            report.attempted_identities += 1;
            let sizes = match reread_file(&path, identity, serial64, cancel) {
                Ok(sizes) => sizes,
                Err(issue) => {
                    report.issue(issue, 1);
                    continue;
                }
            };
            batch.resolutions.push((identity, sizes));
        }
    }

    const BATCH_RECORDS: usize = 2000;
    const FLUSH_INTERVAL: Duration = Duration::from_millis(40);

    thread_local! {
        static METRICS: std::cell::Cell<WorkerMetrics> = const { std::cell::Cell::new(WorkerMetrics {
            open_ns: 0, open_calls: 0, enumerate_ns: 0, enumerate_calls: 0,
            ancestry_ns: 0, ancestry_calls: 0, send_blocked_ns: 0, channel_full_count: 0,
            idle_wait_ns: 0, idle_wait_count: 0, directories: 0, records: 0, batches_sent: 0,
        }) };
    }
    fn metrics(update: impl FnOnce(&mut WorkerMetrics)) {
        METRICS.with(|cell| {
            let mut value = cell.get();
            update(&mut value);
            cell.set(value);
        });
    }
    fn nanos(duration: Duration) -> u64 {
        u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
    }

    // Absolute extended paths bypass MAX_PATH without relying on the registry opt-in.
    // OsString/UTF-16 conversion preserves unpaired surrogates; no lossy names reach FFI.
    pub(crate) fn extended_path(path: &Path) -> Result<Vec<u16>, ScanError> {
        if matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::DeviceNS(_)))
        {
            return Err(ScanError {
                code: ERROR_INVALID_NAME,
            });
        }
        let absolute = std::path::absolute(path).map_err(|_| ScanError {
            code: ERROR_INVALID_NAME,
        })?;
        let mut units: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        if units.contains(&0) || units.contains(&(b'/' as u16)) {
            return Err(ScanError {
                code: ERROR_INVALID_NAME,
            });
        }
        let extended: Vec<u16> = r"\\?\".encode_utf16().collect();
        if !units.starts_with(&extended) {
            if units.starts_with(&[92, 92]) {
                units = r"\\?\UNC\"
                    .encode_utf16()
                    .chain(units.into_iter().skip(2))
                    .collect();
            } else {
                units = extended.into_iter().chain(units).collect();
            }
        }
        // Reject device namespaces and non-drive/non-UNC verbatim prefixes.
        let drive = units.len() >= 7
            && matches!(units[4], 65..=90 | 97..=122)
            && units[5] == 58
            && units[6] == 92;
        let unc = units.starts_with(&r"\\?\UNC\".encode_utf16().collect::<Vec<_>>());
        if !drive && !unc {
            return Err(ScanError {
                code: ERROR_INVALID_NAME,
            });
        }
        if units.len() >= 32767 {
            return Err(ScanError {
                code: ERROR_FILENAME_EXCED_RANGE,
            });
        }
        units.push(0);
        Ok(units)
    }

    fn valid_name(units: &[u16]) -> bool {
        !units.is_empty()
            && units.len() <= 255
            && units != [46]
            && units != [46, 46]
            && !units.iter().any(|unit| matches!(unit, 0 | 47 | 92 | 58))
    }

    fn skip_code(attributes: u32, tag: u32) -> Option<u32> {
        // CLOUD and CLOUD_1..F differ only in bits 12..15 (IO_REPARSE_TAG_CLOUD family).
        let cloud_tag = tag & !0x0000_F000 == 0x9000_001A;
        if cloud_tag
            || attributes & (FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
                != 0
        {
            Some(SKIP_CLOUD_CODE)
        } else if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            Some(SKIP_REPARSE_CODE)
        } else {
            None
        }
    }

    struct DirectoryHandle(HANDLE);
    impl Drop for DirectoryHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns exactly one valid CreateFileW handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct FindHandle(HANDLE);
    impl Drop for FindHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns exactly one valid FindFirstFileExW search handle.
            unsafe {
                FindClose(self.0);
            }
        }
    }

    #[derive(Default)]
    struct PendingMemory {
        bytes: AtomicUsize,
        peak_bytes: AtomicUsize,
        nodes: AtomicUsize,
        peak_nodes: AtomicUsize,
    }
    struct DirectoryPath {
        parent: Option<Arc<DirectoryPath>>,
        component: OsString,
        memory: Arc<PendingMemory>,
        charged_bytes: usize,
    }
    impl DirectoryPath {
        fn new(
            parent: Option<Arc<Self>>,
            component: OsString,
            memory: &Arc<PendingMemory>,
            limit: usize,
        ) -> Option<Arc<Self>> {
            // Includes Arc headers and one pending Work slot; allocator slack is not included.
            let charge =
                size_of::<Self>() + size_of::<Work>() + 2 * size_of::<usize>() + component.len();
            let previous = memory
                .bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    used.checked_add(charge).filter(|total| *total <= limit)
                })
                .ok()?;
            memory
                .peak_bytes
                .fetch_max(previous + charge, Ordering::Relaxed);
            let nodes = memory.nodes.fetch_add(1, Ordering::Relaxed) + 1;
            memory.peak_nodes.fetch_max(nodes, Ordering::Relaxed);
            Some(Arc::new(Self {
                parent,
                component,
                memory: memory.clone(),
                charged_bytes: charge,
            }))
        }
        fn path(&self) -> PathBuf {
            let mut components = Vec::new();
            let mut cursor = Some(self);
            while let Some(node) = cursor {
                components.push(&node.component);
                cursor = node.parent.as_deref();
            }
            let mut path = PathBuf::new();
            for component in components.into_iter().rev() {
                path.push(component);
            }
            path
        }
    }
    impl Drop for DirectoryPath {
        fn drop(&mut self) {
            self.memory
                .bytes
                .fetch_sub(self.charged_bytes, Ordering::AcqRel);
            self.memory.nodes.fetch_sub(1, Ordering::Relaxed);
            // Avoid recursive Arc destruction for a deep parent chain.
            let mut parent = self.parent.take();
            while let Some(node) = parent {
                match Arc::try_unwrap(node) {
                    Ok(mut node) => parent = node.parent.take(),
                    Err(_) => break,
                }
            }
        }
    }
    fn mft_order_for(seek_penalty: Option<bool>, filesystem: Option<&str>) -> bool {
        seek_penalty == Some(true) && filesystem.is_some_and(|fs| fs.eq_ignore_ascii_case("NTFS"))
    }

    fn pending_priority(file_id: Option<u128>, key: NodeKey) -> (Reverse<u64>, Reverse<u64>) {
        // NTFS sequence bits are not part of the low-48-bit MFT record number.
        // Find fallback has no identity; those entries follow known records in discovery order.
        (
            Reverse(file_id.map_or(u64::MAX, |id| (id & 0xFFFF_FFFF_FFFF) as u64)),
            Reverse(key.value()),
        )
    }

    struct Work {
        path: Arc<DirectoryPath>,
        key: NodeKey,
        file_id: Option<u128>,
    }
    impl PartialEq for Work {
        fn eq(&self, other: &Self) -> bool {
            self.cmp(other) == CompareOrdering::Equal
        }
    }
    impl Eq for Work {}
    impl PartialOrd for Work {
        fn partial_cmp(&self, other: &Self) -> Option<CompareOrdering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Work {
        fn cmp(&self, other: &Self) -> CompareOrdering {
            pending_priority(self.file_id, self.key)
                .cmp(&pending_priority(other.file_id, other.key))
        }
    }
    enum Frontier {
        DepthFirst(VecDeque<Work>),
        Mft(BinaryHeap<Work>),
    }
    impl Frontier {
        fn new(mft_order: bool, capacity: usize) -> Self {
            if mft_order {
                Self::Mft(BinaryHeap::with_capacity(capacity))
            } else {
                Self::DepthFirst(VecDeque::with_capacity(capacity))
            }
        }
        fn push(&mut self, work: Work) {
            match self {
                Self::DepthFirst(stack) => stack.push_back(work),
                Self::Mft(heap) => heap.push(work),
            }
        }
        fn pop(&mut self) -> Option<Work> {
            match self {
                Self::DepthFirst(stack) => stack.pop_back(),
                Self::Mft(heap) => heap.pop(),
            }
        }
        fn steal(&mut self) -> Option<Work> {
            match self {
                Self::DepthFirst(stack) => stack.pop_front(),
                Self::Mft(heap) => heap.pop(),
            }
        }
        fn peek(&self) -> Option<&Work> {
            match self {
                Self::DepthFirst(stack) => stack.back(),
                Self::Mft(heap) => heap.peek(),
            }
        }
        fn len(&self) -> usize {
            match self {
                Self::DepthFirst(stack) => stack.len(),
                Self::Mft(heap) => heap.len(),
            }
        }
        fn is_empty(&self) -> bool {
            self.len() == 0
        }
    }
    struct ActiveWork {
        path: PathBuf,
        node: Arc<DirectoryPath>,
        key: NodeKey,
    }
    struct Queue {
        entries: Frontier,
        outstanding: usize,
        report: ScanReport,
    }
    struct Output {
        batch: Batch,
        unpublished: Vec<Work>,
        last_flush: Instant,
        progress: Progress,
    }
    struct Shared<'a> {
        generation: ScanGeneration,
        sender: ScanSender,
        cancel: &'a ScanCancellation,
        stopped: AtomicBool,
        next_key: AtomicU64,
        queue: Mutex<Queue>,
        wake: Arc<ScanWake>,
        output: Mutex<Output>,
        options: ScanOptions,
        started: Instant,
        pending_memory: Arc<PendingMemory>,
        idle_workers: AtomicUsize,
        mft_order: bool,
        observed: Option<&'a Mutex<ObservedState>>,
    }

    struct ObservedState {
        facts: ScanOriginObservations,
        capture: Option<Arc<crate::volume::OriginCapture>>,
    }

    impl Shared<'_> {
        fn stopping(&self) -> bool {
            self.stopped.load(Ordering::Relaxed) || self.cancel.is_cancelled()
        }
        fn send(&self, event: ScanEventKind) -> bool {
            if self.stopping() {
                return false;
            }
            let envelope = ScanEvent {
                generation: self.generation,
                event,
            };
            let batch = matches!(envelope.event, ScanEventKind::Batch(_));
            let sent = match self.sender.try_send(envelope) {
                Ok(()) => true,
                Err(TrySendError::Full(event)) => {
                    let started = Instant::now();
                    let sent = send_event(&self.sender, event, self.cancel) == SendOutcome::Sent;
                    metrics(|m| {
                        m.channel_full_count += 1;
                        m.send_blocked_ns += nanos(started.elapsed());
                    });
                    sent
                }
                Err(TrySendError::Disconnected(_)) => false,
            };
            if sent {
                metrics(|m| m.batches_sent += u64::from(batch));
                if batch {
                    self.queue
                        .lock()
                        .unwrap()
                        .report
                        .first_batch_sent_ns
                        .get_or_insert(nanos(self.started.elapsed()));
                }
            } else {
                self.stopped.store(true, Ordering::Relaxed);
                self.wake.notify();
            }
            sent
        }
        fn flush(&self, output: &mut Output) -> bool {
            if output.batch.is_empty() && output.unpublished.is_empty() {
                return true;
            }
            let sent = output.batch.is_empty()
                || self.send(ScanEventKind::Batch(std::mem::take(&mut output.batch)));
            if sent {
                // Only publish stealable work after its directory record is in the channel.
                let mut queue = self.queue.lock().unwrap();
                for work in output.unpublished.drain(..) {
                    queue.entries.push(work);
                }
                queue.report.max_queue_depth =
                    queue.report.max_queue_depth.max(queue.entries.len());
                drop(queue);
                self.wake.notify();
            }
            output.last_flush = Instant::now();
            sent
        }
        fn record(&self, record: ScanRecord) -> bool {
            let mut output = self.output.lock().unwrap();
            if !output.batch.push(record.clone()) {
                if !self.flush(&mut output) {
                    return false;
                }
                // An empty batch accepts even one oversized record.
                assert!(output.batch.push(record));
            }
            output.progress.records += 1;
            metrics(|m| m.records += 1);
            if output.batch.len() >= BATCH_RECORDS || output.last_flush.elapsed() >= FLUSH_INTERVAL
            {
                return self.flush(&mut output);
            }
            true
        }
        fn directory_outcome(&self, key: NodeKey, code: Option<u32>) -> bool {
            let mut output = self.output.lock().unwrap();
            let outcome = DirectoryOutcome { key, code };
            if !output.batch.push_outcome(outcome) {
                if !self.flush(&mut output) {
                    return false;
                }
                assert!(output.batch.push_outcome(outcome));
            }
            if code.is_some() {
                output.progress.directories_failed += 1;
            } else {
                output.progress.directories_done += 1;
            }
            self.queue.lock().unwrap().report.last_outcome_ns = Some(nanos(self.started.elapsed()));
            if output.batch.len() + output.batch.outcomes().len() >= BATCH_RECORDS
                || output.last_flush.elapsed() >= FLUSH_INTERVAL
            {
                self.flush(&mut output)
            } else {
                true
            }
        }
        fn count_skip(&self, code: u32) {
            let mut queue = self.queue.lock().unwrap();
            match code {
                SKIP_CLOUD_CODE => queue.report.cloud_skips += 1,
                SKIP_REPARSE_CODE => queue.report.reparse_skips += 1,
                _ => {}
            }
        }
        fn schedule(&self, work: Work, local: &mut Frontier) -> bool {
            let mut output = self.output.lock().unwrap();
            let mut queue = self.queue.lock().unwrap();
            let shared_depth = queue.entries.len() + output.unpublished.len();
            // Shared work is published by flush only after its parent record was sent.
            if shared_depth < self.options.queue_capacity {
                output.unpublished.push(work);
                queue.outstanding += 1;
                queue.report.max_queue_depth = queue.report.max_queue_depth.max(shared_depth + 1);
            } else {
                local.push(work);
                queue.outstanding += 1;
                queue.report.max_local_stack_depth =
                    queue.report.max_local_stack_depth.max(local.len());
            }
            drop(queue);
            if self.idle_workers.load(Ordering::Relaxed) > 0 && !output.unpublished.is_empty() {
                return self.flush(&mut output);
            }
            true
        }
    }

    pub fn run_win_scan_with_options(
        generation: ScanGeneration,
        root: PathBuf,
        sender: ScanSender,
        cancel: &ScanCancellation,
        options: ScanOptions,
    ) -> (ProducerOutcome, ScanReport) {
        let seek_penalty = crate::volume::seek_penalty_for(&root);
        // Only a seek-penalty volume can enable the MFT frontier, so skip the extra query otherwise.
        let mft_order = seek_penalty == Some(true)
            && mft_order_for(
                seek_penalty,
                crate::volume::filesystem_for(&root).as_deref(),
            );
        run_scan(
            generation,
            root,
            sender,
            cancel,
            options,
            seek_penalty,
            mft_order,
        )
    }

    /// Observation preserves the legacy device hint, worker policy and NTFS MFT frontier.
    pub fn run_win_scan_observed_with_options(
        generation: ScanGeneration,
        root: PathBuf,
        sender: ScanSender,
        cancel: &ScanCancellation,
        options: ScanOptions,
    ) -> ObservedScanResult {
        let observed = Mutex::new(ObservedState {
            facts: ScanOriginObservations::new(generation),
            capture: None,
        });
        let seek_penalty = crate::volume::seek_penalty_for(&root);
        let mft_order = seek_penalty == Some(true)
            && mft_order_for(
                seek_penalty,
                crate::volume::filesystem_for(&root).as_deref(),
            );
        let (outcome, report) = run_scan_inner(
            generation,
            root,
            sender,
            cancel,
            options,
            (seek_penalty, mft_order),
            Some(&observed),
        );
        let facts = observed
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .facts;
        ObservedScanResult {
            outcome,
            report,
            observations: facts,
        }
    }

    fn run_scan(
        generation: ScanGeneration,
        root: PathBuf,
        sender: ScanSender,
        cancel: &ScanCancellation,
        options: ScanOptions,
        seek_penalty: Option<bool>,
        mft_order: bool,
    ) -> (ProducerOutcome, ScanReport) {
        run_scan_inner(
            generation,
            root,
            sender,
            cancel,
            options,
            (seek_penalty, mft_order),
            None,
        )
    }

    fn run_scan_inner(
        generation: ScanGeneration,
        root: PathBuf,
        sender: ScanSender,
        cancel: &ScanCancellation,
        options: ScanOptions,
        policy: (Option<bool>, bool),
        observed: Option<&Mutex<ObservedState>>,
    ) -> (ProducerOutcome, ScanReport) {
        let (seek_penalty, mft_order) = policy;
        let available = thread::available_parallelism().map_or(1, usize::from);
        let options = ScanOptions {
            workers: if options.workers == 0 {
                worker_count_for(seek_penalty, available)
            } else {
                options.workers.clamp(1, 16)
            },
            queue_capacity: options.queue_capacity.clamp(1, 1024),
            local_stack_capacity: options.local_stack_capacity.clamp(1, 4096),
            max_pending_bytes: options.max_pending_bytes.clamp(1024, 1 << 30),
        };
        let root_key = NodeKey::new(0);
        let root_name = root
            .file_name()
            .filter(|name| valid_name(&name.encode_wide().collect::<Vec<_>>()))
            .unwrap_or(OsStr::new("Volume"))
            .to_owned();
        let pending_memory = Arc::new(PendingMemory::default());
        let Some(root_path) = DirectoryPath::new(
            None,
            root.into_os_string(),
            &pending_memory,
            options.max_pending_bytes,
        ) else {
            let sent = send_terminal(
                &sender,
                generation,
                Terminal::Failed(PENDING_MEMORY_LIMIT_CODE),
            );
            return (
                if sent {
                    ProducerOutcome::Finished
                } else {
                    ProducerOutcome::Disconnected
                },
                ScanReport {
                    pending_memory_limit_hits: 1,
                    selected_workers: options.workers,
                    seek_penalty,
                    mft_ordered: mft_order,
                    ..ScanReport::default()
                },
            );
        };
        let shared = Shared {
            generation,
            sender,
            cancel,
            stopped: AtomicBool::new(false),
            next_key: AtomicU64::new(1),
            queue: Mutex::new(Queue {
                entries: {
                    let mut frontier = Frontier::new(mft_order, options.queue_capacity);
                    frontier.push(Work {
                        path: root_path,
                        key: root_key,
                        file_id: None,
                    });
                    frontier
                },
                outstanding: 1,
                report: ScanReport {
                    max_queue_depth: 1,
                    selected_workers: options.workers,
                    seek_penalty,
                    mft_ordered: mft_order,
                    ..ScanReport::default()
                },
            }),
            wake: Arc::new(ScanWake::default()),
            output: Mutex::new(Output {
                batch: Batch::new(),
                unpublished: Vec::new(),
                last_flush: Instant::now(),
                progress: Progress::default(),
            }),
            options,
            started: Instant::now(),
            pending_memory,
            idle_workers: AtomicUsize::new(0),
            mft_order,
            observed,
        };
        cancel.register(&shared.wake);
        let root_sent = shared.record(ScanRecord {
            key: root_key,
            parent: None,
            name: root_name,
            kind: Kind::Directory,
            sizes: Sizes::default(),
            identity: None,
        }) && shared.flush(&mut shared.output.lock().unwrap());
        let mut worker_failed = false;
        if root_sent {
            thread::scope(|scope| {
                let shared = &shared;
                let mut workers = Vec::new();
                for worker_id in 0..options.workers {
                    match thread::Builder::new()
                        .name("hogmap-directory".into())
                        .spawn_scoped(scope, move || {
                            let shared = &shared;
                            // Catch before joining other workers, so a panic cannot strand idle waiters.
                            if !std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                let result = worker(shared);
                                shared.queue.lock().unwrap().report.workers[worker_id] =
                                    METRICS.with(|cell| cell.get());
                                result
                            }))
                            .unwrap_or(false)
                            {
                                shared.stopped.store(true, Ordering::Relaxed);
                                shared.wake.notify();
                                false
                            } else {
                                true
                            }
                        }) {
                        Ok(worker) => workers.push(worker),
                        Err(_) => {
                            worker_failed = true;
                            shared.stopped.store(true, Ordering::Relaxed);
                            shared.wake.notify();
                            break;
                        }
                    }
                }
                for worker in workers {
                    if !worker.join().unwrap_or(false) {
                        worker_failed = true;
                    }
                }
            });
        }
        let terminal = if shared
            .queue
            .lock()
            .unwrap()
            .report
            .pending_memory_limit_hits
            > 0
        {
            Terminal::Failed(PENDING_MEMORY_LIMIT_CODE)
        } else if worker_failed {
            Terminal::Failed(WORKER_FAILED_CODE)
        } else if cancel.is_cancelled() {
            Terminal::Cancelled
        } else {
            Terminal::Finished
        };
        if let Ok(mut output) = shared.output.lock() {
            if !shared.stopping() {
                let _ = shared.flush(&mut output);
            }
            // Final counters are essential, unlike periodic coalescable progress.
            if !shared.stopping() {
                let _ = shared.send(ScanEventKind::Progress(output.progress));
            }
        }
        if let Some(observed) = shared.observed {
            use crate::volume::{ObservedContinuity, OriginIssue};
            let finished_at = std::time::SystemTime::now();
            let capture = observed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .capture
                .clone();
            let mut omitted = if cancel.is_cancelled() {
                Some(OriginIssue::Cancelled)
            } else if terminal != Terminal::Finished || shared.stopping() {
                Some(OriginIssue::ScanNotFinished)
            } else {
                None
            };
            let mode = omitted.is_none().then(crate::volume::ErrorModeGuard::new);
            if let Some(Err(error)) = &mode {
                omitted = Some(OriginIssue::QueryFailed {
                    query: crate::volume::OriginQuery::ErrorMode,
                    code: error.code,
                });
            }
            let (after, validation) = if let Some(reason) = omitted {
                (Err(reason), Err(reason))
            } else if let Some(capture) = capture.as_ref() {
                let validation = capture.validate(&|| shared.stopping());
                let after = match (validation.directory_identity, validation.normalized_path) {
                    (
                        Ok(ObservedContinuity::UnchangedAtEndpoints),
                        Ok(ObservedContinuity::UnchangedAtEndpoints),
                    ) => capture.capacity(&|| shared.stopping()),
                    (Ok(ObservedContinuity::Changed), _) => Err(OriginIssue::IdentityChanged),
                    (_, Ok(ObservedContinuity::Changed)) => Err(OriginIssue::NamespaceChanged),
                    (Err(error), _) | (_, Err(error)) => Err(error),
                };
                (after, Ok(validation))
            } else {
                (
                    Err(OriginIssue::RootNotCaptured),
                    Err(OriginIssue::RootNotCaptured),
                )
            };
            let mut state = observed.lock().unwrap_or_else(|p| p.into_inner());
            state.facts.enumeration_finished_at = Some(finished_at);
            state.facts.after = after;
            state.facts.validation = validation;
        }
        // All scoped workers have joined. Cancellation never suppresses the one terminal attempt.
        let sent = send_terminal(&shared.sender, generation, terminal);
        shared.queue.lock().unwrap().report.terminal_ns = Some(nanos(shared.started.elapsed()));
        {
            let mut queue = shared.queue.lock().unwrap();
            queue.report.peak_pending_bytes =
                shared.pending_memory.peak_bytes.load(Ordering::Relaxed);
            queue.report.peak_pending_nodes =
                shared.pending_memory.peak_nodes.load(Ordering::Relaxed);
            queue.report.pending_node_struct_bytes =
                size_of::<DirectoryPath>() + size_of::<Work>() + 2 * size_of::<usize>();
        }
        let report = shared
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .report;
        let outcome = if !sent {
            ProducerOutcome::Disconnected
        } else if terminal == Terminal::Cancelled {
            ProducerOutcome::Cancelled
        } else {
            ProducerOutcome::Finished
        };
        (outcome, report)
    }

    fn worker(shared: &Shared<'_>) -> bool {
        let Ok(_error_mode) = crate::volume::ErrorModeGuard::new() else {
            return false;
        };
        let mut local = Frontier::new(shared.mft_order, shared.options.local_stack_capacity);
        let mut observation_buffer = shared.observed.map(|_| vec![0_u16; 32768]);
        loop {
            if shared.stopping() {
                shared.wake.notify();
                return true;
            }
            // Both heaps participate in HDD selection; queue overflow must not revert to DFS.
            // Entries move between frontiers, never into a second retained ordering index.
            let shared_work = if shared.mft_order {
                let mut queue = shared.queue.lock().unwrap();
                if queue
                    .entries
                    .peek()
                    .is_some_and(|candidate| local.peek().is_none_or(|work| candidate > work))
                {
                    queue.entries.pop()
                } else {
                    None
                }
            } else {
                None
            };
            let work = if let Some(work) = shared_work.or_else(|| local.pop()) {
                work
            } else {
                let needs_work = shared.queue.lock().unwrap().entries.is_empty();
                if needs_work && !shared.flush(&mut shared.output.lock().unwrap()) {
                    return true;
                }
                loop {
                    let listener = shared.wake.listen();
                    let mut queue = shared.queue.lock().unwrap();
                    if shared.stopping() {
                        if shared.cancel.is_cancelled() {
                            queue.report.cancellation_wakeups += 1;
                        }
                        return true;
                    }
                    if let Some(work) = queue.entries.steal() {
                        break work;
                    }
                    if queue.outstanding == 0 {
                        return true;
                    }
                    shared.idle_workers.fetch_add(1, Ordering::Relaxed);
                    drop(queue);
                    let started = Instant::now();
                    shared.wake.wait(listener);
                    shared.idle_workers.fetch_sub(1, Ordering::Relaxed);
                    metrics(|m| {
                        m.idle_wait_count += 1;
                        m.idle_wait_ns += nanos(started.elapsed());
                    });
                }
            };
            let work = ActiveWork {
                path: work.path.path(),
                node: work.path,
                key: work.key,
            };
            metrics(|m| m.directories += 1);
            let result = enumerate(&work, shared, &mut local, observation_buffer.as_deref_mut());
            if !shared.stopping() {
                let _ = shared.directory_outcome(work.key, result.err().map(|error| error.code));
            }
            let mut queue = shared.queue.lock().unwrap();
            queue.outstanding -= 1;
            let finished = queue.outstanding == 0;
            drop(queue);
            if finished {
                shared.wake.notify();
            }
        }
    }

    struct Child {
        name: Vec<u16>,
        attributes: u32,
        tag: u32,
        sizes: Sizes,
        identity: Option<FileIdentity>,
    }
    fn child(work: &ActiveWork, shared: &Shared<'_>, local: &mut Frontier, child: Child) -> bool {
        if shared.stopping() {
            return false;
        }
        if child.name == [46] || child.name == [46, 46] {
            return true;
        }
        if !valid_name(&child.name) {
            shared.queue.lock().unwrap().report.invalid_names += 1;
            return true;
        }
        let skip = skip_code(child.attributes, child.tag);
        if let Some(code) = skip {
            shared.count_skip(code);
        }
        let kind = if child.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            Kind::Directory
        } else {
            Kind::File
        };
        let key = NodeKey::new(shared.next_key.fetch_add(1, Ordering::Relaxed));
        let name = OsString::from_wide(&child.name);
        // The shared output lock orders this record before any descendant can be appended,
        // even when another worker takes the directory before the batch is flushed.
        if !shared.record(ScanRecord {
            key,
            parent: Some(work.key),
            name: name.clone(),
            kind,
            sizes: if skip.is_some() {
                Sizes::default()
            } else {
                child.sizes
            },
            identity: child.identity,
        }) {
            return false;
        }
        if kind == Kind::Directory {
            if let Some(code) = skip {
                shared.directory_outcome(key, Some(code))
            } else {
                if let Some(path) = DirectoryPath::new(
                    Some(work.node.clone()),
                    name,
                    &shared.pending_memory,
                    shared.options.max_pending_bytes,
                ) {
                    shared.schedule(
                        Work {
                            path,
                            key,
                            file_id: child.identity.map(|identity| identity.file),
                        },
                        local,
                    )
                } else {
                    shared
                        .queue
                        .lock()
                        .unwrap()
                        .report
                        .pending_memory_limit_hits += 1;
                    shared.directory_outcome(key, Some(PENDING_MEMORY_LIMIT_CODE))
                }
            }
        } else {
            true
        }
    }

    fn attributes(path: &Path) -> Result<u32, ScanError> {
        let path = extended_path(path)?;
        // SAFETY: the input path is NUL-terminated and remains alive for this call.
        let attributes = unsafe { GetFileAttributesW(path.as_ptr()) };
        if attributes == INVALID_FILE_ATTRIBUTES {
            Err(ScanError::last())
        } else {
            Ok(attributes)
        }
    }

    fn check_path(path: &Path, shared: &Shared<'_>) -> Result<(), ScanError> {
        // Check ancestors too: opening an ordinary child of a junction could otherwise follow it.
        let absolute = std::path::absolute(path).map_err(|_| ScanError {
            code: ERROR_INVALID_NAME,
        })?;
        let mut prefix = PathBuf::new();
        for component in absolute.components() {
            if shared.stopping() {
                return Err(ScanError {
                    code: windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED,
                });
            }
            if matches!(component, Component::ParentDir) {
                return Err(ScanError {
                    code: ERROR_INVALID_NAME,
                });
            }
            prefix.push(component);
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            check_directory(&prefix, shared)?;
        }
        Ok(())
    }

    fn check_directory(path: &Path, shared: &Shared<'_>) -> Result<(), ScanError> {
        let started = Instant::now();
        let observed = attributes(path);
        metrics(|m| {
            m.ancestry_calls += 1;
            m.ancestry_ns += nanos(started.elapsed());
        });
        let attributes = observed?;
        if let Some(code) = skip_code(attributes, 0) {
            shared.count_skip(code);
            return Err(ScanError { code });
        }
        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(ScanError {
                code: ERROR_DIRECTORY,
            });
        }
        Ok(())
    }

    fn enumerate(
        work: &ActiveWork,
        shared: &Shared<'_>,
        local: &mut Frontier,
        observation_buffer: Option<&mut [u16]>,
    ) -> Result<(), ScanError> {
        if work.key == NodeKey::new(0) {
            check_path(&work.path, shared)?;
        }
        let path = extended_path(&work.path)?;
        if shared.stopping() {
            return Ok(());
        }
        // SAFETY: NUL-terminated path, null security/template, metadata-only directory access.
        self_first_open(shared);
        let open_started = Instant::now();
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS
                    | FILE_FLAG_OPEN_REPARSE_POINT
                    | FILE_FLAG_OPEN_NO_RECALL,
                null_mut(),
            )
        };
        metrics(|m| {
            m.open_calls += 1;
            m.open_ns += nanos(open_started.elapsed());
        });
        if handle == INVALID_HANDLE_VALUE {
            let error = ScanError::last();
            if fallback_allowed(error.code) {
                return fallback(work, shared, local, error.code);
            }
            return Err(error);
        }
        let handle = DirectoryHandle(handle);
        if let Some(observed) = shared.observed {
            let mut state = observed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.facts.directories.opened_directories += 1;
            state.facts.directories.not_queried += 1;
        }
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        if shared.stopping() {
            return Ok(());
        }
        // SAFETY: handle is owned and open; the output has the exact Win32 layout and size.
        if unsafe { GetFileInformationByHandle(handle.0, &mut information) } == 0 {
            return Err(ScanError::last());
        }
        // Validate the object actually opened, not another path lookup. Ordinary descendants
        // need only the attributes already returned with the volume serial. Query the tag for
        // the root and observed reparse objects, where it distinguishes cloud from other skips.
        let mut current_attributes = information.dwFileAttributes;
        let mut reparse_tag = 0;
        if work.key == NodeKey::new(0) || current_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
            if shared.stopping() {
                return Ok(());
            }
            // SAFETY: owned live handle and writable fixed-size FILE_ATTRIBUTE_TAG_INFO output.
            if unsafe {
                GetFileInformationByHandleEx(
                    handle.0,
                    FileAttributeTagInfo,
                    (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                    size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
                )
            } != 0
            {
                current_attributes = tag.FileAttributes;
                reparse_tag = tag.ReparseTag;
            } else if current_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                // Some redirectors and filters do not support the tag query. A reparse object
                // without a readable tag is still never enumerated or followed.
                shared.count_skip(SKIP_REPARSE_CODE);
                return Err(ScanError {
                    code: SKIP_REPARSE_CODE,
                });
            }
            // An ordinary root whose filesystem lacks the tag query keeps the attributes already
            // read from the handle, as before; cloud recall bits are still checked below.
        }
        if let Some(code) = skip_code(current_attributes, reparse_tag) {
            shared.count_skip(code);
            return Err(ScanError { code });
        }
        if current_attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(ScanError {
                code: ERROR_DIRECTORY,
            });
        }
        if let (Some(observed), Some(buffer)) = (shared.observed, observation_buffer) {
            use std::os::windows::io::BorrowedHandle;
            // SAFETY: DirectoryHandle owns this live handle for the entire observation call.
            let borrowed = unsafe { BorrowedHandle::borrow_raw(handle.0) };
            let capture = if work.key == NodeKey::new(0) {
                // No child can be published before this root capture and before sample finish.
                let (root, capture) =
                    crate::volume::capture_origin(borrowed, &|| shared.stopping());
                let before = capture
                    .as_ref()
                    .map_err(|error| *error)
                    .and_then(|capture| capture.capacity(&|| shared.stopping()));
                let capture = capture.ok().map(Arc::new);
                let mut state = observed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.facts.root = Ok(root);
                state.facts.before = before;
                state.capture = capture.clone();
                capture
            } else {
                observed
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .capture
                    .clone()
            };
            let observation =
                crate::volume::observe_namespace(capture.as_deref(), borrowed, buffer, &|| {
                    shared.stopping()
                });
            let mut state = observed.lock().unwrap_or_else(|p| p.into_inner());
            crate::volume::record_namespace_result(&mut state.facts.directories, observation);
        }
        // u64 storage guarantees 8-byte alignment; 64 KiB bounds one enumeration response.
        let mut buffer = vec![0_u64; 8192];
        let mut started = false;
        loop {
            if shared.stopping() {
                return Ok(());
            }
            buffer.fill(0);
            // SAFETY: buffer is 8-byte aligned, writable for 64 KiB, and the handle is a directory.
            let enumerate_started = Instant::now();
            let success = unsafe {
                GetFileInformationByHandleEx(
                    handle.0,
                    FileIdExtdDirectoryInfo,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                )
            } != 0;
            metrics(|m| {
                m.enumerate_calls += 1;
                m.enumerate_ns += nanos(enumerate_started.elapsed());
            });
            if !success {
                let error = ScanError::last();
                if error.code == ERROR_NO_MORE_FILES {
                    return Ok(());
                }
                if !started && fallback_allowed(error.code) {
                    return fallback(work, shared, local, error.code);
                }
                return Err(error);
            }
            started = true;
            parse_buffer(&buffer, information.dwVolumeSerialNumber, |entry| {
                child(work, shared, local, entry)
            })?;
        }
    }

    fn self_first_open(shared: &Shared<'_>) {
        shared
            .queue
            .lock()
            .unwrap()
            .report
            .first_open_ns
            .get_or_insert(nanos(shared.started.elapsed()));
    }

    fn fallback_allowed(code: u32) -> bool {
        matches!(
            code,
            ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER | ERROR_INVALID_FUNCTION
        )
    }

    fn parse_buffer(
        buffer: &[u64],
        volume: u32,
        mut accept: impl FnMut(Child) -> bool,
    ) -> Result<(), ScanError> {
        let bytes = buffer.len() * 8;
        let header = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        let malformed = || ScanError {
            code: ERROR_INVALID_DATA,
        };
        let mut offset = 0_usize;
        loop {
            if !offset.is_multiple_of(8)
                || offset
                    .checked_add(size_of::<FILE_ID_EXTD_DIR_INFO>())
                    .is_none_or(|end| end > bytes)
            {
                return Err(malformed());
            }
            // SAFETY: aligned offset and full fixed header were checked inside the allocated buffer.
            let entry = unsafe {
                &*buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_EXTD_DIR_INFO>()
            };
            let name_bytes = entry.FileNameLength as usize;
            let end = offset
                .checked_add(header)
                .and_then(|start| start.checked_add(name_bytes))
                .ok_or_else(malformed)?;
            let next = entry.NextEntryOffset as usize;
            if !name_bytes.is_multiple_of(2)
                || end > bytes
                || (next != 0
                    && (!next.is_multiple_of(8)
                        || next < header + name_bytes
                        || offset + next >= bytes))
            {
                return Err(malformed());
            }
            // SAFETY: the variable-length UTF-16 tail is aligned and its complete range was checked.
            let name = unsafe {
                std::slice::from_raw_parts(
                    buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(offset + header)
                        .cast::<u16>(),
                    name_bytes / 2,
                )
            };
            let child = Child {
                name: name.to_vec(),
                attributes: entry.FileAttributes,
                tag: entry.ReparsePointTag,
                sizes: Sizes {
                    logical: u64::try_from(entry.EndOfFile).ok(),
                    allocated: u64::try_from(entry.AllocationSize).ok(),
                },
                // MS-FSCC requires ignoring FileId=0 when 128-bit IDs are unsupported.
                identity: (entry.FileId.Identifier != [0; 16]).then(|| FileIdentity {
                    volume: u64::from(volume),
                    file: u128::from_le_bytes(entry.FileId.Identifier),
                }),
            };
            if !accept(child) || next == 0 {
                return Ok(());
            }
            offset += next;
        }
    }

    fn fallback(
        work: &ActiveWork,
        shared: &Shared<'_>,
        local: &mut Frontier,
        code: u32,
    ) -> Result<(), ScanError> {
        {
            let mut queue = shared.queue.lock().unwrap();
            queue.report.fallback_directories += 1;
            queue.report.last_fallback_code = Some(code);
        }
        check_directory(&work.path, shared)?;
        let pattern = extended_path(&work.path.join("*"))?;
        let mut entry = WIN32_FIND_DATAW::default();
        if shared.stopping() {
            return Ok(());
        }
        // SAFETY: pattern is NUL-terminated, output has Win32 layout; no filter pointer is required.
        let handle = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FindExInfoBasic,
                (&mut entry as *mut WIN32_FIND_DATAW).cast(),
                FindExSearchNameMatch,
                null(),
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let error = ScanError::last();
            // An empty wildcard match is only success if the directory still exists and is safe.
            if error.code == ERROR_FILE_NOT_FOUND {
                check_directory(&work.path, shared)?;
                return Ok(());
            }
            return Err(error);
        }
        let handle = FindHandle(handle);
        loop {
            let end = entry
                .cFileName
                .iter()
                .position(|unit| *unit == 0)
                .ok_or(ScanError {
                    code: ERROR_INVALID_DATA,
                })?;
            let accepted = child(
                work,
                shared,
                local,
                Child {
                    name: entry.cFileName[..end].to_vec(),
                    attributes: entry.dwFileAttributes,
                    tag: entry.dwReserved0,
                    sizes: Sizes {
                        logical: Some(
                            (u64::from(entry.nFileSizeHigh) << 32) | u64::from(entry.nFileSizeLow),
                        ),
                        allocated: None,
                    },
                    identity: None,
                },
            );
            if !accepted || shared.stopping() {
                return Ok(());
            }
            // SAFETY: owned search handle and valid writable WIN32_FIND_DATAW output.
            if unsafe { FindNextFileW(handle.0, &mut entry) } == 0 {
                let error = ScanError::last();
                return if error.code == ERROR_NO_MORE_FILES {
                    Ok(())
                } else {
                    Err(error)
                };
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::scan::{ScanEvent, ScanReducer};
        use std::collections::BTreeMap;
        use windows_sys::Win32::Foundation::{ERROR_PATH_NOT_FOUND, GetLastError};

        // Keep FFI oracles inside the existing audited boundary, but exercise only the public
        // scanner API. Neither oracle uses the scanner's path conversion or enumeration parser.
        struct OwnedTree(PathBuf);
        impl OwnedTree {
            fn new() -> Self {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = std::env::temp_dir()
                    .join(format!("hogmap-boundary-{}-{nanos}", std::process::id()));
                std::fs::create_dir(&path).unwrap();
                Self(path)
            }
        }
        impl Drop for OwnedTree {
            fn drop(&mut self) {
                let result = std::fs::remove_dir_all(fixture_extended(&self.0));
                if let Err(error) = result {
                    eprintln!("boundary_fixture_cleanup_error={:?}", error.raw_os_error());
                    assert!(thread::panicking(), "owned tree cleanup failed");
                }
            }
        }
        fn fixture_extended(path: &Path) -> PathBuf {
            let mut name = OsString::from(r"\\?\");
            name.push(path);
            PathBuf::from(name)
        }
        type ListedTree = BTreeMap<PathBuf, (Kind, Option<u64>)>;

        fn find_basic_walk(root: &Path) -> ListedTree {
            struct FindHandle(HANDLE);
            impl Drop for FindHandle {
                fn drop(&mut self) {
                    // SAFETY: this guard owns a successful FindFirstFileExW search handle.
                    unsafe {
                        FindClose(self.0);
                    }
                }
            }
            let mut pending = vec![PathBuf::new()];
            let mut entries = BTreeMap::new();
            while let Some(relative) = pending.pop() {
                let pattern: Vec<u16> = fixture_extended(&root.join(&relative))
                    .join("*")
                    .as_os_str()
                    .encode_wide()
                    .chain(Some(0))
                    .collect();
                let mut found = WIN32_FIND_DATAW::default();
                // SAFETY: terminated wildcard and exact writable binding output; no contents.
                let handle = unsafe {
                    FindFirstFileExW(
                        pattern.as_ptr(),
                        FindExInfoBasic,
                        (&mut found as *mut WIN32_FIND_DATAW).cast(),
                        FindExSearchNameMatch,
                        null(),
                        0,
                    )
                };
                assert_ne!(handle, INVALID_HANDLE_VALUE, "oracle open: {}", unsafe {
                    GetLastError()
                });
                let _handle = FindHandle(handle);
                loop {
                    let end = found.cFileName.iter().position(|unit| *unit == 0).unwrap();
                    let name = OsString::from_wide(&found.cFileName[..end]);
                    if name != "." && name != ".." {
                        assert_eq!(found.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT, 0);
                        let child = relative.join(name);
                        let directory = found.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                        let logical = (!directory).then_some(
                            (u64::from(found.nFileSizeHigh) << 32) | u64::from(found.nFileSizeLow),
                        );
                        assert!(
                            entries
                                .insert(
                                    child.clone(),
                                    (
                                        if directory {
                                            Kind::Directory
                                        } else {
                                            Kind::File
                                        },
                                        logical
                                    )
                                )
                                .is_none()
                        );
                        if directory {
                            pending.push(child);
                        }
                    }
                    // SAFETY: live search handle and writable binding output.
                    if unsafe { FindNextFileW(handle, &mut found) } == 0 {
                        assert_eq!(unsafe { GetLastError() }, ERROR_NO_MORE_FILES);
                        break;
                    }
                }
            }
            entries
        }

        fn scan_owned_tree(
            root: PathBuf,
            sender: ScanSender,
            receiver: crate::scan::ScanReceiver,
            mut inspect: impl FnMut(&ScanEvent),
        ) -> (ScanReducer, ScanReport) {
            let generation = ScanGeneration::new(960);
            let worker = thread::spawn(move || {
                crate::scan_win::run_win_scan_with_options(
                    generation,
                    root,
                    sender,
                    &ScanCancellation::new(),
                    ScanOptions {
                        workers: 1,
                        ..ScanOptions::default()
                    },
                )
            });
            let mut events = Vec::new();
            loop {
                match receiver.recv_timeout(Duration::from_secs(10)) {
                    Ok(event) => {
                        inspect(&event);
                        events.push(event);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(error) => {
                        drop(receiver); // Release backpressure before joining on a failed test.
                        panic!("owned scan did not finish: {error}");
                    }
                }
            }
            let (outcome, report) = worker.join().unwrap();
            assert_eq!(outcome, ProducerOutcome::Finished);
            assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
            let mut reducer = ScanReducer::new(generation);
            let (sender, receiver) = crate::scan::event_channel(1);
            for event in events {
                sender.send(event).unwrap();
                reducer.drain(&receiver, 4096);
            }
            assert_eq!(reducer.terminal(), Some(Terminal::Finished));
            assert_eq!(reducer.diagnostics().unknown_parent_records, 0);
            assert_eq!(reducer.diagnostics().invalid_records, 0);
            (reducer, report)
        }

        fn indexed_tree(reducer: &ScanReducer) -> ListedTree {
            let index = reducer.index().unwrap();
            let mut paths = BTreeMap::new();
            let mut entries = BTreeMap::new();
            for (id, entry) in index.entries() {
                let path = if let Some(parent) = entry.parent {
                    let parent: &PathBuf = paths.get(&parent).expect("parent before child");
                    parent.join(entry.name)
                } else {
                    PathBuf::new()
                };
                if entry.parent.is_some() {
                    entries.insert(
                        path.clone(),
                        (
                            entry.kind,
                            if entry.kind == Kind::File {
                                entry.sizes.logical
                            } else {
                                None
                            },
                        ),
                    );
                }
                paths.insert(id, path);
            }
            entries
        }

        #[test]
        fn owned_tree_counts_names_and_lengths_agree_with_find_basic_walk() {
            let fixture = OwnedTree::new();
            std::fs::create_dir_all(fixture.0.join("nested/雪-😀/empty")).unwrap();
            std::fs::create_dir(fixture.0.join("empty")).unwrap();
            std::fs::write(fixture.0.join("nested/雪-😀/known"), [1; 11]).unwrap();
            std::fs::write(fixture.0.join("first"), [2; 7]).unwrap();
            std::fs::hard_link(fixture.0.join("first"), fixture.0.join("second")).unwrap();
            for number in 0..96 {
                std::fs::write(
                    fixture.0.join(format!("sibling-{number:03}")),
                    vec![0; number],
                )
                .unwrap();
            }
            let expected = find_basic_walk(&fixture.0);
            assert_eq!(
                expected
                    .values()
                    .filter(|(kind, _)| *kind == Kind::Directory)
                    .count(),
                4
            );
            assert_eq!(
                expected
                    .values()
                    .filter(|(kind, _)| *kind == Kind::File)
                    .count(),
                99
            );
            let (sender, receiver) = crate::scan::event_channel(1);
            let (reducer, report) = scan_owned_tree(fixture.0.clone(), sender, receiver, |_| {});
            assert_eq!(
                report.fallback_directories, 0,
                "primary path must be exercised"
            );
            assert_eq!(indexed_tree(&reducer), expected);
            let summary = reducer.index().unwrap().summary().unwrap();
            assert_eq!(summary.files, 99);
            assert_eq!(summary.known_logical_bytes, 4585);
            assert!(summary.complete);
            assert_eq!(reducer.diagnostics().directory_failures, 0);
        }

        #[test]
        fn concurrent_owned_changes_preserve_listed_sizes_and_report_vanished_directory() {
            use std::sync::mpsc;
            let fixture = OwnedTree::new();
            std::fs::create_dir(fixture.0.join("vanishing")).unwrap();
            std::fs::write(fixture.0.join("removed"), [0; 7]).unwrap();
            std::fs::write(fixture.0.join("stable"), [0; 11]).unwrap();
            let (sender, receiver) = crate::scan::event_channel(1);
            let gated = Arc::new(AtomicBool::new(true));
            let gate = gated.clone();
            let gate_failed = Arc::new(AtomicBool::new(false));
            let wake_failed = gate_failed.clone();
            let (ack, released) = mpsc::channel();
            let released = Mutex::new(released);
            sender.set_waker(move || {
                if !gate.load(Ordering::SeqCst) {
                    return;
                }
                let released = released
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !gate.load(Ordering::SeqCst) {
                    return;
                }
                // Drop can invoke this waker again during unwind: report ACK failure to the
                // test instead of panicking here and risking a second panic from destruction.
                if released.recv_timeout(Duration::from_secs(10)).is_err() {
                    wake_failed.store(true, Ordering::SeqCst);
                    gate.store(false, Ordering::SeqCst);
                }
            });
            let mut vanished = None;
            let mut failure = None;
            let mut changed = false;
            let mut listed = BTreeMap::new();
            let mut seen = std::collections::HashSet::new();
            let (reducer, _) = scan_owned_tree(fixture.0.clone(), sender, receiver, |event| {
                if let ScanEventKind::Batch(batch) = &event.event {
                    for record in batch.records() {
                        if let Some(parent) = record.parent {
                            assert!(seen.contains(&parent));
                        }
                        assert!(seen.insert(record.key));
                        if record.kind == Kind::File {
                            let expected = match record.name.to_str().unwrap() {
                                "stable" => 11,
                                "removed" => 7,
                                "created" => 13,
                                other => panic!("unexpected owned file {other}"),
                            };
                            assert!(record.sizes.logical.is_none_or(|value| value == expected));
                            listed.insert(record.name.clone(), record.sizes);
                        }
                        if record.name == "vanishing" {
                            vanished = Some(record.key);
                        }
                    }
                    if vanished.is_some() && !changed {
                        // The public transport waker holds this single producer at publication,
                        // before it can open the discovered directory. No timing sleeps needed.
                        std::fs::remove_dir(fixture.0.join("vanishing")).unwrap();
                        std::fs::remove_file(fixture.0.join("removed")).unwrap();
                        std::fs::write(fixture.0.join("created"), [0; 13]).unwrap();
                        std::fs::create_dir(fixture.0.join("new-directory")).unwrap();
                        std::fs::remove_dir(fixture.0.join("new-directory")).unwrap();
                        let wide: Vec<_> = fixture_extended(&fixture.0.join("vanishing"))
                            .as_os_str()
                            .encode_wide()
                            .chain(Some(0))
                            .collect();
                        // SAFETY: terminated owned missing directory path, metadata only.
                        assert_eq!(
                            unsafe { GetFileAttributesW(wide.as_ptr()) },
                            INVALID_FILE_ATTRIBUTES
                        );
                        failure = Some(unsafe { GetLastError() });
                        assert!(matches!(
                            failure,
                            Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
                        ));
                        changed = true;
                    }
                    for outcome in batch.outcomes() {
                        if Some(outcome.key) == vanished {
                            assert_eq!(outcome.code, failure);
                            assert!(
                                outcome.code.is_some(),
                                "vanished directory is not empty success"
                            );
                        }
                    }
                }
                if gated.load(Ordering::SeqCst) {
                    if changed {
                        gated.store(false, Ordering::SeqCst);
                    }
                    ack.send(()).unwrap();
                }
            });
            assert!(
                !gate_failed.load(Ordering::SeqCst),
                "publication gate ACK timed out or disconnected"
            );
            assert!(changed);
            assert_eq!(reducer.diagnostics().directory_failures, 1);
            assert_eq!(reducer.diagnostics().last_directory_failure_code, failure);
            assert!(!reducer.index().unwrap().summary().unwrap().complete);
            for (_, entry) in reducer.index().unwrap().entries() {
                if entry.kind == Kind::File {
                    assert_eq!(Some(&entry.sizes), listed.get(entry.name));
                }
                if entry.name == "vanishing" {
                    assert_eq!(entry.state, crate::index::State::Inaccessible);
                }
            }
        }

        #[test]
        fn owned_long_paths_scan_from_plain_and_extended_roots() {
            let fixture = OwnedTree::new();
            let mut deepest = fixture.0.clone();
            for level in 0..8 {
                deepest.push(format!("level-{level}-{}", "x".repeat(32)));
                std::fs::create_dir(fixture_extended(&deepest)).unwrap();
            }
            assert!(deepest.as_os_str().encode_wide().count() > 260);
            std::fs::create_dir(fixture_extended(&deepest.join("empty"))).unwrap();
            std::fs::write(fixture_extended(&deepest.join("leaf")), [0; 17]).unwrap();
            std::fs::write(fixture_extended(&deepest.join("雪-😀")), [0; 5]).unwrap();
            for (root, directories) in [(&fixture.0, 9), (&deepest, 1)] {
                for extended in [false, true] {
                    let path = if extended {
                        fixture_extended(root)
                    } else {
                        root.clone()
                    };
                    let (sender, receiver) = crate::scan::event_channel(1);
                    let (reducer, report) = scan_owned_tree(path, sender, receiver, |_| {});
                    let entries = indexed_tree(&reducer);
                    assert_eq!(
                        entries
                            .values()
                            .filter(|(kind, _)| *kind == Kind::Directory)
                            .count(),
                        directories
                    );
                    assert_eq!(entries, find_basic_walk(root));
                    let summary = reducer.index().unwrap().summary().unwrap();
                    assert_eq!(summary.files, 2);
                    assert_eq!(summary.known_logical_bytes, 22);
                    assert!(summary.complete);
                    assert_eq!(report.fallback_directories, 0);
                    assert_eq!(reducer.diagnostics().directory_failures, 0);
                }
            }
            for extended in [false, true] {
                let path = if extended {
                    fixture_extended(&deepest)
                } else {
                    deepest.clone()
                };
                let generation = ScanGeneration::new(962);
                // This owned subtree has only two directories and two files. Sixteen envelopes
                // hold its complete publication, so the synchronous producer cannot block.
                let (sender, receiver) = crate::scan::event_channel(16);
                let result = crate::scan_win::run_win_scan_observed_with_options(
                    generation,
                    path,
                    sender,
                    &ScanCancellation::new(),
                    ScanOptions {
                        workers: 1,
                        ..ScanOptions::default()
                    },
                );
                assert_eq!(result.outcome, ProducerOutcome::Finished);
                assert_eq!(result.report.fallback_directories, 0);
                let mut reducer = ScanReducer::new(generation);
                let (replay, events) = crate::scan::event_channel(1);
                let mut terminals = 0;
                while let Ok(event) = receiver.try_recv() {
                    terminals += usize::from(event.is_terminal());
                    replay.send(event).unwrap();
                    reducer.drain(&events, 4096);
                }
                assert_eq!(terminals, 1);
                assert_eq!(reducer.terminal(), Some(Terminal::Finished));
                assert_eq!(reducer.diagnostics().directory_failures, 0);
                assert_eq!(reducer.diagnostics().unknown_parent_records, 0);
                let index = reducer.index().unwrap();
                assert_eq!(index.entries().count(), 4);
                assert_eq!(
                    index
                        .entries()
                        .filter(|(_, entry)| entry.kind == Kind::Directory)
                        .count(),
                    2
                );
                let summary = index.summary().unwrap();
                assert_eq!(summary.files, 2);
                assert_eq!(summary.known_logical_bytes, 22);
                assert!(summary.complete);
                assert_eq!(indexed_tree(&reducer), find_basic_walk(&deepest));
                let observations = result.observations;
                assert_eq!(observations.generation, generation);
                let root = observations.root.unwrap();
                assert_eq!(root.scope, crate::volume::RootScope::Folder);
                assert_eq!(root.scope_issue, None);
                assert_ne!(
                    root.directory_identity.unwrap(),
                    root.volume_root_identity.unwrap()
                );
                let validation = observations.validation.unwrap();
                assert_eq!(
                    validation.directory_identity,
                    Ok(crate::volume::ObservedContinuity::UnchangedAtEndpoints)
                );
                assert_eq!(
                    validation.normalized_path,
                    Ok(crate::volume::ObservedContinuity::UnchangedAtEndpoints)
                );
                for sample in [observations.before.unwrap(), observations.after.unwrap()] {
                    assert_eq!(sample.metadata.issue, None);
                    assert_eq!(
                        sample.metadata.source,
                        crate::volume::CapacitySource::WindowsActualGlobalFilesystem
                    );
                    let total = sample.total_bytes.unwrap();
                    let free = sample.free_bytes.unwrap();
                    assert!(total > 0 && free <= total);
                }
                assert_eq!(observations.directories.opened_directories, 2);
                println!(
                    "unit62_observed_long_root extended={} files=2 directories=2 logical=22 folder=1 endpoints_unchanged=1 capacity_known=1 fallback=0 terminal=1",
                    u8::from(extended)
                );
            }
            // Separate legacy-API probe, not the production Actual* capacity provider. Its
            // caller allowance/total can be quota-limited; no equality across APIs is asserted.
            let mut directory = fixture_extended(&deepest).into_os_string();
            directory.push(r"\");
            let directory: Vec<u16> = directory.encode_wide().chain(Some(0)).collect();
            assert!(directory.len() > 260);
            let (mut caller_free, mut caller_total, mut volume_free) = (0_u64, 0_u64, 0_u64);
            // SAFETY: terminated owned extended directory path with trailing separator and
            // three binding-defined writable u64 outputs. Metadata only, no content access.
            let success = unsafe {
                GetDiskFreeSpaceExW(
                    directory.as_ptr(),
                    &mut caller_free,
                    &mut caller_total,
                    &mut volume_free,
                )
            };
            let error = (success == 0).then(|| unsafe { GetLastError() });
            println!(
                "unit62_long_GetDiskFreeSpaceExW success={} error={error:?} caller_free={caller_free} caller_total={caller_total} volume_free={volume_free}",
                u8::from(success != 0)
            );
            assert_ne!(
                success, 0,
                "extended-directory capacity query failed: {error:?}"
            );
            assert!(caller_total > 0 && caller_free <= caller_total);
            assert!(caller_free <= volume_free);
            let root = fixture.0.clone();
            drop(fixture);
            assert!(!root.try_exists().unwrap());
            println!("unit62_long_fixture_cleanup=1");
        }

        #[test]
        fn conflict_reread_uses_verified_native_hardlink_handles_not_listed_sizes() {
            use crate::index::{Index, SizeMetric, State};
            use std::os::windows::io::AsRawHandle;
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("hogmap-reread-{}-{unique}", std::process::id()));
            std::fs::create_dir(&root).unwrap();
            struct Fixture(PathBuf);
            impl Drop for Fixture {
                fn drop(&mut self) {
                    std::fs::remove_dir_all(&self.0).expect("owned reread fixture cleanup");
                }
            }
            let fixture = Fixture(root.clone());
            std::fs::write(root.join("first"), [7_u8; 4096]).unwrap();
            std::fs::hard_link(root.join("first"), root.join("second")).unwrap();
            let cancel = ScanCancellation::new();
            let directory = reread_open(&root, true, &cancel).unwrap();
            let (_, root_identity) = reread_binding(&directory, true, &cancel).unwrap();
            let file = reread_open(&root.join("first"), false, &cancel).unwrap();
            let (serial32, native_id) = reread_binding(&file, false, &cancel).unwrap();
            let identity = FileIdentity {
                volume: u64::from(serial32),
                file: u128::from_le_bytes(native_id.FileId.Identifier),
            };
            let mut standard = FILE_STANDARD_INFO::default();
            // SAFETY: independent oracle on a live fixture handle and exact binding output.
            assert_ne!(
                unsafe {
                    GetFileInformationByHandleEx(
                        file.as_raw_handle(),
                        FileStandardInfo,
                        (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                        size_of::<FILE_STANDARD_INFO>() as u32,
                    )
                },
                0
            );
            let expected = Sizes {
                logical: u64::try_from(standard.EndOfFile).ok(),
                allocated: u64::try_from(standard.AllocationSize).ok(),
            };
            assert!(expected.logical.is_some() && expected.allocated.is_some());
            assert_eq!(
                reread_file(
                    &root.join("second"),
                    identity,
                    root_identity.VolumeSerialNumber,
                    &cancel
                ),
                Ok(expected)
            );
            let wrong = FileIdentity {
                file: identity.file ^ 1,
                ..identity
            };
            assert_eq!(
                reread_file(
                    &root.join("first"),
                    wrong,
                    root_identity.VolumeSerialNumber,
                    &cancel
                ),
                Err(ConflictRereadIssue::IdentityMismatch)
            );
            assert_eq!(
                reread_file(
                    &root.join("first"),
                    identity,
                    root_identity.VolumeSerialNumber ^ (1_u64 << 63),
                    &cancel
                ),
                Err(ConflictRereadIssue::VolumeMismatch)
            );
            assert_eq!(
                reread_file(
                    &root.join("first"),
                    FileIdentity {
                        volume: identity.volume ^ 1,
                        ..identity
                    },
                    root_identity.VolumeSerialNumber,
                    &cancel
                ),
                Err(ConflictRereadIssue::VolumeMismatch)
            );
            // Disagreement is synthetic input to Index, NOT a claimed NTFS stale-entry reproduction.
            let mut index = Index::new("Fixture".into()).unwrap();
            let mut ids = Vec::new();
            for (name, bytes) in [("first", 1), ("second", 2)] {
                ids.push(
                    index
                        .insert(
                            index.root(),
                            name.into(),
                            Kind::File,
                            Sizes {
                                logical: Some(bytes),
                                allocated: Some(bytes),
                            },
                            Some(identity),
                            State::Complete,
                        )
                        .unwrap(),
                );
            }
            index.set_state(index.root(), State::Complete).unwrap();
            let report = reread_identity_conflicts(&mut index, &root, &cancel);
            assert_eq!(
                (
                    report.candidate_identities,
                    report.attempted_identities,
                    report.resolved_identities,
                    report.unresolved_identities
                ),
                (1, 1, 1, 0)
            );
            assert!(report.issues.is_empty());
            assert_eq!(
                index.summary_for(SizeMetric::Logical).unwrap().unique_bytes,
                expected.logical.unwrap()
            );
            assert_eq!(
                index
                    .summary_for(SizeMetric::Allocated)
                    .unwrap()
                    .unique_bytes,
                expected.allocated.unwrap()
            );
            assert_eq!(index.get(ids[0]).unwrap().sizes.logical, Some(1));
            assert_eq!(index.get(ids[1]).unwrap().sizes.allocated, Some(2));
            drop(file);
            drop(directory);
            drop(fixture);
            assert!(!root.exists());
            println!(
                "native_conflict_reread hardlinks=1 handle_standard_oracle=1 mismatched_file_rejected=1 mismatched_volume32_rejected=1 mismatched_volume64_rejected=1 synthetic_conflict_input=1 resolved=1 listed_records_unchanged=1 cleanup=1"
            );
        }

        #[test]
        fn mft_frontier_orders_only_ntfs_seek_devices_and_ignores_sequence_bits() {
            assert!(mft_order_for(Some(true), Some("ntfs")));
            for (seek, fs) in [
                (Some(false), Some("NTFS")),
                (None, Some("NTFS")),
                (Some(true), Some("ReFS")),
                (Some(true), None),
            ] {
                assert!(!mft_order_for(seek, fs));
            }
            let memory = Arc::new(PendingMemory::default());
            let path = DirectoryPath::new(None, OsString::from("fixture"), &memory, 4096).unwrap();
            for ordered in [false, true] {
                let mut frontier = Frontier::new(ordered, 1);
                for (key, file_id) in [
                    (0, Some(90)),
                    (1, Some((7_u128 << 48) | 10)),
                    (2, None),
                    (3, Some(40)),
                ] {
                    frontier.push(Work {
                        path: path.clone(),
                        key: NodeKey::new(key),
                        file_id,
                    });
                }
                let mut keys = Vec::new();
                while let Some(work) = frontier.pop() {
                    keys.push(work.key.value());
                }
                assert_eq!(
                    keys,
                    if ordered {
                        vec![1, 3, 0, 2]
                    } else {
                        vec![3, 2, 1, 0]
                    }
                );
            }
            assert!(
                pending_priority(Some(12), NodeKey::new(1))
                    > pending_priority(Some(12), NodeKey::new(2))
            );
        }

        #[test]
        fn mft_scheduling_preserves_publication_outcomes_and_terminal() {
            use std::collections::HashSet;
            struct Fixture(PathBuf);
            impl Drop for Fixture {
                fn drop(&mut self) {
                    std::fs::remove_dir_all(&self.0).unwrap();
                }
            }
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            let fixture = Fixture(std::env::temp_dir().join(format!(
                "hogmap-mft-order-test-{}-{unique}-{nanos}",
                std::process::id()
            )));
            std::fs::create_dir(&fixture.0).unwrap();
            for number in 0..64 {
                std::fs::create_dir_all(fixture.0.join(format!("dir-{number}/nested"))).unwrap();
            }
            let generation = ScanGeneration::new(901);
            let (sender, receiver) = crate::scan::event_channel(1);
            let cancel = ScanCancellation::new();
            thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    run_scan(
                        generation,
                        fixture.0.clone(),
                        sender,
                        &cancel,
                        ScanOptions {
                            workers: 4,
                            queue_capacity: 1,
                            local_stack_capacity: 1,
                            ..ScanOptions::default()
                        },
                        Some(true),
                        true,
                    )
                });
                let mut seen = HashSet::new();
                let mut done = HashSet::new();
                let mut terminals = 0;
                while let Ok(event) = receiver.recv_timeout(Duration::from_secs(10)) {
                    assert_eq!(event.generation, generation);
                    match event.event {
                        ScanEventKind::Batch(batch) => {
                            for record in batch.records() {
                                if let Some(parent) = record.parent {
                                    assert!(seen.contains(&parent));
                                    assert!(!done.contains(&parent));
                                }
                                assert!(seen.insert(record.key));
                            }
                            for outcome in batch.outcomes() {
                                assert!(seen.contains(&outcome.key));
                                assert!(done.insert(outcome.key));
                                assert_eq!(outcome.code, None);
                            }
                        }
                        ScanEventKind::Terminal(terminal) => {
                            assert_eq!(terminal, Terminal::Finished);
                            terminals += 1;
                        }
                        ScanEventKind::Progress(_) => {}
                        _ => panic!("unexpected unbatched outcome"),
                    }
                }
                let (outcome, report) = worker.join().unwrap();
                assert_eq!(outcome, ProducerOutcome::Finished);
                assert_eq!(seen.len(), 129);
                assert_eq!(done, seen);
                assert_eq!(terminals, 1);
                assert_eq!(report.pending_memory_limit_hits, 0);
                assert!(report.peak_pending_bytes <= ScanOptions::default().max_pending_bytes);
            });
        }

        #[test]
        fn native_allocation_modes_preserve_scanner_records_and_stream_lengths() {
            use crate::index::{SizeMetric, State};
            use crate::scan::ScanReducer;
            use std::fs::{File, OpenOptions};
            use std::io::{Seek, SeekFrom, Write};
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
            use windows_sys::Win32::System::IO::DeviceIoControl;
            use windows_sys::Win32::System::Ioctl::{FSCTL_SET_COMPRESSION, FSCTL_SET_SPARSE};

            struct Fixture(PathBuf);
            impl Drop for Fixture {
                fn drop(&mut self) {
                    if let Err(error) = std::fs::remove_dir_all(&self.0) {
                        eprintln!(
                            "allocation_fixture_cleanup_error={:?}",
                            error.raw_os_error()
                        );
                        assert!(thread::panicking(), "owned fixture cleanup failed");
                    } else {
                        println!("allocation_fixture_cleanup=1");
                    }
                }
            }
            fn code(error: std::io::Error) -> u32 {
                error.raw_os_error().unwrap_or(-1) as u32
            }
            fn set_compression(file: &File, format: u16) -> Result<(), u32> {
                let mut returned = 0;
                // SAFETY: this is a live owned fixture file, not a volume; the synchronous
                // request reads one USHORT and returns no payload. No pointer outlives the call.
                if unsafe {
                    DeviceIoControl(
                        file.as_raw_handle(),
                        FSCTL_SET_COMPRESSION,
                        (&format as *const u16).cast(),
                        size_of::<u16>() as u32,
                        null_mut(),
                        0,
                        &mut returned,
                        null_mut(),
                    )
                } == 0
                {
                    Err(unsafe { GetLastError() })
                } else {
                    Ok(())
                }
            }
            fn set_sparse(file: &File) -> Result<(), u32> {
                let mut returned = 0;
                // SAFETY: live owned fixture file and synchronous request. The documented
                // null input means SetSparse=TRUE; a nonnull bytes-returned output is provided.
                if unsafe {
                    DeviceIoControl(
                        file.as_raw_handle(),
                        FSCTL_SET_SPARSE,
                        null(),
                        0,
                        null_mut(),
                        0,
                        &mut returned,
                        null_mut(),
                    )
                } == 0
                {
                    Err(unsafe { GetLastError() })
                } else {
                    Ok(())
                }
            }
            fn activated(case: usize, result: Result<(), u32>) -> bool {
                match result {
                    Ok(()) => true,
                    Err(error) => {
                        println!("allocation_case={case} mode_omitted=1 error={error}");
                        assert!(
                            matches!(
                                error,
                                ERROR_INVALID_FUNCTION
                                    | ERROR_NOT_SUPPORTED
                                    | ERROR_INVALID_PARAMETER
                                    | ERROR_INVALID_NAME
                            ),
                            "unexpected fixture activation error {error}"
                        );
                        false
                    }
                }
            }
            fn create(path: &Path) -> File {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map_err(code)
                    .expect("create owned fixture file");
                let attributes = file.metadata().map_err(code).unwrap().file_attributes();
                assert_eq!(skip_code(attributes, 0), None);
                assert_eq!(attributes & FILE_ATTRIBUTE_SPARSE_FILE, 0);
                // Compression may be inherited. Only the new empty file is normalized.
                if attributes & FILE_ATTRIBUTE_COMPRESSED != 0 {
                    set_compression(&file, COMPRESSION_FORMAT_NONE)
                        .expect("clear inherited compression on owned fixture file");
                }
                file
            }
            struct Observation {
                logical: u64,
                allocation: u64,
                compressed_size: u64,
                attributes: u32,
            }
            fn observe(path: &Path) -> Result<Observation, u32> {
                let file = OpenOptions::new()
                    .access_mode(FILE_READ_ATTRIBUTES)
                    .open(path)
                    .map_err(code)?;
                let mut standard = FILE_STANDARD_INFO::default();
                // SAFETY: live metadata-only handle and exact binding-defined writable output.
                if unsafe {
                    GetFileInformationByHandleEx(
                        file.as_raw_handle(),
                        FileStandardInfo,
                        (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                        size_of::<FILE_STANDARD_INFO>() as u32,
                    )
                } == 0
                {
                    return Err(unsafe { GetLastError() });
                }
                assert!(!standard.Directory);
                let attributes = file.metadata().map_err(code)?.file_attributes();
                let path = extended_path(path).map_err(|error| error.code)?;
                let mut high = 0;
                // SAFETY: terminated fixture/default-or-named-stream path and writable high
                // DWORD. Clear last error to disambiguate a successful INVALID_FILE_SIZE result.
                let (low, error) = unsafe {
                    SetLastError(0);
                    let low = GetCompressedFileSizeW(path.as_ptr(), &mut high);
                    (low, GetLastError())
                };
                if low == INVALID_FILE_SIZE && error != 0 {
                    return Err(error);
                }
                Ok(Observation {
                    logical: u64::try_from(standard.EndOfFile).unwrap(),
                    allocation: u64::try_from(standard.AllocationSize).unwrap(),
                    compressed_size: (u64::from(high) << 32) | u64::from(low),
                    attributes,
                })
            }

            // Independent same-field oracle: fresh native enumeration, never parse_buffer,
            // child, reducer or scanner output. Cross-API probes below remain observations only.
            fn raw_directory_allocations(path: &Path, names: &[&str; 4]) -> Result<[i64; 4], u32> {
                let file = OpenOptions::new()
                    .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                    .custom_flags(
                        FILE_FLAG_BACKUP_SEMANTICS
                            | FILE_FLAG_OPEN_REPARSE_POINT
                            | FILE_FLAG_OPEN_NO_RECALL,
                    )
                    .open(path)
                    .map_err(code)?;
                let mut buffer = vec![0_u64; 8192];
                let bytes = buffer.len() * size_of::<u64>();
                let header = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
                let mut values = [0_i64; 4];
                let mut seen = [false; 4];
                let mut records = 0;
                // Four owned files and dot entries need only a tiny bounded listing.
                for _ in 0..8 {
                    buffer.fill(0);
                    // SAFETY: live metadata directory handle and initialized 8-byte-aligned
                    // writable buffer. This fresh handle has its own enumeration cursor.
                    if unsafe {
                        GetFileInformationByHandleEx(
                            file.as_raw_handle(),
                            FileIdExtdDirectoryInfo,
                            buffer.as_mut_ptr().cast(),
                            bytes as u32,
                        )
                    } == 0
                    {
                        let error = unsafe { GetLastError() };
                        if error == ERROR_NO_MORE_FILES && seen.iter().all(|value| *value) {
                            return Ok(values);
                        }
                        return Err(if error == ERROR_NO_MORE_FILES {
                            ERROR_INVALID_DATA
                        } else {
                            error
                        });
                    }
                    let mut offset = 0_usize;
                    loop {
                        records += 1;
                        if records > 16
                            || !offset.is_multiple_of(8)
                            || offset
                                .checked_add(size_of::<FILE_ID_EXTD_DIR_INFO>())
                                .is_none_or(|end| end > bytes)
                        {
                            return Err(ERROR_INVALID_DATA);
                        }
                        // SAFETY: offset alignment and full binding-defined header checked.
                        let entry = unsafe {
                            &*buffer
                                .as_ptr()
                                .cast::<u8>()
                                .add(offset)
                                .cast::<FILE_ID_EXTD_DIR_INFO>()
                        };
                        let name_bytes = entry.FileNameLength as usize;
                        let next = entry.NextEntryOffset as usize;
                        let end = offset
                            .checked_add(header)
                            .and_then(|start| start.checked_add(name_bytes))
                            .ok_or(ERROR_INVALID_DATA)?;
                        if !name_bytes.is_multiple_of(2)
                            || name_bytes > 510
                            || end > bytes
                            || (next != 0
                                && (!next.is_multiple_of(8)
                                    || next < header + name_bytes
                                    || offset.checked_add(next).is_none_or(|next| next >= bytes)))
                        {
                            return Err(ERROR_INVALID_DATA);
                        }
                        // SAFETY: complete UTF-16 tail is in bounds and naturally aligned.
                        let name = unsafe {
                            std::slice::from_raw_parts(
                                buffer
                                    .as_ptr()
                                    .cast::<u8>()
                                    .add(offset + header)
                                    .cast::<u16>(),
                                name_bytes / 2,
                            )
                        };
                        if name != [46] && name != [46, 46] {
                            let name = OsString::from_wide(name);
                            let position = names
                                .iter()
                                .position(|expected| name == OsStr::new(expected))
                                .ok_or(ERROR_INVALID_DATA)?;
                            if seen[position] {
                                return Err(ERROR_INVALID_DATA);
                            }
                            seen[position] = true;
                            values[position] = entry.AllocationSize;
                        }
                        if next == 0 {
                            break;
                        }
                        offset += next;
                    }
                }
                Err(ERROR_INVALID_DATA)
            }

            static NEXT: AtomicU64 = AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hogmap-allocation-test-{}-{unique}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir(&path)
                .map_err(code)
                .expect("create unique owned directory");
            let fixture = Fixture(path.clone());
            let names = ["normal.bin", "compressed.bin", "sparse.bin", "streams.bin"];
            let paths = names.map(|name| path.join(name));
            const MIB: usize = 1 << 20;
            // Deterministic data is written only during fixture construction, never read back.
            let normal: Vec<u8> = (0..MIB).map(|n| ((n * 37 + n / 251) % 256) as u8).collect();
            {
                let mut file = create(&paths[0]);
                file.write_all(&normal).map_err(code).unwrap();
                file.sync_all().map_err(code).unwrap();
            }
            let compressed_active = {
                let mut file = create(&paths[1]);
                let active = activated(1, set_compression(&file, COMPRESSION_FORMAT_DEFAULT));
                file.write_all(&vec![0; MIB]).map_err(code).unwrap();
                file.sync_all().map_err(code).unwrap();
                active
            };
            let sparse_active = {
                let mut file = create(&paths[2]);
                let active = activated(2, set_sparse(&file));
                file.set_len(MIB as u64).map_err(code).unwrap();
                file.seek(SeekFrom::Start(512 << 10)).map_err(code).unwrap();
                file.write_all(&normal[..4096]).map_err(code).unwrap();
                file.sync_all().map_err(code).unwrap();
                active
            };
            create(&paths[3]).sync_all().map_err(code).unwrap();
            let stream_path = path.join("streams.bin:fixture:$DATA");
            let named_active = match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&stream_path)
            {
                Ok(mut file) => {
                    // The named stream has its own compression state, independent of default.
                    let attributes = file.metadata().map_err(code).unwrap().file_attributes();
                    if attributes & FILE_ATTRIBUTE_COMPRESSED != 0 {
                        set_compression(&file, COMPRESSION_FORMAT_NONE).unwrap();
                    }
                    file.write_all(&normal[..65536]).map_err(code).unwrap();
                    file.sync_all().map_err(code).unwrap();
                    true
                }
                Err(error) => activated(3, Err(code(error))),
            };
            let observations = paths.each_ref().map(|path| observe(path).unwrap());
            let raw_allocations = match raw_directory_allocations(&path, &names) {
                Ok(values) => Some(values),
                Err(error) if fallback_allowed(error) => {
                    println!("allocation_oracle_omitted=1 error={error}");
                    None
                }
                Err(error) => panic!("independent directory oracle failed: {error}"),
            };
            assert_eq!(observations[0].attributes & FILE_ATTRIBUTE_COMPRESSED, 0);
            assert_eq!(
                observations[1].attributes & FILE_ATTRIBUTE_COMPRESSED != 0,
                compressed_active
            );
            assert_eq!(
                observations[2].attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0,
                sparse_active
            );
            if named_active {
                let named = observe(&stream_path).unwrap();
                assert_eq!(named.logical, 65536);
                assert_eq!(
                    named.attributes & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_SPARSE_FILE),
                    0
                );
                assert_eq!(named.compressed_size, named.logical);
                println!(
                    "allocation_named_stream logical={} standard_allocation={} compressed_size={}",
                    named.logical, named.allocation, named.compressed_size
                );
            }
            let generation = ScanGeneration::new(902);
            // Four children bound the complete run to fewer than sixteen envelopes, so this
            // synchronous producer cannot fill the channel before the reducer starts draining.
            let (sender, receiver) = crate::scan::event_channel(16);
            let (outcome, report) = run_scan(
                generation,
                path.clone(),
                sender,
                &ScanCancellation::new(),
                ScanOptions {
                    workers: 1,
                    ..ScanOptions::default()
                },
                None,
                false,
            );
            assert_eq!(outcome, ProducerOutcome::Finished);
            let mut reducer = ScanReducer::new(generation);
            reducer.drain(&receiver, 4096);
            assert_eq!(reducer.terminal(), Some(Terminal::Finished));
            assert_eq!(reducer.diagnostics().invalid_records, 0);
            assert_eq!(reducer.diagnostics().directory_failures, 0);
            assert_eq!(reducer.diagnostics().unknown_parent_records, 0);
            assert_eq!(report.cloud_skips, 0);
            assert_eq!(report.reparse_skips, 0);
            let index = reducer.index().unwrap();
            assert_eq!(index.entries().count(), 5);
            for (case, observation) in observations.iter().enumerate() {
                let entry = index
                    .entries()
                    .map(|(_, entry)| entry)
                    .find(|entry| entry.name == OsStr::new(names[case]))
                    .expect("fixture record preserved");
                assert_eq!(entry.kind, Kind::File);
                assert_eq!(entry.state, State::Complete);
                assert_eq!(observation.logical, if case == 3 { 0 } else { MIB as u64 });
                assert_eq!(entry.sizes.logical, Some(observation.logical));
                if report.fallback_directories == 0 {
                    let raw = raw_allocations
                        .as_ref()
                        .expect("primary path requires the raw oracle")[case];
                    assert_eq!(
                        entry.sizes.allocated,
                        u64::try_from(raw).ok(),
                        "native allocation value fidelity, case={case}"
                    );
                } else {
                    assert_eq!(entry.sizes.allocated, None);
                    assert_eq!(entry.identity, None);
                }
                if observation.attributes & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_SPARSE_FILE)
                    == 0
                {
                    // GetCompressedFileSizeW documents file length for an ordinary stream,
                    // not equality with either allocation API for compressed/sparse streams.
                    assert_eq!(observation.compressed_size, observation.logical);
                }
                println!(
                    "allocation_case={case} logical={} scanner_allocation={:?} raw_directory_allocation={:?} standard_allocation={} compressed_size={} compressed={} sparse={} fallback={}",
                    observation.logical,
                    entry.sizes.allocated,
                    raw_allocations.as_ref().map(|values| values[case]),
                    observation.allocation,
                    observation.compressed_size,
                    observation.attributes & FILE_ATTRIBUTE_COMPRESSED != 0,
                    observation.attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0,
                    report.fallback_directories
                );
            }
            let allocated = index.summary_for(SizeMetric::Allocated).unwrap();
            assert_eq!(allocated.files, 4);
            let unknown_allocations = if report.fallback_directories == 0 {
                raw_allocations
                    .unwrap()
                    .iter()
                    .filter(|value| **value < 0)
                    .count()
            } else {
                4
            };
            assert_eq!(allocated.complete, unknown_allocations == 0);
            assert_eq!(allocated.unknown_size_files, unknown_allocations);
            // A passing portable test can have explicit capability omissions. Only this output
            // establishes whether all requested modes and the primary path were exercised.
            println!(
                "allocation_verification_complete={} fallback={} last_fallback_code={:?}",
                u8::from(
                    compressed_active
                        && sparse_active
                        && named_active
                        && report.fallback_directories == 0
                        && unknown_allocations == 0
                ),
                report.fallback_directories,
                report.last_fallback_code
            );
            drop(fixture);
            assert!(!path.try_exists().unwrap());
        }

        #[test]
        fn extended_paths_preserve_drive_unc_long_and_native_names() {
            let text = |path: &str| {
                OsString::from_wide(
                    &extended_path(Path::new(path)).unwrap()
                        [..extended_path(Path::new(path)).unwrap().len() - 1],
                )
            };
            assert_eq!(text(r"D:\folder"), OsString::from(r"\\?\D:\folder"));
            assert_eq!(
                text(r"\\server\share\folder"),
                OsString::from(r"\\?\UNC\server\share\folder")
            );
            assert_eq!(text(r"\\?\D:\folder"), OsString::from(r"\\?\D:\folder"));
            let long = format!("D:\\{}\\{}", "a".repeat(200), "b".repeat(100));
            assert!(extended_path(Path::new(&long)).unwrap().len() > 260);
            let native = OsString::from_wide(&[68, 58, 92, 0xD800]);
            assert!(extended_path(Path::new(&native)).unwrap().contains(&0xD800));
            assert!(extended_path(Path::new(r"\\.\PhysicalDrive0")).is_err());
        }
        #[test]
        fn name_and_cloud_decisions_are_conservative_and_lossless() {
            assert!(valid_name(&[0xD800]));
            assert!(valid_name(&vec![65; 255]));
            assert!(!valid_name(&vec![65; 256]));
            for name in [
                vec![],
                vec![46],
                vec![46, 46],
                vec![65, 0],
                vec![92],
                vec![58],
            ] {
                assert!(!valid_name(&name));
            }
            assert_eq!(
                skip_code(FILE_ATTRIBUTE_REPARSE_POINT, 0xA000000C),
                Some(SKIP_REPARSE_CODE)
            );
            assert_eq!(skip_code(0, 0x9000F01A), Some(SKIP_CLOUD_CODE));
            assert_eq!(
                skip_code(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, 0),
                Some(SKIP_CLOUD_CODE)
            );
            assert_eq!(
                skip_code(FILE_ATTRIBUTE_RECALL_ON_OPEN, 0),
                Some(SKIP_CLOUD_CODE)
            );
            assert_eq!(skip_code(FILE_ATTRIBUTE_DIRECTORY, 0), None);
        }
        #[test]
        fn unsupported_zero_file_ids_keep_sizes_and_charge_files_individually() {
            use crate::index::{Index, SizeMetric, State};

            let volume = 0xFEDC_BA98;
            let hardlink_id = (1_u128 << 100) | 42;
            let distinct_id = (2_u128 << 100) | 42;
            // Both unsupported-ID files have equal known sizes, including a known-zero case.
            for (logical, allocated, expected_logical, expected_allocated) in [
                (7, 4096, (47, 36), (32768, 24576)),
                (0, 0, (33, 22), (24576, 16384)),
            ] {
                let mut index = Index::new(OsString::from("fixture")).unwrap();
                index.set_state(index.root(), State::Complete).unwrap();
                for (name, file_id, logical, allocated) in [
                    (vec![97, 0xD800], 0, logical, allocated),
                    (vec![98], 0, logical, allocated),
                    (vec![99], hardlink_id, 11, 8192),
                    (vec![100], hardlink_id, 11, 8192),
                    (vec![101], distinct_id, 11, 8192),
                ] {
                    let header = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
                    let bytes = size_of::<FILE_ID_EXTD_DIR_INFO>().max(header + name.len() * 2);
                    let mut buffer = vec![0_u64; bytes.div_ceil(8)];
                    // SAFETY: initialized u64 storage has the binding's required alignment
                    // and room for the fixed header. Field writes preserve initialized padding.
                    unsafe {
                        let entry = &mut *buffer.as_mut_ptr().cast::<FILE_ID_EXTD_DIR_INFO>();
                        entry.EndOfFile = logical;
                        entry.AllocationSize = allocated;
                        entry.FileNameLength = (name.len() * 2) as u32;
                        entry.FileAttributes = FILE_ATTRIBUTE_NORMAL;
                        entry.FileId.Identifier = file_id.to_le_bytes();
                    }
                    // SAFETY: the fixed-header borrow ended and the full UTF-16 tail fits
                    // in the aligned allocation; the separate name allocation cannot overlap.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            name.as_ptr(),
                            buffer.as_mut_ptr().cast::<u8>().add(header).cast::<u16>(),
                            name.len(),
                        );
                    }
                    let mut records = 0;
                    parse_buffer(&buffer, volume, |child| {
                        records += 1;
                        assert_eq!(child.name, name);
                        assert_eq!(child.attributes, FILE_ATTRIBUTE_NORMAL);
                        assert_eq!(child.tag, 0);
                        assert_eq!(child.sizes.logical, Some(logical as u64));
                        assert_eq!(child.sizes.allocated, Some(allocated as u64));
                        index
                            .insert(
                                index.root(),
                                OsString::from_wide(&child.name),
                                Kind::File,
                                child.sizes,
                                child.identity,
                                State::Complete,
                            )
                            .unwrap();
                        true
                    })
                    .unwrap();
                    assert_eq!(records, 1);
                }
                assert_eq!(index.entries().count(), 6);
                for (metric, (known, unique), shared) in [
                    (SizeMetric::Logical, expected_logical, 11),
                    (SizeMetric::Allocated, expected_allocated, 8192),
                ] {
                    let summary = index.summary_for(metric).unwrap();
                    assert_eq!(summary.files, 5);
                    assert_eq!(summary.known_bytes, known);
                    assert_eq!(summary.conflicting_identities, 0);
                    assert_eq!(summary.unique_bytes, unique, "{summary:?}");
                    assert_eq!(summary.shared_bytes, shared);
                    assert_eq!(summary.identity_missing_files, 2);
                    assert_eq!(summary.unknown_size_files, 0);
                    assert!(summary.complete);
                }
                let entries: Vec<_> = index.entries().skip(1).map(|(_, entry)| entry).collect();
                for entry in &entries {
                    assert_eq!(entry.kind, Kind::File);
                    assert_eq!(entry.state, State::Complete);
                }
                assert_eq!(entries[0].identity, None);
                assert_eq!(entries[1].identity, None);
                for (position, file) in [(2, hardlink_id), (3, hardlink_id), (4, distinct_id)] {
                    assert_eq!(
                        entries[position].identity,
                        Some(FileIdentity {
                            volume: u64::from(volume),
                            file,
                        })
                    );
                }
            }
        }

        #[test]
        fn malformed_directory_offsets_and_names_are_rejected() {
            let mut buffer = vec![0_u64; 32];
            // SAFETY: aligned, initialized buffer exceeds the fixed structure size.
            let entry = unsafe { &mut *buffer.as_mut_ptr().cast::<FILE_ID_EXTD_DIR_INFO>() };
            entry.NextEntryOffset = 1;
            assert_eq!(
                parse_buffer(&buffer, 1, |_| true).unwrap_err().code,
                ERROR_INVALID_DATA
            );
            // SAFETY: same live aligned allocation, no overlapping reference remains.
            let entry = unsafe { &mut *buffer.as_mut_ptr().cast::<FILE_ID_EXTD_DIR_INFO>() };
            entry.NextEntryOffset = 0;
            entry.FileNameLength = 257;
            assert_eq!(
                parse_buffer(&buffer, 1, |_| true).unwrap_err().code,
                ERROR_INVALID_DATA
            );
        }
    }
}
