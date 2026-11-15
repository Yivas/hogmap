//! Audited Common Item Dialog boundary. Called only from eframe's UI-thread logic tick.
#![allow(unsafe_code)]
use super::files::{Action, Choice};
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr::{NonNull, null, null_mut};
use windows_sys::Win32::Foundation::{E_POINTER, HWND, RPC_E_CHANGED_MODE};
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows_sys::Win32::UI::Shell::{Common::COMDLG_FILTERSPEC, *};
use windows_sys::core::{GUID, HRESULT, w};

const IID_DIALOG: GUID = GUID::from_u128(0x42f85136_db7e_439c_85f1_e4075d135fc8);
const CANCELLED: HRESULT = 0x800704c7_u32 as i32;

// Windows SDK 10.0.28000.0 um/ShObjIdl_core.h:8898,19765,20053,20373.
// IFileDialog inherits IModalWindow::Show after IUnknown. Unused slots stay opaque.
#[repr(C)]
struct Unknown {
    query: *const c_void,
    add_ref: *const c_void,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}
#[repr(C)]
struct Dialog {
    unknown: Unknown,
    show: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    set_types: unsafe extern "system" fn(*mut c_void, u32, *const COMDLG_FILTERSPEC) -> HRESULT,
    type_index: *const c_void,
    get_type_index: *const c_void,
    advise: *const c_void,
    unadvise: *const c_void,
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    default_folder: *const c_void,
    folder: *const c_void,
    get_folder: *const c_void,
    selection: *const c_void,
    set_name: *const c_void,
    get_name: *const c_void,
    set_title: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    ok_label: *const c_void,
    name_label: *const c_void,
    result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    place: *const c_void,
    extension: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    close: *const c_void,
    client_guid: *const c_void,
    clear_client: *const c_void,
    filter: *const c_void,
}
#[repr(C)]
struct ShellItem {
    unknown: Unknown,
    bind: *const c_void,
    parent: *const c_void,
    display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> HRESULT,
    attributes: *const c_void,
    compare: *const c_void,
}

fn check(stage: &str, code: HRESULT) -> Result<(), String> {
    if code < 0 {
        Err(super::text::file_dialog_error(stage, code))
    } else {
        Ok(())
    }
}
struct Apartment(bool);
impl Apartment {
    fn enter() -> Result<Self, String> {
        // SAFETY: called on the UI thread; only a successful owned initialization is balanced.
        let code = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
        if code != RPC_E_CHANGED_MODE {
            check("initialization", code)?;
        }
        Ok(Self(code >= 0))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}
struct ComPointer(NonNull<c_void>);
impl ComPointer {
    fn owned(pointer: *mut c_void, code: HRESULT, stage: &str) -> Result<Self, String> {
        let owned = NonNull::new(pointer).map(Self);
        check(stage, code)?;
        owned.ok_or_else(|| super::text::file_dialog_error(stage, E_POINTER))
    }
    // SAFETY: T is the vtable of the interface IID returned by COM at this call site.
    unsafe fn table<T>(&self) -> &T {
        unsafe { &**self.0.as_ptr().cast::<*const T>() }
    }
}
impl Drop for ComPointer {
    fn drop(&mut self) {
        unsafe {
            (self.table::<Unknown>().release)(self.0.as_ptr());
        }
    }
}
struct TaskString(*mut u16);
impl Drop for TaskString {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(self.0.cast()) };
    }
}

