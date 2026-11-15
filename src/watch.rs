//! Local metadata notifications only. No notification name is opened or followed.
//! Isolates ReadDirectoryChangesW and overlapped completion/cancellation FFI so buffer and
//! handle lifetimes remain owned by one worker, including while cancellation is reaped.
#![allow(unsafe_code)]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub const BUFFER_BYTES: usize = 64 * 1024;
pub const MAX_PATHS: usize = 64;
#[derive(Default, Debug)]
pub struct Update {
    pub started: bool,
    pub changes: u64,
    pub paths: Vec<PathBuf>,
    /// Kernel overflow, malformed records or a coalescing cap: affected paths are unknown.
    pub lost: bool,
    pub error: Option<String>,
}
pub struct Watch {
    cancel: Arc<AtomicBool>,
    update: Arc<Mutex<Update>>,
    worker: Option<JoinHandle<()>>,
}
impl Watch {
    pub fn start(root: PathBuf) -> std::io::Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let update = Arc::new(Mutex::new(Update::default()));
        let flag = cancel.clone();
        let output = update.clone();
        let worker = std::thread::Builder::new()
            .name("directory-watch".into())
            .spawn(move || {
                if let Err(error) = run(root, &flag, &output) {
                    output
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .error = Some(error);
                }
            })?;
        Ok(Self {
            cancel,
            update,
            worker: Some(worker),
        })
    }
    pub fn poll(&self) -> Update {
        std::mem::take(
            &mut *self
                .update
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub fn collect(&mut self) -> bool {
        if self.is_finished()
            && let Some(worker) = self.worker.take()
        {
            return worker.join().is_ok();
        }
        true
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Parse FILE_NOTIFY_INFORMATION without casting potentially unaligned/untrusted records.
#[cfg(any(windows, test))]
fn changes(bytes: &[u8]) -> Result<Vec<PathBuf>, ()> {
    let mut paths = Vec::new();
    let mut offset = 0usize;
    loop {
        let record = bytes.get(offset..).ok_or(())?;
        if record.len() < 12 {
            return Err(());
        }
        let number = |start| {
            u32::from_le_bytes(record[start..start + 4].try_into().expect("checked header"))
                as usize
        };
        let next = number(0);
        if !(1..=5).contains(&number(4)) {
            return Err(());
        }
        let length = number(8);
        if length == 0 || !length.is_multiple_of(2) || length > record.len() - 12 {
            return Err(());
        }
        let units: Vec<u16> = record[12..12 + length]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        if units.contains(&0) {
            return Err(());
        }
        #[cfg(windows)]
        let name = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&units)
        };
        #[cfg(not(windows))]
        let name = std::ffi::OsString::from(String::from_utf16(&units).map_err(|_| ())?);
        let path = PathBuf::from(name);
        // Every reported name is one relative component. `Path::components` splits on both
        // separators, so "a\b" would otherwise pass as two ordinary parts.
        let mut parts = path.components();
        if !matches!(parts.next(), Some(std::path::Component::Normal(_))) || parts.next().is_some()
        {
            return Err(());
        }
        paths.push(path);
        if paths.len() > MAX_PATHS {
            return Err(());
        }
        if next == 0 {
            return Ok(paths);
        }
        if next < 12 + length || !next.is_multiple_of(4) {
            return Err(());
        }
        offset = offset.checked_add(next).ok_or(())?;
    }
}

#[cfg(not(windows))]
fn run(_: PathBuf, _: &AtomicBool, _: &Mutex<Update>) -> Result<(), String> {
    Err("Watching requires Windows; the scan is unchanged.".into())
}

// DRIVE_REMOTE is defined in WindowsProgramming, a feature this crate does not enable.
// GetDriveTypeW documents value 4 for remote drives; no extra binding feature is needed.
#[cfg(windows)]
const DRIVE_REMOTE: u32 = 4;

#[cfg(windows)]
fn watch_failed(stage: &str, code: u32) -> String {
    format!("Watch unavailable: {stage} (Win32 code {code}). Scan unchanged; use Rescan.")
}

#[cfg(windows)]
fn watch_io_failed(stage: &str, error: std::io::Error) -> String {
    watch_failed(stage, error.raw_os_error().unwrap_or(-1) as u32)
}

/// Pure decision, so the rejection is testable without a mapped remote drive.
#[cfg(windows)]
fn remote_drive_rejection(drive_type: u32) -> Option<&'static str> {
    (drive_type == DRIVE_REMOTE)
        .then_some("Watch unavailable on a network drive; scan unchanged. Use Rescan.")
}

