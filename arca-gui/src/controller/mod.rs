//! Toolkit-independent application state and action controller.

mod actions;
mod state;
pub(crate) use actions::*;
pub(crate) use state::AppState;

use crate::archive_ops::*;
use crate::model::*;
use crate::settings::Settings;
use crate::tree::{children_of, entries_under, kind_of, parent_of, search_under, Row};
use crate::{
    clipboard,
    i18n::{strings, Strings},
    tree,
};
#[cfg(windows)]
use arca_core::Entry;
use arca_core::{Codec, Level};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::time::Instant;

pub(crate) struct AppController {
    pub(crate) state: AppState,
}

fn rename_destination(path: &str, name: &str) -> String {
    let path = path.trim_end_matches('/');
    match path.rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/{name}"),
        None => name.to_string(),
    }
}

impl AppController {
    pub(crate) fn dispatch(&mut self, action: AppAction) {
        match action {
            AppAction::Open(path) => self.open(path),
            AppAction::Run(job) => self.run_job(job),
            AppAction::ExtractTo { only_checked, dest } => {
                self.start_extract_to(only_checked, dest)
            }
            AppAction::PrepareCompress(paths) => self.prepare_compress(paths),
            AppAction::SetFilter(filter) => self.state.filter = filter,
            AppAction::SelectAllVisible => self.select_all_visible(),
            AppAction::InvertVisible => self.invert_visible(),
            AppAction::Add(paths) => self.add_files(paths),
            AppAction::Drop(paths) => self.dropped(paths),
            AppAction::Copy { cut } => self.copy_to_clipboard(cut),
            AppAction::Paste => self.paste_from_clipboard(),
            AppAction::OpenFile(index) => self.open_file(index),
            AppAction::Navigate(path) => self.go_to(path),
            AppAction::Back => self.go_back(),
            AppAction::Forward => self.go_forward(),
            AppAction::SetChecked { row, value } => self.set_checked(&row, value),
            AppAction::ClearSelection => self.clear_picked(),
            AppAction::Sort(column) => self.sort_by(column),
            AppAction::ToggleColumn(column) => {
                let on = self.state.settings.columns.on(column);
                self.state.settings.columns.set(column, !on);
                self.state.settings.save();
            }
            AppAction::SetLanguage(lang) => {
                self.state.settings.lang = lang;
                self.state.settings.save();
            }
            AppAction::SetTheme(theme) => {
                self.state.settings.theme = theme;
                self.state.settings.save();
            }
            AppAction::AnswerConflict(answer) => {
                if let Some(tx) = &self.state.replies {
                    let _ = tx.send(answer);
                }
                self.state.conflict = None;
            }
            AppAction::CancelPassword => self.cancel_password(),
            AppAction::SetPasswordInput(password) => self.state.password_input = password,
            AppAction::SubmitPassword(password) => self.submit_password(password),
            AppAction::BeginPasswordChange => self.begin_password_change(),
            AppAction::RequestDelete => self.request_delete(),
            AppAction::ConfirmDelete(confirmed) => self.confirm_delete(confirmed),
            AppAction::AnswerDrop(choice) => self.answer_drop(choice),
            // The worker reads this at the end of every entry. Let it go first,
            // or the news would sit unread until somebody pressed Resume.
            AppAction::CancelJob => {
                use std::sync::atomic::Ordering;
                self.state.stop.store(true, Ordering::Relaxed);
                self.state.hold.store(false, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn begin_password_change(&mut self) {
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        if self.state.format != Format::Zip || self.state.busy {
            return;
        }
        let job = Job::Password {
            archive,
            current: self.state.archive_password.clone(),
            new: None,
        };
        self.state.password_input.clear();
        // Two questions at most, and never none: the password the archive
        // already has, when it has one and the window does not know it, and
        // then the one it is about to get. Going straight to the job here is
        // what left the button only able to take a password off.
        let locked = self.state.entries.iter().any(|entry| entry.encrypted)
            && self.state.archive_password.is_none();
        self.state.waiting_on_password = Some(if locked {
            Pending::CurrentPassword(Box::new(job))
        } else {
            Pending::NewPassword(Box::new(job))
        });
    }

    pub(crate) fn sort_by(&mut self, column: SortColumn) {
        if self.state.order.0 == column {
            self.state.order.1 = !self.state.order.1;
        } else {
            self.state.order = (column, true);
        }
    }

    pub(crate) fn start_extract_to(&mut self, only_checked: bool, mut dest: PathBuf) {
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        if self.state.into_subfolder {
            dest = dest.join(archive_stem(&archive));
        }
        let wanted = if only_checked {
            self.state.checked.clone()
        } else {
            vec![true; self.state.entries.len()]
        };
        let s = self.s();
        let total = wanted.iter().filter(|b| **b).count();
        let pw = self.state.archive_password.clone();
        self.state.close_when_done = false;
        let (reply_tx, reply_rx) = channel::<Answer>();
        self.state.replies = Some(reply_tx);
        self.spawn(total, move |tx| {
            let notify = |i: usize, n: usize, name: &str| {
                let _ = tx.send(Message::Progress(i, n, name.to_string()));
                true
            };
            let ask = conflict_asker(tx, &reply_rx);
            let result = extract(&archive, &dest, &wanted, &notify, &ask, pw.as_deref());
            let _ = tx.send(match result {
                Ok(bytes) => Message::Done(fill(
                    s.extracted_to,
                    &[
                        ("size", &human(bytes)),
                        ("dest", &dest.display().to_string()),
                    ],
                )),
                Err(e) => Message::Failed(e.to_string()),
            });
        });
    }

    pub(crate) fn submit_password(&mut self, password: String) {
        // Empty is an answer only to "what should it be": nothing, meaning take
        // the password off. Everywhere else it is a press on an empty box.
        if password.is_empty()
            && !matches!(
                self.state.waiting_on_password,
                Some(Pending::NewPassword(_))
            )
        {
            return;
        }
        let Some(pending) = self.state.waiting_on_password.take() else {
            return;
        };
        self.state.password_input.clear();
        match pending {
            Pending::Extract(job) => {
                if let Job::Extract { archives, dest, .. } = *job {
                    self.run_job(Job::Extract {
                        archives,
                        dest,
                        password: Some(password),
                    });
                }
            }
            Pending::OpenArchive => self.state.archive_password = Some(password),
            Pending::CurrentPassword(job) => {
                if let Job::Password { archive, new, .. } = *job {
                    self.state.archive_password = Some(password.clone());
                    self.state.waiting_on_password =
                        Some(Pending::NewPassword(Box::new(Job::Password {
                            archive,
                            current: Some(password),
                            new,
                        })));
                }
            }
            Pending::NewPassword(job) => {
                if let Job::Password {
                    archive, current, ..
                } = *job
                {
                    self.run_job(Job::Password {
                        archive,
                        current,
                        new: (!password.is_empty()).then_some(password),
                    });
                }
            }
        }
    }

    /// Opens the add box, with whatever was just picked added to it.
    ///
    /// Adding rather than replacing is what lets a selection be built out of
    /// both files and folders: the native dialog only offers one or the other,
    /// so a mixed selection takes more than one pass through here.
    pub(crate) fn prepare_compress(&mut self, inputs: Vec<PathBuf>) {
        if !matches!(self.state.view, View::Add) {
            self.state.pending_inputs.clear();
            self.state.output_name.clear();
        }
        for path in inputs {
            if !self.state.pending_inputs.contains(&path) {
                self.state.pending_inputs.push(path);
            }
        }
        // Only while the box has no name in it, so a name typed by hand is not
        // overwritten by the next folder added.
        if self.state.output_name.is_empty() && !self.state.pending_inputs.is_empty() {
            self.state.output_name = quick_output(&self.state.pending_inputs, self.state.format)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
        }
        self.state.view = View::Add;
    }
    pub(crate) fn select_all_visible(&mut self) {
        for row in self.visible_rows() {
            self.set_checked(&row, true);
        }
    }
    pub(crate) fn invert_visible(&mut self) {
        let rows = self.visible_rows();
        let values: Vec<bool> = rows.iter().map(|r| !self.is_checked(r)).collect();
        for (r, v) in rows.iter().zip(values) {
            self.set_checked(r, v);
        }
    }
    pub(crate) fn request_delete(&mut self) {
        let names = self.selected_names();
        if !names.is_empty() {
            self.state.confirm_delete = Some(names);
        }
    }
    pub(crate) fn confirm_delete(&mut self, yes: bool) {
        if let Some(names) = self.state.confirm_delete.take() {
            if yes {
                if let Some(archive) = self.state.archive.clone() {
                    self.run_job(Job::Delete {
                        archive,
                        names,
                        password: self.state.archive_password.clone(),
                    });
                }
            }
        }
    }
    pub(crate) fn answer_drop(&mut self, choice: DropChoice) {
        if let Some(paths) = self.state.confirm_drop.take() {
            match choice {
                DropChoice::Open => {
                    if let Some(p) = paths.into_iter().next() {
                        self.open(p);
                    }
                }
                DropChoice::Add => self.add_files(paths),
                DropChoice::Cancel => {}
            }
        }
    }

    pub(crate) fn new(settings: Settings) -> Self {
        AppController {
            state: AppState {
                view: View::Browse,
                settings,
                archive: None,
                entries: Vec::new(),
                checked: Vec::new(),
                filter: String::new(),
                order: (SortColumn::Name, true),
                channel: None,
                notice: String::new(),
                error: false,
                busy: false,
                done_count: 0,
                total_count: 0,
                current_file: String::new(),
                started: None,
                format: Format::Zip,
                codec: Codec::Deflate,
                level: Level::Normal,
                into_subfolder: false,
                pending_inputs: Vec::new(),
                output_name: String::new(),
                close_when_done: false,
                one_shot: false,
                title: String::new(),
                window_title: "Arca".to_string(),
                current_dir: String::new(),
                show_settings: false,
                conflict: None,
                replies: None,
                waiting_on_password: None,
                password_input: String::new(),
                add_password: String::new(),
                archive_password: None,
                reread_after: None,
                reread_dir: None,
                history: vec![String::new()],
                here: 0,
                cursor: None,
                confirm_delete: None,
                clip_dir: None,
                cut_names: HashSet::new(),
                confirm_drop: None,
                renaming: None,
                asking_folder: false,
                undo: None,
                update: None,
                update_rx: None,
                asked_about_updates: false,
                in_bytes: false,
                subject: String::new(),
                overlay: false,
                stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                hold: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                viewing: None,
                picking_group: None,
                folders: tree::Folder::default(),
                last_click: None,
                cut_armed: None,
                cut_pending: None,
                show_shortcuts: false,
                quiet: false,
            },
        }
    }
    pub(crate) fn summary(&self) -> String {
        let s = self.s();
        let n = self.state.entries.iter().filter(|e| !e.is_dir).count();
        let raw: u64 = self.state.entries.iter().map(|e| e.size).sum();
        let packed: u64 = self.state.entries.iter().map(|e| e.compressed_size).sum();
        let ratio = if raw == 0 {
            0.0
        } else {
            (1.0 - packed as f64 / raw as f64) * 100.0
        };
        format!(
            "{n} {} · {} {} · {} {} · {ratio:.1}% {}",
            s.files_word,
            human(raw),
            s.uncompressed_word,
            human(packed),
            s.in_archive,
            s.saved_word
        )
    }
    pub(crate) fn go_back(&mut self) {
        if self.can_go_back() {
            self.state.here -= 1;
            self.state.current_dir = self.state.history[self.state.here].clone();
            self.state.filter.clear();
            self.clear_picked();
        }
    }
    pub(crate) fn selected_roots(&self) -> Vec<String> {
        let names: Vec<String> = self
            .state
            .entries
            .iter()
            .map(|e| e.name.replace('\\', "/"))
            .collect();
        // Whether everything under a prefix is ticked, worked out once per
        // prefix: the same ancestors come round again for every file in a
        // folder, and there can be thousands of them.
        let mut whole: HashMap<String, bool> = HashMap::new();
        let mut roots: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for (i, full) in names.iter().enumerate() {
            if !self.state.checked.get(i).copied().unwrap_or(false) {
                continue;
            }
            let trimmed = full.trim_end_matches('/');
            let mut root = trimmed.to_string();
            // Shortest ancestor first: the outermost folder that is ticked all
            // the way down is the one that was meant.
            let mut at = 0usize;
            while let Some(cut) = trimmed[at..].find('/') {
                at += cut + 1;
                let prefix = &trimmed[..at];
                let all = *whole.entry(prefix.to_string()).or_insert_with(|| {
                    names
                        .iter()
                        .enumerate()
                        .filter(|(_, n)| n.starts_with(prefix))
                        .all(|(j, _)| self.state.checked.get(j).copied().unwrap_or(false))
                });
                if all {
                    root = prefix.trim_end_matches('/').to_string();
                    break;
                }
            }
            if seen.insert(root.clone()) {
                roots.push(root);
            }
        }
        roots
    }

    // Ctrl+C and Ctrl+X. The clipboard carries paths, not archive entries, so
    // what is picked is extracted into a folder of its own under the temporary
    // directory first and those paths are what the shell is handed.
    //
    // A cut marks the rows and asks the shell to move rather than copy, which
    // is what empties the temporary folder afterwards. It does not take the
    // entries out of the archive: nothing tells this window whether the paste
    // ever happened, and removing them on the guess that it did would lose them
    // for good the moment somebody changed their mind.
    pub(crate) fn clear_picked(&mut self) {
        self.state.checked.iter_mut().for_each(|c| *c = false);
        self.state.cursor = None;
        // A row number means something else in the folder now on screen.
        self.state.last_click = None;
    }
    pub(crate) fn go_forward(&mut self) {
        if self.can_go_forward() {
            self.state.here += 1;
            self.state.current_dir = self.state.history[self.state.here].clone();
            self.state.filter.clear();
            self.clear_picked();
        }
    }
    pub(crate) fn s(&self) -> &'static Strings {
        strings(self.state.settings.effective_lang())
    }
    pub(crate) fn selected_names(&self) -> Vec<String> {
        self.state
            .entries
            .iter()
            .zip(&self.state.checked)
            .filter(|(_, &on)| on)
            .map(|(e, _)| e.name.clone())
            .collect()
    }

    // The top of what is ticked. A folder with every one of its entries ticked
    // stands for all of them, so a copy hands the clipboard one folder instead
    // of the fifteen hundred files inside it, and the Explorer pastes a folder
    // rather than a heap of loose files.
    pub(crate) fn cancel_password(&mut self) {
        let was_job = matches!(self.state.waiting_on_password, Some(Pending::Extract(_)));
        self.state.waiting_on_password = None;
        self.state.password_input.clear();
        // Only a job left the window on the running view with nothing running.
        if was_job {
            self.state.view = View::Browse;
        }
    }
    // Puts the archive back the way it was before the last change.
    //
    // A swap of two names, because the version before the change was moved
    // aside rather than thrown away. There is one step and no more: taking it
    // back leaves nothing to take back, and the sidecar goes with it.
    //
    // Lives on the controller rather than in a view because there is nothing
    // about it that belongs to a toolkit, and both surfaces offer it.
    pub(crate) fn undo_last(&mut self) {
        let Some((archive, _)) = self.state.undo.take() else {
            return;
        };
        let keep = undo_path(&archive);
        if !keep.exists() {
            return;
        }
        let pw = self.state.archive_password.clone();
        if let Err(e) = fs::remove_file(&archive).and_then(|_| fs::rename(&keep, &archive)) {
            self.state.notice = e.to_string();
            self.state.error = true;
            return;
        }
        self.open(archive);
        self.state.archive_password = pw;
    }

    // Reads the names in the archive again under another code page.
    //
    // Only the person looking at it can know which one an unflagged zip was
    // written in, so it is a choice and not a guess, and the choice is
    // remembered.
    pub(crate) fn reread_names(&mut self, page: arca_zip::pages::Page) {
        self.state.settings.page = page;
        self.state.settings.save();
        for e in &mut self.state.entries {
            if e.utf8 {
                continue;
            }
            e.name = arca_zip::pages::decode(&e.raw_name, page);
            e.is_dir = e.name.ends_with('/') || e.name.ends_with('\\');
        }
        self.state.folders = tree::folders_of(&self.state.entries);
        self.clear_picked();
        self.state.cursor = None;
        self.state.current_dir.clear();
        self.state.history = vec![String::new()];
        self.state.here = 0;
        self.state.notice = self.summary();
        self.state.error = false;
    }

    /// A fresh pair of flags for a job about to start, handed back so the
    /// worker and the window end up holding the same two.
    ///
    /// Fresh rather than lowered: a thread that was told to stop may still be
    /// on its way out, and it must not read the flag the next job is watching.
    pub(crate) fn fresh_flags(
        &mut self,
    ) -> (
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.state.stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.state.hold = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        (self.state.stop.clone(), self.state.hold.clone())
    }

    /// Where a running job is shown: over the list it was started from, or as
    /// the whole window when it came from the Explorer and there is no list
    /// behind it to go back to.
    pub(crate) fn show_job(&mut self, verb: &str, subject: String, from_here: bool) {
        self.state.title = verb.to_string();
        self.state.subject = subject;
        self.state.overlay = matches!(self.state.view, View::Browse) && from_here;
        if !self.state.overlay {
            self.state.view = View::Running;
            self.state.window_title = if self.state.subject.is_empty() {
                "Arca".to_string()
            } else {
                format!("{} - {}", self.state.title, self.state.subject)
            };
        }
    }

    // Asks, once, whether there is a newer Arca.
    //
    // On a thread and without a word: something nobody asked for must not make
    // the window look busy. If the answer never comes -- no network, no reply,
    // a machine that says no -- nothing happens and nothing is said. There is
    // nothing here worth a complaint.
    pub(crate) fn ask_about_updates(&mut self) {
        if self.state.asked_about_updates || !self.state.settings.updates {
            return;
        }
        self.state.asked_about_updates = true;
        let (tx, rx) = channel::<Release>();
        self.state.update_rx = Some(rx);
        let running = env!("CARGO_PKG_VERSION").to_string();
        std::thread::spawn(move || {
            // GitHub turns away anything that does not name itself, and naming
            // the program and its version is what a user agent is for. Nothing
            // else is sent: no machine, no user, no archive.
            let agent = format!("Arca/{running}");
            let Some(reply) = arca_net::get(RELEASES_API, &agent) else {
                return;
            };
            if let Some(release) = release_of(&reply) {
                if newer(&running, &release.tag) {
                    let _ = tx.send(release);
                }
            }
        });
    }

    // Takes the new version, if this copy is one that can replace itself.
    //
    // A copy put here by the installer updates itself. One unpacked from the
    // .zip is loose files in a folder somebody chose, and there is nothing to
    // update: for that one the only honest offer is the page.
    pub(crate) fn start_update(&mut self) {
        match self.state.update.clone() {
            Some(Release {
                tag,
                installer: Some(installer),
                sums: Some(sums),
            }) if installed_by_setup() => self.run_job(Job::Update {
                tag,
                installer,
                sums,
            }),
            _ => {
                let _ = launch_with_system(Path::new(RELEASES_PAGE));
            }
        }
    }

    // Moves what was being carried into `target`, which is a folder's path or
    // the empty string for the root.
    //
    // One job for all of it. A move is a rename with a different folder in
    // front of it, and a rename is a rewrite of the whole archive: doing them
    // one at a time would rewrite it once per file.
    pub(crate) fn move_into(&mut self, roots: &[String], target: &str) {
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        let s = self.s();
        let mut moves: Vec<(String, String)> = Vec::new();
        for root in roots {
            let from = root.trim_end_matches('/').to_string();
            let leaf = from.rsplit('/').next().unwrap_or(&from).to_string();
            let to = format!("{target}{leaf}");
            // Already there, or into itself: nothing to do rather than a
            // rewrite that changes nothing.
            if from == to || to.starts_with(&format!("{from}/")) {
                continue;
            }
            moves.push((from, to));
        }
        if moves.is_empty() {
            return;
        }
        // Nothing in the destination may already answer to the name. The
        // archive would take it -- a zip can hold the same name twice -- and
        // what came out afterwards would be anybody's guess.
        let taken: HashSet<&str> = self
            .state
            .entries
            .iter()
            .map(|e| e.name.trim_end_matches('/'))
            .collect();
        if let Some((_, to)) = moves.iter().find(|(_, to)| taken.contains(to.as_str())) {
            let leaf = to.rsplit('/').next().unwrap_or(to);
            self.state.notice = fill(s.name_taken, &[("name", leaf)]);
            self.state.error = true;
            return;
        }
        self.run_job(Job::Move {
            archive,
            moves,
            password: self.state.archive_password.clone(),
        });
    }

    pub(crate) fn extract_here(&mut self) {
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        self.run_job(Job::Extract {
            archives: vec![archive],
            dest: if self.state.into_subfolder {
                Destination::Subfolder
            } else {
                Destination::Beside
            },
            password: self.state.archive_password.clone(),
        });
    }
    /// Everything under a folder picked, and nothing else.
    ///
    /// The tree points at folders the list may not be showing, so the pick is
    /// made from the path and not from a row on screen.
    pub(crate) fn pick_folder(&mut self, path: &str) {
        self.clear_picked();
        for i in entries_under(&self.state.entries, path) {
            self.state.checked[i] = true;
        }
    }

    pub(crate) fn set_checked(&mut self, row: &Row, value: bool) {
        // Nothing behind it, and its path is the folder above: ticking it would
        // pick everything in the archive up to and including where you came
        // from. Select all has to leave it alone.
        if row.up {
            return;
        }
        match row.entry {
            Some(i) => self.state.checked[i] = value,
            None => {
                for i in entries_under(&self.state.entries, &row.path) {
                    self.state.checked[i] = value;
                }
            }
        }
    }

    // Double clicking a file pulls that one entry out to a temporary folder and
    // hands it to whatever the system opens it with. It runs on its own thread
    // because the entry can be large, and reports through the same progress
    // window as everything else.
    #[cfg(windows)]
    pub(crate) fn dragged_files(&self) -> Vec<(Entry, String)> {
        let base = &self.state.current_dir;
        self.state
            .entries
            .iter()
            .zip(&self.state.checked)
            .filter(|(e, &on)| on && !e.is_dir)
            .map(|(e, _)| {
                let full = e.name.replace('\\', "/");
                let rel = full.strip_prefix(base.as_str()).unwrap_or(&full);
                (e.clone(), rel.replace('/', "\\"))
            })
            .filter(|(_, rel)| !rel.is_empty())
            .collect()
    }

    // Dragging the selection out of the window. Blocks until it has been
    // dropped or abandoned, because that is what `DoDragDrop` does: the window
    // stops repainting for as long as the drag lasts, which nobody sees because
    // the pointer is somewhere else by then.
    //
    // Nothing is extracted here. The shell is handed a list of names and sizes
    // and asks for one file at a time while it is dropping, so a drag that is
    // thought better of costs nothing, and a drag of six gigabytes starts as
    // fast as a drag of one file.
    pub(crate) fn go_to(&mut self, path: String) {
        if self.state.history.get(self.state.here) == Some(&path) {
            return;
        }
        self.state.history.truncate(self.state.here + 1);
        self.state.history.push(path.clone());
        self.state.here = self.state.history.len() - 1;
        self.state.current_dir = path;
        self.state.filter.clear();
        self.clear_picked();
    }

    // Every folder starts with nothing picked, the way the Explorer does.
    // A folder is picked here by ticking every entry underneath it, which is
    // what lets one be extracted whole, so clicking a folder and walking into
    // it used to arrive with all of its contents already ticked.
    pub(crate) fn remember(&mut self, path: &Path) {
        let text = path.to_string_lossy().to_string();
        self.state.settings.recent.retain(|p| *p != text);
        self.state.settings.recent.insert(0, text);
        self.state.settings.recent.truncate(10);
        self.state.settings.save();
    }
    pub(crate) fn spawn<F>(&mut self, total: usize, work: F)
    where
        F: FnOnce(&Sender<Message>) + Send + 'static,
    {
        let (tx, rx) = channel();
        self.state.channel = Some(rx);
        self.state.busy = true;
        self.state.quiet = false;
        self.state.error = false;
        self.state.done_count = 0;
        self.state.total_count = total;
        self.state.current_file.clear();
        self.state.started = Some(Instant::now());
        std::thread::spawn(move || {
            work(&tx);
        });
    }

    // The folders of the archive down the side, the way WinRAR and the Explorer
    // both offer one.
    //
    // It earns its place in a deep archive, where walking to a folder six
    // levels down and back is a dozen double clicks. Off by default: in a flat
    // archive it would be an empty column taking a fifth of the window.
    pub(crate) fn can_go_forward(&self) -> bool {
        self.state.here + 1 < self.state.history.len()
    }
    pub(crate) fn codec_name(&self, c: Codec) -> &'static str {
        let s = self.s();
        match c {
            Codec::Store => s.codec_store,
            Codec::Deflate => s.codec_deflate,
            Codec::Zstd => s.codec_zstd,
        }
    }
    // Dragging the selection out of the window. Blocks until it has been
    // dropped or abandoned, because that is what `DoDragDrop` does: the window
    // stops repainting for as long as the drag lasts, which nobody sees because
    // the pointer is somewhere else by then.
    //
    // Nothing is extracted here. The shell is handed a list of names and sizes
    // and asks for one file at a time while it is dropping, so a drag that is
    // thought better of costs nothing, and a drag of six gigabytes starts as
    // fast as a drag of one file.
    #[cfg(windows)]
    pub(crate) fn drag_out(&mut self) {
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        let picked = self.dragged_files();
        if picked.is_empty() {
            return;
        }
        let items: Vec<arca_drag::Item> = picked
            .iter()
            .map(|(e, name)| arca_drag::Item {
                name: name.clone(),
                size: e.size,
                mtime: e.mtime,
            })
            .collect();
        let password = self.state.archive_password.clone();
        let entries: Vec<Entry> = picked.into_iter().map(|(e, _)| e).collect();
        let deliver = Box::new(move |i: usize| {
            entries
                .get(i)
                .and_then(|e| extract_one(&archive, e, password.as_deref()).ok())
        });
        // Copy only. Moving would mean taking the entries out of the archive,
        // and the one gesture that does that already asks first.
        let _ = arca_drag::drag(items, deliver, false);
    }

    #[cfg(not(windows))]
    pub(crate) fn drag_out(&mut self) {}

    // Escape and the Cancel button are the same act, so they go through the
    // same code: two copies of this would drift apart the first time one side
    // grew a step.
    // The names ticked right now, which is what every action that works on a
    // selection needs.
    pub(crate) fn cut_landed(&mut self) {
        let Some(cut) = &self.state.cut_pending else {
            return;
        };
        if self.state.busy || self.state.archive.as_ref() != Some(&cut.archive) {
            return;
        }
        if cut.paths.iter().any(|p| p.exists()) {
            return;
        }
        let Some(cut) = self.state.cut_pending.take() else {
            return;
        };
        self.state.cut_names.clear();
        self.run_job(Job::Delete {
            archive: cut.archive,
            names: cut.names,
            password: self.state.archive_password.clone(),
        });
    }

    // What is picked, named the way it should land where it is dropped: the
    // folder on screen is the base, so dragging a folder out puts that folder
    // down rather than scattering what was inside it.
    pub(crate) fn level_name(&self, l: Level) -> &'static str {
        let s = self.s();
        match l {
            Level::Store => s.level_none,
            Level::Fast => s.level_fast,
            Level::Normal => s.level_normal,
            Level::Best => s.level_best,
        }
    }
    pub(crate) fn can_go_back(&self) -> bool {
        self.state.here > 0
    }
    pub(crate) fn add_files(&mut self, inputs: Vec<PathBuf>) {
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        self.run_job(Job::Add {
            archive,
            inputs,
            dir: self.state.current_dir.clone(),
            codec: self.state.codec,
            level: self.state.level,
            password: self.state.archive_password.clone(),
        });
    }

    // What a drop means depends on what the window is already showing. With
    // nothing open there is only one thing it can be, and that is what it has
    // always done: open it. With an archive open, dropping a file on it means
    // putting the file inside, which is what every other archiver does and what
    // opening a second archive over the first never was.
    //
    // The exception is dropping an archive onto an archive, which is honestly
    // both, so it asks instead of picking one and being wrong half the time.
    pub(crate) fn copy_to_clipboard(&mut self, cut: bool) {
        let s: &'static Strings = self.s();
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        let roots = self.selected_roots();
        if roots.is_empty() {
            return;
        }
        // A folder of its own per copy, so the paths already on the clipboard
        // never end up pointing at something a later copy overwrote.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join("Arca")
            .join(format!("clip-{stamp:x}"));
        let previous = self.state.clip_dir.replace(dir.clone());
        let names = self.selected_names();
        // Before the old folder is thrown away further down: an earlier cut
        // still waiting was watching for those files to disappear, and this is
        // about to delete them itself.
        self.state.cut_armed = None;
        self.state.cut_pending = None;
        self.state.cut_names = if cut {
            names.iter().cloned().collect()
        } else {
            HashSet::new()
        };
        // Where each picked thing will land once extracted. Worked out here
        // rather than in the thread because it is also what a pending cut has
        // to watch, and only a .zip can have entries taken out of it in place.
        let landed: Vec<PathBuf> = roots
            .iter()
            .filter_map(|r| arca_core::safe_name(r).ok())
            .map(|r| dir.join(r))
            .collect();
        if cut && detect(&archive) == Some(Format::Zip) {
            self.state.cut_armed = Some(Cut {
                archive: archive.clone(),
                paths: landed.clone(),
                names,
            });
        }

        let wanted = self.state.checked.clone();
        let total = wanted.iter().filter(|b| **b).count();
        let pw = self.state.archive_password.clone();
        self.state.close_when_done = false;
        self.spawn(total, move |tx| {
            let notify = |i: usize, n: usize, name: &str| {
                let _ = tx.send(Message::Progress(i, n, name.to_string()));
                true
            };
            // A folder nobody has seen yet has nothing in it to overwrite, so
            // there is no question to put on screen.
            let ask = |_: &Path| Answer::Replace;
            let outcome = extract(&archive, &dir, &wanted, &notify, &ask, pw.as_deref())
                .map_err(|e| e.to_string())
                .and_then(|_| clipboard::set_files(&landed, cut).map(|()| landed.len()));
            // Only once the new list is on the clipboard: until that moment the
            // old paths are still what a paste would reach for.
            if let Some(old) = previous {
                let _ = fs::remove_dir_all(old);
            }
            let _ = tx.send(match outcome {
                // Nothing to say. Copying somewhere else does not announce
                // itself either, and the rows a cut is holding are already
                // faded; what is worth a line is the entries leaving the
                // archive, and that has its own. An empty message hands the
                // status bar back to the summary of what is open.
                Ok(_) => {
                    if cut {
                        let _ = tx.send(Message::CutReady);
                    }
                    Message::Done(String::new())
                }
                Err(why) => Message::Failed(fill(s.clipboard_failed, &[("why", &why)])),
            });
        });
        // After `spawn`, which clears it: this is the one job that runs without
        // saying so.
        self.state.quiet = true;
    }

    // Whether the cut waiting on a paste has had it.
    //
    // Windows never says. What it does instead, when the clipboard asked for a
    // move rather than a copy, is take the files out of the folder they were
    // handed over in, so their absence is the whole of the evidence. It is
    // checked when the window gets the keyboard back, because pasting somewhere
    // else means having gone somewhere else first.
    //
    // Only all of them counts. A cut that is half gone is more likely to be a
    // paste still running than one that finished, and leaving the entries where
    // they are costs nothing: they will still be there next time. Every way
    // this can be wrong leaves the archive untouched, which is the side to be
    // wrong on when there is no undo.
    pub(crate) fn open_file(&mut self, index: usize) {
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        let Some(entry) = self.state.entries.get(index).cloned() else {
            return;
        };
        if entry.is_dir {
            return;
        }
        let password = self.state.archive_password.clone();
        let s = self.s();
        self.state.close_when_done = false;
        self.state.title = s.opening.to_string();
        self.state.view = View::Running;
        self.spawn(1, move |tx| {
            let _ = tx.send(Message::Progress(0, 1, entry.name.clone()));
            let outcome = extract_one(&archive, &entry, password.as_deref())
                .and_then(|path| launch_with_system(&path).map(|()| path));
            let _ = tx.send(match outcome {
                Ok(path) => Message::Done(fill(
                    s.opened_with_system,
                    &[("name", &path.display().to_string())],
                )),
                Err(e) => Message::Failed(e.to_string()),
            });
        });
    }

    // Everything the keyboard does to the list, in one place. `rows` is what is
    // on screen right now, which is what the arrows should walk: filtering or
    // changing folder changes the list under the cursor, so it is clamped here
    // rather than tracked separately.
    pub(crate) fn receive(&mut self) -> bool {
        // The answer about a newer version, if it ever came. Its own channel,
        // because it is not a job and must not make the window look busy.
        if let Some(rx) = &self.state.update_rx {
            if let Ok(release) = rx.try_recv() {
                self.state.update = Some(release);
                self.state.update_rx = None;
            }
        }
        let mut close = false;
        let mut finished_ok = false;
        // Set when the installer is down and checked, and answered as "close
        // the window": running it is Inno replacing the program that is open.
        let mut installing = false;
        if let Some(rx) = &self.state.channel {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Message::Listing(path, v) => {
                        if v.iter().any(|e| e.encrypted) && self.state.archive_password.is_none() {
                            self.state.password_input.clear();
                            self.state.archive_password = None;
                            self.state.waiting_on_password = Some(Pending::OpenArchive);
                        }
                        // Nothing picked to begin with. It used to be
                        // everything, which was invisible while the ticks were
                        // the only sign of it; now that a picked row is painted
                        // it would open as a wall of blue, and "everything is
                        // selected" is not what a list means when you open it.
                        // The buttons that work on the whole archive never
                        // looked at the ticks anyway.
                        self.state.checked = vec![false; v.len()];
                        self.state.folders = tree::folders_of(&v);
                        self.state.entries = v;
                        if let Some(f) = detect(&path) {
                            self.state.format = f;
                        }
                        // The name of what is open goes where every other
                        // program puts it, which frees a whole row above the
                        // list for nothing at all.
                        self.state.window_title = format!(
                            "{} - Arca",
                            path.file_name()
                                .map(|x| x.to_string_lossy().to_string())
                                .unwrap_or_default()
                        );
                        self.state.archive = Some(path);
                        let restore_dir = self.state.reread_dir.take();
                        self.state.history = vec![String::new()];
                        self.state.here = 0;
                        self.state.current_dir = restore_dir
                            .map(|dir| nearest_existing_dir(&self.state.entries, &dir))
                            .unwrap_or_default();
                        if !self.state.current_dir.is_empty() {
                            self.state.history = vec![self.state.current_dir.clone()];
                        }
                        self.state.busy = false;
                        close = true;
                    }
                    Message::Conflict(path) => {
                        self.state.conflict = Some(path);
                    }
                    Message::Progress(done, total, name) => {
                        self.state.done_count = done;
                        self.state.total_count = total;
                        self.state.current_file = name;
                    }
                    Message::Done(text) => {
                        self.state.notice = text;
                        self.state.busy = false;
                        close = true;
                        finished_ok = true;
                    }
                    Message::Failed(text) => {
                        self.state.reread_dir = None;
                        // Stopping is not failing. Nothing is wrong with the
                        // archive and there is nothing to report in red: the
                        // rewrite gave up before it swapped anything.
                        let quit = text == arca_core::Error::Cancelled.to_string();
                        self.state.notice = if quit {
                            self.s().stopped.to_string()
                        } else {
                            text
                        };
                        self.state.error = !quit;
                        self.state.busy = false;
                        close = true;
                    }
                    // The installer is down and checked. It is run silently and
                    // Arca stands aside: Inno Setup closes the program it is
                    // about to replace and opens it again when it finishes,
                    // which is how something that is running gets updated.
                    Message::Downloaded(path) => {
                        self.state.busy = false;
                        close = true;
                        let version = self
                            .state
                            .update
                            .as_ref()
                            .map(|r| r.tag.clone())
                            .unwrap_or_default();
                        self.state.notice =
                            fill(self.s().update_installing, &[("version", &version)]);
                        match install_update(&path) {
                            Ok(()) => installing = true,
                            Err(e) => {
                                self.state.notice = e;
                                self.state.error = true;
                            }
                        }
                    }
                    Message::CutReady => {
                        self.state.cut_pending = self.state.cut_armed.take();
                    }
                }
            }
        }
        if close {
            self.state.channel = None;
            self.state.quiet = false;
            if !self.state.entries.is_empty() && self.state.notice.is_empty() {
                self.state.notice = self.summary();
            }
        }
        // The archive on disk is not the one that was listed any more. Reopen it
        // with the password it now carries, so the browse view shows the new
        // state and does not ask for a password it was just handed.
        if finished_ok {
            if let Some((path, pw, dir)) = self.state.reread_after.take() {
                let notice = std::mem::take(&mut self.state.notice);
                self.state.reread_dir = Some(dir);
                self.open(path);
                self.state.archive_password = pw;
                self.state.notice = notice;
                self.state.view = View::Browse;
            }
        }
        installing
            || (finished_ok
                && self.state.close_when_done
                && matches!(self.state.view, View::Running))
    }
    pub(crate) fn paste_from_clipboard(&mut self) {
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        let here = fs::canonicalize(&archive).unwrap_or_else(|_| archive.clone());
        // Pasting the archive into itself would have the rewrite reading the
        // file it is replacing.
        let inputs: Vec<PathBuf> = clipboard::files()
            .into_iter()
            .filter(|p| fs::canonicalize(p).unwrap_or_else(|_| p.clone()) != here)
            .collect();
        if inputs.is_empty() {
            self.state.notice = self.s().clipboard_empty.to_string();
            self.state.error = true;
            return;
        }
        self.add_files(inputs);
    }

    // Files from anywhere outside into the folder the window is showing. Both
    // the paste and the drop end here so they cannot answer the same question
    // two different ways.
    /// The folder the visible rows are read against: a deep search lists what
    /// is underneath the folder you are in, so the folder shown beside a row is
    /// the part below here. Empty at the root and in the flat view, where a
    /// path is already read from the top.
    pub(crate) fn row_root(&self) -> &str {
        if self.state.settings.flat || self.state.filter.trim().is_empty() {
            ""
        } else {
            &self.state.current_dir
        }
    }

    pub(crate) fn visible_rows(&self) -> Vec<Row> {
        let filter = self.state.filter.trim().to_lowercase();
        // A search reaches down through the folders, so it starts where you
        // are: from the root it still walks the whole archive, and inside a
        // folder it stays under that folder. The flat view is the same list
        // without a name to look for, and it always starts at the root.
        let flat = self.state.settings.flat;
        let mut rows = if !filter.is_empty() {
            search_under(&self.state.entries, self.row_root(), &filter)
        } else if flat {
            self.state
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.is_dir)
                .map(|(i, e)| {
                    let full = e.name.replace('\\', "/");
                    Row {
                        // The leaf here and the folder in its own column, the
                        // way WinRAR splits them: a column of paths that all
                        // begin the same way is a column you read the end of.
                        label: full.rsplit('/').next().unwrap_or(&full).to_string(),
                        kind: kind_of(&full, false),
                        path: full,
                        is_dir: false,
                        entry: Some(i),
                        size: e.size,
                        packed: e.compressed_size,
                        method: e.method.name(),
                        encrypted: e.encrypted,
                        zipcrypto: e.zipcrypto,
                        count: 0,
                        mtime: e.mtime,
                        created: e.created,
                        accessed: e.accessed,
                        attributes: e.attributes,
                        crc32: e.crc32,
                        up: false,
                    }
                })
                .collect()
        } else {
            children_of(&self.state.entries, &self.state.current_dir)
        };

        let (col, asc) = self.state.order;
        rows.sort_by(|x, y| {
            if x.is_dir != y.is_dir {
                return if x.is_dir {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            let o = match col {
                // Folded a character at a time rather than through two new
                // strings. `to_lowercase()` here allocated twice per
                // comparison, which for fifteen hundred rows is some thirty
                // thousand allocations every time the list is built -- and the
                // list is built on every pointer move while a band is being
                // pulled.
                SortColumn::Name => x
                    .label
                    .chars()
                    .flat_map(char::to_lowercase)
                    .cmp(y.label.chars().flat_map(char::to_lowercase)),
                SortColumn::Size => x.size.cmp(&y.size),
                SortColumn::Packed => x.packed.cmp(&y.packed),
                SortColumn::Method => x.method.cmp(y.method),
                SortColumn::Saved => saved_of(x)
                    .partial_cmp(&saved_of(y))
                    .unwrap_or(std::cmp::Ordering::Equal),
                SortColumn::Modified => x.mtime.cmp(&y.mtime),
                SortColumn::Crc => x.crc32.cmp(&y.crc32),
                // By extension, which is what the type is worked out from:
                // sorting by the words themselves would need the shell asked
                // about every entry in the archive to answer one click.
                SortColumn::Type => arca_icons::cache_key(&x.label, x.is_dir)
                    .cmp(&arca_icons::cache_key(&y.label, y.is_dir)),
                SortColumn::Path => folder_of(&x.path).cmp(folder_of(&y.path)),
                SortColumn::Created => x.created.cmp(&y.created),
                SortColumn::Accessed => x.accessed.cmp(&y.accessed),
                SortColumn::Attributes => x.attributes.cmp(&y.attributes),
            };
            if asc {
                o
            } else {
                o.reverse()
            }
        });
        // Put on after the sort, because it belongs at the top whichever column
        // the list is held by and whichever way round.
        if !flat && filter.is_empty() && !self.state.current_dir.is_empty() {
            rows.insert(0, up_row(&self.state.current_dir));
        }
        rows
    }
    pub(crate) fn rename_to(&mut self, rows: &[Row], path: &str, name: &str) {
        let Some(row) = rows.iter().find(|r| r.path == path) else {
            return;
        };
        // Against the name the entry is filed under, not the label, which
        // carries the folder a search found the row in.
        let trimmed = row.path.trim_end_matches('/');
        if name == trimmed.rsplit('/').next().unwrap_or(trimmed) {
            return;
        }
        let s = self.s();
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            self.state.notice = s.bad_name.to_string();
            self.state.error = true;
            return;
        }
        // Only against what is in this folder: the same name elsewhere in the
        // archive is somebody else's business.
        if rows
            .iter()
            .any(|r| r.path != path && r.label.eq_ignore_ascii_case(name))
        {
            self.state.notice = fill(s.name_taken, &[("name", name)]);
            self.state.error = true;
            return;
        }
        let to = rename_destination(path, name);
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        self.run_job(Job::Rename {
            archive,
            from: path.to_string(),
            to,
            folder: row.is_dir,
            password: self.state.archive_password.clone(),
        });
    }

    // The rule between two columns, and the handle that moves it.
    //
    // The handle is a hand's width either side of the rule and only as tall as
    // the header, which is where every list of files on the machine puts it.
    // The table's own went from the header to the foot of the list, so six
    // columns meant six invisible strips down the length of it and a press
    // near any of them was a column edge rather than the start of a selection.
    // The rule is still drawn the whole way down: that is what tells you which
    // number belongs under which heading halfway down a page.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dropped(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.state.notice.clear();
        self.state.error = false;
        let open_first = |me: &mut Self, paths: Vec<PathBuf>| {
            if let Some(p) = paths.into_iter().next() {
                me.open(p);
            }
        };
        let Some(archive) = self.state.archive.clone() else {
            open_first(self, paths);
            return;
        };
        let all_archives = paths.iter().all(|p| detect(p).is_some());
        // Only a .zip can be added to in place. With a .tar open there is
        // nothing to weigh up: an archive opens, and anything else has to say
        // why it cannot go in rather than quietly do nothing.
        if detect(&archive) != Some(Format::Zip) {
            if all_archives {
                open_first(self, paths);
            } else {
                self.state.notice = self.s().only_zip_can_change.to_string();
                self.state.error = true;
            }
            return;
        }
        if all_archives {
            self.state.confirm_drop = Some(paths);
            return;
        }
        self.add_files(paths);
    }

    // A drop used to mean one thing and now means another, so while something
    // is held over the window it says which. Guessing in silence is what made
    // the old behaviour surprising in the first place.
    pub(crate) fn view_entry(&mut self, index: usize) {
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        let Some(entry) = self.state.entries.get(index).cloned() else {
            return;
        };
        let s = self.s();
        if entry.is_dir {
            return;
        }
        if entry.size > VIEW_LIMIT {
            self.state.notice = fill(s.too_big_to_view, &[("size", &human(VIEW_LIMIT))]);
            self.state.error = true;
            return;
        }
        let mut bytes = Vec::with_capacity(entry.size as usize);
        if let Err(e) = read_entry(
            &archive,
            index,
            &mut bytes,
            self.state.archive_password.as_deref(),
        ) {
            self.state.notice = e.to_string();
            self.state.error = true;
            return;
        }

        let name = entry.name.rsplit(['/', '\\']).next().unwrap_or(&entry.name);
        // Asked once, and only of the names that claim to be pictures: handing
        // every unknown file to a decoder to find out is a decoder run on
        // whatever happens to be in the archive.
        let picture = looks_like_picture(name)
            && image::guess_format(&bytes).is_ok_and(|f| {
                image::ImageReader::new(std::io::Cursor::new(&bytes))
                    .with_guessed_format()
                    .is_ok_and(|r| r.format() == Some(f))
            });
        let look = if picture {
            Look::Picture
        } else if looks_like_text(&bytes) {
            Look::Text
        } else {
            Look::Hex
        };
        // Split now, once. The text is drawn a line at a time and only the
        // lines on screen are laid out, so a log of a million lines opens as
        // fast as a note of three.
        let lines = String::from_utf8_lossy(&bytes)
            .lines()
            .map(|l| l.to_string())
            .collect();
        self.state.viewing = Some(Viewed {
            name: name.to_string(),
            bytes: bytes.into(),
            look,
            lines,
            picture,
        });
    }

    // The file being looked at, in its own window over the list.
    pub(crate) fn open(&mut self, path: PathBuf) {
        self.state.archive_password = None;
        // Whatever was cut belonged to the listing being replaced, and so did
        // whatever the status bar was saying: the summary of the archive being
        // closed sat there over the one that had just opened.
        self.state.cut_names.clear();
        self.state.cut_armed = None;
        self.state.cut_pending = None;
        self.state.notice.clear();
        self.state.error = false;
        self.remember(&path);
        self.spawn(0, move |tx| {
            let m = match list_entries(&path) {
                Ok(v) => Message::Listing(path, v),
                Err(e) => Message::Failed(e.to_string()),
            };
            let _ = tx.send(m);
        });
    }

    // Reading the central directory is enough to know whether the archive is
    // encrypted, and costs nothing next to extracting it. Asking here, before
    // any work starts, keeps the question on the window's own thread.
    pub(crate) fn run_job(&mut self, job: Job) {
        if let Job::Extract {
            archives,
            password: None,
            ..
        } = &job
        {
            if archives.iter().any(|a| is_encrypted(a)) {
                self.state.password_input.clear();
                self.state.waiting_on_password = Some(Pending::Extract(Box::new(job)));
                self.state.view = View::Running;
                self.state.title = self.s().extracting.to_string();
                return;
            }
        }
        let s: &'static Strings = self.s();
        let verb = match &job {
            Job::Extract { .. } => s.extracting.to_string(),
            Job::Test { .. } => s.testing.to_string(),
            Job::Password { .. } => s.changing_password.to_string(),
            Job::Delete { .. } => s.deleting.to_string(),
            Job::Rename { .. } => s.renaming.to_string(),
            Job::CopyTo { .. } => s.copying_word.to_string(),
            Job::NewFolder { .. } => s.adding.to_string(),
            Job::Move { .. } => s.moving_word.to_string(),
            Job::Compress { .. } => s.compressing.to_string(),
            Job::Add { .. } => s.adding.to_string(),
            // The only verb with a hole in it: which version is coming down is
            // known to the job, not to the list of words.
            Job::Update { tag, .. } => fill(s.update_downloading, &[("version", tag)]),
        };
        // The download measures itself in bytes; everything else counts
        // entries.
        self.state.in_bytes = matches!(job, Job::Update { .. });
        // Getting the new version is asked for from this window's menu; the
        // rest can come from the Explorer, and then there is no list behind it.
        let from_here = self.state.archive.is_some() || matches!(job, Job::Update { .. });
        self.show_job(&verb, subject_of(&job), from_here);
        // Only a window that was opened to do this one job goes away when it is
        // done. Started from the toolbar, finishing a job and closing the whole
        // program is indistinguishable from a crash.
        self.state.close_when_done = self.state.one_shot
            && !matches!(
                job,
                Job::Test { .. }
                    | Job::Password { .. }
                    | Job::Delete { .. }
                    | Job::Rename { .. }
                    | Job::CopyTo { .. }
                    | Job::NewFolder { .. }
                    | Job::Move { .. }
                    | Job::Add { .. }
            );
        // The file on disk is about to change, so the listing has to be redone.
        //
        // One match rather than a copy of the same `if let` per job: written as
        // copies, a new folder and a move were left out, and the window went on
        // showing a listing the archive no longer matched until it was reopened
        // by hand. This list is the jobs that rebuild the file, and it is the
        // same one the undo entry below is built from.
        //
        // A window that leaves when the job ends has nothing to show, and
        // pointing it at the new archive would cancel its own departure: what
        // decides the window closes is still being on View::Running, and
        // rereading is what puts it back on View::Browse.
        self.state.reread_after = if self.state.close_when_done {
            None
        } else {
            reread_target(&job, &self.state.current_dir)
        };
        // The ones that build the archive again leave the old one beside it.
        // What is kept here is the word for the change, so that offering to
        // take it back can say what it would be taking back.
        //
        // Without this the undo entry was never written, which is why Ctrl+Z
        // and the menu entry were permanently greyed out.
        let words = self.s();
        self.state.undo = match &job {
            Job::Delete { archive, .. } => Some((archive.clone(), words.delete_word)),
            Job::Rename { archive, .. } => Some((archive.clone(), words.rename_word)),
            Job::Add { archive, .. } => Some((archive.clone(), words.add_to_archive)),
            Job::Password { archive, .. } => Some((archive.clone(), words.password_word)),
            Job::NewFolder { archive, .. } => Some((archive.clone(), words.new_folder)),
            Job::Move { archive, .. } => Some((archive.clone(), words.moving_word)),
            _ => None,
        };

        let (reply_tx, reply_rx) = channel::<Answer>();
        self.state.replies = Some(reply_tx);
        let (stop, hold) = self.fresh_flags();
        self.spawn(0, move |tx| {
            use std::sync::atomic::Ordering;
            let notify = |i: usize, n: usize, name: &str| {
                let _ = tx.send(Message::Progress(i, n, name.to_string()));
                // Held right here while it is paused. This is the end of an
                // entry, which is the one moment the work is not in the middle
                // of something; stopping still gets through, so a paused job
                // can be given up on without being let go first.
                while hold.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(60));
                }
                // The answer to "carry on?". Read on every step because that is
                // the only place a long job looks up from what it is doing.
                !stop.load(Ordering::Relaxed)
            };
            // Getting the new version does not end in a text to read but in a
            // file to run, and running it closes Arca. That is why it leaves by
            // its own message and not by Done: who decides to install is the
            // window, not this thread.
            if let Job::Update {
                installer, sums, ..
            } = &job
            {
                let _ = tx.send(match download_update(installer, sums, s, &notify) {
                    Ok(path) => Message::Downloaded(path),
                    Err(text) => Message::Failed(text),
                });
                return;
            }
            let ask = conflict_asker(tx, &reply_rx);
            let outcome = run_job_blocking(job, s, &notify, &ask);
            let _ = tx.send(match outcome {
                Ok(text) => Message::Done(text),
                Err(text) => Message::Failed(text),
            });
        });
    }
    pub(crate) fn is_checked(&self, row: &Row) -> bool {
        // The way out of the folder is not a thing that can be picked.
        if row.up {
            return false;
        }
        match row.entry {
            Some(i) => self.state.checked[i],
            None => {
                let under = entries_under(&self.state.entries, &row.path);
                !under.is_empty() && under.iter().all(|&i| self.state.checked[i])
            }
        }
    }
}

