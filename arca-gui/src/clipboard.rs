//! Files on the system clipboard, in both directions.
//!
//! Copying names as text was never what Ctrl+C means in a file list: the
//! Explorer pastes files, not a list of words. What it actually wants is
//! CF_HDROP, a list of paths on disk, so the entries have to exist as real
//! files before the clipboard can carry them. That is why every copy from
//! here extracts first, into a folder under the temporary directory, and puts
//! *those* paths on the clipboard.
//!
//! Alongside CF_HDROP goes "Preferred DropEffect", a private format the shell
//! reads to tell a cut from a copy. Without it a cut still pastes, but the
//! Explorer treats it as a copy and leaves the temporary file behind.
//!
//! Only Windows has this. The X11 and Wayland side of the same idea is
//! text/uri-list on a selection, which is a different mechanism with different
//! owners, and nothing here pretends otherwise: the other platforms get a
//! stub that says so, and the window turns the menu entries off.

use std::path::PathBuf;

/// Whether this build can put files on the clipboard at all. The menu asks so
/// it can leave the entries out rather than offer something that only fails.
pub const AVAILABLE: bool = cfg!(windows);

#[cfg(windows)]
pub fn set_files(paths: &[PathBuf], cut: bool) -> Result<(), String> {
    use clipboard_win::{formats, raw, Clipboard, Setter};

    // DROPEFFECT_COPY and DROPEFFECT_MOVE, out of the shell's own header.
    const COPY: u32 = 1;
    const MOVE: u32 = 2;

    let list: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    if list.is_empty() {
        return Err("nothing to copy".into());
    }
    // The clipboard is one global lock: another program may be holding it for
    // the moment it takes to read something. Retrying is what every clipboard
    // library does, and is cheaper than telling the user to press it again.
    let _clip = Clipboard::new_attempts(10).map_err(|e| e.to_string())?;
    raw::empty().map_err(|e| e.to_string())?;
    formats::FileList
        .write_clipboard(&list)
        .map_err(|e| e.to_string())?;
    // After the list, and without clearing: this is a second format on the
    // same clipboard contents, not a replacement for them.
    if let Some(fmt) = raw::register_format("Preferred DropEffect") {
        let effect = if cut { MOVE } else { COPY };
        raw::set_without_clear(fmt.get(), &effect.to_le_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The files the clipboard is carrying right now, if it is carrying any.
#[cfg(windows)]
pub fn files() -> Vec<PathBuf> {
    use clipboard_win::{formats, Clipboard, Getter};

    let Ok(_clip) = Clipboard::new_attempts(10) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    if formats::FileList.read_clipboard(&mut out).is_err() {
        return Vec::new();
    }
    out.into_iter().map(PathBuf::from).collect()
}

#[cfg(not(windows))]
pub fn set_files(_paths: &[PathBuf], _cut: bool) -> Result<(), String> {
    Err("putting files on the clipboard is Windows only for now".into())
}

#[cfg(not(windows))]
pub fn files() -> Vec<PathBuf> {
    Vec::new()
}
