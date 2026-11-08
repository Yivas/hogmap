//! Drive metadata only; this module does not enumerate network resources.
#![allow(unsafe_code)]

use std::path::Path;
use std::time::SystemTime;

pub use crate::scan_win::ScanError;

/// Provider and scope of the capacity query, including failed attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacitySource {
    /// GetDiskSpaceInformationW Actual* fields: whole filesystem, without caller quotas.
    WindowsActualGlobalFilesystem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityIssue {
    QueryFailed(i32),
    InvalidAllocationUnit,
    SizeOverflow,
    IncoherentFreeSpace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacityMetadata {
    pub source: CapacitySource,
    /// Wall-clock completion time of the query, not an atomic snapshot with file metadata.
    pub captured_at: SystemTime,
    pub issue: Option<CapacityIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    pub root: String,
    pub label: Option<String>,
    pub filesystem: Option<String>,
    /// Global filesystem capacity, never a caller-quota total or partition length.
    pub total_bytes: Option<u64>,
    /// Global available filesystem bytes, ignoring caller quotas. Some(0) is known full.
    pub free_bytes: Option<u64>,
    pub capacity: CapacityMetadata,
}

/// Evidence about the selected directory, not a reconciliation decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootScope {
    WholeVolume,
    Folder,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginQuery {
    Attributes,
    FinalPath,
    FileIdentity,
    Filesystem,
    OpenVolumeRoot,
    RetainRoot,
    ErrorMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginIssue {
    QueryFailed { query: OriginQuery, code: u32 },
    UnsupportedNamespace,
    InvalidFinalPath,
    MissingFileIdentity,
    UnsafeDirectory,
    UnprovenVolumeRelation,
    IdentityChanged,
    NamespaceChanged,
    Cancelled,
    ScanNotFinished,
    RootNotCaptured,
    UnsupportedPlatform,
}

/// Opaque handle identity. It is deliberately unrelated to the enumerator's serial32 key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct OriginIdentity {
    serial: u64,
    file: u128,
}
impl std::fmt::Debug for OriginIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OriginIdentity([redacted])")
    }
}

#[derive(Clone, Debug)]
pub struct RootObservation {
    pub scope: RootScope,
    pub scope_issue: Option<OriginIssue>,
    pub directory_identity: Result<OriginIdentity, OriginIssue>,
    pub volume_root_identity: Result<OriginIdentity, OriginIssue>,
    /// GetVolumeInformationByHandleW support bits, not evidence of actual sharing or compression.
    pub filesystem_flags: Result<u32, OriginIssue>,
    pub captured_at: SystemTime,
}

#[derive(Clone, Copy, Debug)]
pub struct OriginCapacitySample {
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub metadata: CapacityMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedContinuity {
    UnchangedAtEndpoints,
    Changed,
}

#[derive(Clone, Copy, Debug)]
pub struct OriginValidation {
    pub directory_identity: Result<ObservedContinuity, OriginIssue>,
    pub normalized_path: Result<ObservedContinuity, OriginIssue>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DirectoryNamespaceObservations {
    pub opened_directories: u64,
    pub same_guid_name: u64,
    /// Different GUID names may be aliases; this is not a foreign-volume count.
    pub different_guid_name_unproven: u64,
    pub unavailable: u64,
    /// Opened handles rejected by safety/metadata checks or stopped before supplementary capture.
    pub not_queried: u64,
    pub last_issue: Option<OriginIssue>,
}

#[cfg(not(windows))]
pub fn list_drives() -> Result<Vec<VolumeInfo>, ScanError> {
    Err(ScanError {
        code: crate::scan_win::UNSUPPORTED_PLATFORM_CODE,
    })
}

#[cfg(not(windows))]
pub fn volume_root_for(_path: &Path) -> Option<String> {
    None
}

/// Unknown includes unsupported paths, devices, descriptors and query failures.
#[cfg(not(windows))]
pub fn seek_penalty_for(_path: &Path) -> Option<bool> {
    None
}

#[cfg(windows)]
pub(crate) use platform::{OriginCapture, capture_origin, filesystem_for, observe_namespace};
#[cfg(windows)]
pub use platform::{list_drives, seek_penalty_for, volume_root_for};

/// Suppresses removable-device dialogs on this thread only, preserving other mode bits.
#[cfg(windows)]
pub(crate) struct ErrorModeGuard {
    previous: u32,
    // A saved thread mode must never be restored on a different thread.
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(windows)]
impl ErrorModeGuard {
    pub(crate) fn new() -> Result<Self, ScanError> {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            GetThreadErrorMode, SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX, SetThreadErrorMode,
        };
        // SAFETY: this query has no preconditions and changes no state.
        let current = unsafe { GetThreadErrorMode() };
        let mut previous = 0;
        // SAFETY: previous is writable; the mode affects only the calling thread.
        if unsafe {
            SetThreadErrorMode(
                current | SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX,
                &mut previous,
            )
        } == 0
        {
            return Err(ScanError::last());
        }
        Ok(Self {
            previous,
            _thread_bound: std::marker::PhantomData,
        })
    }
}

#[cfg(windows)]
impl Drop for ErrorModeGuard {
    fn drop(&mut self) {
        // SAFETY: the saved mode was returned by SetThreadErrorMode on this same thread.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::SetThreadErrorMode(
                self.previous,
                std::ptr::null_mut(),
            );
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ptr::null_mut;
    use windows_sys::Win32::Storage::FileSystem::{
        DISK_SPACE_INFORMATION, GetDiskSpaceInformationW, GetLogicalDriveStringsW,
        GetVolumeInformationW, GetVolumePathNameW,
    };