/// Where to look again once `job` has finished, the password the archive
/// carries by then, and the folder to restore after rereading it.
///
/// A job that rebuilds the archive leaves the window listing a file that is no
/// longer there. Every one of them belongs here; the jobs that only read
/// (testing, extracting) or write somewhere else (copying, downloading) do not.
///
/// Compressing is the odd one: nothing that was open changed, but what it wrote
/// is an archive and the window that asked for it has nothing else to show.
/// Opening it answers "where did it go", which is the question a window left
/// sitting on its own success is asking.
fn reread_target(job: &Job, current_dir: &str) -> Option<(PathBuf, Option<String>, String)> {
    match job {
        Job::Password { archive, new, .. } => {
            Some((archive.clone(), new.clone(), current_dir.to_string()))
        }
        Job::Delete {
            archive, password, ..
        }
        | Job::Add {
            archive, password, ..
        }
        | Job::NewFolder {
            archive, password, ..
        } => Some((archive.clone(), password.clone(), current_dir.to_string())),
        Job::Rename {
            archive,
            from,
            to,
            folder,
            password,
        } => Some((
            archive.clone(),
            password.clone(),
            renamed_dir(current_dir, from, to, *folder),
        )),
        Job::Move {
            archive,
            moves,
            password,
        } => Some((
            archive.clone(),
            password.clone(),
            moved_name(current_dir, moves),
        )),
        Job::Compress { out, password, .. } => Some((out.clone(), password.clone(), String::new())),
        Job::Extract { .. } | Job::Test { .. } | Job::CopyTo { .. } | Job::Update { .. } => None,
    }
}

