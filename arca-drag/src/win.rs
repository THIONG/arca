//! The Windows half: an `IDataObject` that extracts on demand, an
//! `IDropSource` that says when the drag is over, and the call that starts it.

use std::cell::RefCell;
use std::path::PathBuf;

use windows::core::{implement, Result, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    BOOL,
    DATA_S_SAMEFORMATETC, DV_E_FORMATETC, DV_E_TYMED, E_NOTIMPL, HGLOBAL, OLE_E_ADVISENOTSUPPORTED,
    POINT, S_OK,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, DATADIR,
    DATADIR_GET, FORMATETC, STGMEDIUM, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::Common::STRRET;
use windows::Win32::UI::Shell::{
    SHCreateStdEnumFmtEtc, SHCreateStreamOnFileEx, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW,
    FD_ATTRIBUTES, FD_FILESIZE, FD_WRITESTIME,
};

/// What one entry looks like to the shell before anything has been extracted.
pub struct Item {
    /// Where it lands relative to the drop, backslashes and all. The shell
    /// makes any folders the path names.
    pub name: String,
    pub size: u64,
    /// Seconds since the Unix epoch, when the archive recorded one.
    pub mtime: Option<i64>,
}

/// What the drop turned out to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    None,
    Copy,
    Move,
}

fn format(name: &str) -> u16 {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // Registering a name that is already registered returns the same number,
    // which is how every program agrees on what these mean.
    unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) as u16 }
}

struct Formats {
    descriptor: u16,
    contents: u16,
    preferred: u16,
    performed: u16,
}

impl Formats {
    fn get() -> Self {
        Formats {
            descriptor: format("FileGroupDescriptorW"),
            contents: format("FileContents"),
            preferred: format("Preferred DropEffect"),
            performed: format("Performed DropEffect"),
        }
    }
}

/// Copies `bytes` into a moveable global block, which is how everything travels
/// through the clipboard and through a drag.
unsafe fn to_global(bytes: &[u8]) -> Result<HGLOBAL> {
    let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
    let at = GlobalLock(handle);
    if at.is_null() {
        return Err(windows::core::Error::from_win32());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), at.cast::<u8>(), bytes.len());
    let _ = GlobalUnlock(handle);
    Ok(handle)
}

/// Unix seconds as a Windows FILETIME: hundreds of nanoseconds since 1601.
fn to_filetime(unix: i64) -> u64 {
    const EPOCH_DIFFERENCE: i64 = 11_644_473_600;
    ((unix + EPOCH_DIFFERENCE).max(0) as u64).saturating_mul(10_000_000)
}

#[implement(IDataObject)]
struct Source {
    items: Vec<Item>,
    deliver: Box<dyn Fn(usize) -> Option<PathBuf>>,
    formats: Formats,
    /// Set by the shell through `SetData` when a move has gone through, which
    /// is the only way it ever says so.
    performed: RefCell<DROPEFFECT>,
}

impl Source {
    /// The block that says what is being dragged: a count, then one fixed-size
    /// record per item, in one allocation.
    unsafe fn descriptor(&self) -> Result<HGLOBAL> {
        let count = self.items.len();
        let head = std::mem::size_of::<u32>();
        let each = std::mem::size_of::<FILEDESCRIPTORW>();
        let mut bytes = vec![0u8; head + each * count];
        std::ptr::write_unaligned(bytes.as_mut_ptr().cast::<u32>(), count as u32);

        for (i, item) in self.items.iter().enumerate() {
            let mut fd = FILEDESCRIPTORW {
                dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0) as u32,
                dwFileAttributes: FILE_ATTRIBUTE_NORMAL.0,
                nFileSizeHigh: (item.size >> 32) as u32,
                nFileSizeLow: (item.size & 0xFFFF_FFFF) as u32,
                ..Default::default()
            };
            if let Some(t) = item.mtime.filter(|t| *t > 0) {
                let ticks = to_filetime(t);
                fd.dwFlags |= FD_WRITESTIME.0 as u32;
                fd.ftLastWriteTime.dwHighDateTime = (ticks >> 32) as u32;
                fd.ftLastWriteTime.dwLowDateTime = (ticks & 0xFFFF_FFFF) as u32;
            }
            // cFileName is a fixed 260 wide characters with a terminator. A
            // name longer than that cannot be described here at all, so it is
            // cut rather than left to run over the end of the record.
            // Filled to one side and assigned whole: the record is packed, so
            // taking a reference to the field inside it would be unaligned.
            let mut name = [0u16; 260];
            let wide: Vec<u16> = item.name.encode_utf16().take(259).collect();
            name[..wide.len()].copy_from_slice(&wide);
            fd.cFileName = name;
            std::ptr::write_unaligned(
                bytes.as_mut_ptr().add(head + each * i).cast::<FILEDESCRIPTORW>(),
                fd,
            );
        }
        to_global(&bytes)
    }
}

