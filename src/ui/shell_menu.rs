//! Audited Windows shell boundary. No command strings, file writes or background invocation.
//! Shell extensions execute inside this process: neither a timeout nor crash isolation is promised.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem::size_of;
use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};
use std::path::Path;
use std::ptr::{NonNull, null, null_mut};
use std::rc::Rc;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_POINTER, GetLastError, HWND, LPARAM, LRESULT, POINT,
    RPC_E_CHANGED_MODE, SetLastError, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::{
    CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMIC_MASK_SHIFT_DOWN,
    CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, SEE_MASK_UNICODE, SHBindToParent,
    SHParseDisplayName,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::core::{GUID, HRESULT, w};

use super::{ContextMenuRequest, text};

const FIRST_COMMAND: u32 = 1;
const LAST_COMMAND: u32 = 0x7fff;
const CLASS_NAME: *const u16 = w!("HogMapShellMenuOwner");
const IID_FOLDER: GUID = GUID::from_u128(0x000214e6_0000_0000_c000_000000000046);
const IID_MENU: GUID = GUID::from_u128(0x000214e4_0000_0000_c000_000000000046);
const IID_MENU2: GUID = GUID::from_u128(0x000214f4_0000_0000_c000_000000000046);
const IID_MENU3: GUID = GUID::from_u128(0xbcfce0a0_ec17_11d0_8d10_00a0c90f2719);

// windows-sys intentionally has no COM interface wrappers. ABI source: Windows SDK
// 10.0.28000.0, um/ShObjIdl_core.h:2266-2320,2447-2511,2611-2687,4643-4750.
// Every inherited/unused slot remains present; opaque pointers are never called.
#[repr(C)]
struct UnknownVtable {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    _add_ref: *const c_void,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}
#[repr(C)]
struct MenuVtable {
    unknown: UnknownVtable,
    query_menu: unsafe extern "system" fn(*mut c_void, HMENU, u32, u32, u32, u32) -> HRESULT,
    invoke: unsafe extern "system" fn(*mut c_void, *const CMINVOKECOMMANDINFO) -> HRESULT,
    _command_string:
        unsafe extern "system" fn(*mut c_void, usize, u32, *mut u32, *mut u8, u32) -> HRESULT,
}
#[repr(C)]
struct Menu2Vtable {
    menu: MenuVtable,
    handle_message: unsafe extern "system" fn(*mut c_void, u32, WPARAM, LPARAM) -> HRESULT,
}
#[repr(C)]
struct Menu3Vtable {
    menu2: Menu2Vtable,
    handle_message2:
        unsafe extern "system" fn(*mut c_void, u32, WPARAM, LPARAM, *mut LRESULT) -> HRESULT,
}
#[repr(C)]
struct FolderVtable {
    unknown: UnknownVtable,
    _parse_display_name: *const c_void,
    _enum_objects: *const c_void,
    _bind_to_object: *const c_void,
    _bind_to_storage: *const c_void,
    _compare_ids: *const c_void,
    _create_view_object: *const c_void,
    _get_attributes: *const c_void,
    get_ui_object: unsafe extern "system" fn(
        *mut c_void,
        HWND,
        u32,
        *const *const ITEMIDLIST,
        *const GUID,
        *mut u32,
        *mut *mut c_void,
    ) -> HRESULT,
    _get_display_name: *const c_void,
    _set_name: *const c_void,
}

#[derive(Debug)]
enum Error {
    Missing,
    Native(&'static str, HRESULT),
    Invocation { code: HRESULT, changed: bool },
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str(text::SHELL_MISSING),
            Self::Native(stage, code) => write!(
                f,
                "Windows shell: {stage} failed (HRESULT 0x{:08X}).",
                *code as u32
            ),
            Self::Invocation { code, changed } => {
                write!(
                    f,
                    "Windows shell: InvokeCommand failed (HRESULT 0x{:08X}).",
                    *code as u32
                )?;
                if *changed {
                    write!(f, " {}", text::SHELL_CHANGED)?;
                }
                Ok(())
            }
        }
    }
}
fn check(stage: &'static str, code: HRESULT) -> Result<(), Error> {
    if code < 0 {
        Err(Error::Native(stage, code))
    } else {
        Ok(())
    }
}
fn win32_error(stage: &'static str) -> Error {
    // HRESULT_FROM_WIN32; zero on a failed API without extended error information is E_FAIL.
    let code = unsafe { GetLastError() };
    Error::Native(
        stage,
        if code == 0 {
            E_FAIL
        } else {
            (0x80070000 | (code & 0xffff)) as i32
        },
    )
}