fn renamed_dir(current_dir: &str, from: &str, to: &str, folder: bool) -> String {
    if !folder {
        return current_dir.to_string();
    }
    let current = normalized_dir(current_dir);
    let from = normalized_dir(from);
    if current == from || current.starts_with(&from) {
        return format!(
            "{}{rest}",
            normalized_dir(to),
            rest = &current[from.len()..]
        );
    }
    current
}

fn normalized_dir(dir: &str) -> String {
    let dir = dir.replace('\\', "/");
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    }
}

fn nearest_existing_dir(entries: &[arca_core::Entry], desired: &str) -> String {
    let mut dir = normalized_dir(desired);
    loop {
        if dir.is_empty() {
            return dir;
        }
        let prefix = dir.trim_end_matches('/');
        if entries.iter().any(|entry| {
            let name = entry.name.replace('\\', "/");
            (entry.is_dir && name.trim_end_matches('/') == prefix) || name.starts_with(&dir)
        }) {
            return dir;
        }
        dir = parent_of(&dir);
    }
}

#[cfg(test)]
mod reread_tests {
    use super::*;

    fn archive() -> PathBuf {
        PathBuf::from("a.zip")
    }

    /// Every job that rebuilds the archive has to ask for a reread. This is
    /// written against the job list rather than against one case, because the
    /// bug it is here for was a job quietly missing from the list.
    #[test]
    fn jobs_that_rewrite_the_archive_are_reread() {
        let rewrites = [
            Job::NewFolder {
                archive: archive(),
                name: "f/".into(),
                password: None,
            },
            Job::Move {
                archive: archive(),
                moves: vec![("a".into(), "b/a".into())],
                password: None,
            },
            Job::Delete {
                archive: archive(),
                names: vec!["a".into()],
                password: None,
            },
            Job::Rename {
                archive: archive(),
                from: "a".into(),
                to: "b".into(),
                folder: false,
                password: None,
            },
        ];
        // Named by position rather than by value: `Job` carries passwords in
        // the clear and must not learn to print itself.
        for (ix, job) in rewrites.iter().enumerate() {
            assert_eq!(
                reread_target(job, "").map(|(path, _, _)| path),
                Some(archive()),
                "job {ix} rewrites the archive and must be reread"
            );
        }
    }