/// Local drive root (`X:\`) of `path`, read from the prefix and root components alone. Nothing
/// beyond those two is resolved, so the answer costs no file-system access. A path that is not a
/// plain local absolute root has no such prefix.
#[cfg(windows)]
fn local_drive_prefix(path: &Path) -> Option<PathBuf> {
    let mut parts = path.components();
    let letter = match parts.next()? {
        std::path::Component::Prefix(value) => match value.kind() {
            std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => letter,
            _ => return None,
        },
        _ => return None,
    };
    matches!(parts.next()?, std::path::Component::RootDir)
        .then(|| PathBuf::from(format!("{}:\\", char::from(letter))))
}

/// Win32 type of the drive named by a root such as `X:\`.
#[cfg(windows)]
fn drive_type_of(root: &Path) -> u32 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a live NUL-terminated UTF-16 buffer.
    unsafe { GetDriveTypeW(wide.as_ptr()) }
}

/// Win32 type of the drive that holds `path`, decided from the lexical drive prefix alone. This is
/// the first check, so a mapped remote drive is refused before anything resolves the path.
#[cfg(windows)]
fn drive_type(path: &Path) -> Option<u32> {
    Some(drive_type_of(&local_drive_prefix(path)?))
}

/// Win32 type of the volume that holds `path`, resolved through the volume root. Only the final
/// check uses it, after the lexical prefix check has already refused remote drives.
#[cfg(windows)]
fn volume_drive_type(path: &Path) -> Option<u32> {
    let root = crate::volume::volume_root_for(path)?;
    Some(drive_type_of(Path::new(&root)))
}

#[cfg(windows)]
fn observed_attributes(path: &Path) -> std::io::Result<u32> {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path).map(|metadata| metadata.file_attributes())
}

#[cfg(windows)]
fn reject_remote_drive(
    root: &Path,
    drive_type: &mut impl FnMut(&Path) -> Option<u32>,
) -> Result<(), String> {
    let drive = drive_type(root)
        .ok_or_else(|| "Watch cannot establish a local volume; scan unchanged.".to_string())?;
    match remote_drive_rejection(drive) {
        Some(rejection) => Err(rejection.to_string()),
        None => Ok(()),
    }
}

/// Refuses a root before the watcher opens it. Both drive-type sources and the per-path attributes
/// are injected so the order of the checks can be tested without a mapped remote drive.
#[cfg(windows)]
fn validate_root(
    root: &Path,
    cancel: &AtomicBool,
    mut prefix_drive_type: impl FnMut(&Path) -> Option<u32>,
    mut volume_drive_type: impl FnMut(&Path) -> Option<u32>,
    mut attributes: impl FnMut(&Path) -> std::io::Result<u32>,
) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let mut local = false;
    for part in root.components() {
        if let std::path::Component::Prefix(value) = part {
            local = matches!(
                value.kind(),
                std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
            );
            if !local {
                return Err(
                    "Watch unavailable for network or device paths; scan unchanged. Use Rescan."
                        .into(),
                );
            }
        }
        if matches!(
            part,
            std::path::Component::ParentDir | std::path::Component::CurDir
        ) {
            return Err("Watch rejected an unsafe root; scan unchanged.".into());
        }
    }
    if !local || !root.is_absolute() {
        return Err("Watch needs an absolute local scan root; scan unchanged.".into());
    }
    // Decide the drive type from the lexical drive prefix alone, before anything resolves the
    // full path: GetVolumePathNameW and the first component query both reach the volume, and a
    // mapped remote drive would answer them with SMB metadata I/O, which can block.
    reject_remote_drive(root, &mut prefix_drive_type)?;
    let mut prefix = PathBuf::new();
    for part in root.components() {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        prefix.push(part);
        if matches!(part, std::path::Component::Prefix(_)) {
            continue;
        }
        let observed = attributes(&prefix).map_err(|e| watch_io_failed("path check", e))?;
        if observed
            & (FILE_ATTRIBUTE_REPARSE_POINT
                | FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
        {
            return Err(
                "Watch does not follow observed reparse points or cloud paths; scan unchanged."
                    .into(),
            );
        }
    }
    // The volume could be re-mounted while the components were checked, and the open below
    // resolves the whole path again, so re-check the type of the real volume root immediately
    // before it.
    reject_remote_drive(root, &mut volume_drive_type)
}

