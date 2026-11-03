//! Read-only Windows directory enumeration. No file contents or per-file opens.
#![allow(unsafe_code)]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::SyncSender;

use crate::scan::{ScanEvent, ScanGeneration};
use crate::synthetic::ProducerOutcome;

/// Producer-defined codes, deliberately outside the Win32 error range.
/// Skipped entries are not followed, not necessarily denied by permissions.
pub const SKIP_REPARSE_CODE: u32 = 0xFFFF_FE01;
pub const SKIP_CLOUD_CODE: u32 = 0xFFFF_FE02;
pub const LOCAL_STACK_LIMIT_CODE: u32 = 0xFFFF_FE03;
pub const UNSUPPORTED_PLATFORM_CODE: u32 = 0xFFFF_FE04;
pub const WORKER_FAILED_CODE: u32 = 0xFFFF_FE05;

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
    /// Clamped to 1..=8; the default is two directory workers.
    pub workers: usize,
    /// Shared queue bound, clamped to 1..=1024. Overflow stays local to a worker.
    pub queue_capacity: usize,
    /// Local pending directories per worker, clamped to 1..=4096.
    pub local_stack_capacity: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            workers: 2,
            queue_capacity: 1024,
            local_stack_capacity: 4096,
        }
    }
}

/// Counts only: no scanned paths or names are retained in diagnostics.
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
}

pub fn run_win_scan(
    generation: ScanGeneration,
    root: PathBuf,
    sender: SyncSender<ScanEvent>,
    cancel: &AtomicBool,
) -> ProducerOutcome {
    run_win_scan_with_options(generation, root, sender, cancel, ScanOptions::default()).0
}