    /// The password the archive carries afterwards is the new one, not the one
    /// it was opened with, or the reread would ask for a password that no
    /// longer opens it.
    #[test]
    fn a_password_job_is_reread_with_the_new_password() {
        let job = Job::Password {
            archive: archive(),
            current: Some("old".into()),
            new: Some("new".into()),
        };
        assert_eq!(
            reread_target(&job, "folder/"),
            Some((archive(), Some("new".to_string()), "folder/".to_string()))
        );
    }

    /// Reading an archive, or writing a different one, leaves the listing alone.
    #[test]
    fn jobs_that_leave_the_archive_alone_are_not_reread() {
        let job = Job::Test {
            archive: archive(),
            only: None,
        };
        assert!(reread_target(&job, "").is_none());
    }

    #[test]
    fn rewritten_archive_restores_the_folder_after_a_folder_rename() {
        assert_eq!(
            renamed_dir(
                "Projects/Big Ambitions/Levels/",
                "Projects/Big Ambitions/",
                "Projects/Renamed",
                true
            ),
            "Projects/Renamed/Levels/"
        );
        assert_eq!(
            renamed_dir(
                "Other/",
                "Projects/Big Ambitions/",
                "Projects/Renamed",
                true
            ),
            "Other/"
        );
    }

    #[test]
    fn rewritten_archive_restores_the_folder_after_a_move() {
        assert_eq!(
            moved_name("Old/Levels/", &[("Old".into(), "New/Old".into())]),
            "New/Old/Levels/"
        );
    }