struct Apartment {
    uninitialize: bool,
    _thread: PhantomData<Rc<()>>,
}
impl Apartment {
    fn enter() -> Result<Self, Error> {
        let code = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
        if code != RPC_E_CHANGED_MODE {
            check("CoInitializeEx", code)?;
        }
        Ok(Self {
            uninitialize: code >= 0,
            _thread: PhantomData,
        })
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe { CoUninitialize() };
        }
    }
}

struct ComPtr(NonNull<c_void>);
impl ComPtr {
    fn from_result(
        stage: &'static str,
        code: HRESULT,
        pointer: *mut c_void,
    ) -> Result<Self, Error> {
        let owned = NonNull::new(pointer).map(Self);
        check(stage, code)?;
        owned.ok_or(Error::Native(stage, E_POINTER))
    }
    // SAFETY: T must be the vtable of the IID that produced this owned interface pointer.
    unsafe fn table<T>(&self) -> &T {
        unsafe { &**self.0.as_ptr().cast::<*const T>() }
    }
    fn query(&self, iid: &GUID) -> Result<Option<Self>, Error> {
        let mut pointer = null_mut();
        let code = unsafe {
            (self.table::<UnknownVtable>().query_interface)(self.0.as_ptr(), iid, &mut pointer)
        };
        if code == E_NOINTERFACE {
            return Ok(None);
        }
        Self::from_result("QueryInterface", code, pointer).map(Some)
    }
}
impl Drop for ComPtr {
    fn drop(&mut self) {
        // Every supported interface starts with IUnknown and owns precisely one reference.
        unsafe {
            (self.table::<UnknownVtable>().release)(self.0.as_ptr());
        }
    }
}
struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(self.0.cast()) };
    }
}
struct Popup(HMENU);
impl Drop for Popup {
    fn drop(&mut self) {
        unsafe {
            DestroyMenu(self.0);
        }
    }
}

struct PreparedMenu {
    // Declaration order is release order: destroy the menu before its provider and borrowed PIDLs.
    popup: Popup,
    context: ComPtr,
    _parent: ComPtr,
    _pidl: Pidl,
    command_count: u32,
}
impl PreparedMenu {
    fn new(path: &Path, owner: HWND, extended: bool) -> Result<Self, Error> {
        let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
        if name.is_empty() || name.len() >= 32767 || name.contains(&0) || !path.is_absolute() {
            return Err(Error::Native("native path", E_INVALIDARG));
        }
        name.push(0);
        let mut raw_pidl = null_mut();
        let code =
            unsafe { SHParseDisplayName(name.as_ptr(), null_mut(), &mut raw_pidl, 0, null_mut()) };
        let pidl = Pidl(raw_pidl);
        check("SHParseDisplayName", code)?;
        if pidl.0.is_null() {
            return Err(Error::Native("SHParseDisplayName", E_POINTER));
        }
        let mut raw_parent = null_mut();
        let mut child = null_mut();
        let code = unsafe { SHBindToParent(pidl.0, &IID_FOLDER, &mut raw_parent, &mut child) };
        let parent = ComPtr::from_result("SHBindToParent", code, raw_parent)?;
        if child.is_null() {
            return Err(Error::Native("SHBindToParent", E_POINTER));
        }
        let mut raw_context = null_mut();
        let children = [child.cast_const()];
        let code = unsafe {
            (parent.table::<FolderVtable>().get_ui_object)(
                parent.0.as_ptr(),
                owner,
                1,
                children.as_ptr(),
                &IID_MENU,
                null_mut(),
                &mut raw_context,
            )
        };
        let context = ComPtr::from_result("GetUIObjectOf(IContextMenu)", code, raw_context)?;
        let popup = Popup(unsafe { CreatePopupMenu() });
        if popup.0.is_null() {
            return Err(win32_error("CreatePopupMenu"));
        }
        let flags = CMF_NORMAL | CMF_EXPLORE | if extended { CMF_EXTENDEDVERBS } else { 0 };
        let code = unsafe {
            (context.table::<MenuVtable>().query_menu)(
                context.0.as_ptr(),
                popup.0,
                0,
                FIRST_COMMAND,
                LAST_COMMAND,
                flags,
            )
        };
        check("QueryContextMenu", code)?;
        let command_count = (code as u32) & 0xffff;
        if command_count == 0
            || command_count > LAST_COMMAND
            || unsafe { GetMenuItemCount(popup.0) } <= 0
        {
            return Err(Error::Native("QueryContextMenu (no usable items)", E_FAIL));
        }
        Ok(Self {
            popup,
            context,
            _parent: parent,
            _pidl: pidl,
            command_count,
        })
    }
}

