//! Runtime state owned by the application controller.

use super::{Cut, Message, Pending, Release, View, Viewed};
use crate::archive_ops::Answer;
use crate::model::{Format, SortColumn};
use crate::settings::Settings;
use crate::tree;
use arca_core::{Codec, Entry, Level};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

pub(crate) struct AppState {
    pub(crate) view: View,
    pub(crate) settings: Settings,
    pub(crate) archive: Option<PathBuf>,
    pub(crate) entries: Vec<Entry>,
    pub(crate) checked: Vec<bool>,
    pub(crate) filter: String,
    pub(crate) order: (SortColumn, bool),
    pub(crate) channel: Option<Receiver<Message>>,
    pub(crate) notice: String,
    pub(crate) error: bool,
    pub(crate) busy: bool,
    pub(crate) done_count: usize,
    pub(crate) total_count: usize,
    pub(crate) current_file: String,
    pub(crate) started: Option<Instant>,
    pub(crate) format: Format,
    pub(crate) codec: Codec,
    pub(crate) level: Level,
    pub(crate) into_subfolder: bool,
    pub(crate) pending_inputs: Vec<PathBuf>,
    pub(crate) output_name: String,
    pub(crate) close_when_done: bool,
    pub(crate) title: String,
    pub(crate) window_title: String,
    pub(crate) current_dir: String,
    pub(crate) show_settings: bool,
    pub(crate) conflict: Option<String>,
    pub(crate) replies: Option<Sender<Answer>>,
    // A job held back until the password window has an answer. Extraction asks
    // once, before it starts, rather than per entry: every entry in a .zip is
    // encrypted with the same password, and asking again per file is noise.
    pub(crate) waiting_on_password: Option<Pending>,
    pub(crate) password_input: String,
    pub(crate) show_password: bool,
    pub(crate) add_password: String,
    // Held for the archive currently open in the window, so extracting from it
    // does not ask again for every button press.
    pub(crate) archive_password: Option<String>,
    // Where to look again once a password job has rewritten the archive, and the
    // password it now carries.
    pub(crate) after_password: Option<(PathBuf, Option<String>)>,
    // Where the window has been, so the mouse back and forward buttons have
    // somewhere to go. `here` indexes into it; going somewhere new throws away
    // whatever was ahead, the way a browser does.
    pub(crate) history: Vec<String>,
    pub(crate) here: usize,
    // The row the keyboard is on. Everything the arrows, Enter and Space do
    // hangs off this, and there was no such thing before: the table had
    // checkboxes but no cursor. None means nothing is focused yet.
    pub(crate) cursor: Option<usize>,
    // The entry being renamed and what has been typed into it so far. Held by
    // path rather than by row number so that sorting or filtering underneath a
    // half typed name cannot move the box onto somebody else's row.
    pub(crate) renaming: Option<(String, String)>,
    // One password to try before asking, for a folder of archives all locked
    // with the same word. Never written anywhere: see `default_password_window`.
    pub(crate) default_password: Option<String>,
    pub(crate) asking_default_password: bool,
    // Set while the box that asks for a new folder's name is up.
    pub(crate) asking_folder: bool,
    // The archive that has a previous version kept beside it, and the word for
    // what was done to it. One step back, which is the one anybody wants:
    // deeper than that and the sidecars would pile up.
    pub(crate) undo: Option<(PathBuf, &'static str)>,
    // The newer Arca, once the announcement has answered. Its own channel
    // rather than a job, because something nobody asked for must not make the
    // window look busy.
    pub(crate) update: Option<Release>,
    pub(crate) update_rx: Option<std::sync::mpsc::Receiver<Release>>,
    pub(crate) asked_about_updates: bool,
    // Whether the two counts are bytes rather than entries. Only the download
    // measures itself that way.
    pub(crate) in_bytes: bool,
    // What the job is being done to, beside the verb in the title.
    pub(crate) subject: String,
    // Whether the job is a panel over the list it was started from, rather than
    // the whole window. A job that came from the Explorer has no list behind it
    // to go back to.
    pub(crate) overlay: bool,
    // Told to give up, and told to hold. Shared with the thread doing the work,
    // which reads both at the end of every entry -- the one moment it is not in
    // the middle of something.
    pub(crate) stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(crate) hold: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // The folders of the archive, rebuilt when a listing arrives rather than
    // every frame: it is fifteen hundred paths split on every slash and the
    // answer only changes when the archive does.
    pub(crate) folders: tree::Folder,
    // The file being looked at without taking it out of the archive.
    pub(crate) viewing: Option<Viewed>,
    // Set while the box that picks a group by name is up: true to add what
    // matches to the selection, false to take it away.
    pub(crate) picking_group: Option<bool>,
    // Set while the wheel is being used to walk the list up and down.
    // The last row a left click landed on, and when. What tells a second click
    // on the same row from the first one of a new pair.
    pub(crate) last_click: Option<(usize, f64)>,
    // Names waiting on a yes before they are taken out of the archive. There
    // is no undo, so this one asks.
    pub(crate) confirm_delete: Option<Vec<String>>,
    // An archive dropped onto an open archive, which is two reasonable things
    // at once and so gets asked about rather than guessed at.
    pub(crate) confirm_drop: Option<Vec<PathBuf>>,
    // The folder the last Ctrl+C or Ctrl+X extracted into. The clipboard is
    // holding paths inside it, so it stays until the next copy replaces it and
    // makes those paths meaningless anyway.
    pub(crate) clip_dir: Option<PathBuf>,
    // What the last Ctrl+X put on the clipboard, so those rows can show it.
    pub(crate) cut_names: HashSet<String>,
    // Made ready before the extraction runs and armed only when it says the
    // clipboard took it, which is what `Message::CutReady` reports.
    pub(crate) cut_armed: Option<Cut>,
    pub(crate) cut_pending: Option<Cut>,
    pub(crate) show_shortcuts: bool,
    // Set for work that says nothing while it runs. Copying to the clipboard is
    // the only such job: it is over before a bar has finished appearing, and a
    // bar that flashes past says less than nothing.
    pub(crate) quiet: bool,
}