    #[test]
    fn deleted_folder_restores_the_nearest_existing_parent() {
        let entries = vec![arca_core::Entry {
            name: "kept/file.txt".into(),
            size: 0,
            compressed_size: 0,
            method: arca_core::Method::Store,
            crc32: 0,
            is_dir: false,
            mtime: None,
            created: None,
            accessed: None,
            attributes: 0,
            offset: 0,
            raw_name: Vec::new(),
            utf8: true,
            encrypted: false,
            zipcrypto: false,
        }];
        assert_eq!(nearest_existing_dir(&entries, "gone/child/"), "");
        assert_eq!(nearest_existing_dir(&entries, "kept/"), "kept/");
    }

    #[test]
    fn folder_rename_destination_is_a_sibling_not_a_child() {
        assert_eq!(rename_destination("Big Ambitions/", "Renamed"), "Renamed");
        assert_eq!(
            rename_destination("Projects/Big Ambitions/", "Renamed"),
            "Projects/Renamed"
        );
        assert_eq!(rename_destination("file.txt", "new.txt"), "new.txt");
    }
}

/// The password button asks before it writes.
///
/// It used to build the job with no new password in it and run it, so the one
/// thing the button could do was take a password off.
/// The add box is built up, not replaced, and a window nobody handed a job to
/// on the command line survives the job it was asked for.
///
/// Both of these were bugs: compressing from the toolbar closed the whole
/// program when it finished, and a second pass through the picker threw away
/// the first one, which is why a selection could never mix files and folders.
#[cfg(test)]
mod compress_tests {
    use super::*;