    use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ID_INFO,
        FILE_NAME_NORMALIZED, FileIdInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
        GetVolumeInformationByHandleW, VOLUME_NAME_GUID,
    };

    struct GuidPath {
        full: Vec<u16>,
        root: Vec<u16>,
        at_root: bool,
    }

    fn issue(query: OriginQuery, code: u32) -> OriginIssue {
        OriginIssue::QueryFailed { query, code }
    }
    fn running(cancelled: &impl Fn() -> bool) -> Result<(), OriginIssue> {
        if cancelled() {
            Err(OriginIssue::Cancelled)
        } else {
            Ok(())
        }
    }
    fn identity_result(result: Result<FILE_ID_INFO, u32>) -> Result<OriginIdentity, OriginIssue> {
        let info = result.map_err(|code| issue(OriginQuery::FileIdentity, code))?;
        if info.FileId.Identifier == [0; 16] {
            return Err(OriginIssue::MissingFileIdentity);
        }
        Ok(OriginIdentity {
            serial: info.VolumeSerialNumber,
            file: u128::from_le_bytes(info.FileId.Identifier),
        })
    }
    fn identity(
        handle: BorrowedHandle<'_>,
        cancelled: &impl Fn() -> bool,
    ) -> Result<OriginIdentity, OriginIssue> {
        running(cancelled)?;
        let mut info = FILE_ID_INFO::default();
        // SAFETY: borrowed live directory handle and exact binding-defined writable output.
        let result = unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle(),
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        identity_result(if result != 0 {
            Ok(info)
        } else {
            Err(ScanError::last().code)
        })
    }
    fn guid_path(units: Vec<u16>) -> Result<GuidPath, OriginIssue> {
        let prefix: Vec<u16> = r"\\?\Volume{".encode_utf16().collect();
        if !units.starts_with(&prefix) {
            return Err(OriginIssue::UnsupportedNamespace);
        }
        let end = units
            .iter()
            .position(|unit| *unit == b'}' as u16)
            .filter(|end| *end == prefix.len() + 36)
            .ok_or(OriginIssue::InvalidFinalPath)?;
        let guid = &units[prefix.len()..end];
        if !guid.iter().enumerate().all(|(position, unit)| {
            if [8, 13, 18, 23].contains(&position) {
                *unit == b'-' as u16
            } else {
                matches!(*unit, 48..=57 | 65..=70 | 97..=102)
            }
        }) || units.get(end + 1) != Some(&(b'\\' as u16))
            || units.contains(&0)
        {
            return Err(OriginIssue::InvalidFinalPath);
        }
        let root = units[..end + 2].to_vec(); // Trailing slash is mandatory: this is a directory.
        Ok(GuidPath {
            at_root: units.len() == root.len(),
            root,
            full: units,
        })
    }
    fn final_path(
        handle: BorrowedHandle<'_>,
        cancelled: &impl Fn() -> bool,
    ) -> Result<GuidPath, OriginIssue> {
        running(cancelled)?;
        let mut buffer = vec![0_u16; 32768];
        // SAFETY: live borrowed handle and bounded writable UTF-16 output, normalized GUID mode.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
            )
        } as usize;
        if length == 0 {
            return Err(issue(OriginQuery::FinalPath, ScanError::last().code));
        }
        if length >= buffer.len() {
            return Err(OriginIssue::InvalidFinalPath);
        }
        buffer.truncate(length);
        guid_path(buffer)
    }
    fn safe_directory(
        handle: BorrowedHandle<'_>,
        cancelled: &impl Fn() -> bool,
    ) -> Result<(), OriginIssue> {
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        running(cancelled)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: live borrowed handle and binding-defined writable output.
        if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
            return Err(issue(OriginQuery::Attributes, ScanError::last().code));
        }
        let forbidden = FILE_ATTRIBUTE_REPARSE_POINT
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || info.dwFileAttributes & forbidden != 0
        {
            return Err(OriginIssue::UnsafeDirectory);
        }
        Ok(())
    }
    fn open_root(
        path: &[u16],
        cancelled: &impl Fn() -> bool,
    ) -> Result<std::fs::File, OriginIssue> {
        use std::os::windows::ffi::OsStringExt;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::*;
        running(cancelled)?;
        if path.last() != Some(&(b'\\' as u16)) {
            return Err(OriginIssue::InvalidFinalPath);
        }
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(
                FILE_FLAG_BACKUP_SEMANTICS
                    | FILE_FLAG_OPEN_REPARSE_POINT
                    | FILE_FLAG_OPEN_NO_RECALL,
            )
            .open(std::ffi::OsString::from_wide(path))
            .map_err(|error| {
                issue(
                    OriginQuery::OpenVolumeRoot,
                    error.raw_os_error().unwrap_or(-1) as u32,
                )
            })?;
        safe_directory(file.as_handle(), cancelled)?;
        Ok(file)
    }
    use std::os::windows::io::AsHandle;

    fn scope_for(
        path: &GuidPath,
        candidate: &GuidPath,
        directory: OriginIdentity,
        volume: OriginIdentity,
    ) -> Result<RootScope, OriginIssue> {
        if !candidate.at_root {
            return Err(OriginIssue::UnprovenVolumeRelation);
        }
        if path.at_root && directory == volume {
            return Ok(RootScope::WholeVolume);
        }
        if !path.at_root
            && path.root == candidate.root
            && directory.serial == volume.serial
            && directory != volume
        {
            return Ok(RootScope::Folder);
        }
        Err(OriginIssue::UnprovenVolumeRelation)
    }

    // Handles and normalized names never leave this private, in-memory capture or enter Debug.
    pub(crate) struct OriginCapture {
        directory: OwnedHandle,
        path: GuidPath,
        directory_identity: OriginIdentity,
        volume_identity: OriginIdentity,
    }
    pub(crate) fn capture_origin(
        handle: BorrowedHandle<'_>,
        cancelled: &impl Fn() -> bool,
    ) -> (RootObservation, Result<OriginCapture, OriginIssue>) {
        let safety = safe_directory(handle, cancelled);
        let directory_identity = safety.and_then(|()| identity(handle, cancelled));
        let flags = (|| {
            safety?;
            running(cancelled)?;
            let mut flags = 0;
            // SAFETY: live directory handle; only the flags output is requested.
            if unsafe {
                GetVolumeInformationByHandleW(
                    handle.as_raw_handle(),
                    null_mut(),
                    0,
                    null_mut(),
                    null_mut(),
                    &mut flags,
                    null_mut(),
                    0,
                )
            } == 0
            {
                Err(issue(OriginQuery::Filesystem, ScanError::last().code))
            } else {
                Ok(flags)
            }
        })();
        let mut facts = RootObservation {
            scope: RootScope::Unknown,
            scope_issue: None,
            directory_identity,
            volume_root_identity: Err(OriginIssue::RootNotCaptured),
            filesystem_flags: flags,
            captured_at: SystemTime::now(),
        };
        let capture = (|| {
            safety?;
            let directory_identity = directory_identity?;
            let path = final_path(handle, cancelled)?;
            let volume = open_root(&path.root, cancelled)?;
            let volume_identity = identity(volume.as_handle(), cancelled);
            facts.volume_root_identity = volume_identity;
            let volume_identity = volume_identity?;
            let candidate = final_path(volume.as_handle(), cancelled)?;
            facts.scope = scope_for(&path, &candidate, directory_identity, volume_identity)?;
            running(cancelled)?;
            let directory = handle.try_clone_to_owned().map_err(|error| {
                issue(
                    OriginQuery::RetainRoot,
                    error.raw_os_error().unwrap_or(-1) as u32,
                )
            })?;
            Ok(OriginCapture {
                directory,
                path,
                directory_identity,
                volume_identity,
            })
        })();
        facts.scope_issue = capture.as_ref().err().copied();
        facts.captured_at = SystemTime::now();
        (facts, capture)
    }
    fn continuity(
        expected: OriginIdentity,
        read: Result<OriginIdentity, OriginIssue>,
    ) -> Result<ObservedContinuity, OriginIssue> {
        read.map(|value| {
            if value == expected {
                ObservedContinuity::UnchangedAtEndpoints
            } else {
                ObservedContinuity::Changed
            }
        })
    }
    fn capacity_sample(
        result: i32,
        space: &DISK_SPACE_INFORMATION,
        captured_at: SystemTime,
    ) -> OriginCapacitySample {
        let values = capacity_values(result, space);
        OriginCapacitySample {
            total_bytes: values.ok().map(|value| value.0),
            free_bytes: values.ok().map(|value| value.1),
            metadata: CapacityMetadata {
                source: CapacitySource::WindowsActualGlobalFilesystem,
                captured_at,
                issue: values.err(),
            },
        }
    }
    impl OriginCapture {
        fn verified_volume_root(
            &self,
            cancelled: &impl Fn() -> bool,
        ) -> Result<std::fs::File, OriginIssue> {
            let volume = open_root(&self.path.root, cancelled)?;
            if identity(volume.as_handle(), cancelled)? != self.volume_identity {
                return Err(OriginIssue::IdentityChanged);
            }
            let path = final_path(volume.as_handle(), cancelled)?;
            if !path.at_root || path.root != self.path.root {
                return Err(OriginIssue::UnprovenVolumeRelation);
            }
            Ok(volume)
        }
        pub(crate) fn capacity(
            &self,
            cancelled: &impl Fn() -> bool,
        ) -> Result<OriginCapacitySample, OriginIssue> {
            let _before_handle = self.verified_volume_root(cancelled)?;
            running(cancelled)?;
            let mut root = self.path.root.clone();
            root.push(0);
            let mut space = DISK_SPACE_INFORMATION::default();
            // SAFETY: normalized GUID root retains its trailing slash and NUL; exact output.
            let result = unsafe { GetDiskSpaceInformationW(root.as_ptr(), &mut space) };
            let captured_at = SystemTime::now();
            // Reopen the queried GUID directory after the path-based call: querying the same
            // retained handle twice would not detect a changed path binding. Still not atomic.
            let _after_handle = self.verified_volume_root(cancelled)?;
            Ok(capacity_sample(result, &space, captured_at))
        }
        pub(crate) fn validate(&self, cancelled: &impl Fn() -> bool) -> OriginValidation {
            OriginValidation {
                directory_identity: continuity(
                    self.directory_identity,
                    identity(self.directory.as_handle(), cancelled),
                ),
                normalized_path: final_path(self.directory.as_handle(), cancelled).map(|value| {
                    if value.full == self.path.full {
                        ObservedContinuity::UnchangedAtEndpoints
                    } else {
                        ObservedContinuity::Changed
                    }
                }),
            }
        }
    }
    pub(crate) fn observe_namespace(
        capture: Option<&OriginCapture>,
        handle: BorrowedHandle<'_>,
        counts: &mut DirectoryNamespaceObservations,
        cancelled: &impl Fn() -> bool,
    ) {
        let observation = capture
            .ok_or(OriginIssue::RootNotCaptured)
            .and_then(|capture| {
                final_path(handle, cancelled).map(|path| path.root == capture.path.root)
            });
        record_namespace_result(counts, observation);
    }
    fn record_namespace_result(
        counts: &mut DirectoryNamespaceObservations,
        observation: Result<bool, OriginIssue>,
    ) {
        // One pending item is added for this traversal open under the same mutex.
        // This sole resolution consumes it; early safety/cancellation returns leave it pending.
        counts.not_queried -= 1;
        match observation {
            Ok(true) => counts.same_guid_name += 1,
            Ok(false) => counts.different_guid_name_unproven += 1,
            Err(error) => {
                counts.unavailable += 1;
                counts.last_issue = Some(error);
            }
        }
    }

    pub(crate) fn filesystem_for(path: &Path) -> Option<String> {
        let _mode = ErrorModeGuard::new().ok()?;
        let root: Vec<u16> = volume_root_for(path)?.encode_utf16().chain([0]).collect();
        let mut filesystem = [0_u16; 261];
        // SAFETY: terminated volume root and writable filesystem buffer; other outputs unused.
        if unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        } == 0
        {
            return None;
        }
        Some(text(&filesystem))
    }

    pub fn seek_penalty_for(path: &Path) -> Option<bool> {
        use std::mem::{offset_of, size_of};
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::{
            DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
            STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
        };
        let _mode = ErrorModeGuard::new().ok()?;
        let root = volume_root_for(path)?;
        let bytes = root.as_bytes();
        if bytes.len() != 3 || !bytes[0].is_ascii_alphabetic() || bytes[1..] != *b":\\" {
            return None;
        }
        let device: Vec<u16> = format!(r"\\.\{}:", bytes[0] as char)
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: terminated local volume path; zero desired access, all share modes, no security/template.
        let handle = unsafe {
            CreateFileW(
                device.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            ..Default::default()
        };
        // Read bytes, not a Rust bool: an unexpected driver value must be rejected before conversion.
        let mut descriptor = [0_u8; size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>()];
        let mut returned = 0;
        // SAFETY: synchronous owned handle; exact input/output buffers and writable byte-count pointer.
        let success = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                (&query as *const STORAGE_PROPERTY_QUERY).cast(),
                size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                descriptor.as_mut_ptr().cast(),
                descriptor.len() as u32,
                &mut returned,
                null_mut(),
            )
        } != 0;
        // SAFETY: the handle is owned here and is closed exactly once, including query failure.
        unsafe {
            CloseHandle(handle);
        }
        let size = descriptor.len() as u32;
        if !success
            || returned != size
            || u32::from_le_bytes(descriptor[..4].try_into().ok()?) != size
            || u32::from_le_bytes(descriptor[4..8].try_into().ok()?) != size
        {
            return None;
        }
        match descriptor[offset_of!(DEVICE_SEEK_PENALTY_DESCRIPTOR, IncursSeekPenalty)] {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn text(buffer: &[u16]) -> String {
        let end = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }

    pub fn list_drives() -> Result<Vec<VolumeInfo>, ScanError> {
        let _error_mode = ErrorModeGuard::new()?;
        let mut buffer = vec![0_u16; 256];
        loop {
            // SAFETY: the writable buffer has the advertised number of UTF-16 units.
            let length =
                unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
            if length == 0 {
                return Err(ScanError::last());
            }
            if length as usize >= buffer.len() {
                buffer.resize(length as usize + 1, 0);
                continue;
            }
            break;
        }
        let mut drives = Vec::new();
        for root in buffer
            .split(|unit| *unit == 0)
            .take_while(|root| !root.is_empty())
        {
            let mut terminated = root.to_vec();
            terminated.push(0);
            let mut label = [0_u16; 261];
            let mut filesystem = [0_u16; 261];
            // SAFETY: root is NUL-terminated; all output buffers/pointers are valid for the call.
            let information_ok = unsafe {
                GetVolumeInformationW(
                    terminated.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    filesystem.as_mut_ptr(),
                    filesystem.len() as u32,
                )
            } != 0;
            let mut space = DISK_SPACE_INFORMATION::default();
            // SAFETY: root is NUL-terminated; space is a writable binding-defined output.
            // This metadata query needs no raw-disk handle or privilege request.
            let result = unsafe { GetDiskSpaceInformationW(terminated.as_ptr(), &mut space) };
            let captured_at = SystemTime::now();
            let values = capacity_values(result, &space);
            let (total_bytes, free_bytes) =
                values.map_or((None, None), |(total, free)| (Some(total), Some(free)));
            drives.push(VolumeInfo {
                root: text(root),
                label: information_ok.then(|| text(&label)),
                filesystem: information_ok.then(|| text(&filesystem)),
                total_bytes,
                free_bytes,
                capacity: CapacityMetadata {
                    source: CapacitySource::WindowsActualGlobalFilesystem,
                    captured_at,
                    issue: values.err(),
                },
            });
        }
        Ok(drives)
    }

    fn capacity_values(
        result: i32,
        space: &DISK_SPACE_INFORMATION,
    ) -> Result<(u64, u64), CapacityIssue> {
        // The documented success is S_OK. Never reinterpret a failed/unsupported query's
        // zero-initialized output as a known empty filesystem, or fall back to caller quotas.
        if result != 0 {
            return Err(CapacityIssue::QueryFailed(result));
        }
        if space.SectorsPerAllocationUnit == 0 || space.BytesPerSector == 0 {
            return Err(CapacityIssue::InvalidAllocationUnit);
        }
        if space.ActualAvailableAllocationUnits > space.ActualTotalAllocationUnits {
            return Err(CapacityIssue::IncoherentFreeSpace);
        }
        let unit = u64::from(space.SectorsPerAllocationUnit)
            .checked_mul(u64::from(space.BytesPerSector))
            .ok_or(CapacityIssue::SizeOverflow)?;
        let total = space
            .ActualTotalAllocationUnits
            .checked_mul(unit)
            .ok_or(CapacityIssue::SizeOverflow)?;
        let free = space
            .ActualAvailableAllocationUnits
            .checked_mul(unit)
            .ok_or(CapacityIssue::SizeOverflow)?;
        Ok((total, free))
    }

    pub fn volume_root_for(path: &Path) -> Option<String> {
        let _error_mode = ErrorModeGuard::new().ok()?;
        let path = crate::scan_win::extended_path(path).ok()?;
        let mut buffer = vec![0_u16; 32768];
        // SAFETY: input is NUL-terminated and output has the specified UTF-16 capacity.
        if unsafe { GetVolumePathNameW(path.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) }
            == 0
        {
            return None;
        }
        let root = text(&buffer);
        if let Some(unc) = root.strip_prefix(r"\\?\UNC\") {
            Some(format!(r"\\{unc}"))
        } else {
            Some(root.strip_prefix(r"\\?\").unwrap_or(&root).to_owned())
        }
    }

    #[cfg(test)]
    mod capacity_tests {
        use super::*;

        #[test]
        fn observed_scan_binds_folder_and_volume_metadata_without_scanning_the_volume() {
            use crate::scan::{
                ScanCancellation, ScanGeneration, ScanReducer, Terminal, event_channel,
            };
            use crate::scan_win::{
                ScanOptions, run_win_scan_observed_with_options, run_win_scan_with_options,
            };
            use crate::synthetic::ProducerOutcome;
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::*;
            struct Fixture(std::path::PathBuf);
            impl Drop for Fixture {
                fn drop(&mut self) {
                    std::fs::remove_dir_all(&self.0).expect("owned fixture cleanup");
                }
            }
            let unique = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("hogmap-origin-{}-{unique}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            let fixture = Fixture(path.clone());
            std::fs::create_dir(path.join("one")).unwrap();
            std::fs::create_dir(path.join("two")).unwrap();
            let legacy_generation = ScanGeneration::new(489);
            let (legacy_sender, legacy_receiver) = event_channel(16);
            let (legacy_outcome, legacy_report) = run_win_scan_with_options(
                legacy_generation,
                path.clone(),
                legacy_sender,
                &ScanCancellation::new(),
                ScanOptions {
                    workers: 2,
                    ..Default::default()
                },
            );
            assert_eq!(legacy_outcome, ProducerOutcome::Finished);
            let mut legacy = ScanReducer::new(legacy_generation);
            legacy.drain(&legacy_receiver, 4096);
            assert_eq!(legacy.terminal(), Some(Terminal::Finished));
            assert_eq!(legacy.index().unwrap().entries().count(), 3);
            let generation = ScanGeneration::new(490);
            let (sender, receiver) = event_channel(16);
            let result = run_win_scan_observed_with_options(
                generation,
                path.clone(),
                sender,
                &ScanCancellation::new(),
                ScanOptions {
                    workers: 2,
                    ..Default::default()
                },
            );
            assert_eq!(result.outcome, ProducerOutcome::Finished);
            let mut reducer = ScanReducer::new(generation);
            reducer.drain(&receiver, 4096);
            assert_eq!(reducer.terminal(), Some(Terminal::Finished));
            assert_eq!(reducer.index().unwrap().entries().count(), 3);
            assert_eq!(reducer.diagnostics().directory_failures, 0);
            assert_eq!(result.report.fallback_directories, 0);
            // Compare real policies, without assuming SSD/HDD/NTFS for the owned fixture.
            assert_eq!(result.report.seek_penalty, legacy_report.seek_penalty);
            assert_eq!(result.report.mft_ordered, legacy_report.mft_ordered);
            let observations = result.observations;
            assert_eq!(observations.generation, generation);
            let root = observations.root.unwrap();
            assert_eq!(root.scope, RootScope::Folder);
            assert_eq!(root.scope_issue, None);
            assert!(root.directory_identity.is_ok() && root.volume_root_identity.is_ok());
            assert!(
                root.filesystem_flags.is_ok(),
                "native by-handle flags must activate here"
            );
            let before = observations.before.unwrap();
            let after = observations.after.unwrap();
            assert!(
                before.metadata.issue.is_none(),
                "GUID capacity begin query must activate here"
            );
            assert!(
                after.metadata.issue.is_none(),
                "GUID capacity end query must activate here"
            );
            assert!(before.total_bytes.is_some() && before.free_bytes.is_some());
            assert!(after.total_bytes.is_some() && after.free_bytes.is_some());
            let finished = observations.enumeration_finished_at.unwrap();
            assert!(observations.started_at <= before.metadata.captured_at);
            assert!(
                before.metadata.captured_at <= finished && finished <= after.metadata.captured_at
            );
            let validation = observations.validation.unwrap();
            assert_eq!(
                validation.directory_identity,
                Ok(ObservedContinuity::UnchangedAtEndpoints)
            );
            assert_eq!(
                validation.normalized_path,
                Ok(ObservedContinuity::UnchangedAtEndpoints)
            );
            assert_eq!(observations.directories.opened_directories, 3);
            assert_eq!(observations.directories.same_guid_name, 3);
            assert_eq!(observations.directories.unavailable, 0);
            assert_eq!(observations.directories.not_queried, 0);
            assert_eq!(observations.directories.different_guid_name_unproven, 0);
            // Metadata only: reuse the production capture helper on the GUID-root DIRECTORY.
            // Never run the scanner on that volume root.
            {
                let _mode = ErrorModeGuard::new().unwrap();
                let file = std::fs::OpenOptions::new()
                    .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                    .custom_flags(
                        FILE_FLAG_BACKUP_SEMANTICS
                            | FILE_FLAG_OPEN_REPARSE_POINT
                            | FILE_FLAG_OPEN_NO_RECALL,
                    )
                    .open(&path)
                    .unwrap();
                let (_, capture) = capture_origin(file.as_handle(), &|| false);
                let capture = capture.expect("fixture origin available");
                let volume = open_root(&capture.path.root, &|| false).unwrap();
                let (whole, _) = capture_origin(volume.as_handle(), &|| false);
                assert_eq!(whole.scope, RootScope::WholeVolume);
                assert_eq!(whole.scope_issue, None);
                assert_eq!(whole.directory_identity, whole.volume_root_identity);
            }
            let cancelled = ScanCancellation::new();
            cancelled.cancel();
            let (sender, receiver) = event_channel(16);
            let result = run_win_scan_observed_with_options(
                ScanGeneration::new(491),
                path.clone(),
                sender,
                &cancelled,
                ScanOptions::default(),
            );
            assert_eq!(result.outcome, ProducerOutcome::Cancelled);
            assert_eq!(result.observations.generation, ScanGeneration::new(491));
            assert_eq!(result.observations.directories.opened_directories, 0);
            assert!(matches!(
                result.observations.after,
                Err(OriginIssue::Cancelled)
            ));
            assert!(matches!(
                receiver.recv().unwrap().event,
                crate::scan::ScanEventKind::Terminal(Terminal::Cancelled)
            ));
            let (sender, receiver) = event_channel(16);
            let result = run_win_scan_observed_with_options(
                ScanGeneration::new(492),
                path.join("missing"),
                sender,
                &ScanCancellation::new(),
                ScanOptions::default(),
            );
            assert_eq!(result.outcome, ProducerOutcome::Finished);
            assert!(matches!(
                result.observations.root,
                Err(OriginIssue::RootNotCaptured)
            ));
            assert!(matches!(
                result.observations.after,
                Err(OriginIssue::RootNotCaptured)
            ));
            let mut missing = ScanReducer::new(ScanGeneration::new(492));
            missing.drain(&receiver, 4096);
            assert_eq!(missing.diagnostics().directory_failures, 1);
            assert_eq!(missing.index().unwrap().entries().count(), 1);
            for number in 0..16 {
                std::fs::create_dir(path.join(format!("bounded-{number}"))).unwrap();
            }
            let (sender, receiver) = event_channel(16);
            let result = run_win_scan_observed_with_options(
                ScanGeneration::new(493),
                path.clone(),
                sender,
                &ScanCancellation::new(),
                ScanOptions {
                    workers: 1,
                    queue_capacity: 1,
                    max_pending_bytes: 1024,
                    ..Default::default()
                },
            );
            assert!(result.report.pending_memory_limit_hits > 0);
            assert!(matches!(
                result.observations.after,
                Err(OriginIssue::ScanNotFinished)
            ));
            assert!(matches!(
                result.observations.validation,
                Err(OriginIssue::ScanNotFinished)
            ));
            let mut failed = ScanReducer::new(ScanGeneration::new(493));
            failed.drain(&receiver, 4096);
            assert_eq!(
                failed.terminal(),
                Some(Terminal::Failed(crate::scan_win::PENDING_MEMORY_LIMIT_CODE))
            );
            assert_eq!(failed.index().unwrap().entries().count(), 19);
            drop(fixture);
            assert!(!path.exists());
            println!(
                "origin_native policy_parity=1 folder=1 whole_volume_metadata_only=1 by_handle_flags=1 guid_capacity_begin=1 guid_capacity_end=1 namespaces_same=3 identity_endpoints_unchanged=1 generation=1 cancelled_without_end_query=1 failed_without_end_query=1 missing_root_preserved=1 cleanup=1"
            );
        }

        #[test]
        fn native_identity_and_capacity_errors_never_become_root_proof_or_known_zero() {
            let zero = identity_result(Ok(FILE_ID_INFO::default()));
            assert_eq!(zero, Err(OriginIssue::MissingFileIdentity));
            let denied = identity_result(Err(5));
            assert_eq!(denied, Err(issue(OriginQuery::FileIdentity, 5)));
            let mut info = FILE_ID_INFO {
                VolumeSerialNumber: 0xFEDC_BA98_7654_3210,
                ..Default::default()
            };
            info.FileId.Identifier = (1_u128 << 100).to_le_bytes();
            let directory = identity_result(Ok(info)).unwrap();
            assert_eq!(directory.serial, 0xFEDC_BA98_7654_3210);
            assert_eq!(directory.file, 1_u128 << 100);
            assert_eq!(format!("{directory:?}"), "OriginIdentity([redacted])");
            assert_eq!(
                continuity(directory, zero),
                Err(OriginIssue::MissingFileIdentity)
            );
            info.FileId.Identifier = (2_u128 << 100).to_le_bytes();
            let different = identity_result(Ok(info)).unwrap();
            assert_eq!(
                continuity(directory, Ok(different)),
                Ok(ObservedContinuity::Changed)
            );
            let path = |value: &str| guid_path(value.encode_utf16().collect()).unwrap();
            let folder = path(r"\\?\Volume{00000000-0000-0000-0000-000000000001}\fixture");
            let alias = path(r"\\?\Volume{00000000-0000-0000-0000-000000000002}\");
            assert_eq!(
                scope_for(&folder, &alias, directory, different),
                Err(OriginIssue::UnprovenVolumeRelation)
            );
            let root = path(r"\\?\Volume{00000000-0000-0000-0000-000000000001}\");
            assert_eq!(
                scope_for(&root, &alias, directory, directory),
                Ok(RootScope::WholeVolume)
            );
            assert!(matches!(
                guid_path(r"\\server\share\".encode_utf16().collect()),
                Err(OriginIssue::UnsupportedNamespace)
            ));
            assert_eq!(running(&|| true), Err(OriginIssue::Cancelled));
            let mut counts = DirectoryNamespaceObservations {
                opened_directories: 2,
                not_queried: 2,
                ..Default::default()
            };
            record_namespace_result(&mut counts, Ok(false));
            record_namespace_result(&mut counts, Err(issue(OriginQuery::FinalPath, 5)));
            assert_eq!(counts.same_guid_name, 0);
            assert_eq!(counts.different_guid_name_unproven, 1);
            assert_eq!(counts.unavailable, 1);
            assert_eq!(counts.not_queried, 0);
            // Binding-defined external outputs, including populated bytes after a failed query.
            let space = DISK_SPACE_INFORMATION {
                ActualTotalAllocationUnits: 10,
                ActualAvailableAllocationUnits: 20,
                SectorsPerAllocationUnit: 1,
                BytesPerSector: 512,
                ..Default::default()
            };
            for (result, expected) in [
                (-2147467263, CapacityIssue::QueryFailed(-2147467263)),
                (0, CapacityIssue::IncoherentFreeSpace),
            ] {
                let sample = capacity_sample(result, &space, SystemTime::now());
                assert_eq!(sample.total_bytes, None);
                assert_eq!(sample.free_bytes, None);
                assert_eq!(sample.metadata.issue, Some(expected));
            }
        }

        #[test]
        fn actual_capacity_ignores_caller_quotas_and_rejects_invalid_observations() {
            let mut space = DISK_SPACE_INFORMATION {
                ActualTotalAllocationUnits: 100,
                ActualAvailableAllocationUnits: 60,
                CallerTotalAllocationUnits: 40,
                CallerAvailableAllocationUnits: 20,
                SectorsPerAllocationUnit: 2,
                BytesPerSector: 512,
                ..Default::default()
            };
            // Both an impossible mixed pair (40,60) and a plausible one (80,60) must use Actual*.
            for caller_total in [40, 80] {
                space.CallerTotalAllocationUnits = caller_total;
                assert_eq!(capacity_values(0, &space), Ok((102400, 61440)));
            }
            space.ActualAvailableAllocationUnits = 0;
            assert_eq!(capacity_values(0, &space), Ok((102400, 0)));
            space.ActualTotalAllocationUnits = 0;
            assert_eq!(capacity_values(0, &space), Ok((0, 0)));
            assert_eq!(
                capacity_values(-2147467263, &space),
                Err(CapacityIssue::QueryFailed(-2147467263))
            );
            space.ActualAvailableAllocationUnits = 1;
            assert_eq!(
                capacity_values(0, &space),
                Err(CapacityIssue::IncoherentFreeSpace)
            );
            space.ActualAvailableAllocationUnits = 0;
            space.SectorsPerAllocationUnit = 0;
            assert_eq!(
                capacity_values(0, &space),
                Err(CapacityIssue::InvalidAllocationUnit)
            );
            space.SectorsPerAllocationUnit = 1;
            space.BytesPerSector = 0;
            assert_eq!(
                capacity_values(0, &space),
                Err(CapacityIssue::InvalidAllocationUnit)
            );
            space.BytesPerSector = 2;
            space.ActualTotalAllocationUnits = u64::MAX;
            assert_eq!(capacity_values(0, &space), Err(CapacityIssue::SizeOverflow));
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn device_error_mode_is_restored_after_unwinding() {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            GetThreadErrorMode, SEM_FAILCRITICALERRORS,
        };
        // SAFETY: thread-local query has no preconditions.
        let previous = unsafe { GetThreadErrorMode() };
        let unwind = std::panic::catch_unwind(|| {
            let _guard = ErrorModeGuard::new().unwrap();
            // SAFETY: same thread-local query while the guard is alive.
            assert_ne!(unsafe { GetThreadErrorMode() } & SEM_FAILCRITICALERRORS, 0);
            panic!("exercise thread-mode restoration");
        });
        assert!(unwind.is_err());
        // SAFETY: thread-local query has no preconditions.
        assert_eq!(unsafe { GetThreadErrorMode() }, previous);
        let _ = list_drives().unwrap();
        // SAFETY: thread-local query has no preconditions.
        assert_eq!(unsafe { GetThreadErrorMode() }, previous);
    }

    #[test]
    fn local_drive_listing_activates_global_capacity_and_reports_filesystem() {
        // A broken query would silently disable the MFT frontier; compare it with the listing.
        let path = std::env::temp_dir();
        let root = volume_root_for(&path).expect("temp directory volume root");
        let before = SystemTime::now();
        let volume = list_drives()
            .unwrap()
            .into_iter()
            .find(|volume| volume.root.eq_ignore_ascii_case(&root))
            .expect("temp volume listed");
        assert!(
            volume.filesystem.is_some(),
            "the temp volume must report its filesystem"
        );
        assert_eq!(filesystem_for(&path), volume.filesystem);
        assert_eq!(
            volume.capacity.source,
            CapacitySource::WindowsActualGlobalFilesystem
        );
        assert_eq!(
            volume.capacity.issue, None,
            "the native global provider must activate on the local test volume"
        );
        assert!(
            volume.capacity.captured_at >= before
                && volume.capacity.captured_at <= SystemTime::now()
        );
        let total = volume.total_bytes.expect("global total known");
        let free = volume.free_bytes.expect("global free known");
        assert!(total > 0 && free <= total);
        println!(
            "native_capacity_source=WindowsActualGlobalFilesystem total_bytes={total} free_bytes={free} coherent=1 capture_time_recorded=1"
        );
    }
}
