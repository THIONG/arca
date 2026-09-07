//! Dragging entries out of an archive and dropping them somewhere else.
//!
//! Receiving a drag is something the window toolkit already does. Being the
//! *source* of one is not: it means calling `DoDragDrop` and handing the shell
//! a live COM object it can pull the bytes out of. That is the whole of this
//! crate, and the reason it is a crate: `arca-gui` forbids unsafe code, and
//! this is nothing but unsafe code.
//!
//! The interesting part is *when* the bytes are produced. A plain `CF_HDROP`
//! would mean the files already exist on disk, so a drag of six gigabytes
//! would have to extract all six before the pointer could move. What the shell
//! offers instead is `CFSTR_FILEDESCRIPTORW` plus `CFSTR_FILECONTENTS`: the
//! first says what is being dragged, and the second is asked for one item at a
//! time, during the drop. So nothing is extracted until something is actually
//! being dropped, and a drag that is abandoned costs nothing at all. It is what
//! WinRAR and 7-Zip do, and there is no cheaper way to do it properly.
//!
//! On anything that is not Windows this crate is empty.

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::{drag, Effect, Item};

/// What one entry looks like to the shell before anything has been extracted.
#[cfg(not(windows))]
pub struct Item {
    /// Where it lands relative to the drop, backslashes and all. The shell
    /// makes any folders the path names.
    pub name: String,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// What the drop turned out to be.
#[cfg(not(windows))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    None,
    Copy,
    Move,
}

/// Always [`Effect::None`] away from Windows: there is no drag to start.
#[cfg(not(windows))]
pub fn drag(
    _items: Vec<Item>,
    _deliver: Box<dyn Fn(usize) -> Option<std::path::PathBuf>>,
    _allow_move: bool,
) -> Effect {
    Effect::None
}