    #[test]
    fn picking_twice_keeps_both_picks_and_the_typed_name() {
        let mut controller = AppController::new(Settings::default());
        controller.prepare_compress(vec![PathBuf::from("/tmp/a.txt")]);
        let derived = controller.state.output_name.clone();
        assert!(!derived.is_empty());
        controller.state.output_name = "mine.zip".into();
        controller.prepare_compress(vec![
            PathBuf::from("/tmp/folder"),
            PathBuf::from("/tmp/a.txt"),
        ]);
        assert_eq!(
            controller.state.pending_inputs,
            vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/folder")]
        );
        assert_eq!(controller.state.output_name, "mine.zip");
    }

    #[test]
    fn opening_the_box_afresh_starts_from_nothing() {
        let mut controller = AppController::new(Settings::default());
        controller.prepare_compress(vec![PathBuf::from("/tmp/a.txt")]);
        controller.state.view = View::Browse;
        controller.prepare_compress(Vec::new());
        assert!(controller.state.pending_inputs.is_empty());
        assert!(controller.state.output_name.is_empty());
        assert!(matches!(controller.state.view, View::Add));
    }

    #[test]
    fn a_window_opened_by_hand_stays_open_when_the_job_ends() {
        let mut by_hand = AppController::new(Settings::default());
        by_hand.run_job(compressing(None));
        assert!(!by_hand.state.close_when_done);

        let mut from_explorer = AppController::new(Settings::default());
        from_explorer.state.one_shot = true;
        from_explorer.run_job(compressing(None));
        assert!(from_explorer.state.close_when_done);
    }