#[cfg(not(windows))]
pub fn run_win_scan_with_options(
    generation: ScanGeneration,
    _root: PathBuf,
    sender: SyncSender<ScanEvent>,
    _cancel: &AtomicBool,
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
        Batch, NodeKey, Progress, ScanEventKind, ScanRecord, Terminal, send_terminal,
    };
    use std::collections::VecDeque;
    use std::ffi::{OsStr, OsString};
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Path, Prefix};
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::TrySendError;
    use std::sync::{Condvar, Mutex};
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
    const WAIT_INTERVAL: Duration = Duration::from_millis(10);

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

    struct Work {
        path: PathBuf,
        key: NodeKey,
    }
    struct Queue {
        entries: VecDeque<Work>,
        outstanding: usize,
        report: ScanReport,
    }
    struct Output {
        batch: Batch,
        last_flush: Instant,
        progress: Progress,
    }
    struct Shared<'a> {
        generation: ScanGeneration,
        sender: SyncSender<ScanEvent>,
        cancel: &'a AtomicBool,
        stopped: AtomicBool,
        next_key: AtomicU64,
        queue: Mutex<Queue>,
        wake: Condvar,
        output: Mutex<Output>,
        options: ScanOptions,
    }

    impl Shared<'_> {
        fn stopping(&self) -> bool {
            self.stopped.load(Ordering::Relaxed) || self.cancel.load(Ordering::Relaxed)
        }
        fn send(&self, event: ScanEventKind) -> bool {
            let mut envelope = ScanEvent {
                generation: self.generation,
                event,
            };
            loop {
                if self.stopping() {
                    self.wake.notify_all();
                    return false;
                }
                match self.sender.try_send(envelope) {
                    Ok(()) => return true,
                    Err(TrySendError::Full(returned)) => {
                        envelope = returned;
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        self.stopped.store(true, Ordering::Relaxed);
                        self.wake.notify_all();
                        return false;
                    }
                }
            }
        }
        fn flush(&self, output: &mut Output) -> bool {
            if output.batch.is_empty() {
                return true;
            }
            let sent = self.send(ScanEventKind::Batch(std::mem::take(&mut output.batch)));
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
            if output.batch.len() >= BATCH_RECORDS || output.last_flush.elapsed() >= FLUSH_INTERVAL
            {
                return self.flush(&mut output);
            }
            true
        }
        fn directory_outcome(&self, key: NodeKey, code: Option<u32>) -> bool {
            let mut output = self.output.lock().unwrap();
            if !self.flush(&mut output) {
                return false;
            }
            let event = if let Some(code) = code {
                output.progress.directories_failed += 1;
                ScanEventKind::DirectoryFailed { key, code }
            } else {
                output.progress.directories_done += 1;
                ScanEventKind::DirectoryDone(key)
            };
            self.send(event)
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
            let mut queue = self.queue.lock().unwrap();
            if queue.entries.len() < self.options.queue_capacity {
                queue.entries.push_back(work);
                queue.outstanding += 1;
                queue.report.max_queue_depth =
                    queue.report.max_queue_depth.max(queue.entries.len());
                self.wake.notify_one();
            } else if local.len() < self.options.local_stack_capacity {
                local.push(work);
                queue.outstanding += 1;
                queue.report.max_local_stack_depth =
                    queue.report.max_local_stack_depth.max(local.len());
            } else {
                queue.report.local_stack_limit_hits += 1;
                drop(queue);
                return self.directory_outcome(work.key, Some(LOCAL_STACK_LIMIT_CODE));
            }
            true
        }
    }

    pub fn run_win_scan_with_options(
        generation: ScanGeneration,
        root: PathBuf,
        sender: SyncSender<ScanEvent>,
        cancel: &AtomicBool,
        options: ScanOptions,
    ) -> (ProducerOutcome, ScanReport) {
        let options = ScanOptions {
            workers: options.workers.clamp(1, 8),
            queue_capacity: options.queue_capacity.clamp(1, 1024),
            local_stack_capacity: options.local_stack_capacity.clamp(1, 4096),
        };
        let root_key = NodeKey::new(0);
        let root_name = root
            .file_name()
            .filter(|name| valid_name(&name.encode_wide().collect::<Vec<_>>()))
            .unwrap_or(OsStr::new("Volume"))
            .to_owned();
        let shared = Shared {
            generation,
            sender,
            cancel,
            stopped: AtomicBool::new(false),
            next_key: AtomicU64::new(1),
            queue: Mutex::new(Queue {
                entries: VecDeque::from([Work {
                    path: root,
                    key: root_key,
                }]),
                outstanding: 1,
                report: ScanReport {
                    max_queue_depth: 1,
                    ..ScanReport::default()
                },
            }),
            wake: Condvar::new(),
            output: Mutex::new(Output {
                batch: Batch::new(),
                last_flush: Instant::now(),
                progress: Progress::default(),
            }),
            options,
        };
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
                let mut workers = Vec::new();
                for _ in 0..options.workers {
                    match thread::Builder::new()
                        .name("hogmap-directory".into())
                        .spawn_scoped(scope, || {
                            // Catch before joining other workers, so a panic cannot strand idle waiters.
                            if !std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                worker(&shared)
                            }))
                            .unwrap_or(false)
                            {
                                shared.stopped.store(true, Ordering::Relaxed);
                                shared.wake.notify_all();
                                false
                            } else {
                                true
                            }
                        }) {
                        Ok(worker) => workers.push(worker),
                        Err(_) => {
                            worker_failed = true;
                            shared.stopped.store(true, Ordering::Relaxed);
                            shared.wake.notify_all();
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
        let terminal = if worker_failed {
            Terminal::Failed(WORKER_FAILED_CODE)
        } else if cancel.load(Ordering::Relaxed) {
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
        let mut local = Vec::new();
        loop {
            if shared.stopping() {
                shared.wake.notify_all();
                return true;
            }
            let work = if let Some(work) = local.pop() {
                work
            } else {
                let mut queue = shared.queue.lock().unwrap();
                loop {
                    if shared.stopping() {
                        if shared.cancel.load(Ordering::Relaxed) {
                            queue.report.cancellation_wakeups += 1;
                        }
                        shared.wake.notify_all();
                        return true;
                    }
                    if let Some(work) = queue.entries.pop_front() {
                        break work;
                    }
                    if queue.outstanding == 0 {
                        return true;
                    }
                    // The caller owns AtomicBool, so a timed wait also detects cancel with no notifier.
                    queue = shared.wake.wait_timeout(queue, WAIT_INTERVAL).unwrap().0;
                }
            };
            let result = enumerate(&work, shared, &mut local);
            if !shared.stopping() {
                let _ = shared.directory_outcome(work.key, result.err().map(|error| error.code));
            }
            let mut queue = shared.queue.lock().unwrap();
            queue.outstanding -= 1;
            shared.wake.notify_all();
        }
    }

    struct Child {
        name: Vec<u16>,
        attributes: u32,
        tag: u32,
        sizes: Sizes,
        identity: Option<FileIdentity>,
    }
    fn child(work: &Work, shared: &Shared<'_>, local: &mut Vec<Work>, child: Child) -> bool {
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
                shared.schedule(
                    Work {
                        path: work.path.join(name),
                        key,
                    },
                    local,
                )
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
        let attributes = attributes(path)?;
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

    fn enumerate(work: &Work, shared: &Shared<'_>, local: &mut Vec<Work>) -> Result<(), ScanError> {
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
            let success = unsafe {
                GetFileInformationByHandleEx(
                    handle.0,
                    FileIdExtdDirectoryInfo,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                )
            } != 0;
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
        work: &Work,
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