pub(super) fn choose(action: Action) -> Result<Choice, String> {
    let _apartment = Apartment::enter()?;
    // GetActiveWindow belongs to this calling thread, not another process's foreground window.
    let owner = unsafe { GetActiveWindow() };
    if owner.is_null() {
        return Err(super::text::DIALOG_NO_OWNER.into());
    }
    let open = action == Action::Open;
    let mut pointer = null_mut();
    // SAFETY: exact CLSID/IID, writable interface output and no aggregation.
    let code = unsafe {
        CoCreateInstance(
            if open {
                &FileOpenDialog
            } else {
                &FileSaveDialog
            },
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_DIALOG,
            &mut pointer,
        )
    };
    let dialog = ComPointer::owned(pointer, code, "creation")?;
    // SAFETY: CoCreateInstance returned IID_IFileDialog; all borrowed strings live through Show.
    let table = unsafe { dialog.table::<Dialog>() };
    let this = dialog.0.as_ptr();
    let (title, extension, filter) = match action {
        Action::Open => (
            w!("Open snapshot"),
            w!("hogmap"),
            COMDLG_FILTERSPEC {
                pszName: w!("HogMap snapshots (*.hogmap)"),
                pszSpec: w!("*.hogmap"),
            },
        ),
        Action::Save => (
            w!("Save snapshot"),
            w!("hogmap"),
            COMDLG_FILTERSPEC {
                pszName: w!("HogMap snapshots (*.hogmap)"),
                pszSpec: w!("*.hogmap"),
            },
        ),
        Action::Export => (
            w!("Export view as text"),
            w!("txt"),
            COMDLG_FILTERSPEC {
                pszName: w!("Text files (*.txt)"),
                pszSpec: w!("*.txt"),
            },
        ),
    };
    unsafe {
        let mut options = 0;
        check("options", (table.get_options)(this, &mut options))?;
        options |=
            FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_DONTADDTORECENT | FOS_NODEREFERENCELINKS;
        options |= if open {
            FOS_FILEMUSTEXIST
        } else {
            FOS_OVERWRITEPROMPT | FOS_STRICTFILETYPES
        };
        check("options", (table.set_options)(this, options))?;
        check("file types", (table.set_types)(this, 1, &filter))?;
        check("extension", (table.extension)(this, extension))?;
        check("title", (table.set_title)(this, title))?;
        let code = (table.show)(this, owner);
        if code == CANCELLED {
            return Ok(Choice::Cancelled);
        }
        check("selection", code)?;
        let mut pointer = null_mut();
        let code = (table.result)(this, &mut pointer);
        let item = ComPointer::owned(pointer, code, "result")?;
        let mut name = null_mut();
        let code =
            (item.table::<ShellItem>().display_name)(item.0.as_ptr(), SIGDN_FILESYSPATH, &mut name);
        let name = TaskString(name);
        check("filesystem path", code)?;
        if name.0.is_null() {
            return Err(super::text::file_dialog_error("filesystem path", E_POINTER));
        }
        // The shell owns the allocation contract for a terminated PWSTR. The application caps the
        // accepted Win32 path, never truncates it, and frees it on every return path.
        let mut length = 0;
        while length < hogmap::snapshot::MAX_PATH_UNITS && *name.0.add(length) != 0 {
            length += 1;
        }
        if length == hogmap::snapshot::MAX_PATH_UNITS {
            return Err(super::text::DIALOG_LONG_PATH.into());
        }
        let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(
            name.0, length,
        )));
        if !path.is_absolute() {
            return Err(super::text::DIALOG_NOT_FILESYSTEM.into());
        }
        Ok(Choice::Selected(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};
    #[repr(C)]
    struct OpenDialog {
        dialog: Dialog,
        results: *const c_void,
        selected_items: *const c_void,
    }
    #[repr(C)]
    struct SaveDialog {
        dialog: Dialog,
        save_as: *const c_void,
        properties: *const c_void,
        collected: *const c_void,
        get_properties: *const c_void,
        apply_properties: *const c_void,
    }
    #[test]
    fn file_dialog_vtables_match_literal_sdk_x64_slots() {
        assert_eq!(size_of::<Unknown>(), 24);
        assert_eq!(offset_of!(Unknown, release), 16);
        assert_eq!(size_of::<Dialog>(), 216);
        for (actual, expected) in [
            (offset_of!(Dialog, show), 24),
            (offset_of!(Dialog, set_types), 32),
            (offset_of!(Dialog, set_options), 72),
            (offset_of!(Dialog, get_options), 80),
            (offset_of!(Dialog, set_title), 136),
            (offset_of!(Dialog, result), 160),
            (offset_of!(Dialog, extension), 176),
            (offset_of!(Dialog, filter), 208),
        ] {
            assert_eq!(actual, expected);
        }
        assert_eq!(size_of::<OpenDialog>(), 232);
        assert_eq!(offset_of!(OpenDialog, results), 216);
        assert_eq!(offset_of!(OpenDialog, selected_items), 224);
        assert_eq!(size_of::<SaveDialog>(), 256);
        assert_eq!(offset_of!(SaveDialog, save_as), 216);
        assert_eq!(offset_of!(SaveDialog, apply_properties), 248);
        assert_eq!(size_of::<ShellItem>(), 64);
        assert_eq!(offset_of!(ShellItem, display_name), 40);
        assert_eq!(size_of::<COMDLG_FILTERSPEC>(), 16);
        assert_eq!(offset_of!(COMDLG_FILTERSPEC, pszName), 0);
        assert_eq!(offset_of!(COMDLG_FILTERSPEC, pszSpec), 8);
    }
}
