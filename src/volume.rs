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
pub(crate) use platform::filesystem_for;
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