struct Forwarding {
    menu3: Option<ComPtr>,
    menu2: Option<ComPtr>,
    panicked: Cell<bool>,
}
impl Forwarding {
    fn message(&self, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        if !matches!(
            message,
            WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
        ) {
            return None;
        }
        if let Some(menu) = &self.menu3 {
            let mut result = 0;
            let code = unsafe {
                (menu.table::<Menu3Vtable>().handle_message2)(
                    menu.0.as_ptr(),
                    message,
                    wparam,
                    lparam,
                    &mut result,
                )
            };
            if code == 0 {
                return Some(result);
            }
        }
        if message != WM_MENUCHAR
            && let Some(menu) = &self.menu2
        {
            let code = unsafe {
                (menu.table::<Menu2Vtable>().handle_message)(
                    menu.0.as_ptr(),
                    message,
                    wparam,
                    lparam,
                )
            };
            if code == 0 {
                return Some(isize::from(matches!(message, WM_DRAWITEM | WM_MEASUREITEM)));
            }
        }
        None
    }
}
unsafe extern "system" fn owner_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Only MenuLink installs this stable Box pointer, on this window's UI thread, and clears it
    // before releasing the interfaces. No reducer, egui or application lock is touched here.
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const Forwarding;
    if !pointer.is_null() {
        let forwarding = unsafe { &*pointer };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            forwarding.message(message, wparam, lparam)
        })) {
            Ok(Some(result)) => return result,
            Err(_) => forwarding.panicked.set(true),
            Ok(None) => {}
        }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

struct OwnerWindow(HWND);
impl OwnerWindow {
    fn new(parent: HWND) -> Result<Self, Error> {
        static CLASS: OnceLock<Result<u16, HRESULT>> = OnceLock::new();
        let registration = CLASS.get_or_init(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(owner_proc),
                hInstance: unsafe { GetModuleHandleW(null()) },
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            let atom = unsafe { RegisterClassW(&class) };
            if atom == 0 { Err(E_FAIL) } else { Ok(atom) }
        });
        check(
            "RegisterClassW",
            match registration {
                Ok(_) => 0,
                Err(code) => *code,
            },
        )?;
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                CLASS_NAME,
                w!("HogMap shell menu owner"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                parent,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        if window.is_null() {
            Err(win32_error("CreateWindowExW"))
        } else {
            Ok(Self(window))
        }
    }
}
impl Drop for OwnerWindow {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}
struct MenuLink<'a> {
    owner: &'a OwnerWindow,
    forwarding: Box<Forwarding>,
}
impl<'a> MenuLink<'a> {
    fn new(owner: &'a OwnerWindow, context: &ComPtr) -> Result<Self, Error> {
        let forwarding = Box::new(Forwarding {
            menu3: context.query(&IID_MENU3)?,
            menu2: context.query(&IID_MENU2)?,
            panicked: Cell::new(false),
        });
        unsafe {
            SetLastError(0);
            let previous = SetWindowLongPtrW(
                owner.0,
                GWLP_USERDATA,
                (&*forwarding as *const Forwarding) as isize,
            );
            if previous == 0 && GetLastError() != 0 {
                return Err(win32_error("SetWindowLongPtrW"));
            }
        }
        Ok(Self { owner, forwarding })
    }
}
impl Drop for MenuLink<'_> {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.owner.0, GWLP_USERDATA, 0);
        }
    }
}

#[derive(PartialEq)]
struct Stamp {
    attributes: u32,
    bytes: u64,
    created: u64,
    modified: u64,
}
fn metadata(path: &Path) -> Result<Stamp, Error> {
    // Do not open contents, canonicalize, or follow the leaf reparse point.
    let value = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::Missing
        } else {
            Error::Native(
                "item metadata",
                (0x80070000 | (error.raw_os_error().unwrap_or(1) as u32 & 0xffff)) as i32,
            )
        }
    })?;
    Ok(Stamp {
        attributes: value.file_attributes(),
        bytes: value.file_size(),
        created: value.creation_time(),
        modified: value.last_write_time(),
    })
}