    fn compressing(password: Option<String>) -> Job {
        Job::Compress {
            out: std::env::temp_dir().join("arca-close-test.zip"),
            inputs: vec![std::env::temp_dir().join("arca-close-test-input")],
            format: Format::Zip,
            codec: Codec::Deflate,
            level: Level::Normal,
            password,
        }
    }

    /// The archive that was just written is what the window then shows, with
    /// the password it was given so it does not turn round and ask for one it
    /// already knows.
    ///
    /// Without this the window sat on "Created ..." and an empty list, and the
    /// only way to see what had just been built was to go and open it by hand.
    #[test]
    fn the_new_archive_is_what_the_window_opens_next() {
        let mut controller = AppController::new(Settings::default());
        controller.run_job(compressing(Some("secret".into())));
        assert_eq!(
            controller.state.reread_after,
            Some((
                std::env::temp_dir().join("arca-close-test.zip"),
                Some("secret".to_string()),
                String::new(),
            ))
        );
    }

    /// The two halves have to agree: a window that is leaving must not be sent
    /// to the new archive, because arriving there is what would keep it. What
    /// closes the window is still being on `View::Running`, and a reread is
    /// exactly what puts it back on `View::Browse`.
    #[test]
    fn a_window_that_is_leaving_is_not_sent_to_the_new_archive() {
        let mut controller = AppController::new(Settings::default());
        controller.state.one_shot = true;
        controller.run_job(compressing(None));
        assert!(controller.state.close_when_done);
        assert!(controller.state.reread_after.is_none());
    }
}