impl IDataObject_Impl for Source_Impl {
    fn GetData(&self, request: *const FORMATETC) -> Result<STGMEDIUM> {
        let request = unsafe { *request };
        let wanted = request.cfFormat;

        if wanted == self.formats.descriptor && request.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
            let block = unsafe { self.descriptor()? };
            return Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: windows::Win32::System::Com::STGMEDIUM_0 { hGlobal: block },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            });
        }

        if wanted == self.formats.contents && request.tymed & TYMED_ISTREAM.0 as u32 != 0 {
            // lindex says which one, and this is the moment it gets extracted.
            let which = request.lindex;
            if which < 0 || which as usize >= self.items.len() {
                return Err(DV_E_FORMATETC.into());
            }
            let Some(path) = (self.deliver)(which as usize) else {
                return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
            };
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let stream = unsafe {
                SHCreateStreamOnFileEx(
                    PCWSTR(wide.as_ptr()),
                    windows::Win32::System::Com::STGM_READ.0,
                    FILE_ATTRIBUTE_NORMAL.0,
                    false,
                    None,
                )?
            };
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: windows::Win32::System::Com::STGMEDIUM_0 {
                    pstm: std::mem::ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            });
        }

        if wanted == self.formats.preferred && request.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
            let effect: u32 = DROPEFFECT_COPY.0;
            let block = unsafe { to_global(&effect.to_le_bytes())? };
            return Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: windows::Win32::System::Com::STGMEDIUM_0 { hGlobal: block },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            });
        }

        Err(DV_E_FORMATETC.into())
    }

    fn GetDataHere(&self, _f: *const FORMATETC, _m: *mut STGMEDIUM) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, request: *const FORMATETC) -> HRESULT {
        let request = unsafe { *request };
        let known = request.cfFormat == self.formats.descriptor
            || request.cfFormat == self.formats.contents
            || request.cfFormat == self.formats.preferred;
        if !known {
            return DV_E_FORMATETC;
        }
        let tymed = if request.cfFormat == self.formats.contents {
            TYMED_ISTREAM.0 as u32
        } else {
            TYMED_HGLOBAL.0 as u32
        };
        if request.tymed & tymed == 0 {
            return DV_E_TYMED;
        }
        S_OK
    }

    fn GetCanonicalFormatEtc(&self, _i: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        unsafe { (*out).ptd = std::ptr::null_mut() };
        DATA_S_SAMEFORMATETC
    }

    /// The one that matters here: after a move, the shell reports back through
    /// this what it actually did, and that is the only notice a source ever
    /// gets that its files have been taken rather than copied.
    fn SetData(&self, request: *const FORMATETC, medium: *const STGMEDIUM, _release: BOOL) -> Result<()> {
        let request = unsafe { *request };
        if request.cfFormat == self.formats.performed
            || request.cfFormat == self.formats.preferred
        {
            unsafe {
                let handle = (*medium).u.hGlobal;
                let at = GlobalLock(handle);
                if !at.is_null() {
                    let value = std::ptr::read_unaligned(at.cast::<u32>());
                    *self.performed.borrow_mut() = DROPEFFECT(value);
                    let _ = GlobalUnlock(handle);
                }
            }
            return Ok(());
        }
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        if DATADIR(direction as i32) != DATADIR_GET {
            return Err(E_NOTIMPL.into());
        }
        let offered = [
            FORMATETC {
                cfFormat: self.formats.descriptor,
                ptd: std::ptr::null_mut(),
                dwAspect: 1,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            },
            FORMATETC {
                cfFormat: self.formats.contents,
                ptd: std::ptr::null_mut(),
                dwAspect: 1,
                lindex: -1,
                tymed: TYMED_ISTREAM.0 as u32,
            },
            FORMATETC {
                cfFormat: self.formats.preferred,
                ptd: std::ptr::null_mut(),
                dwAspect: 1,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            },
        ];
        // The shell will build the enumerator; there is nothing to be gained
        // from writing a third COM object to walk an array of three.
        unsafe { SHCreateStdEnumFmtEtc(&offered) }
    }

    fn DAdvise(&self, _f: *const FORMATETC, _a: u32, _s: Option<&IAdviseSink>) -> Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _c: u32) -> Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[implement(IDropSource)]
struct Hand;

impl IDropSource_Impl for Hand_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        const LEFT: u32 = 0x0001;
        const RIGHT: u32 = 0x0002;
        if escape.as_bool() {
            return windows::Win32::Foundation::DRAGDROP_S_CANCEL;
        }
        // Letting go of every button is the drop. This is the whole of what a
        // drop source has to decide.
        if keys.0 & (LEFT | RIGHT) == 0 {
            return windows::Win32::Foundation::DRAGDROP_S_DROP;
        }
        S_OK
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        // The cursors Windows already has say all of this better than a
        // hand-drawn set would.
        windows::Win32::Foundation::DRAGDROP_S_USEDEFAULTCURSORS
    }
}

use std::os::windows::ffi::OsStrExt;

/// Starts a drag and does not return until it has been dropped or abandoned.
///
/// `deliver` is asked for one item at a time, by index, and only once something
/// is being dropped. It runs on this thread, so it must not need the caller's
/// state back.
pub fn drag(
    items: Vec<Item>,
    deliver: Box<dyn Fn(usize) -> Option<PathBuf>>,
    allow_move: bool,
) -> Effect {
    if items.is_empty() {
        return Effect::None;
    }
    unsafe {
        // The window toolkit already does this to be a drop target, and asking
        // twice is harmless; without it on a thread that has not, DoDragDrop
        // fails outright.
        let _ = OleInitialize(None);

        let performed = RefCell::new(DROPEFFECT_NONE);
        let source: IDataObject = Source {
            items,
            deliver,
            formats: Formats::get(),
            performed,
        }
        .into();
        let hand: IDropSource = Hand.into();

        let allowed = if allow_move {
            DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0)
        } else {
            DROPEFFECT_COPY
        };
        let mut outcome = DROPEFFECT_NONE;
        let _ = DoDragDrop(&source, &hand, allowed, &mut outcome);

        match outcome {
            e if e.0 & DROPEFFECT_MOVE.0 != 0 => Effect::Move,
            e if e.0 & DROPEFFECT_COPY.0 != 0 => Effect::Copy,
            _ => Effect::None,
        }
    }
}

// Silences the unused warnings for the pieces the shell only needs to see.
const _: Option<POINT> = None;
const _: Option<STRRET> = None;
const _: Option<FILEGROUPDESCRIPTORW> = None;
const _: Option<FILE_FLAGS_AND_ATTRIBUTES> = None;