/// Called only for a deferred explicit secondary click, outside painting/reducer borrows.
/// Nothing is created automatically at startup or in headless renderer tests.
pub(super) fn show(request: &ContextMenuRequest) -> Result<bool, String> {
    show_native(request).map_err(|error| error.to_string())
}
fn show_native(request: &ContextMenuRequest) -> Result<bool, Error> {
    let before = metadata(&request.path)?;
    let _apartment = Apartment::enter()?;
    // GetActiveWindow is thread-local (never GetForegroundWindow/FindWindow on another process).
    // A click delivered to the real eframe window makes it the active client window here.
    let main = unsafe { GetActiveWindow() };
    if main.is_null() {
        return Err(Error::Native("active application window", E_POINTER));
    }
    let mut point = POINT { x: 0, y: 0 };
    for (value, target) in request.position.iter().zip([&mut point.x, &mut point.y]) {
        let physical = (*value * request.pixels_per_point).round();
        if !physical.is_finite()
            || !request.pixels_per_point.is_finite()
            || request.pixels_per_point <= 0.0
            || physical < i32::MIN as f32
            || physical >= i32::MAX as f32
        {
            return Err(Error::Native("menu position", E_INVALIDARG));
        }
        *target = physical as i32;
    }
    if unsafe { ClientToScreen(main, &mut point) } == 0 {
        return Err(win32_error("ClientToScreen"));
    }
    let owner = OwnerWindow::new(main)?;
    let menu = PreparedMenu::new(&request.path, owner.0, request.extended)?;
    let link = MenuLink::new(&owner, &menu.context)?;
    unsafe {
        SetForegroundWindow(main);
    }
    let command = unsafe {
        TrackPopupMenuEx(
            menu.popup.0,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            owner.0,
            null(),
        )
    } as u32;
    unsafe {
        PostMessageW(owner.0, WM_NULL, 0, 0);
    }
    if link.forwarding.panicked.get() {
        return Err(Error::Native("menu message handler", E_FAIL));
    }
    if command == 0 {
        return Ok(false);
    }
    if !(FIRST_COMMAND..FIRST_COMMAND + menu.command_count).contains(&command) {
        return Err(Error::Native("selected command", E_INVALIDARG));
    }
    let offset = (command - FIRST_COMMAND) as usize;
    let invocation = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        // ShObjIdl_core.h:2130 aliases CMIC_MASK_UNICODE to SEE_MASK_UNICODE.
        fMask: SEE_MASK_UNICODE
            | CMIC_MASK_PTINVOKE
            | if request.extended {
                CMIC_MASK_SHIFT_DOWN
            } else {
                0
            },
        hwnd: main,
        lpVerb: offset as *const u8,
        lpVerbW: offset as *const u16,
        nShow: SW_SHOWNORMAL,
        ptInvoke: point,
        ..Default::default()
    };
    // The only invocation site: a nonzero, range-checked command returned by the user's menu.
    let code = unsafe {
        (menu.context.table::<MenuVtable>().invoke)(
            menu.context.0.as_ptr(),
            (&invocation as *const CMINVOKECOMMANDINFOEX).cast(),
        )
    };
    let after = metadata(&request.path);
    let changed = match after {
        Ok(after) => before != after,
        Err(Error::Missing) => true,
        Err(error) => {
            check("InvokeCommand", code)?;
            return Err(error);
        }
    };
    if code < 0 {
        return Err(Error::Invocation { code, changed });
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn sdk_28000_x64_com_layout_matches_literal_slots_and_invoke_offsets() {
        // SDK um/ShObjIdl_core.h:2266/2447/2611/4643; IUnknown prefix:2269-2283.
        assert_eq!(size_of::<usize>(), 8);
        assert_eq!(size_of::<UnknownVtable>(), 24);
        assert_eq!(
            [
                offset_of!(UnknownVtable, query_interface),
                offset_of!(UnknownVtable, _add_ref),
                offset_of!(UnknownVtable, release)
            ],
            [0, 8, 16]
        );
        assert_eq!(size_of::<MenuVtable>(), 48);
        assert_eq!(
            [
                offset_of!(MenuVtable, unknown),
                offset_of!(MenuVtable, query_menu),
                offset_of!(MenuVtable, invoke),
                offset_of!(MenuVtable, _command_string)
            ],
            [0, 24, 32, 40]
        );
        assert_eq!(size_of::<Menu2Vtable>(), 56);
        assert_eq!(
            [
                offset_of!(Menu2Vtable, menu),
                offset_of!(Menu2Vtable, handle_message)
            ],
            [0, 48]
        );
        assert_eq!(size_of::<Menu3Vtable>(), 64);
        assert_eq!(
            [
                offset_of!(Menu3Vtable, menu2),
                offset_of!(Menu3Vtable, handle_message2)
            ],
            [0, 56]
        );
        assert_eq!(size_of::<FolderVtable>(), 104);
        assert_eq!(
            [
                offset_of!(FolderVtable, unknown),
                offset_of!(FolderVtable, _parse_display_name),
                offset_of!(FolderVtable, _enum_objects),
                offset_of!(FolderVtable, _bind_to_object),
                offset_of!(FolderVtable, _bind_to_storage),
                offset_of!(FolderVtable, _compare_ids),
                offset_of!(FolderVtable, _create_view_object),
                offset_of!(FolderVtable, _get_attributes),
                offset_of!(FolderVtable, get_ui_object),
                offset_of!(FolderVtable, _get_display_name),
                offset_of!(FolderVtable, _set_name),
            ],
            [0, 24, 32, 40, 48, 56, 64, 72, 80, 88, 96]
        );
        // SDK um/ShObjIdl_core.h:2169-2187; windows-sys supplies this repr(C) record.
        assert_eq!(size_of::<CMINVOKECOMMANDINFO>(), 56);
        assert_eq!(size_of::<CMINVOKECOMMANDINFOEX>(), 104);
        assert_eq!(
            [
                offset_of!(CMINVOKECOMMANDINFOEX, cbSize),
                offset_of!(CMINVOKECOMMANDINFOEX, fMask),
                offset_of!(CMINVOKECOMMANDINFOEX, hwnd),
                offset_of!(CMINVOKECOMMANDINFOEX, lpVerb),
                offset_of!(CMINVOKECOMMANDINFOEX, lpParameters),
                offset_of!(CMINVOKECOMMANDINFOEX, lpDirectory),
                offset_of!(CMINVOKECOMMANDINFOEX, nShow),
                offset_of!(CMINVOKECOMMANDINFOEX, dwHotKey),
                offset_of!(CMINVOKECOMMANDINFOEX, hIcon),
                offset_of!(CMINVOKECOMMANDINFOEX, lpTitle),
                offset_of!(CMINVOKECOMMANDINFOEX, lpVerbW),
                offset_of!(CMINVOKECOMMANDINFOEX, lpParametersW),
                offset_of!(CMINVOKECOMMANDINFOEX, lpDirectoryW),
                offset_of!(CMINVOKECOMMANDINFOEX, lpTitleW),
                offset_of!(CMINVOKECOMMANDINFOEX, ptInvoke),
            ],
            [0, 4, 8, 16, 24, 32, 40, 44, 48, 56, 64, 72, 80, 88, 96]
        );
    }

    #[test]
    fn owned_file_parses_binds_and_exposes_an_open_like_verb_without_tracking_or_invoking() {
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).expect("owned shell fixture cleanup");
            }
        }
        let _apartment = Apartment::enter().expect("COM apartment");
        let root = std::env::temp_dir().join(format!(
            "hogmap-shell-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).expect("create owned shell fixture");
        let fixture = Fixture(root);
        let path = fixture.0.join("sample.txt");
        std::fs::write(&path, []).expect("create empty owned item");
        let menu = PreparedMenu::new(&path, null_mut(), false)
            .expect("parse, bind, GetUIObjectOf, QueryContextMenu");
        assert!(unsafe { GetMenuItemCount(menu.popup.0) } > 0);
        let mut verbs = Vec::new();
        for offset in 0..menu.command_count.min(512) {
            let mut name = [0u16; 256];
            let code = unsafe {
                (menu.context.table::<MenuVtable>()._command_string)(
                    menu.context.0.as_ptr(),
                    offset as usize,
                    windows_sys::Win32::UI::Shell::GCS_VERBW,
                    null_mut(),
                    name.as_mut_ptr().cast(),
                    name.len() as u32,
                )
            };
            if code >= 0 {
                let end = name
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(name.len());
                verbs.push(String::from_utf16_lossy(&name[..end]).to_ascii_lowercase());
            }
        }
        assert!(
            verbs.iter().any(|verb| verb.starts_with("open")),
            "canonical verbs: {verbs:?}"
        );
        assert!(matches!(
            metadata(&fixture.0.join("missing.txt")),
            Err(Error::Missing)
        ));
        // No helper HWND, TrackPopupMenuEx, InvokeCommand or content read occurs in this test.
    }
}