#[cfg(test)]
mod password_tests {
    use super::*;

    fn controller(encrypted: bool) -> AppController {
        let mut controller = AppController::new(Settings::default());
        controller.state.archive = Some(std::env::temp_dir().join("arca-password-test.zip"));
        controller.state.entries = vec![arca_core::Entry {
            name: "a.txt".into(),
            size: 0,
            compressed_size: 0,
            method: arca_core::Method::Store,
            crc32: 0,
            is_dir: false,
            mtime: None,
            created: None,
            accessed: None,
            attributes: 0,
            offset: 0,
            raw_name: Vec::new(),
            utf8: true,
            encrypted,
            zipcrypto: false,
        }];
        controller.state.checked = vec![false];
        controller
    }

    #[test]
    fn a_plain_archive_is_asked_what_password_to_put_on_it() {
        let mut controller = controller(false);
        controller.begin_password_change();
        assert!(matches!(
            controller.state.waiting_on_password,
            Some(Pending::NewPassword(_))
        ));
        assert!(!controller.state.busy);
    }

    /// The password it already has does not answer the question of what it
    /// should have next, so the window stays up for the second one.
    #[test]
    fn a_locked_archive_is_asked_for_both_passwords() {
        let mut controller = controller(true);
        controller.begin_password_change();
        assert!(matches!(
            controller.state.waiting_on_password,
            Some(Pending::CurrentPassword(_))
        ));
        controller.submit_password("old".into());
        match &controller.state.waiting_on_password {
            Some(Pending::NewPassword(job)) => match &**job {
                Job::Password { current, new, .. } => {
                    assert_eq!(current.as_deref(), Some("old"));
                    assert!(new.is_none());
                }
                _ => panic!("the held job is the password one"),
            },
            _ => panic!("the new password is still to be asked for"),
        }
    }

    /// What is typed into the second window is the password the archive ends up
    /// with. `reread_after` carries it because the listing has to be reopened
    /// with it; an empty answer is the way to take the password off.
    #[test]
    fn the_answer_is_the_password_the_archive_ends_up_with() {
        for (given, expected) in [("new", Some("new".to_string())), ("", None)] {
            let mut controller = controller(false);
            controller.begin_password_change();
            controller.submit_password(given.into());
            assert_eq!(
                controller
                    .state
                    .reread_after
                    .as_ref()
                    .map(|(_, password, _)| password.clone()),
                Some(expected),
                "password {given:?}"
            );
        }
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    fn entry(name: &str) -> arca_core::Entry {
        arca_core::Entry {
            raw_name: name.as_bytes().to_vec(),
            utf8: true,
            name: name.to_string(),
            size: 1,
            compressed_size: 1,
            method: arca_core::Method::Store,
            crc32: 0,
            is_dir: false,
            mtime: None,
            created: None,
            accessed: None,
            attributes: 0,
            offset: 0,
            encrypted: false,
            zipcrypto: false,
        }
    }

    fn searching(dir: &str, filter: &str) -> Vec<String> {
        let mut controller = AppController::new(Settings::default());
        controller.state.entries = vec![
            entry("Game/_CommonRedist/oalinst.exe"),
            entry("Game/_CommonRedist/dxwebsetup.exe"),
            entry("Game/_CommonRedist/DirectX/redist.txt"),
            entry("Game/Data/level.dat"),
            entry("Other/redistributable.bin"),
        ];
        controller.state.current_dir = dir.to_string();
        controller.state.filter = filter.to_string();
        controller
            .visible_rows()
            .into_iter()
            .map(|r| r.label)
            .collect()
    }

    /// The folder a row sits in is not part of the name it is matched against:
    /// matching the whole path made a search for `redist` inside
    /// `_CommonRedist` answer with every file in the folder. Folders match on
    /// their own name too, and are answered with.
    #[test]
    fn a_search_matches_names_and_answers_with_folders_too() {
        assert_eq!(
            searching("", "redist"),
            vec![
                // The folder first, the way the list always sorts.
                "Game/_CommonRedist",
                "Game/_CommonRedist/DirectX/redist.txt",
                "Other/redistributable.bin",
            ]
        );
    }

    /// A hit is named by the path from the folder being searched down, so it
    /// says which folder it came out of without repeating the way back to the
    /// root of the archive.
    #[test]
    fn a_hit_is_named_from_the_folder_being_searched_down() {
        assert_eq!(
            searching("Game/_CommonRedist/", "redist"),
            vec!["DirectX/redist.txt"]
        );
    }

    /// From the root a search reaches the whole archive; from a folder it stays
    /// under that folder.
    #[test]
    fn a_search_is_deep_but_stays_under_the_folder_it_starts_in() {
        assert_eq!(
            searching("", "exe"),
            vec![
                "Game/_CommonRedist/dxwebsetup.exe",
                "Game/_CommonRedist/oalinst.exe",
            ]
        );
        assert_eq!(
            searching("Game/Data/", "exe"),
            Vec::<String>::new(),
            "a search inside one folder does not reach into another"
        );
    }
}
