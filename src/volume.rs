//! Drive metadata only; this module does not enumerate network resources.
#![allow(unsafe_code)]

use std::path::Path;

pub use crate::scan_win::ScanError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    pub root: String,
    pub label: Option<String>,
    pub filesystem: Option<String>,
    pub total_bytes: Option<u64>,
    /// Free bytes on the volume, not quota-available bytes. Some(0) means full; None is unknown.
    pub free_bytes: Option<u64>,
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

#[cfg(windows)]
pub use platform::{list_drives, volume_root_for};

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
        GetDiskFreeSpaceExW, GetLogicalDriveStringsW, GetVolumeInformationW, GetVolumePathNameW,
    };

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
            let mut total = 0;
            let mut free = 0;
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
            // SAFETY: root is NUL-terminated and the capacity outputs are valid u64 pointers.
            let capacity_ok = unsafe {
                GetDiskFreeSpaceExW(terminated.as_ptr(), null_mut(), &mut total, &mut free)
            } != 0;
            drives.push(VolumeInfo {
                root: text(root),
                label: information_ok.then(|| text(&label)),
                filesystem: information_ok.then(|| text(&filesystem)),
                total_bytes: capacity_ok.then_some(total),
                free_bytes: capacity_ok.then_some(free),
            });
        }
        Ok(drives)
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
}
