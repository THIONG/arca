use std::ffi::c_void;
use std::path::PathBuf;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Ole::{ReleaseStgMedium, CF_HDROP};
use windows::Win32::System::Registry::HKEY;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, InsertMenuW, HMENU, MF_BYPOSITION, MF_POPUP, MF_STRING,
};

const CLSID_ARCA: GUID = GUID::from_u128(0xe075ad96_f5bd_4bff_8c33_a29d05352efa);
const CLSID_ARCA_CLASSIC: GUID = GUID::from_u128(0xb528a7f3_c889_4c98_b052_5d7f7f778e14);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    ExtractHere,
    ExtractToFolder,
    CompressZip,
}

impl Action {
    fn title(self) -> PCWSTR {
        match self {
            Action::ExtractHere => w!("Extract here"),
            Action::ExtractToFolder => w!("Extract to a new folder"),
            Action::CompressZip => w!("Compress to .zip"),
        }
    }

    fn applies_to(self, paths: &[PathBuf]) -> bool {
        match self {
            Action::CompressZip => !paths.is_empty(),
            _ => paths.iter().any(|p| is_archive(p)),
        }
    }
}

// The name is all this looks at. Nothing here opens a file, let alone parses
// one: this DLL runs inside Explorer, and the only thing it is allowed to do
// with an archive is hand its path to arca.exe.
fn is_archive(p: &std::path::Path) -> bool {
    let n = p.to_string_lossy().to_ascii_lowercase();
    [".zip", ".tar", ".tar.gz", ".tgz"]
        .iter()
        .any(|e| n.ends_with(e))
}

fn to_pwstr(s: PCWSTR) -> Result<PWSTR> {
    unsafe { SHStrDupW(s) }
}

fn paths_from(items: Option<&IShellItemArray>) -> Vec<PathBuf> {
    let Some(items) = items else {
        return Vec::new();
    };
    let mut v = Vec::new();
    unsafe {
        let Ok(n) = items.GetCount() else {
            return v;
        };
        for i in 0..n {
            let Ok(item) = items.GetItemAt(i) else { continue };
            let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) else { continue };
            if let Ok(s) = name.to_string() {
                v.push(PathBuf::from(s));
            }
            CoTaskMemFree(Some(name.0 as *const c_void));
        }
    }
    v
}

// arca.exe sits next to this DLL, so the path is derived from where the DLL
// itself was loaded from rather than from the registry or the PATH.
fn exe_path() -> Result<PathBuf> {
    use windows::Win32::System::LibraryLoader::*;
    let mut buf = [0u16; 32_768];
    unsafe {
        let mut module = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(exe_path as *const u16),
            &mut module,
        )?;
        let n = GetModuleFileNameW(module, &mut buf) as usize;
        if n == 0 || n >= buf.len() {
            return Err(E_FAIL.into());
        }
        let dll = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        Ok(dll.with_file_name("arca.exe"))
    }
}

// Explorer is waiting on this thread. Spawning the work elsewhere keeps the
// menu from freezing if starting the process takes a moment.
fn launch(action: Action, paths: &[PathBuf]) -> Result<()> {
    let copy: Vec<PathBuf> = paths.to_vec();
    std::thread::spawn(move || {
        let _ = run(action, &copy);
    });
    Ok(())
}

fn run(action: Action, paths: &[PathBuf]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    match action {
        Action::ExtractHere | Action::ExtractToFolder => {
            for p in paths.iter().filter(|p| is_archive(p)) {
                let dest = if action == Action::ExtractHere {
                    p.parent().map(PathBuf::from).unwrap_or_default()
                } else {
                    p.with_extension("")
                };
                let mut cmd = std::process::Command::new(exe_path()?);
                cmd.creation_flags(CREATE_NO_WINDOW)
                    .arg("extract")
                    .arg(p)
                    .arg("-o")
                    .arg(dest);
                cmd.spawn().map_err(|_| Error::from(E_FAIL))?;
            }
        }
        Action::CompressZip => {
            let Some(first) = paths.first() else {
                return Ok(());
            };
            let mut cmd = std::process::Command::new(exe_path()?);
            cmd.creation_flags(CREATE_NO_WINDOW)
                .arg("create")
                .arg(first.with_extension("zip"));
            for p in paths {
                cmd.arg(p);
            }
            cmd.spawn().map_err(|_| Error::from(E_FAIL))?;
        }
    }
    Ok(())
}

