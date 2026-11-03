//! Read-only Windows directory enumeration. No file contents or per-file opens.
#![allow(unsafe_code)]

use crate::scan::{ScanCancellation, ScanGeneration, ScanSender};
use crate::synthetic::ProducerOutcome;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

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
    /// Zero selects the volume policy; explicit values are clamped to 1..=8.
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

/// Unknown storage is treated conservatively, like a seek-penalty device.
pub fn worker_count_for(seek_penalty: Option<bool>, available: usize) -> usize {
    let desired = if seek_penalty == Some(false) { 4 } else { 1 };
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
    pub workers: [WorkerMetrics; 8],
    pub selected_workers: usize,
    pub seek_penalty: Option<bool>,
    pub first_open_ns: Option<u64>,
    pub first_batch_sent_ns: Option<u64>,
    pub last_outcome_ns: Option<u64>,
    pub terminal_ns: Option<u64>,
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

#[cfg(windows)]
pub(crate) use platform::extended_path;
#[cfg(windows)]
pub use platform::run_win_scan_with_options;

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::index::{FileIdentity, Kind, Sizes};
    use crate::scan::{
        Batch, DirectoryOutcome, NodeKey, Progress, ScanEvent, ScanEventKind, ScanRecord, ScanWake,
        SendOutcome, Terminal, send_event, send_terminal,
    };
    use std::collections::VecDeque;
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
    struct Work {
        path: Arc<DirectoryPath>,
        key: NodeKey,
    }
    struct ActiveWork {
        path: PathBuf,
        node: Arc<DirectoryPath>,
        key: NodeKey,
    }
    struct Queue {
        entries: VecDeque<Work>,
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
                queue.entries.extend(output.unpublished.drain(..));
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
        fn schedule(&self, work: Work, local: &mut Vec<Work>) -> bool {
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
        let available = thread::available_parallelism().map_or(1, usize::from);
        let options = ScanOptions {
            workers: if options.workers == 0 {
                worker_count_for(seek_penalty, available)
            } else {
                options.workers.clamp(1, 8)
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
                entries: VecDeque::from([Work {
                    path: root_path,
                    key: root_key,
                }]),
                outstanding: 1,
                report: ScanReport {
                    max_queue_depth: 1,
                    selected_workers: options.workers,
                    seek_penalty,
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
        let mut local = Vec::with_capacity(shared.options.local_stack_capacity);
        loop {
            if shared.stopping() {
                shared.wake.notify();
                return true;
            }
            let work = if let Some(work) = local.pop() {
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
                    if let Some(work) = queue.entries.pop_front() {
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
            let result = enumerate(&work, shared, &mut local);
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
    fn child(work: &ActiveWork, shared: &Shared<'_>, local: &mut Vec<Work>, child: Child) -> bool {
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
                    shared.schedule(Work { path, key }, local)
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
        local: &mut Vec<Work>,
    ) -> Result<(), ScanError> {
        if work.key == NodeKey::new(0) {
            check_path(&work.path, shared)?;
        } else {
            // Ancestors were checked when their work was processed; recheck this final component.
            check_directory(&work.path, shared)?;
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
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        if shared.stopping() {
            return Ok(());
        }
        // SAFETY: handle is owned and open; the output has the exact Win32 layout and size.
        if unsafe { GetFileInformationByHandle(handle.0, &mut information) } == 0 {
            return Err(ScanError::last());
        }
        let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
        if shared.stopping() {
            return Ok(());
        }
        // SAFETY: owned live handle and writable fixed-size FILE_ATTRIBUTE_TAG_INFO output.
        let tag_ok = unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileAttributeTagInfo,
                (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        } != 0;
        let current_attributes = if tag_ok {
            tag.FileAttributes
        } else {
            information.dwFileAttributes
        };
        if let Some(code) = skip_code(current_attributes, if tag_ok { tag.ReparseTag } else { 0 }) {
            shared.count_skip(code);
            return Err(ScanError { code });
        }
        if current_attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(ScanError {
                code: ERROR_DIRECTORY,
            });
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
                identity: Some(FileIdentity {
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
        local: &mut Vec<Work>,
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