#[cfg(windows)]
fn run(root: PathBuf, cancel: &AtomicBool, output: &Mutex<Update>) -> Result<(), String> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::IO::*};
    validate_root(
        &root,
        cancel,
        drive_type,
        volume_drive_type,
        observed_attributes,
    )?;
    // Validation returns Ok on cancel, which is not a success: do not open the root afterwards.
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    let extended =
        crate::scan_win::extended_path(&root).map_err(|e| watch_failed("root path", e.code))?;
    use std::os::windows::ffi::OsStringExt;
    let extended = std::ffi::OsString::from_wide(&extended[..extended.len() - 1]);
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS
                | FILE_FLAG_OPEN_REPARSE_POINT
                | FILE_FLAG_OPEN_NO_RECALL
                | FILE_FLAG_OVERLAPPED,
        )
        .open(&extended)
        .map_err(|e| watch_io_failed("open root", e))?;
    let handle = file.as_raw_handle();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: owned root handle and correctly sized output; no file data access.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(watch_failed("root attributes", unsafe { GetLastError() }));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || info.dwFileAttributes
            & (FILE_ATTRIBUTE_REPARSE_POINT
                | FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
    {
        return Err("Watch root became unsafe; scan unchanged.".into());
    }
    // u32 storage gives the DWORD alignment required by ReadDirectoryChangesW.
    let mut buffer = vec![0u32; BUFFER_BYTES / 4];
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut overlapped = OVERLAPPED::default();
        // SAFETY: buffer and OVERLAPPED stay alive and stationary until completion (including
        // cancellation). Kernel subtree notifications never open notification names in HogMap.
        let submitted = unsafe {
            ReadDirectoryChangesW(
                handle,
                buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
                1,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE,
                std::ptr::null_mut(),
                &mut overlapped,
                None,
            )
        };
        if submitted == 0 {
            return Err(watch_failed("start notifications", unsafe {
                GetLastError()
            }));
        }
        output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .started = true;
        let mut transferred = 0;
        loop {
            if cancel.load(Ordering::Acquire) {
                // SAFETY: cancel exactly this operation, then reap it before buffer/handle drop.
                // A driver can delay completion; only this worker waits, never the UI.
                unsafe {
                    CancelIoEx(handle, &overlapped);
                    GetOverlappedResult(handle, &overlapped, &mut transferred, 1);
                }
                return Ok(());
            }
            // SAFETY: live pending request, nonblocking query, valid output pointer.
            if unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 0) } != 0 {
                break;
            }
            let code = unsafe { GetLastError() };
            if code == ERROR_NOTIFY_ENUM_DIR {
                transferred = 0;
                break;
            }
            if code != ERROR_IO_INCOMPLETE {
                return Err(watch_failed("notifications", code));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // SAFETY: operation completed, byte count is bounded by the submitted allocation.
        let bytes = if transferred as usize <= BUFFER_BYTES {
            unsafe {
                std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), transferred as usize)
            }
        } else {
            &[]
        };
        let parsed = if bytes.is_empty() {
            Err(())
        } else {
            changes(bytes)
        };
        let mut update = output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match parsed {
            Ok(paths) => {
                update.changes = update.changes.saturating_add(paths.len() as u64);
                for path in paths {
                    if update.paths.len() < MAX_PATHS {
                        update.paths.push(path);
                    } else {
                        update.lost = true;
                    }
                }
            }
            Err(()) => update.lost = true,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_parser_rejects_truncated_oversized_and_unsafe_names() {
        assert!(changes(&[]).is_err());
        let record = |name: &str| {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut bytes = vec![0; 12];
            bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&(units.len() as u32 * 2).to_le_bytes());
            for unit in units {
                bytes.extend(unit.to_le_bytes());
            }
            bytes
        };
        assert_eq!(
            changes(&record("child.bin")).unwrap(),
            [PathBuf::from("child.bin")]
        );
        assert!(changes(&record("../outside")).is_err());
        // One relative component only: a separator would otherwise split into two names.
        assert!(changes(&record("a/b")).is_err());
        // The shapes a real name can have and the rule must keep accepting: a space, a dot that
        // is not a whole name, and non-ASCII characters.
        assert_eq!(
            changes(&record("two words.txt")).unwrap(),
            [PathBuf::from("two words.txt")]
        );
        assert_eq!(
            changes(&record("ñandú.dat")).unwrap(),
            [PathBuf::from("ñandú.dat")]
        );
        // The backslash splits a name only on Windows, where the notifications come from.
        #[cfg(windows)]
        assert!(changes(&record(r"a\b")).is_err());
        let mut bytes = record("child");
        bytes[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(changes(&bytes).is_err());
        let mut bytes = record("child");
        bytes.pop();
        assert!(changes(&bytes).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn cancelling_an_idle_watch_reaps_the_operation_and_releases_the_root_handle() {
        let root =
            std::env::temp_dir().join(format!("hogmap-watch-fixture-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let mut watch = Watch::start(root.clone()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let update = watch.poll();
            assert!(update.error.is_none(), "{:?}", update.error);
            if update.started {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        watch.stop();
        while !watch.is_finished() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(watch.collect());
        std::fs::remove_dir(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn a_local_drive_prefix_is_read_lexically_from_the_path() {
        assert_eq!(
            local_drive_prefix(Path::new(r"C:\fixture\child")),
            Some(PathBuf::from(r"C:\"))
        );
        assert_eq!(
            local_drive_prefix(Path::new(r"\\?\D:\fixture")),
            Some(PathBuf::from(r"D:\"))
        );
        // Anything that is not a plain local absolute root has no lexical drive prefix.
        for rejected in [
            r"C:fixture",
            r"relative\child",
            r"\fixture",
            r"\\server\share\child",
            r"\\?\UNC\server\share\child",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\child",
        ] {
            assert_eq!(local_drive_prefix(Path::new(rejected)), None, "{rejected}");
        }
    }
    #[cfg(windows)]
    #[test]
    fn a_mapped_remote_drive_is_refused_before_any_component_metadata_query() {
        let root = PathBuf::from(r"C:\fixture\child");
        let cancel = AtomicBool::new(false);
        // One shared log, because all three injected sources record into it.
        let order = std::cell::RefCell::new(Vec::new());
        let rejection = validate_root(
            &root,
            &cancel,
            |path| {
                order
                    .borrow_mut()
                    .push(format!("prefix {}", path.display()));
                Some(DRIVE_REMOTE)
            },
            |path| {
                order
                    .borrow_mut()
                    .push(format!("volume {}", path.display()));
                Some(3)
            },
            |path: &Path| {
                order
                    .borrow_mut()
                    .push(format!("attributes {}", path.display()));
                Ok(0)
            },
        );
        assert_eq!(
            rejection,
            Err("Watch unavailable on a network drive; scan unchanged. Use Rescan.".to_string())
        );
        // The lexical prefix is the only thing touched: no volume resolution, no component query.
        assert_eq!(order.into_inner(), [format!("prefix {}", root.display())]);
        // The same root on a local drive does reach the per-component checks, and the volume root
        // is resolved only for the final check, immediately before the open.
        let order = std::cell::RefCell::new(Vec::new());
        let accepted = validate_root(
            &root,
            &cancel,
            |path| {
                order
                    .borrow_mut()
                    .push(format!("prefix {}", path.display()));
                Some(3)
            },
            |path| {
                order
                    .borrow_mut()
                    .push(format!("volume {}", path.display()));
                Some(3)
            },
            |path: &Path| {
                order
                    .borrow_mut()
                    .push(format!("attributes {}", path.display()));
                Ok(0)
            },
        );
        assert_eq!(accepted, Ok(()));
        assert_eq!(
            order.into_inner(),
            [
                format!("prefix {}", root.display()),
                r"attributes C:\".to_string(),
                r"attributes C:\fixture".to_string(),
                format!("attributes {}", root.display()),
                format!("volume {}", root.display()),
            ]
        );
    }
    /// Owns a unique fixture directory. The guard exists before the directory does, so a failure
    /// while creating it cannot leave a tree behind, and it never removes a directory it did not
    /// create itself.
    #[cfg(windows)]
    struct JunctionFixture {
        base: PathBuf,
        junction: PathBuf,
        owned: bool,
    }
    #[cfg(windows)]
    impl JunctionFixture {
        /// Creates the base directory and arms the guard. `create_dir` refuses an existing path, so
        /// a stale directory is an error instead of being reused and then removed.
        fn create() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let base = std::env::temp_dir().join(format!(
                "hogmap-watch-junction-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut fixture = Self {
                junction: base.join("junction"),
                base,
                owned: false,
            };
            std::fs::create_dir(&fixture.base).expect("unique fixture directory");
            fixture.owned = true;
            fixture
        }
    }
    #[cfg(windows)]
    impl Drop for JunctionFixture {
        fn drop(&mut self) {
            if !self.owned {
                return;
            }
            // The junction goes first, so cleanup never walks into its target.
            let _ = std::fs::remove_dir(&self.junction);
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }
    /// Creates a real directory junction. Junctions need no elevation, unlike symbolic links.
    #[cfg(windows)]
    fn create_junction(junction: &Path, target: &Path) -> std::io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
        // IO_REPARSE_TAG_MOUNT_POINT lives in Win32_System_SystemServices, a feature this crate
        // does not enable; 0xA0000003 is the documented junction tag.
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
        // REPARSE_DATA_BUFFER: tag, data length, reserved, then the MOUNT_POINT_REPARSE_BUFFER
        // name offsets and lengths, followed by the substitute and print names. Unlike the
        // symbolic-link layout, the mount-point one has no flags field.
        const HEADER_BYTES: usize = 16;
        const MOUNT_POINT_BYTES: usize = 8;
        let mut substitute: Vec<u16> = r"\??\".encode_utf16().collect();
        substitute.extend(target.as_os_str().encode_wide());
        let print: Vec<u16> = target.as_os_str().encode_wide().collect();
        // PathBuffer holds the substitute name, a NUL, the print name and a NUL. The recorded
        // lengths exclude the terminators, and the print name starts past the first one.
        let mut names = substitute.clone();
        names.push(0);
        names.extend(&print);
        names.push(0);
        let names_bytes = names.len() * 2;
        let mut buffer = vec![0u8; HEADER_BYTES + names_bytes];
        buffer[0..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        buffer[4..6].copy_from_slice(&((MOUNT_POINT_BYTES + names_bytes) as u16).to_le_bytes());
        buffer[10..12].copy_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
        buffer[12..14].copy_from_slice(&(((substitute.len() + 1) * 2) as u16).to_le_bytes());
        buffer[14..16].copy_from_slice(&((print.len() * 2) as u16).to_le_bytes());
        for (index, unit) in names.iter().enumerate() {
            let at = HEADER_BYTES + index * 2;
            buffer[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        let directory = std::fs::OpenOptions::new()
            .access_mode(GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(junction)?;
        let mut returned = 0;
        // SAFETY: the handle is a live directory opened for write, the input buffer holds the
        // documented reparse layout, and this control code takes no output buffer.
        let written = unsafe {
            DeviceIoControl(
                directory.as_raw_handle(),
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if written == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    #[test]
    fn a_junction_ancestor_is_refused_without_querying_beyond_it() {
        // The guard owns the base before anything exists under it, so every later failure cleans
        // up: the junction first, then the tree.
        let fixture = JunctionFixture::create();
        let target = fixture.base.join("target");
        std::fs::create_dir_all(target.join("child")).unwrap();
        std::fs::create_dir(&fixture.junction).unwrap();
        create_junction(&fixture.junction, &target).unwrap();
        let junction = fixture.junction.clone();
        let root = junction.join("child");
        let mut queried = Vec::new();
        let rejection = validate_root(
            &root,
            &AtomicBool::new(false),
            drive_type,
            volume_drive_type,
            |path: &Path| {
                queried.push(path.to_path_buf());
                observed_attributes(path)
            },
        );
        assert_eq!(
            rejection,
            Err(
                "Watch does not follow observed reparse points or cloud paths; scan unchanged."
                    .to_string()
            )
        );
        assert_eq!(queried.last(), Some(&junction));
        assert!(!queried.contains(&root));
        // The real watcher refuses the same root and reports why instead of opening it.
        let watch = Watch::start(root).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let error = loop {
            if let Some(error) = watch.poll().error {
                break error;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the watcher accepted a junction root"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(
            error,
            "Watch does not follow observed reparse points or cloud paths; scan unchanged."
        );
        watch.stop();
        drop(watch);
        drop(fixture);
    }
}