#[implement(IExplorerCommand)]
struct Item(Action);

impl IExplorerCommand_Impl for Item_Impl {
    fn GetTitle(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        to_pwstr(self.0.title())
    }

    fn GetIcon(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        let exe = exe_path()?;
        let s: Vec<u16> = format!("{},0", exe.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        to_pwstr(PCWSTR(s.as_ptr()))
    }

    fn GetToolTip(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(GUID::zeroed())
    }

    fn GetState(&self, items: Option<&IShellItemArray>, _hide: BOOL) -> Result<u32> {
        let paths = paths_from(items);
        Ok(if self.0.applies_to(&paths) {
            ECS_ENABLED.0 as u32
        } else {
            ECS_HIDDEN.0 as u32
        })
    }

    fn Invoke(&self, items: Option<&IShellItemArray>, _ctx: Option<&IBindCtx>) -> Result<()> {
        let paths = paths_from(items);
        if paths.is_empty() {
            return Ok(());
        }
        launch(self.0, &paths)
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IExplorerCommand)]
struct Root;

impl IExplorerCommand_Impl for Root_Impl {
    fn GetTitle(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        to_pwstr(w!("Arca"))
    }

    fn GetIcon(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        let exe = exe_path()?;
        let s: Vec<u16> = format!("{},0", exe.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        to_pwstr(PCWSTR(s.as_ptr()))
    }

    fn GetToolTip(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(GUID::zeroed())
    }

    fn GetState(&self, items: Option<&IShellItemArray>, _hide: BOOL) -> Result<u32> {
        let paths = paths_from(items);
        Ok(if paths.is_empty() {
            ECS_HIDDEN.0 as u32
        } else {
            ECS_ENABLED.0 as u32
        })
    }

    fn Invoke(&self, _items: Option<&IShellItemArray>, _ctx: Option<&IBindCtx>) -> Result<()> {
        Ok(())
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_HASSUBCOMMANDS.0 as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        let children: Vec<IExplorerCommand> = vec![
            Item(Action::ExtractHere).into(),
            Item(Action::ExtractToFolder).into(),
            Item(Action::CompressZip).into(),
        ];
        Ok(Enumerator::new(children).into())
    }
}

#[implement(IEnumExplorerCommand)]
struct Enumerator {
    items: Vec<IExplorerCommand>,
    pos: std::cell::Cell<usize>,
}

impl Enumerator {
    fn new(items: Vec<IExplorerCommand>) -> Self {
        Enumerator { items, pos: std::cell::Cell::new(0) }
    }
}

impl IEnumExplorerCommand_Impl for Enumerator_Impl {
    fn Next(
        &self,
        wanted: u32,
        out: *mut Option<IExplorerCommand>,
        delivered: *mut u32,
    ) -> HRESULT {
        let mut n = 0u32;
        unsafe {
            while n < wanted && self.pos.get() < self.items.len() {
                let item = self.items[self.pos.get()].clone();
                *out.add(n as usize) = Some(item);
                self.pos.set(self.pos.get() + 1);
                n += 1;
            }
            if !delivered.is_null() {
                *delivered = n;
            }
        }
        if n == wanted {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, count: u32) -> Result<()> {
        self.pos.set((self.pos.get() + count as usize).min(self.items.len()));
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        self.pos.set(0);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumExplorerCommand> {
        let copy = Enumerator::new(self.items.clone());
        copy.pos.set(self.pos.get());
        Ok(copy.into())
    }
}

fn applicable_actions(paths: &[PathBuf]) -> Vec<Action> {
    [
        Action::ExtractHere,
        Action::ExtractToFolder,
        Action::CompressZip,
    ]
    .into_iter()
    .filter(|a| a.applies_to(paths))
    .collect()
}

#[implement(IShellExtInit, IContextMenu)]
struct ClassicMenu {
    paths: std::cell::RefCell<Vec<PathBuf>>,
}

impl ClassicMenu {
    fn new() -> Self {
        ClassicMenu {
            paths: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl IShellExtInit_Impl for ClassicMenu_Impl {
    fn Initialize(
        &self,
        _folder: *const ITEMIDLIST,
        data: Option<&IDataObject>,
        _key: HKEY,
    ) -> Result<()> {
        let Some(data) = data else {
            return Err(E_INVALIDARG.into());
        };
        let format = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        unsafe {
            let mut medium = data.GetData(&format)?;
            let drop = HDROP(medium.u.hGlobal.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let mut v = Vec::with_capacity(count as usize);
            for i in 0..count {
                let len = DragQueryFileW(drop, i, None) as usize;
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; len + 1];
                let written = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                if written > 0 && written <= buf.len() {
                    v.push(PathBuf::from(String::from_utf16_lossy(&buf[..written])));
                }
            }
            ReleaseStgMedium(&mut medium);
            *self.paths.borrow_mut() = v;
        }
        Ok(())
    }
}

impl IContextMenu_Impl for ClassicMenu_Impl {
    fn QueryContextMenu(
        &self,
        menu: HMENU,
        position: u32,
        first_id: u32,
        last_id: u32,
        flags: u32,
    ) -> Result<()> {
        if flags & CMF_DEFAULTONLY != 0 {
            return Ok(());
        }
        let paths = self.paths.borrow();
        let actions = applicable_actions(&paths);
        if actions.is_empty() {
            return Ok(());
        }
        if first_id.saturating_add(actions.len() as u32) > last_id {
            return Ok(());
        }
        unsafe {
            let submenu = CreatePopupMenu()?;
            for (i, a) in actions.iter().enumerate() {
                AppendMenuW(
                    submenu,
                    MF_STRING,
                    (first_id + i as u32) as usize,
                    a.title(),
                )?;
            }
            InsertMenuW(
                menu,
                position,
                MF_BYPOSITION | MF_POPUP,
                submenu.0 as usize,
                w!("Arca"),
            )?;
        }
        // Not a failure. QueryContextMenu returns how many items it added
        // inside the HRESULT itself, and in this crate the trait returns
        // Result<()>, so the count can only leave through the Err side.
        Err(Error::from(HRESULT(actions.len() as i32)))
    }

    fn InvokeCommand(&self, info: *const CMINVOKECOMMANDINFO) -> Result<()> {
        if info.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let verb = unsafe { (*info).lpVerb.0 } as usize;
        if verb >> 16 != 0 {
            return Err(E_INVALIDARG.into());
        }
        let paths = self.paths.borrow();
        let actions = applicable_actions(&paths);
        let Some(action) = actions.get(verb & 0xFFFF) else {
            return Err(E_INVALIDARG.into());
        };
        launch(*action, &paths)
    }

    fn GetCommandString(
        &self,
        _id: usize,
        _kind: u32,
        _reserved: *const u32,
        _name: PSTR,
        _max: u32,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

// One factory serves both class IDs: `true` for the classic menu, `false` for
// the modern one.
#[implement(IClassFactory)]
struct Factory(bool);

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Option<&IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> Result<()> {
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if object.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *object = std::ptr::null_mut();
            if self.0 {
                let classic: IUnknown = ClassicMenu::new().into();
                classic.query(iid, object).ok()
            } else {
                let root: IExplorerCommand = Root.into();
                root.query(iid, object).ok()
            }
        }
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

// Clippy wants a function that dereferences raw pointers to be `unsafe fn`. It
// cannot be: this is a COM entry point and its signature is fixed by the ABI
// Explorer calls it through. The three pointers are null-checked first.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    iid: *const GUID,
    object: *mut *mut c_void,
) -> HRESULT {
    if clsid.is_null() || iid.is_null() || object.is_null() {
        return E_POINTER;
    }
    unsafe {
        *object = std::ptr::null_mut();
        let classic = match *clsid {
            c if c == CLSID_ARCA => false,
            c if c == CLSID_ARCA_CLASSIC => true,
            _ => return CLASS_E_CLASSNOTAVAILABLE,
        };
        let factory: IClassFactory = Factory(classic).into();
        factory.query(iid, object)
    }
}

// Explorer keeps this DLL loaded for the life of the process on purpose.
// Saying it can be unloaded invites Explorer to drop it while a menu is still
// on screen, and the crash that follows is Explorer's, not ours.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
