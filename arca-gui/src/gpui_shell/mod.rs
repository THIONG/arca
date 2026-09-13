//! Opt-in GPUI surface for the G4.1 toolbar and navigation migration.
//!
//! This view translates user input into `AppAction`s. Archive work stays in
//! `AppController` workers; native file dialogs are bridged from a thread so
//! they never block GPUI's UI thread. The file table uses GPUI's virtualized
//! `uniform_list`; dialog state and overlays are rendered by GPUI, while native
//! file pickers stay on worker threads.

use super::{
    fill, human, parent_of, saved_of, when, Answer, AppAction, AppController, Columns, DropChoice,
    Job, Pending, Settings, SortColumn, Startup, Strings, View,
};
use crate::{
    clipboard, gpui_theme,
    tree::{Folder, Kind},
};
use gpui::{actions, point};
use gpui::{
    canvas, div, prelude::*, px, size, uniform_list, App, Bounds, ClickEvent, Context, ElementId,
    Entity, FocusHandle, Focusable, KeyBinding, KeyDownEvent, Role, ScrollStrategy, Stateful,
    UniformListScrollHandle, WeakEntity, Window, WindowBounds, WindowOptions,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::{Dialog, DialogAction, DialogClose, DialogDescription, DialogFooter};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::kbd::Kbd;
use gpui_component::list::ListItem;
use gpui_component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::progress::Progress;
use gpui_component::radio::RadioGroup;
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::separator::Separator;
use gpui_component::status_bar::StatusBar;
use gpui_component::switch::Switch;
use gpui_component::table::{Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState};
use gpui_component::tree::{tree, TreeItem, TreeState};
use gpui_component::{ActiveTheme, Icon, IconName};
use gpui_component::{Disableable, Root, Selectable, Sizable as _, TitleBar, WindowExt};
use gpui_platform::application;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

/// The ring GPUI paints around whatever the keyboard is on.
///
/// One function rather than seven copies of the same closure, because a focus
/// ring that is not identical everywhere is a focus ring you have to look for.
/// It reads the tokens once and carries them into the closure, since
/// `focus_visible` runs without a context.
fn focus_ring(cx: &App) -> impl FnOnce(gpui::StyleRefinement) -> gpui::StyleRefinement {
    let (ring, accent) = (cx.theme().ring, cx.theme().accent);
    move |style: gpui::StyleRefinement| style.border_2().border_color(ring).bg(accent)
}

const COMPACT_SIZE: (f32, f32) = (560.0, 300.0);
const NORMAL_SIZE: (f32, f32) = (1000.0, 660.0);
const MINIMUM_SIZE: (f32, f32) = (720.0, 320.0);

/// How many recently opened archives the menu keeps room for. Ten is about as
/// many as anybody scans before giving up and going to the folder instead.
const RECENT_MAX: usize = 10;

actions!(arca_gpui, [FocusFilter, CopyFiles, CutFiles, PasteFiles]);

/// What came back from a native file dialog, which runs off the main thread.
enum DialogResult {
    Open(Option<PathBuf>),
    Compress(Option<Vec<PathBuf>>),
    Extract {
        only_checked: bool,
        destination: Option<PathBuf>,
    },
    AddFiles(Option<Vec<PathBuf>>),
    SaveCopy(Option<PathBuf>),
}

struct GpuiShell {
    controller: AppController,
    filter: Entity<InputState>,
    /// The field that replaces a row's name while it is being renamed. Renaming
    /// happens in the row, the way every file manager does it, so there is no
    /// dialog for it.
    rename_input: Entity<InputState>,
    password: Entity<InputState>,
    output_name: Entity<InputState>,
    add_password: Entity<InputState>,
    name_input: Entity<InputState>,
    focus_handle: FocusHandle,
    list_focus: FocusHandle,
    /// The same handle the kit's table scrolls with. The band needs the list's
    /// geometry and reads it from here, so both must be looking at one handle.
    list_scroll: UniformListScrollHandle,
    /// The kit's table, and the copy of the frame it draws from.
    table: Entity<TableState<FileTable>>,
    dialog: Option<Receiver<DialogResult>>,
    /// Which modal the kit's dialog stack currently holds. The shell derives
    /// its modal from controller state, the kit opens and closes one
    /// imperatively; this is what tells the two apart.
    open_modal: Option<ModalKind>,
    delete_trigger_focus: FocusHandle,
    drop_trigger_focus: FocusHandle,
    conflict_trigger_focus: FocusHandle,
    /// Where the keyboard goes when a dialog closes, when somebody asked for
    /// it. None means leave it alone: a kit button holds its own focus and
    /// still has it when a native file dialog closes on top of it.
    dialog_return_focus: Option<FocusHandle>,
    modal_seen: Option<ModalKind>,
    dialog_primary_focus: FocusHandle,
    dialog_cancel_focus: FocusHandle,
    add_start_focus: FocusHandle,
    /// The eleven controls of the settings dialog, in the order they are drawn.
    /// One vector rather than eleven fields because every one of them is the
    /// same thing -- a row in a list of preferences -- and `SettingsControl`
    /// already says which is which.
    settings_focus: Vec<FocusHandle>,
    /// Which row the right button was pressed on, and where the pointer was,
    /// so the menu opens under it instead of in a fixed corner.
    /// The band being drawn, while it is being drawn.
    band: Option<Band>,
    /// The wheel used as a button: press it and the list runs towards the
    /// pointer until something puts it away.
    wheel: Option<WheelPan>,
    /// Where the pointer was last seen, so the tick that keeps the list running
    /// knows which way to go without an event of its own.
    pointer: gpui::Point<gpui::Pixels>,
    /// A selection is in the air. Where it lands is not decided until it leaves
    /// the list or is dropped on a folder.
    carrying: bool,
    /// How many rows the list last drew.
    ///
    /// Kept because `visible_rows` walks every entry, allocates a `Row` for
    /// each and sorts the lot: asking it for nothing but a count, twice per
    /// pointer move, was most of why dragging a band felt heavy. Render is the
    /// one place that already has the answer.
    row_count: usize,
    /// Which column edge is in hand: its slot in `Settings::widths`, where the
    /// pointer was when it was grabbed, and how wide the column was then.
    /// Kept as the state at the grab rather than as a running delta, so a
    /// dropped mouse-move event cannot make the column drift.
    resizing: Option<(usize, f32, f32)>,
    /// What the shared `Name` field currently holds. The controller has no
    /// business knowing about a half-typed folder name, so it stays here until
    /// the dialog is answered.
    name_value: String,
    /// Text, Hex and Picture, in that order.
    viewer_focus: Vec<FocusHandle>,
    viewer_scroll: UniformListScrollHandle,
    /// The decoded picture, kept by the name it came out of the archive under.
    /// Handing GPUI a fresh `Image` every frame would decode a thirty megabyte
    /// photograph sixty times a second.
    viewer_image: Option<(String, std::sync::Arc<gpui::Image>)>,
    /// The archive's folders as the kit's tree, and the archive they were
    /// grown from. Rebuilding the items every frame would throw away which
    /// branches are open, so they are grown once per archive and the key says
    /// when that archive stopped being the same one.
    folders: Entity<TreeState>,
    folders_key: Option<(Option<PathBuf>, usize)>,
    drop_paths: Vec<PathBuf>,
}

/// What the menu on a row offers. Everything here already exists as a
/// controller call or a dialog; the menu is only a second way in, for the
/// times the hand is already down on the list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowAction {
    Open,
    ExtractSelection,
    ExtractHere,
    TestSelection,
    View,
    Rename,
    Delete,
    Copy,
    Cut,
    Paste,
    CopyNames,
    SelectAll,
}

impl RowAction {
    const ALL: [RowAction; 12] = [
        RowAction::Open,
        RowAction::ExtractSelection,
        RowAction::ExtractHere,
        RowAction::TestSelection,
        RowAction::View,
        RowAction::Rename,
        RowAction::Delete,
        RowAction::Copy,
        RowAction::Cut,
        RowAction::Paste,
        RowAction::CopyNames,
        RowAction::SelectAll,
    ];

    /// What it is called, and the keys that do the same thing. A menu that does
    /// not name the shortcut is a menu nobody graduates from.
    fn label(self, s: &'static Strings) -> (&'static str, &'static str) {
        match self {
            RowAction::Open => (s.open_word, "Enter"),
            RowAction::ExtractSelection => (s.extract_selected, "Ctrl+E"),
            RowAction::ExtractHere => (s.extract_here, "Alt+W"),
            RowAction::TestSelection => (s.test_selection, ""),
            RowAction::View => (s.view_word, "F3"),
            RowAction::Rename => (s.rename_word, "F2"),
            RowAction::Delete => (s.delete_word, "Supr"),
            RowAction::Copy => (s.copy_word, "Ctrl+C"),
            RowAction::Cut => (s.cut_word, "Ctrl+X"),
            RowAction::Paste => (s.paste_word, "Ctrl+V"),
            RowAction::CopyNames => (s.copy_names, "Ctrl+Shift+C"),
            RowAction::SelectAll => (s.select_all, "Ctrl+A"),
        }
    }

    /// Whether a rule goes above this one. Looking at an entry, changing it,
    /// moving it through the clipboard and working on the selection are four
    /// different things, and twelve entries in one run is a wall.
    fn starts_group(self) -> bool {
        matches!(
            self,
            RowAction::Rename | RowAction::Copy | RowAction::CopyNames
        )
    }

    /// Copy, cut and paste are left out rather than greyed out where the shell
    /// has nowhere to put them: a menu entry that can never do anything is
    /// worse than no entry.
    fn offered(self) -> bool {
        !matches!(self, RowAction::Copy | RowAction::Cut | RowAction::Paste) || clipboard::AVAILABLE
    }
}

/// A control in the settings dialog whose choices are named rather than
/// listed: the language, the theme, and the switch that extracts into a
/// subfolder. The settings that pick a value out of a list go through `pick`
/// and need no name of their own.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsControl {
    LangSystem,
    LangEn,
    LangEs,
    ThemeSystem,
    ThemeLight,
    ThemeDark,
    Subfolder,
}

impl SettingsControl {
    const ALL: [SettingsControl; 7] = [
        SettingsControl::LangSystem,
        SettingsControl::LangEn,
        SettingsControl::LangEs,
        SettingsControl::ThemeSystem,
        SettingsControl::ThemeLight,
        SettingsControl::ThemeDark,
        SettingsControl::Subfolder,
    ];
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModalKind {
    Password,
    Conflict,
    Delete,
    Drop,
    Add,
    Viewer,
    NewFolder,
    Mask,
    DefaultPassword,
    Settings,
    Shortcuts,
}

/// One folder and everything under it, as the kit's tree items.
///
/// The id is the path the folder is reached by, which is what `Navigate`
/// takes, so the tree needs no table on the side to say what a row means.
fn folder_items(folder: &Folder, path: &str) -> Vec<TreeItem> {
    folder
        .kids
        .iter()
        .map(|(label, kid)| {
            let id = format!("{path}{label}/");
            TreeItem::new(id.clone(), label.clone()).children(folder_items(kid, &id))
        })
        .collect()
}

impl GpuiShell {
    fn new(window: &mut Window, cx: &mut Context<Self>, startup: Startup) -> Self {
        let owner = cx.weak_entity();
        let strings = super::strings(Settings::load().effective_lang());
        let filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(strings.find_word)
                .context_menu(false)
        });
        cx.subscribe_in(&filter, window, |shell, state, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                shell
                    .controller
                    .dispatch(AppAction::SetFilter(state.read(cx).value().to_string()));
                cx.notify();
            }
        })
        .detach();
        let rename_input = cx.new(|cx| InputState::new(window, cx).context_menu(false));
        let list_scroll = UniformListScrollHandle::new();
        let table = cx.new(|cx| {
            TableState::new(
                FileTable {
                    shell: owner.clone(),
                    rows: Vec::new(),
                    columns: Vec::new(),
                    widths: Settings::default_widths(),
                    checked: Vec::new(),
                    muted: Vec::new(),
                    cursor: None,
                    renaming: None,
                    rename_input: rename_input.clone(),
                    order: (SortColumn::Name, true),
                    strings,
                    idle: true,
                    writable: false,
                },
                window,
                cx,
            )
            // One row at a time is the kit's idea of a selection; this list
            // marks many, and keeps that count itself.
            .row_selectable(true)
            .col_selectable(false)
            .col_movable(false)
        });
        // The band measures the list through this handle, so it has to be the
        // one the table actually scrolls.
        table.update(cx, |state, _| {
            state.vertical_scroll_handle = list_scroll.clone();
        });
        cx.subscribe_in(
            &table,
            window,
            |shell, _, event: &TableEvent, window, cx| {
                match event {
                    // The kit moves its own selected row with the arrows and the
                    // mouse; the shell's cursor follows it, and the marking stays
                    // the shell's business.
                    TableEvent::SelectRow(row) => {
                        shell.controller.state.cursor = Some(*row);
                        cx.notify();
                    }
                    // Right clicking something that is not picked picks it, which
                    // is what every file list does; right clicking inside a
                    // selection leaves the selection alone.
                    TableEvent::RightClickedRow(Some(row)) => {
                        if let Some(target) = shell.controller.visible_rows().get(*row).cloned() {
                            if !shell.controller.is_checked(&target) {
                                shell.controller.dispatch(AppAction::SetChecked {
                                    row: target,
                                    value: true,
                                });
                            }
                        }
                        shell.controller.state.cursor = Some(*row);
                        cx.notify();
                    }
                    TableEvent::DoubleClickedRow(row) => shell.open_row(*row, cx),
                    // Written when the kit says the pull is over, so dragging an
                    // edge across the window is not a stream of writes to disk.
                    TableEvent::ColumnWidthsChanged(widths) => {
                        shell.remember_widths(widths, cx);
                    }
                    _ => {}
                }
                let _ = window;
            },
        )
        .detach();
        cx.subscribe_in(&rename_input, window, |shell, _, event, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => shell.commit_rename(window, cx),
                // Clicking away is how a file manager abandons a rename. It
                // cancels rather than commits: a name changed by accident in an
                // archive costs a full rewrite to undo.
                InputEvent::Blur => shell.cancel_rename(cx),
                _ => {}
            }
        })
        .detach();
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(strings.password_word)
                .masked(true)
        });
        let output_name = cx.new(|cx| InputState::new(window, cx).placeholder(strings.output_name));
        let add_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(strings.password_optional)
                .masked(true)
        });
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder(strings.folder_name));
        // Every dialog field lands in a different place, but they all land the
        // same way: one subscription each, so a new field is one arm here
        // rather than a second path that can be forgotten.
        cx.subscribe(&password, |shell, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let value = state.read(cx).value().to_string();
                shell
                    .controller
                    .dispatch(AppAction::SetPasswordInput(value));
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&output_name, |shell, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                shell.controller.state.output_name = state.read(cx).value().to_string();
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&add_password, |shell, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                shell.controller.state.add_password = state.read(cx).value().to_string();
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&name_input, |shell, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                shell.name_value = state.read(cx).value().to_string();
                cx.notify();
            }
        })
        .detach();
        // The tree owns which folder is picked; the shell hears about it and
        // goes there. The guard is what keeps the two from chasing each other:
        // `sync_folders` puts the mark back on the folder the list is showing,
        // and that must not read as a request to move.
        let folders = cx.new(|cx| TreeState::new(cx));
        cx.observe(&folders, |shell, state, cx| {
            let Some(path) = state
                .read(cx)
                .selected_item()
                .map(|item| item.id.to_string())
            else {
                return;
            };
            if path == shell.controller.state.current_dir || !shell.background_idle() {
                return;
            }
            shell.controller.dispatch(AppAction::Navigate(path));
            shell.route_changed(cx);
        })
        .detach();
        let _ = owner;
        let mut controller = AppController::new(Settings::load());
        apply_startup(&mut controller, startup);
        // The stored preference decides light, dark or whatever the desktop is
        // set to, and it has to land before the first frame or the window opens
        // in one theme and repaints into the other.
        gpui_theme::apply(controller.state.settings.theme, Some(window), cx);
        window.set_window_title(&controller.state.window_title);
        let view = cx.weak_entity();
        window
            .spawn(cx, async move |async_cx| loop {
                async_cx
                    .background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if view
                    .update_in(async_cx, |shell, window, cx| {
                        shell.poll_dialog(window, cx);
                        // The list keeps running while the hand is still, which
                        // is the whole point of the gesture: without a step
                        // here it would only move on a stray mouse event.
                        let pointer = shell.pointer;
                        shell.tick_wheel(pointer);
                        shell.controller.ask_about_updates();
                        let conflict_was_open = shell.controller.state.conflict.is_some();
                        let close_window = shell.controller.receive();
                        if close_window {
                            window.remove_window();
                        } else if !conflict_was_open && shell.controller.state.conflict.is_some() {
                            shell.remember_conflict_focus();
                        }
                        if shell.controller.state.cut_pending.is_some() {
                            shell.controller.cut_landed();
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            })
            .detach();
        Self {
            controller,
            filter,
            rename_input,
            password,
            output_name,
            add_password,
            name_input,
            focus_handle: cx.focus_handle(),
            list_focus: cx.focus_handle(),
            list_scroll,
            table,
            dialog: None,
            open_modal: None,
            delete_trigger_focus: cx.focus_handle(),
            drop_trigger_focus: cx.focus_handle(),
            conflict_trigger_focus: cx.focus_handle(),
            dialog_return_focus: None,
            modal_seen: None,
            dialog_primary_focus: cx.focus_handle().tab_stop(true),
            dialog_cancel_focus: cx.focus_handle().tab_stop(true),
            add_start_focus: cx.focus_handle().tab_stop(true),
            settings_focus: SettingsControl::ALL
                .iter()
                .map(|_| cx.focus_handle().tab_stop(true))
                .collect(),
            band: None,
            wheel: None,
            pointer: point(px(0.), px(0.)),
            carrying: false,
            row_count: 0,
            resizing: None,
            name_value: String::new(),
            viewer_focus: (0..3).map(|_| cx.focus_handle().tab_stop(true)).collect(),
            viewer_scroll: UniformListScrollHandle::new(),
            viewer_image: None,
            folders,
            folders_key: None,
            drop_paths: Vec::new(),
        }
    }

    /// Grow the kit's tree from the archive's folders, and put the mark on the
    /// folder the list is showing.
    ///
    /// The items are only rebuilt when the archive is a different one: they
    /// carry which branches are open, and handing the tree a fresh set every
    /// frame would shut the lot on every repaint. Revealing the current folder
    /// opens the branch that leads to it, so an archive opened three levels
    /// down does not present a closed tree to re-walk by hand.
    fn sync_folders(&mut self, cx: &mut Context<Self>) {
        let key = (
            self.controller.state.archive.clone(),
            self.controller.state.entries.len(),
        );
        if self.folders_key.as_ref() != Some(&key) {
            self.folders_key = Some(key);
            let root = TreeItem::new("", self.controller.s().archive_root)
                .expanded(true)
                .children(folder_items(&self.controller.state.folders, ""));
            self.folders
                .update(cx, |state, cx| state.set_items(vec![root], cx));
        }
        let current: gpui::SharedString = self.controller.state.current_dir.clone().into();
        self.folders.update(cx, |state, cx| {
            if state.selected_item().map(|item| item.id.clone()) == Some(current.clone()) {
                return;
            }
            state.reveal_item(&current, ScrollStrategy::Top, cx);
            let at = state.index_of(&current);
            state.set_selected_index(at, cx);
        });
    }

    /// The archive's folders, down the left edge.
    ///
    /// Breadcrumbs say where you are; this says what else there is. A deep
    /// archive was previously only navigable by descending one double-click at
    /// a time and reversing back out.
    fn sidebar(&mut self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        self.sync_folders(cx);
        let folders = self.controller.s().archive_folders;
        div()
            .id("archive-folders")
            .w(px(224.))
            .flex_none()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .p_2()
            .role(Role::Tree)
            .aria_label(folders)
            .child(tree(&self.folders, |_, entry, selected, _, _| {
                let open = entry.is_expanded();
                let icon = if entry.is_root() {
                    IconName::Inbox
                } else if open {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                };
                // The kit's tree draws no disclosure mark of its own, and a
                // branch with nothing to say it has one is a branch nobody
                // opens.
                let chevron = if entry.is_folder() {
                    Some(Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    }))
                } else {
                    None
                };
                ListItem::new(entry.item().id.clone())
                    .pl(px(4. + 12. * entry.depth() as f32))
                    .px_1()
                    .selected(selected)
                    .role(Role::TreeItem)
                    .aria_label(entry.item().label.clone())
                    .aria_level(entry.depth() + 1)
                    .aria_selected(selected)
                    .aria_expanded(open)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .w(px(14.))
                                    .flex_none()
                                    .children(chevron.map(|icon| icon.small())),
                            )
                            .child(Icon::new(icon).small())
                            .child(div().flex_1().truncate().child(entry.item().label.clone())),
                    )
            }))
    }

    fn begin_dialog(&mut self, kind: DialogKind, cx: &mut Context<Self>) {
        if self.dialog.is_some() || self.controller.state.busy || self.modal_kind().is_some() {
            return;
        }
        if matches!(kind, DialogKind::Extract { .. })
            && (self.controller.state.archive.is_none()
                || (matches!(kind, DialogKind::Extract { only_checked: true })
                    && !self.controller.state.checked.iter().any(|checked| *checked)))
        {
            return;
        }
        let (tx, rx) = channel();
        self.dialog = Some(rx);
        std::thread::spawn(move || {
            let result = match kind {
                DialogKind::Open => DialogResult::Open(
                    rfd::FileDialog::new()
                        .add_filter("Archives", &["zip", "tar", "gz", "tgz"])
                        .pick_file(),
                ),
                DialogKind::Compress => DialogResult::Compress(rfd::FileDialog::new().pick_files()),
                DialogKind::Extract { only_checked } => DialogResult::Extract {
                    only_checked,
                    destination: rfd::FileDialog::new().pick_folder(),
                },
                DialogKind::AddFiles => DialogResult::AddFiles(rfd::FileDialog::new().pick_files()),
                DialogKind::SaveCopy { name, directory } => DialogResult::SaveCopy(
                    rfd::FileDialog::new()
                        .set_file_name(&name)
                        .set_directory(&directory)
                        .save_file(),
                ),
            };
            let _ = tx.send(result);
        });
        cx.notify();
    }

    fn poll_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let Ok(result) = dialog.try_recv() else {
            return;
        };
        self.dialog = None;
        match result {
            DialogResult::Open(Some(path)) => self.controller.dispatch(AppAction::Open(path)),
            DialogResult::Compress(Some(paths)) if !paths.is_empty() => {
                self.controller.dispatch(AppAction::PrepareCompress(paths))
            }
            DialogResult::Extract {
                only_checked,
                destination: Some(dest),
            } => self
                .controller
                .dispatch(AppAction::ExtractTo { only_checked, dest }),
            DialogResult::AddFiles(Some(paths)) if !paths.is_empty() => {
                self.controller.dispatch(AppAction::Add(paths))
            }
            DialogResult::SaveCopy(Some(dest)) => {
                if let Some(archive) = self.controller.state.archive.clone() {
                    // Copying an archive over itself is not a backup, it is a
                    // truncation.
                    if dest != archive {
                        self.controller
                            .dispatch(AppAction::Run(Job::CopyTo { archive, dest }));
                    }
                }
            }
            _ => {}
        }
        if let Some(return_focus) = self.dialog_return_focus.take() {
            window.on_next_frame(move |window, cx| window.focus(&return_focus, cx));
        }
        cx.notify();
    }

    fn background_blocked(&self) -> bool {
        !background_event_allowed(self.modal_kind().is_some(), self.dialog.is_some())
    }

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_kind().is_some() || self.dialog.is_some() {
            cx.stop_propagation();
            return;
        }
        let handle = self.filter.read(cx).focus_handle(cx).clone();
        window.focus(&handle, cx);
    }

    fn route_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// A word that can be pressed.
    ///
    /// Ghost rather than outlined: a row of outlined boxes reads as seven
    /// competing things, and the same row without outlines reads as one
    /// toolbar. The kit's own hover and disabled treatment says the rest.
    fn button(
        id: impl Into<ElementId>,
        label: impl Into<gpui::SharedString>,
        accessible_name: String,
        enabled: bool,
    ) -> Button {
        Button::new(id)
            .label(label)
            .accessibility_label(accessible_name)
            .ghost()
            .compact()
            .text_xs()
            .disabled(!enabled)
    }

    /// A square button carrying one of the kit's icons instead of a word.
    ///
    /// Only for the controls whose meaning is a direction -- back, forward, up.
    /// A named action stays a word, because an icon that needs a tooltip to be
    /// understood has cost a word and bought nothing. The accessible name is
    /// still the word, so nothing changes for a screen reader.
    fn icon_button(
        id: impl Into<ElementId>,
        icon: IconName,
        accessible_name: String,
        enabled: bool,
    ) -> Button {
        Button::new(id)
            .icon(Icon::new(icon).size_4())
            .accessibility_label(accessible_name.clone())
            .tooltip(accessible_name)
            .ghost()
            .compact()
            .disabled(!enabled)
    }

    fn popup_action(
        owner: WeakEntity<GpuiShell>,
        label: impl Into<gpui::SharedString>,
        action: OverflowAction,
    ) -> PopupMenuItem {
        PopupMenuItem::new(label).on_click(move |_, _, cx| {
            let _ = owner.update(cx, |this, cx| {
                this.overflow_action(action, cx);
                cx.notify();
            });
        })
    }

    /// The answer a modal gets when it is dismissed rather than answered.
    ///
    /// One map for every way out -- escape, the kit's close button, its
    /// backdrop -- because a dismissal that does not clear the state leaves
    /// the worker waiting on an answer that is never coming.
    fn cancel_modal(&mut self, kind: ModalKind) {
        match kind {
            ModalKind::Password => self.controller.dispatch(AppAction::CancelPassword),
            ModalKind::Conflict => self.answer_conflict(Answer::Cancel),
            ModalKind::Delete => self.controller.dispatch(AppAction::ConfirmDelete(false)),
            ModalKind::Drop => self
                .controller
                .dispatch(AppAction::AnswerDrop(DropChoice::Cancel)),
            ModalKind::Add => self.controller.state.view = View::Browse,
            ModalKind::Viewer => self.controller.state.viewing = None,
            ModalKind::NewFolder | ModalKind::Mask => self.close_name(),
            ModalKind::DefaultPassword => {
                self.controller.state.asking_default_password = false;
                self.controller.state.password_input.clear();
            }
            ModalKind::Settings => self.controller.state.show_settings = false,
            ModalKind::Shortcuts => self.controller.state.show_shortcuts = false,
        }
    }

    /// The modals that already draw with the kit's `Dialog`. The rest still
    /// render as overlays in `dialogs`, so the migration lands one kind at a
    /// time and never draws both for the same modal.
    fn kit_dialog(kind: ModalKind) -> bool {
        matches!(
            kind,
            ModalKind::Delete
                | ModalKind::Conflict
                | ModalKind::Drop
                | ModalKind::Shortcuts
                | ModalKind::Password
                | ModalKind::DefaultPassword
                | ModalKind::NewFolder
                | ModalKind::Mask
                | ModalKind::Settings
                | ModalKind::Viewer
                | ModalKind::Add
        )
    }

    /// The field a dialog opens on, for the ones that ask for text.
    ///
    /// The kit focuses the dialog itself, which would leave the caret nowhere
    /// and the first keystroke lost.
    fn modal_text_field(&self, kind: ModalKind, cx: &App) -> Option<FocusHandle> {
        match kind {
            ModalKind::Password | ModalKind::DefaultPassword => {
                Some(self.password.read(cx).focus_handle(cx).clone())
            }
            ModalKind::NewFolder | ModalKind::Mask => {
                Some(self.name_input.read(cx).focus_handle(cx).clone())
            }
            _ => None,
        }
    }

    /// Reconcile the derived modal with the kit's dialog stack.
    ///
    /// Must run between frames: opening a dialog notifies `Root`, and doing
    /// that inside `render` would repaint from inside a repaint.
    fn sync_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let want = self.modal_kind().filter(|kind| Self::kit_dialog(*kind));
        if want == self.open_modal {
            return;
        }
        if self.open_modal.is_some() {
            window.close_all_dialogs(cx);
        }
        if let Some(kind) = want {
            // Park the keyboard on the shell before the dialog exists. The row
            // that had the focus drops out of the accessibility tree as soon as
            // the modal opens, and leaving the focus on it until the kit
            // focuses its own dialog a frame later aborts the process the next
            // time something marks an active descendant.
            let neutral = self.focus_handle.clone();
            window.focus(&neutral, cx);
            let shell = cx.weak_entity();
            window.open_dialog(cx, move |dialog, window, cx| {
                let Some(shell) = shell.upgrade() else {
                    return dialog;
                };
                build_dialog(kind, &shell, dialog, window, cx)
            });
            // A frame later, so the field is in the tree to be focused.
            if let Some(field) = self.modal_text_field(kind, cx) {
                window.on_next_frame(move |window, cx| window.focus(&field, cx));
            }
        }
        self.open_modal = want;
    }

    fn modal_kind(&self) -> Option<ModalKind> {
        if self.controller.state.waiting_on_password.is_some() {
            Some(ModalKind::Password)
        } else if self.controller.state.conflict.is_some() {
            Some(ModalKind::Conflict)
        } else if self.controller.state.confirm_delete.is_some() {
            Some(ModalKind::Delete)
        } else if self.controller.state.confirm_drop.is_some() {
            Some(ModalKind::Drop)
        } else if matches!(self.controller.state.view, View::Add) {
            Some(ModalKind::Add)
        } else if self.controller.state.viewing.is_some() {
            Some(ModalKind::Viewer)
        } else if self.controller.state.asking_folder {
            Some(ModalKind::NewFolder)
        } else if self.controller.state.picking_group.is_some() {
            Some(ModalKind::Mask)
        } else if self.controller.state.asking_default_password {
            Some(ModalKind::DefaultPassword)
        } else if self.controller.state.show_settings {
            Some(ModalKind::Settings)
        } else if self.controller.state.show_shortcuts {
            Some(ModalKind::Shortcuts)
        } else {
            None
        }
    }

    fn remember_background_focus(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.modal_seen.is_none() && self.modal_kind().is_none() && self.dialog.is_none() {
            if let Some(focus) = window.focused(cx) {
                self.dialog_return_focus = Some(focus);
            }
        }
    }

    fn remember_conflict_focus(&mut self) {
        self.dialog_return_focus = Some(self.conflict_trigger_focus.clone());
    }

    fn sync_modal_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.modal_kind();
        if current == self.modal_seen {
            return;
        }
        // A kit dialog focuses itself when it opens and restores the previous
        // focus when it closes; steering focus at it would fight that.
        if current.is_some_and(Self::kit_dialog) {
            self.modal_seen = current;
            return;
        }
        if current == Some(ModalKind::Conflict) {
            self.remember_conflict_focus();
        }
        self.modal_seen = current;
        let target = match current {
            Some(ModalKind::Password) => self.password.read(cx).focus_handle(cx).clone(),
            Some(ModalKind::Conflict | ModalKind::Delete | ModalKind::Drop) => {
                self.dialog_primary_focus.clone()
            }
            Some(ModalKind::Add) => self.output_name.read(cx).focus_handle(cx).clone(),
            Some(ModalKind::Viewer) => self.viewer_focus[0].clone(),
            Some(ModalKind::NewFolder | ModalKind::Mask) => {
                self.name_input.read(cx).focus_handle(cx).clone()
            }
            Some(ModalKind::DefaultPassword) => self.password.read(cx).focus_handle(cx).clone(),
            Some(ModalKind::Settings) => self.settings_focus[0].clone(),
            Some(ModalKind::Shortcuts) => self.dialog_cancel_focus.clone(),
            None => match self.dialog_return_focus.clone() {
                Some(focus) => focus,
                None => return,
            },
        };
        window.on_next_frame(move |window, cx| window.focus(&target, cx));
    }

    fn answer_conflict(&mut self, answer: Answer) {
        self.remember_conflict_focus();
        self.controller.dispatch(AppAction::AnswerConflict(answer));
    }

    /// One place where a settings control does its work, so the click handler
    /// and Enter cannot drift apart.
    fn settings_activate(
        &mut self,
        control: SettingsControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match control {
            SettingsControl::LangSystem => {
                self.controller.dispatch(AppAction::SetLanguage(None));
            }
            SettingsControl::LangEn => {
                self.controller
                    .dispatch(AppAction::SetLanguage(Some(super::Lang::En)));
            }
            SettingsControl::LangEs => {
                self.controller
                    .dispatch(AppAction::SetLanguage(Some(super::Lang::Es)));
            }
            SettingsControl::ThemeSystem => {
                self.set_theme(super::ThemePreference::System, window, cx)
            }
            SettingsControl::ThemeLight => {
                self.set_theme(super::ThemePreference::Light, window, cx)
            }
            SettingsControl::ThemeDark => self.set_theme(super::ThemePreference::Dark, window, cx),
            SettingsControl::Subfolder => {
                self.controller.state.into_subfolder = !self.controller.state.into_subfolder;
            }
        }
    }

    /// The preference is stored by the controller and painted by GPUI, and both
    /// have to happen: saving without repainting leaves the window in the old
    /// theme until it is restarted.
    fn set_theme(
        &mut self,
        theme: super::ThemePreference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.controller.dispatch(AppAction::SetTheme(theme));
        gpui_theme::apply(theme, Some(window), cx);
    }

    /// The entries of the overflow menu that change the archive itself.
    fn overflow_action(&mut self, action: OverflowAction, cx: &mut Context<Self>) {
        match action {
            OverflowAction::Release => self.controller.start_update(),
            OverflowAction::AddFiles => self.begin_dialog(DialogKind::AddFiles, cx),
            // Asked for in a box rather than made as "New folder" and renamed
            // after: making it rewrites the whole archive, and doing that twice
            // for one folder would be silly.
            OverflowAction::NewFolder => {
                self.name_value.clear();
                self.controller.state.asking_folder = true;
            }
            OverflowAction::Undo => self.controller.undo_last(),
            OverflowAction::SaveCopy => {
                let Some(archive) = self.controller.state.archive.clone() else {
                    return;
                };
                let name = archive
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                let directory = archive
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                self.begin_dialog(DialogKind::SaveCopy { name, directory }, cx);
            }
            OverflowAction::DefaultPassword => {
                self.controller.state.password_input.clear();
                self.controller.state.asking_default_password = true;
            }
        }
    }

    /// An entry out of the archive, looked at without taking it out: as text,
    /// as hex, or as the picture it is.
    ///
    /// The text and the hex go through `uniform_list`, so only the lines on
    /// screen are laid out and a log of a million lines opens as fast as a
    /// note of three.
    fn viewer_picture(
        &mut self,
        name: &str,
        bytes: &std::sync::Arc<[u8]>,
    ) -> Option<std::sync::Arc<gpui::Image>> {
        if let Some((cached, image)) = &self.viewer_image {
            if cached == name {
                return Some(image.clone());
            }
        }
        let format = match image::guess_format(bytes).ok()? {
            image::ImageFormat::Png => gpui::ImageFormat::Png,
            image::ImageFormat::Jpeg => gpui::ImageFormat::Jpeg,
            image::ImageFormat::Gif => gpui::ImageFormat::Gif,
            image::ImageFormat::Bmp => gpui::ImageFormat::Bmp,
            image::ImageFormat::WebP => gpui::ImageFormat::Webp,
            // The `image` crate is built with five decoders on purpose; any
            // other tag here is a format this build cannot read anyway.
            _ => return None,
        };
        let image = std::sync::Arc::new(gpui::Image::from_bytes(format, bytes.to_vec()));
        self.viewer_image = Some((name.to_string(), image.clone()));
        Some(image)
    }

    /// Shuts whichever text dialog is open and forgets what was typed in it.
    fn close_name(&mut self) {
        self.controller.state.asking_folder = false;
        self.controller.state.renaming = None;
        self.controller.state.picking_group = None;
        self.name_value.clear();
    }

    /// Start renaming a row in place.
    ///
    /// One entry point for the shortcut and the row menu, so the field can
    /// never be shown without the state that says which row it belongs to.
    fn begin_rename(&mut self, row: &super::Row, window: &mut Window, cx: &mut Context<Self>) {
        if self.controller.state.format != super::Format::Zip {
            return;
        }
        let label = row.label.clone();
        self.controller.state.renaming = Some((row.path.clone(), label.clone()));
        self.rename_input.update(cx, |state, cx| {
            state.set_value(label, window, cx);
            // Renaming usually replaces the name rather than appends to it, so
            // the old one starts out selected and the first keystroke wipes it.
            state.select_all(window, cx);
        });
        let field = self.rename_input.read(cx).focus_handle(cx);
        window.focus(&field, cx);
        cx.notify();
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, _)) = self.controller.state.renaming.clone() else {
            return;
        };
        let name = self.rename_input.read(cx).value().trim().to_string();
        let rows = self.controller.visible_rows();
        self.controller.state.renaming = None;
        // Back to the list, or the keyboard would be left on a field that is
        // no longer drawn.
        let list = self.list_focus.clone();
        window.focus(&list, cx);
        self.controller.rename_to(&rows, &path, &name);
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.controller.state.renaming.take().is_some() {
            cx.notify();
        }
    }

    /// Acts on what the shared text field holds, according to which dialog
    /// asked for it.
    fn confirm_name(&mut self, kind: ModalKind, cx: &mut Context<Self>) {
        let s = self.controller.s();
        let name = self.name_value.trim().to_string();
        match kind {
            ModalKind::NewFolder => {
                let Some(archive) = self.controller.state.archive.clone() else {
                    self.close_name();
                    return;
                };
                // The same rules a rename lives by: a name is a name and not a
                // path, and nothing here is called that already.
                if name.is_empty() || name.contains('/') || name.contains('\\') {
                    self.controller.state.notice = s.bad_name.to_string();
                    self.controller.state.error = true;
                    self.close_name();
                    return;
                }
                if self
                    .controller
                    .visible_rows()
                    .iter()
                    .any(|row| row.label.eq_ignore_ascii_case(&name))
                {
                    self.controller.state.notice = fill(s.name_taken, &[("name", &name)]);
                    self.controller.state.error = true;
                    self.close_name();
                    return;
                }
                let full = format!("{}{name}/", self.controller.state.current_dir);
                self.close_name();
                self.controller.dispatch(AppAction::Run(Job::NewFolder {
                    archive,
                    name: full,
                    password: self.controller.state.archive_password.clone(),
                }));
            }
            ModalKind::Mask => {
                // WinRAR's keypad plus and minus: a mask picks or drops every
                // name in this folder that matches it, in one go.
                let adding = self.controller.state.picking_group.unwrap_or(true);
                let rows = self.controller.visible_rows();
                self.close_name();
                if name.is_empty() {
                    return;
                }
                for row in &rows {
                    if super::matches_mask(&name, &row.label) {
                        self.controller.dispatch(AppAction::SetChecked {
                            row: row.clone(),
                            value: adding,
                        });
                    }
                }
            }
            _ => self.close_name(),
        }
        cx.notify();
    }

    /// The password to try on anything that asks for one, so a folder full of
    /// archives locked with the same word is opened once and not fifteen
    /// times. In memory and nowhere else: a password in plain text beside the
    /// theme and the column widths is how an encrypted archive stops being
    /// encrypted.
    fn keep_default_password(&mut self) {
        let given = std::mem::take(&mut self.controller.state.password_input);
        self.controller.state.default_password = (!given.is_empty()).then_some(given);
        self.controller.state.asking_default_password = false;
    }

    fn forget_default_password(&mut self) {
        let s = self.controller.s();
        self.controller.state.default_password = None;
        self.controller.state.asking_default_password = false;
        self.controller.state.password_input.clear();
        self.controller.state.notice = s.password_forgotten.to_string();
        self.controller.state.error = false;
    }

    fn start_add(&mut self, cx: &mut Context<Self>) {
        let Some(first) = self.controller.state.pending_inputs.first() else {
            return;
        };
        self.dialog_return_focus = Some(self.add_start_focus.clone());
        let dir = first.parent().map(PathBuf::from).unwrap_or_default();
        let name = {
            let name = self.controller.state.output_name.trim();
            if name.is_empty() {
                format!("archive.{}", self.controller.state.format.extension())
            } else {
                name.to_string()
            }
        };
        self.controller.dispatch(AppAction::Run(Job::Compress {
            out: dir.join(name),
            inputs: self.controller.state.pending_inputs.clone(),
            format: self.controller.state.format,
            codec: self.controller.state.codec,
            level: self.controller.state.level,
            password: (self.controller.state.format == super::Format::Zip
                && !self.controller.state.add_password.is_empty())
            .then(|| self.controller.state.add_password.clone()),
        }));
        cx.notify();
    }

    /// Runs what the row menu was asked for and shuts it.
    fn row_action(
        &mut self,
        action: RowAction,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.background_idle() {
            return;
        }
        let rows = self.controller.visible_rows();
        let row = rows.get(index).cloned();
        match action {
            RowAction::Open => {
                if let Some(row) = row {
                    if row.is_dir {
                        self.controller.dispatch(AppAction::Navigate(row.path));
                        self.route_changed(cx);
                    } else if let Some(entry) = row.entry {
                        self.controller.dispatch(AppAction::OpenFile(entry));
                    }
                }
            }
            RowAction::ExtractSelection => {
                self.begin_dialog(DialogKind::Extract { only_checked: true }, cx)
            }
            RowAction::ExtractHere => self.controller.extract_here(),
            // Only a file has anything to look at. A folder is a prefix on
            // some names, not a thing with bytes.
            RowAction::View => {
                if let Some(entry) = row.as_ref().and_then(|row| row.entry) {
                    self.controller.view_entry(entry);
                }
            }
            RowAction::TestSelection => {
                let names = self.controller.selected_names();
                if let Some(archive) = self.controller.state.archive.clone() {
                    self.controller.dispatch(AppAction::Run(Job::Test {
                        archive,
                        only: (!names.is_empty()).then(|| names.into_iter().collect()),
                    }));
                }
            }
            // Only a zip can be written to in place, so anywhere else this is
            // left out rather than offered and refused.
            RowAction::Rename => {
                if let Some(row) = row {
                    self.begin_rename(&row, window, cx);
                }
            }
            RowAction::Delete => {
                self.dialog_return_focus = Some(self.delete_trigger_focus.clone());
                self.controller.dispatch(AppAction::RequestDelete);
            }
            RowAction::Copy => self.dispatch_clipboard(false, window, cx),
            RowAction::Cut => self.dispatch_clipboard(true, window, cx),
            RowAction::Paste => self.dispatch_paste(window, cx),
            RowAction::CopyNames => {
                let names = self.controller.selected_names();
                if !names.is_empty() {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(names.join("\r\n")));
                }
            }
            RowAction::SelectAll => self.controller.dispatch(AppAction::SelectAllVisible),
        }
        cx.notify();
    }

    /// The list's geometry, or nothing before it has been laid out once.
    fn list_view(&self, count: usize) -> Option<ListView> {
        let state = self.list_scroll.0.borrow();
        let row = row_height(f32::from(state.last_item_size?.contents.height), count)?;
        let bounds = state.base_handle.bounds();
        let top = f32::from(bounds.origin.y);
        let left = f32::from(bounds.origin.x);
        let height = f32::from(bounds.size.height);
        if height <= 0.0 {
            return None;
        }
        Some(ListView {
            top,
            bottom: top + height,
            left,
            right: left + f32::from(bounds.size.width),
            row,
            offset: -f32::from(state.base_handle.offset().y),
            reach: f32::from(state.base_handle.max_offset().y),
        })
    }

    /// Whether the pointer has left the list. What is being carried goes to the
    /// system from here; inside, it is still a move between folders.
    fn left_the_list(&self, at: gpui::Point<gpui::Pixels>) -> bool {
        let Some(view) = self.list_view(self.row_count) else {
            return false;
        };
        let (x, y) = (f32::from(at.x), f32::from(at.y));
        y < view.top || y > view.bottom || x < view.left || x > view.right
    }

    fn scroll_to(&self, view: &ListView, offset: f32) {
        let state = self.list_scroll.0.borrow();
        let x = state.base_handle.offset().x;
        state
            .base_handle
            .set_offset(point(x, px(-offset.clamp(0.0, view.reach))));
    }

    /// The row under a point, or nothing if that is past the last one.
    fn row_under(view: &ListView, y: f32, len: usize) -> Option<usize> {
        let local = y - view.top + view.offset;
        if local < 0.0 || view.row <= 0.0 {
            return None;
        }
        let index = (local / view.row) as usize;
        (index < len).then_some(index)
    }

    /// Pressing the left button inside the list, which is where a band begins.
    ///
    /// Pressing on a row that is already picked and pulling is how you take the
    /// selection somewhere else, so that one is left to the drag; pressing
    /// anywhere else and pulling draws a new band. That is the rule in the
    /// Explorer, and it is the only one that lets both gestures share a button.
    fn begin_band(&mut self, at: gpui::Point<gpui::Pixels>, secondary: bool, shift: bool) {
        if !self.background_idle() || shift {
            return;
        }
        let rows = self.controller.visible_rows();
        let Some(view) = self.list_view(rows.len()) else {
            return;
        };
        let (x, y) = (f32::from(at.x), f32::from(at.y));
        if y < view.top || y > view.bottom || x < view.left || x > view.right {
            return;
        }
        let anchor = Self::row_under(&view, y, rows.len());
        if let Some(index) = anchor {
            if !secondary && self.controller.is_checked(&rows[index]) {
                return;
            }
        }
        self.band = Some(Band {
            origin: at,
            rows: rows.clone(),
            // Begun in the empty space under the list, where there is no row to
            // hang the band on: it still picks everything between there and
            // wherever it goes.
            anchor: anchor.unwrap_or(rows.len().saturating_sub(1)),
            base: if secondary {
                self.controller.state.checked.clone()
            } else {
                vec![false; self.controller.state.checked.len()]
            },
            head: at,
            live: false,
        });
    }

    /// The band following the pointer, and the list following it past an edge.
    fn drag_band(&mut self, at: gpui::Point<gpui::Pixels>) -> bool {
        // Taken out and put back, so the rows frozen inside it can be read
        // while the controller is being written to.
        let Some(mut band) = self.band.take() else {
            return false;
        };
        band.head = at;
        let travelled = (f32::from(at.x) - f32::from(band.origin.x))
            .hypot(f32::from(at.y) - f32::from(band.origin.y));
        if !band.live && travelled < DRAG_SLOP {
            self.band = Some(band);
            return false;
        }
        band.live = true;
        let rows = &band.rows;
        let Some(view) = self.list_view(rows.len()) else {
            self.band = Some(band);
            return false;
        };
        let y = f32::from(at.y);
        let head = Self::row_under(&view, y.clamp(view.top, view.bottom), rows.len())
            .unwrap_or(rows.len() - 1);
        let (lo, hi) = if band.anchor <= head {
            (band.anchor, head)
        } else {
            (head, band.anchor)
        };
        self.controller.state.checked.clone_from(&band.base);
        for row in &rows[lo..=hi.min(rows.len() - 1)] {
            self.controller.set_checked(row, true);
        }
        // Past either edge the list follows the pointer, the way the Explorer
        // does it. Without this a selection could never be longer than the
        // window, because dragging no longer scrolls.
        let over = if y < view.top {
            y - view.top
        } else if y > view.bottom {
            y - view.bottom
        } else {
            0.0
        };
        if over != 0.0 {
            self.scroll_to(&view, view.offset + over.clamp(-24.0, 24.0));
        }
        self.band = Some(band);
        true
    }

    /// Drops the anchor, or picks it back up.
    fn toggle_wheel(&mut self, at: gpui::Point<gpui::Pixels>) {
        if self.wheel.is_some() || !self.background_idle() {
            self.wheel = None;
            return;
        }
        let Some(view) = self.list_view(self.row_count) else {
            return;
        };
        let (x, y) = (f32::from(at.x), f32::from(at.y));
        if y < view.top || y > view.bottom || x < view.left || x > view.right {
            return;
        }
        self.wheel = Some(WheelPan {
            anchor: at,
            moved: false,
        });
    }

    /// One step of the list running towards the pointer, for the tick that
    /// keeps a gesture moving while the hand is still.
    ///
    /// ponytail: driven by the shell's existing 100 ms poll rather than by a
    /// frame callback, so the run is ten steps a second. Move it onto a frame
    /// request if the stepping ever reads as stutter.
    fn tick_wheel(&mut self, at: gpui::Point<gpui::Pixels>) -> bool {
        let Some(wheel) = &mut self.wheel else {
            return false;
        };
        let speed = super::wheel_speed(f32::from(at.y) - f32::from(wheel.anchor.y));
        wheel.moved |= speed != 0.0;
        if speed == 0.0 {
            return false;
        }
        let Some(view) = self.list_view(self.row_count) else {
            return false;
        };
        self.scroll_to(&view, view.offset + speed * 0.1);
        true
    }

    /// Whether the keyboard is inside a text field.
    ///
    /// A bare key means something different there -- F5 in a filter box is a
    /// key, not a command -- so the shortcuts that carry no modifier stand
    /// aside while one has the focus.
    fn typing(&self, window: &Window, cx: &App) -> bool {
        self.filter.read(cx).focus_handle(cx).is_focused(window)
            || self
                .rename_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
            || [
                &self.password,
                &self.output_name,
                &self.add_password,
                &self.name_input,
            ]
            .iter()
            .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// The shortcuts that belong to the window rather than to the list.
    ///
    /// One handler rather than a dozen `actions!` entries and twice as many
    /// key bindings: every one of these is the same shape -- a key, a guard,
    /// and an action already written -- and a table of them reads in one go.
    fn global_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.background_blocked() {
            return;
        }
        let typing = self.typing(window, cx);
        let Some(shortcut) = shortcut_for(
            event.keystroke.modifiers.secondary(),
            event.keystroke.modifiers.shift,
            event.keystroke.modifiers.alt,
            &event.keystroke.key.to_ascii_lowercase(),
            typing,
        ) else {
            return;
        };
        let archive = self.controller.state.archive.clone();
        let idle = self.background_idle();
        // The shortcuts window is the one that answers while an archive is
        // being read, because it is the one that says what to press.
        if !idle && shortcut != Shortcut::Shortcuts {
            return;
        }
        match shortcut {
            Shortcut::Open => self.begin_dialog(DialogKind::Open, cx),
            Shortcut::Compress => self.begin_dialog(DialogKind::Compress, cx),
            Shortcut::ExtractAll if archive.is_some() => self.begin_dialog(
                DialogKind::Extract {
                    only_checked: false,
                },
                cx,
            ),
            // Everything out, beside the archive, without asking where: the
            // folder the archive is in is where an extraction goes nine times
            // out of ten, and the whole point is that it is one keystroke.
            Shortcut::ExtractHere if archive.is_some() => self.controller.extract_here(),
            // Verifying an archive was reachable from the shell menu and the
            // command line and from nowhere inside the window.
            Shortcut::Test => {
                if let Some(archive) = archive {
                    self.controller.dispatch(AppAction::Run(Job::Test {
                        archive,
                        only: None,
                    }));
                }
            }
            Shortcut::Refresh => {
                if let Some(path) = archive {
                    // Rereading must not ask again for the password of an
                    // archive that has already been unlocked.
                    let keep = self.controller.state.archive_password.clone();
                    self.controller.dispatch(AppAction::Open(path));
                    self.controller.state.archive_password = keep;
                }
            }
            Shortcut::Invert => self.controller.dispatch(AppAction::InvertVisible),
            // Escape backs out of the innermost thing there is to back out of,
            // and while the list is running itself that is the running.
            Shortcut::ClearSelection => {
                if self.wheel.take().is_none() {
                    self.controller.dispatch(AppAction::ClearSelection);
                }
            }
            // The names as text, which is all a desktop without a file
            // clipboard can be given, and useful on one that has it too.
            Shortcut::CopyNames => {
                let names = self.controller.selected_names();
                if !names.is_empty() {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(names.join("\r\n")));
                }
            }
            Shortcut::Shortcuts => {
                self.controller.state.show_shortcuts = !self.controller.state.show_shortcuts;
            }
            // One step back from the last change to the archive, which is the
            // step anybody wants: the one they just took by mistake.
            Shortcut::Undo if self.controller.state.undo.is_some() => self.controller.undo_last(),
            Shortcut::DefaultPassword => {
                self.controller.state.password_input.clear();
                self.controller.state.asking_default_password = true;
            }
            Shortcut::Rename if archive.is_some() => {
                let cursor = self.controller.state.cursor;
                let rows = self.controller.visible_rows();
                match cursor.and_then(|index| rows.get(index)).cloned() {
                    Some(row) => self.begin_rename(&row, window, cx),
                    None => return,
                }
            }
            Shortcut::PickGroup(adding) if archive.is_some() => {
                self.name_value.clear();
                self.controller.state.picking_group = Some(adding);
            }
            Shortcut::View if archive.is_some() => {
                let cursor = self.controller.state.cursor;
                let rows = self.controller.visible_rows();
                match cursor
                    .and_then(|index| rows.get(index))
                    .and_then(|row| row.entry)
                {
                    Some(entry) => self.controller.view_entry(entry),
                    None => return,
                }
            }
            Shortcut::ExtractAll
            | Shortcut::ExtractHere
            | Shortcut::Undo
            | Shortcut::Rename
            | Shortcut::View
            | Shortcut::PickGroup(_) => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// The keys, and what each one does, in the two columns they are read in.
    fn background_idle(&self) -> bool {
        !self.controller.state.busy && self.modal_kind().is_none() && self.dialog.is_none()
    }

    fn menu_enabled(&self) -> bool {
        !self.controller.state.busy && self.modal_kind().is_none() && self.dialog.is_none()
    }

    fn selected_count(&self) -> usize {
        self.controller
            .state
            .checked
            .iter()
            .filter(|checked| **checked)
            .count()
    }

    fn can_copy_files(&self) -> bool {
        clipboard_action_allowed(
            clipboard::AVAILABLE,
            self.menu_enabled(),
            self.controller.state.archive.is_some(),
            self.selected_count(),
            true,
        )
    }

    fn can_paste_files(&self) -> bool {
        clipboard_action_allowed(
            clipboard::AVAILABLE,
            self.menu_enabled(),
            self.controller.state.archive.is_some(),
            self.selected_count(),
            false,
        )
    }

    fn clipboard_focus_is_safe(&self, window: &Window, cx: &mut Context<Self>) -> bool {
        !self.filter.read(cx).focus_handle(cx).is_focused(window) && self.modal_kind().is_none()
    }

    fn dispatch_clipboard(&mut self, cut: bool, window: &Window, cx: &mut Context<Self>) {
        if !self.clipboard_focus_is_safe(window, cx) || !self.menu_enabled() {
            return;
        }
        if self.controller.state.archive.is_none() || self.selected_count() == 0 {
            return;
        }
        if !clipboard::AVAILABLE {
            self.controller.state.notice =
                "File clipboard integration is available on Windows only.".into();
            self.controller.state.error = true;
            cx.notify();
            return;
        }
        self.controller.state.notice = if cut {
            "Preparing selected files to move…".into()
        } else {
            "Preparing selected files to copy…".into()
        };
        self.controller.state.error = false;
        self.controller.dispatch(AppAction::Copy { cut });
        cx.stop_propagation();
        cx.notify();
    }

    fn dispatch_paste(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.clipboard_focus_is_safe(window, cx) || !self.menu_enabled() {
            return;
        }
        if self.controller.state.archive.is_none() {
            return;
        }
        if !clipboard::AVAILABLE {
            self.controller.state.notice =
                "File clipboard integration is available on Windows only.".into();
            self.controller.state.error = true;
            cx.notify();
            return;
        }
        self.controller.dispatch(AppAction::Paste);
        cx.stop_propagation();
        cx.notify();
    }

    fn copy_files_action(&mut self, _: &CopyFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_clipboard(false, window, cx);
    }

    fn cut_files_action(&mut self, _: &CutFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_clipboard(true, window, cx);
    }

    fn paste_files_action(&mut self, _: &PasteFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_paste(window, cx);
    }

    fn status(&self) -> String {
        let s = self.controller.s();
        let state = &self.controller.state;
        if state.busy || (matches!(state.view, View::Running) && state.notice.is_empty()) {
            let progress = if state.total_count == 0 {
                s.working_word.to_string()
            } else {
                format!("{} / {}", state.done_count, state.total_count)
            };
            return format!("{} · {}", state.title, progress);
        }
        if !state.notice.is_empty() {
            return state.notice.clone();
        }
        if matches!(state.view, View::Add) {
            return format!("{}: {}", s.add_to_archive, state.pending_inputs.len());
        }
        if state.entries.is_empty() {
            return s.drop_here.into();
        }
        self.controller.summary()
    }

    fn crumbs(&self) -> Vec<(String, String)> {
        let Some(archive) = &self.controller.state.archive else {
            return Vec::new();
        };
        let root = archive
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut crumbs = vec![(root, String::new())];
        let mut walked = String::new();
        for part in self
            .controller
            .state
            .current_dir
            .split('/')
            .filter(|part| !part.is_empty())
        {
            walked.push_str(part);
            walked.push('/');
            crumbs.push((part.to_string(), walked.clone()));
        }
        crumbs
    }

    fn visible_crumb_indices(len: usize) -> (Vec<usize>, Vec<usize>) {
        if len <= 4 {
            return ((0..len).collect(), Vec::new());
        }
        let mut shown = vec![0];
        let hidden_end = len - 3;
        let hidden: Vec<usize> = (1..hidden_end).collect();
        shown.extend(hidden_end..len);
        (shown, hidden)
    }

    fn shown_columns(&self) -> Vec<SortColumn> {
        let columns = self.controller.state.settings.columns;
        std::iter::once(SortColumn::Name)
            .chain(
                Columns::ALL
                    .iter()
                    .map(|(column, _)| *column)
                    .filter(move |column| columns.on(*column)),
            )
            .collect()
    }

    /// Where a column's width lives in `Settings::widths`: the name first,
    /// then the ones that can be turned off, in the order of `Columns::ALL`.
    /// A column keeps its width while it is off, so turning one back on does
    /// not lose how it was set.
    fn column_slot(column: SortColumn) -> usize {
        Columns::ALL
            .iter()
            .position(|(candidate, _)| *candidate == column)
            .map_or(0, |index| index + 1)
    }

    /// Opening what a row stands for: a folder is walked into, a file is handed
    /// to whatever opens it.
    fn open_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.background_idle() {
            return;
        }
        let Some(row) = self.controller.visible_rows().get(index).cloned() else {
            return;
        };
        if row.is_dir {
            self.controller.dispatch(AppAction::Navigate(row.path));
            self.route_changed(cx);
        } else if let Some(entry) = row.entry {
            self.controller.dispatch(AppAction::OpenFile(entry));
        }
        cx.notify();
    }

    /// The widths the kit ended a pull with, kept where the rest of the window
    /// settings live so they survive the window.
    fn remember_widths(&mut self, widths: &[gpui::Pixels], cx: &mut Context<Self>) {
        for (column, width) in self.shown_columns().into_iter().zip(widths.iter()) {
            let slot = Self::column_slot(column);
            self.set_column_width(slot, f32::from(*width));
        }
        self.controller.state.settings.save();
        cx.notify();
    }

    /// Hand the table the frame it is about to draw.
    ///
    /// The delegate cannot read the shell while the shell is rendering, so what
    /// it needs is copied across first.
    fn sync_table(&mut self, rows: &[super::Row], cx: &mut Context<Self>) {
        let columns = self.shown_columns();
        let checked: Vec<bool> = rows
            .iter()
            .map(|row| self.controller.is_checked(row))
            .collect();
        let muted: Vec<bool> = rows
            .iter()
            .map(|row| {
                row.entry.is_some_and(|entry| {
                    self.controller
                        .state
                        .cut_names
                        .contains(&self.controller.state.entries[entry].name)
                })
            })
            .collect();
        let renaming = self
            .controller
            .state
            .renaming
            .as_ref()
            .and_then(|(path, _)| rows.iter().position(|row| row.path == *path));
        let widths = (0..Settings::default_widths().len())
            .map(|slot| {
                self.controller
                    .state
                    .settings
                    .widths
                    .get(slot)
                    .copied()
                    .unwrap_or_else(|| Settings::default_widths()[slot])
            })
            .collect();
        let cursor = self.controller.state.cursor;
        let order = self.controller.state.order;
        let strings = self.controller.s();
        let idle = self.background_idle();
        let writable = self.controller.state.format == super::Format::Zip;
        let rows = rows.to_vec();
        self.table.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            delegate.rows = rows;
            delegate.columns = columns;
            delegate.widths = widths;
            delegate.checked = checked;
            delegate.muted = muted;
            delegate.cursor = cursor;
            delegate.renaming = renaming;
            delegate.order = order;
            delegate.strings = strings;
            delegate.idle = idle;
            delegate.writable = writable;
            state.refresh(cx);
        });
    }

    fn set_column_width(&mut self, slot: usize, width: f32) {
        if self.controller.state.settings.widths.len() <= slot {
            self.controller.state.settings.widths = Settings::default_widths();
        }
        self.controller.state.settings.widths[slot] = width.max(Settings::least(slot));
    }

    fn kind_icon(kind: Kind) -> (IconName, u32) {
        match kind {
            Kind::Dir => (IconName::Folder, 0xF4B740),
            Kind::Image => (IconName::GalleryVerticalEnd, 0xC084FC),
            Kind::Text => (IconName::FileText, 0x60A5FA),
            Kind::Archive => (IconName::File, 0xFB923C),
            Kind::Audio => (IconName::File, 0xF472B6),
            Kind::Video => (IconName::Play, 0xF87171),
            Kind::Other => (IconName::File, 0x94A3B8),
        }
    }
}

/// What a row says under a column.
///
/// A free function rather than a method: the table's delegate draws the cells
/// and it has the strings but not the shell.
fn column_text(row: &super::Row, column: SortColumn, s: &'static Strings) -> String {
    {
        match column {
            SortColumn::Name => row.label.clone(),
            SortColumn::Size => human(row.size),
            SortColumn::Packed => human(row.packed),
            SortColumn::Method => {
                if row.is_dir {
                    format!("{} {}", row.count, s.items_word)
                } else if row.encrypted {
                    format!("AES-256 {}", row.method)
                } else {
                    row.method.to_string()
                }
            }
            SortColumn::Saved => format!("{:.0}%", saved_of(row) * 100.0),
            SortColumn::Modified => when(row.mtime),
            SortColumn::Created => when(row.created),
            SortColumn::Accessed => when(row.accessed),
            SortColumn::Attributes => super::attribute_letters(row.attributes),
            SortColumn::Crc => {
                if row.is_dir {
                    "—".to_string()
                } else {
                    format!("{:08X}", row.crc32)
                }
            }
            SortColumn::Type => arca_icons::cache_key(&row.label, row.is_dir),
            SortColumn::Path => super::folder_of(&row.path).to_string(),
        }
    }
}

impl GpuiShell {
    fn select_row(
        &mut self,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.background_blocked() || !event.standard_click() {
            return;
        }
        // A click is a drag of no distance. Anything further than that was a
        // band or a carry, and the row it started on is not being clicked.
        if let ClickEvent::Mouse(mouse) = event {
            let travelled = (f32::from(mouse.up.position.x) - f32::from(mouse.down.position.x))
                .hypot(f32::from(mouse.up.position.y) - f32::from(mouse.down.position.y));
            if travelled >= DRAG_SLOP {
                return;
            }
        }
        let rows = self.controller.visible_rows();
        let Some(target) = rows.get(index).cloned() else {
            return;
        };
        let modifiers = event.modifiers();
        if modifiers.shift {
            let from = self.controller.state.cursor.unwrap_or(index);
            let (lo, hi) = if from <= index {
                (from, index)
            } else {
                (index, from)
            };
            for row in &rows[lo..=hi] {
                self.controller.dispatch(AppAction::SetChecked {
                    row: row.clone(),
                    value: true,
                });
            }
        } else if modifiers.secondary() {
            let value = !self.controller.is_checked(&target);
            self.controller.dispatch(AppAction::SetChecked {
                row: target.clone(),
                value,
            });
        } else {
            self.controller.state.checked.fill(false);
            self.controller.dispatch(AppAction::SetChecked {
                row: target.clone(),
                value: true,
            });
        }
        self.controller.state.cursor = Some(index);
        self.list_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        window.focus(&self.list_focus, cx);
        if event.click_count() >= 2 && !modifiers.modified() {
            if target.is_dir {
                self.controller.dispatch(AppAction::Navigate(target.path));
                self.route_changed(cx);
            } else if let Some(entry) = target.entry {
                self.controller.dispatch(AppAction::OpenFile(entry));
            }
        }
        cx.notify();
    }

    fn list_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.background_blocked() {
            cx.stop_propagation();
            return;
        }
        let modifiers = event.keystroke.modifiers;
        let key = event.keystroke.key.to_ascii_lowercase();
        if key == "delete" {
            self.dialog_return_focus = Some(self.delete_trigger_focus.clone());
            window.focus(&self.delete_trigger_focus, cx);
            self.controller.dispatch(AppAction::RequestDelete);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let rows = self.controller.visible_rows();
        if rows.is_empty() {
            if key == "backspace" && !self.controller.state.current_dir.is_empty() {
                let parent = parent_of(&self.controller.state.current_dir);
                self.controller.dispatch(AppAction::Navigate(parent));
                self.route_changed(cx);
                cx.stop_propagation();
            }
            return;
        }
        if modifiers.secondary() && key == "a" {
            self.controller.dispatch(AppAction::SelectAllVisible);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "space" {
            if let Some(index) = self.controller.state.cursor {
                if let Some(row) = rows.get(index) {
                    let value = !self.controller.is_checked(row);
                    self.controller.dispatch(AppAction::SetChecked {
                        row: row.clone(),
                        value,
                    });
                }
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "enter" {
            if let Some(row) = self
                .controller
                .state
                .cursor
                .and_then(|index| rows.get(index))
            {
                if row.is_dir {
                    self.controller
                        .dispatch(AppAction::Navigate(row.path.clone()));
                    self.route_changed(cx);
                } else if let Some(entry) = row.entry {
                    self.controller.dispatch(AppAction::OpenFile(entry));
                }
            }
            cx.stop_propagation();
            return;
        }
        if key == "backspace" {
            if !self.controller.state.current_dir.is_empty() {
                let parent = parent_of(&self.controller.state.current_dir);
                self.controller.dispatch(AppAction::Navigate(parent));
                self.route_changed(cx);
            }
            cx.stop_propagation();
            return;
        }

        let last = rows.len() - 1;
        let current = self.controller.state.cursor;
        let page = 12;
        let next = match key.as_str() {
            "down" | "arrowdown" => Some(current.map_or(0, |index| (index + 1).min(last))),
            "up" | "arrowup" => Some(current.map_or(0, |index| index.saturating_sub(1))),
            "pagedown" | "page-down" => Some(current.map_or(0, |index| (index + page).min(last))),
            "pageup" | "page-up" => Some(current.map_or(0, |index| index.saturating_sub(page))),
            "home" => Some(0),
            "end" => Some(last),
            _ => None,
        };
        let Some(index) = next else { return };
        if modifiers.shift {
            let from = current.unwrap_or(index);
            let (lo, hi) = if from <= index {
                (from, index)
            } else {
                (index, from)
            };
            for row in &rows[lo..=hi] {
                self.controller.dispatch(AppAction::SetChecked {
                    row: row.clone(),
                    value: true,
                });
            }
        }
        self.controller.state.cursor = Some(index);
        self.list_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        cx.stop_propagation();
        cx.notify();
    }

    /// The archive, as the kit's table.
    ///
    /// The header, the column widths, the sorting arrows, the scrolling and the
    /// right button's menu are the kit's. What stays here is what the kit has
    /// no notion of: many rows marked at once, the band that sweeps them and a
    /// drag that leaves the window.
    fn file_table(&mut self, rows: Vec<super::Row>, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let enabled = self.background_idle();
        let strings = self.controller.s();
        self.sync_table(&rows, cx);
        let mut table = div()
            .id("file-table")
            .role(Role::Table)
            .aria_label(strings.archive_contents)
            .aria_row_count(rows.len() + 1)
            .aria_column_count(self.shown_columns().len())
            .track_focus(&self.list_focus)
            .tab_stop(enabled)
            .focus_visible(focus_ring(cx))
            // No border and no radius: the list is the content pane, not a card
            // floating inside it, so it runs to the edges and the bars above and
            // below draw the only lines.
            .flex_1()
            .min_h(px(1.))
            .flex()
            .flex_col()
            .child(DataTable::new(&self.table).stripe(false).bordered(false))
            .child(
                div()
                    .id("delete-trigger-focus")
                    .track_focus(&self.delete_trigger_focus)
                    .size_0(),
            );
        if enabled {
            table = table
                .focusable()
                .on_key_down(cx.listener(Self::list_key_down));
        }
        table
    }
}

#[derive(Clone)]
enum DialogKind {
    Open,
    Compress,
    Extract {
        only_checked: bool,
    },
    /// Files to put inside the archive that is already open.
    AddFiles,
    /// A copy of the open archive under another name, which is the thing to do
    /// before a change nobody is sure about.
    SaveCopy {
        name: String,
        directory: PathBuf,
    },
}

impl Focusable for GpuiShell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GpuiShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_window_title(&self.controller.state.window_title);
        self.remember_background_focus(window, cx);
        // Decoding writes to the cache, and the viewer's dialog body may only
        // read the shell, so the picture has to be decoded here instead. Name
        // and bytes only: `Viewed` holds the split lines and is not cloned per
        // frame on purpose.
        let to_decode = self
            .controller
            .state
            .viewing
            .as_ref()
            .filter(|view| view.look == super::Look::Picture && view.picture)
            .map(|view| (view.name.clone(), view.bytes.clone()));
        if let Some((name, bytes)) = to_decode {
            let _ = self.viewer_picture(&name, &bytes);
        }
        let s = self.controller.s();
        let modal = self.modal_kind();
        let idle = self.background_idle();
        self.sync_modal_focus(window, cx);
        let password_value = self.controller.state.password_input.clone();
        let add_password_value = self.controller.state.add_password.clone();
        let password_masked = !self.controller.state.show_password;
        self.password.update(cx, |input, cx| {
            input.set_placeholder(s.password_word, window, cx);
            input.set_disabled(
                !matches!(
                    modal,
                    Some(ModalKind::Password | ModalKind::DefaultPassword)
                ),
                cx,
            );
            input.set_masked(password_masked, window, cx);
            if input.value() != password_value.as_str() {
                input.set_value(password_value.clone(), window, cx);
            }
        });
        // The shared text field takes the name of whichever dialog is asking.
        let name_label = match modal {
            Some(ModalKind::Mask) => s.mask_hint,
            _ => s.folder_name,
        };
        let name_value = self.name_value.clone();
        self.name_input.update(cx, |input, cx| {
            input.set_placeholder(name_label, window, cx);
            input.set_disabled(
                !matches!(modal, Some(ModalKind::NewFolder | ModalKind::Mask)),
                cx,
            );
            if input.value() != name_value.as_str() {
                input.set_value(name_value.clone(), window, cx);
            }
        });
        let output_value = self.controller.state.output_name.clone();
        self.output_name.update(cx, |input, cx| {
            input.set_placeholder(s.output_name, window, cx);
            input.set_disabled(!matches!(modal, Some(ModalKind::Add)), cx);
            if input.value() != output_value.as_str() {
                input.set_value(output_value.clone(), window, cx);
            }
        });
        let zip = self.controller.state.format == super::Format::Zip;
        self.add_password.update(cx, |input, cx| {
            input.set_placeholder(s.password_optional, window, cx);
            input.set_disabled(!(matches!(modal, Some(ModalKind::Add)) && zip), cx);
            input.set_masked(password_masked, window, cx);
            if input.value() != add_password_value.as_str() {
                input.set_value(add_password_value.clone(), window, cx);
            }
        });
        let state_filter = self.controller.state.filter.clone();
        if self.filter.read(cx).value().as_ref() != state_filter {
            self.filter.update(cx, |input, cx| {
                input.set_value(state_filter.clone(), window, cx);
            });
        }
        let has_archive = self.controller.state.archive.is_some();
        let selected = self.selected_count();
        let rows = self.controller.visible_rows();
        let visible = rows.len();
        // The one place that knows how long the list is. Everything the pointer
        // gestures need from it reads this instead of building the list again.
        self.row_count = visible;
        let can_extract = has_archive && idle;
        let can_extract_selected = can_extract && selected > 0;
        let password_available = can_extract && self.controller.state.format == super::Format::Zip;
        let breadcrumbs = self.crumbs();
        let (shown, hidden) = Self::visible_crumb_indices(breadcrumbs.len());

        let mut toolbar = div()
            .id("toolbar")
            .aria_label(s.toolbar_region)
            .w_full()
            .flex()
            .items_center()
            .gap_2();
        if !self.background_blocked() {
            toolbar = toolbar.role(Role::Toolbar);
        }

        let open = Self::icon_button(
            "open",
            IconName::FolderOpen,
            format!("{} (Ctrl+O)", s.open),
            idle,
        );
        toolbar = toolbar.child(open.on_click(cx.listener(|this, _, _, cx| {
            if this.background_idle() {
                this.begin_dialog(DialogKind::Open, cx);
            }
        })));
        let compress = Self::icon_button(
            "compress",
            IconName::Inbox,
            format!("{} (Ctrl+N)", s.compress),
            idle,
        );
        toolbar = toolbar.child(compress.on_click(cx.listener(|this, _, _, cx| {
            if this.background_idle() {
                this.begin_dialog(DialogKind::Compress, cx);
            }
        })));
        toolbar = toolbar.child(div().px_1().child(Separator::vertical().h(px(16.))));

        let extract_all = Self::icon_button(
            "extract-all",
            IconName::PanelBottomOpen,
            format!("{} (Ctrl+E)", s.extract_all),
            can_extract,
        );
        toolbar = toolbar.child(extract_all.on_click(cx.listener(|this, _, _, cx| {
            if !this.background_idle() {
                return;
            }
            this.begin_dialog(
                DialogKind::Extract {
                    only_checked: false,
                },
                cx,
            );
        })));
        let extract_selected = Self::icon_button(
            "extract-selected",
            IconName::File,
            s.extract_selected.to_string(),
            can_extract_selected,
        );
        toolbar = toolbar.child(extract_selected.on_click(cx.listener(|this, _, _, cx| {
            if !this.background_idle() {
                return;
            }
            this.begin_dialog(DialogKind::Extract { only_checked: true }, cx);
        })));
        let password = Self::icon_button(
            "password",
            IconName::EyeOff,
            format!("{} / {}", s.set_password, s.remove_password),
            password_available,
        );
        toolbar = toolbar.child(password.on_click(cx.listener(|this, _, _, cx| {
            if this.background_idle() {
                this.controller.dispatch(AppAction::BeginPasswordChange);
                cx.notify();
            }
        })));
        toolbar = toolbar.child(div().px_1().child(Separator::vertical().h(px(16.))));
        let owner = cx.entity().downgrade();
        let recent = self
            .controller
            .state
            .settings
            .recent
            .iter()
            .take(RECENT_MAX)
            .cloned()
            .collect::<Vec<_>>();
        let writable = has_archive && self.controller.state.format == super::Format::Zip;
        let can_undo = has_archive && self.controller.state.undo.is_some();
        let can_copy = self.can_copy_files();
        let can_paste = self.can_paste_files();
        let release = self
            .controller
            .state
            .update
            .as_ref()
            .map(|release| fill(s.update_ready, &[("version", &release.tag)]));
        let flat_view = self.controller.state.settings.flat;
        let visible_columns = Columns::ALL
            .iter()
            .map(|(column, _)| (*column, self.controller.state.settings.columns.on(*column)))
            .collect::<Vec<_>>();
        let overflow = Button::new("overflow")
            .icon(IconName::Ellipsis)
            .accessibility_label(s.more_word)
            .tooltip(s.more_word)
            .ghost()
            .compact()
            .disabled(!idle)
            .dropdown_menu(move |menu, window, popup_cx| {
                let mut menu = menu;
                let archive_owner = owner.clone();
                let archive_release = release.clone();
                menu = menu.submenu_with_icon(
                    Some(Icon::new(IconName::Inbox)),
                    s.archive_group,
                    window,
                    popup_cx,
                    move |submenu, _, _| {
                        let test_owner = archive_owner.clone();
                        let add_owner = archive_owner.clone();
                        let folder_owner = archive_owner.clone();
                        let undo_owner = archive_owner.clone();
                        let save_owner = archive_owner.clone();
                        let password_owner = archive_owner.clone();
                        let mut submenu = submenu
                            .item(
                                PopupMenuItem::new(s.test_word)
                                    .disabled(!has_archive)
                                    .on_click(move |_, _, cx| {
                                        let _ = test_owner.update(cx, |this, cx| {
                                            if let Some(archive) =
                                                this.controller.state.archive.clone()
                                            {
                                                this.controller.dispatch(AppAction::Run(
                                                    Job::Test {
                                                        archive,
                                                        only: None,
                                                    },
                                                ));
                                            }
                                            cx.notify();
                                        });
                                    }),
                            )
                            .separator()
                            .item(
                                Self::popup_action(
                                    add_owner,
                                    s.add_to_archive,
                                    OverflowAction::AddFiles,
                                )
                                .disabled(!writable),
                            )
                            .item(
                                Self::popup_action(
                                    folder_owner,
                                    s.new_folder,
                                    OverflowAction::NewFolder,
                                )
                                .disabled(!writable),
                            )
                            .item(
                                Self::popup_action(undo_owner, s.undo_word, OverflowAction::Undo)
                                    .disabled(!can_undo),
                            )
                            .item(
                                Self::popup_action(
                                    save_owner,
                                    s.save_copy,
                                    OverflowAction::SaveCopy,
                                )
                                .disabled(!has_archive),
                            )
                            .item(
                                Self::popup_action(
                                    password_owner,
                                    s.default_password,
                                    OverflowAction::DefaultPassword,
                                )
                                .disabled(!idle),
                            );
                        if let Some(label) = archive_release.clone() {
                            let release_owner = archive_owner.clone();
                            submenu = submenu.item(Self::popup_action(
                                release_owner,
                                label,
                                OverflowAction::Release,
                            ));
                        }
                        submenu
                    },
                );

                let selection_owner = owner.clone();
                menu = menu.submenu_with_icon(
                    Some(Icon::new(IconName::Check)),
                    s.selection_group,
                    window,
                    popup_cx,
                    move |submenu, _, _| {
                        let select_owner = selection_owner.clone();
                        let invert_owner = selection_owner.clone();
                        let clear_owner = selection_owner.clone();
                        submenu
                            .item(
                                PopupMenuItem::new(s.select_all)
                                    .disabled(!has_archive)
                                    .on_click(move |_, _, cx| {
                                        let _ = select_owner.update(cx, |this, cx| {
                                            this.controller.dispatch(AppAction::SelectAllVisible);
                                            cx.notify();
                                        });
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(s.invert_selection)
                                    .disabled(!has_archive)
                                    .on_click(move |_, _, cx| {
                                        let _ = invert_owner.update(cx, |this, cx| {
                                            this.controller.dispatch(AppAction::InvertVisible);
                                            cx.notify();
                                        });
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(s.clear_selection)
                                    .disabled(!has_archive)
                                    .on_click(move |_, _, cx| {
                                        let _ = clear_owner.update(cx, |this, cx| {
                                            this.controller.dispatch(AppAction::ClearSelection);
                                            cx.notify();
                                        });
                                    }),
                            )
                    },
                );

                let clipboard_owner = owner.clone();
                menu = menu.submenu_with_icon(
                    Some(Icon::new(IconName::Copy)),
                    s.clipboard_group,
                    window,
                    popup_cx,
                    move |submenu, _, _| {
                        let copy_owner = clipboard_owner.clone();
                        let cut_owner = clipboard_owner.clone();
                        let paste_owner = clipboard_owner.clone();
                        submenu
                            .item(
                                PopupMenuItem::new(s.copy_word)
                                    .disabled(!can_copy)
                                    .on_click(move |_, _, cx| {
                                        let _ = copy_owner.update(cx, |this, cx| {
                                            this.controller
                                                .dispatch(AppAction::Copy { cut: false });
                                            cx.notify();
                                        });
                                    }),
                            )
                            .item(PopupMenuItem::new(s.cut_word).disabled(!can_copy).on_click(
                                move |_, _, cx| {
                                    let _ = cut_owner.update(cx, |this, cx| {
                                        this.controller.dispatch(AppAction::Copy { cut: true });
                                        cx.notify();
                                    });
                                },
                            ))
                            .item(
                                PopupMenuItem::new(s.paste_word)
                                    .disabled(!can_paste)
                                    .on_click(move |_, _, cx| {
                                        let _ = paste_owner.update(cx, |this, cx| {
                                            this.controller.dispatch(AppAction::Paste);
                                            cx.notify();
                                        });
                                    }),
                            )
                    },
                );

                let recent_owner = owner.clone();
                let recent_menu = recent.clone();
                menu = menu.submenu(s.recent_group, window, popup_cx, move |submenu, _, _| {
                    let mut submenu = submenu;
                    for path in recent_menu.iter() {
                        let open_owner = recent_owner.clone();
                        let target = PathBuf::from(path);
                        let leaf = target
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or(path.clone());
                        submenu =
                            submenu.item(PopupMenuItem::new(leaf).on_click(move |_, _, cx| {
                                let _ = open_owner.update(cx, |this, cx| {
                                    this.controller.dispatch(AppAction::Open(target.clone()));
                                    cx.notify();
                                });
                            }));
                    }
                    submenu.item(
                        PopupMenuItem::new(s.clear_history)
                            .disabled(recent_menu.is_empty())
                            .on_click({
                                let history_owner = recent_owner.clone();
                                move |_, _, cx| {
                                    let _ = history_owner.update(cx, |this, cx| {
                                        this.controller.state.settings.recent.clear();
                                        this.controller.state.settings.save();
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                });

                let view_owner = owner.clone();
                let columns_menu = visible_columns.clone();
                menu = menu.submenu(s.view_group, window, popup_cx, move |submenu, _, _| {
                    let flat_owner = view_owner.clone();
                    let column_owner = view_owner.clone();
                    let mut submenu = submenu.item(
                        PopupMenuItem::new(s.flat_view)
                            .disabled(!has_archive)
                            .checked(flat_view)
                            .on_click(move |_, _, cx| {
                                let _ = flat_owner.update(cx, |this, cx| {
                                    let settings = &mut this.controller.state.settings;
                                    settings.flat = !settings.flat;
                                    if settings.flat && !settings.columns.on(SortColumn::Path) {
                                        settings.columns.set(SortColumn::Path, true);
                                    }
                                    settings.save();
                                    this.controller.dispatch(AppAction::ClearSelection);
                                    cx.notify();
                                });
                            }),
                    );
                    for (column, shown) in columns_menu.iter().copied() {
                        let label = Columns::label(column, s);
                        let column_owner = column_owner.clone();
                        submenu = submenu.item(
                            PopupMenuItem::new(format!(
                                "{} {label}",
                                if shown { s.hide_word } else { s.show_word }
                            ))
                            .checked(shown)
                            .on_click(move |_, _, cx| {
                                let _ = column_owner.update(cx, |this, cx| {
                                    this.controller.dispatch(AppAction::ToggleColumn(column));
                                    cx.notify();
                                });
                            }),
                        );
                    }
                    submenu
                });

                let app_owner = owner.clone();
                menu.submenu(
                    s.application_group,
                    window,
                    popup_cx,
                    move |submenu, _, _| {
                        let shortcuts_owner = app_owner.clone();
                        let settings_owner = app_owner.clone();
                        submenu
                            .item(PopupMenuItem::new(s.shortcuts_title).on_click(
                                move |_, _, cx| {
                                    let _ = shortcuts_owner.update(cx, |this, cx| {
                                        this.controller.state.show_shortcuts = true;
                                        cx.notify();
                                    });
                                },
                            ))
                            .item(PopupMenuItem::new(s.settings).on_click(move |_, _, cx| {
                                let _ = settings_owner.update(cx, |this, cx| {
                                    this.controller.state.show_settings = true;
                                    cx.notify();
                                });
                            }))
                    },
                )
            });
        toolbar = toolbar.child(overflow);

        // The filter sits at the far end of the bar, the way a search field
        // does in every file manager on the desktop, instead of stretching
        // across whatever room the buttons left over.
        let filter_input = Input::new(&self.filter)
            .aria_label(s.find_word)
            .focus_bordered(false)
            .cleanable(true)
            .disabled(!idle);
        toolbar = toolbar
            .child(div().flex_1().min_w(px(8.)))
            .child(div().w(px(220.)).flex_none().child(filter_input));

        let mut nav = div()
            .id("navigation")
            .w_full()
            .relative()
            .flex()
            .items_center()
            .gap_1()
            .text_xs();
        let at_root = self.controller.state.current_dir.is_empty();
        let back = Self::icon_button(
            "back",
            IconName::ArrowLeft,
            s.back.to_string(),
            idle && self.controller.can_go_back(),
        );
        nav = nav.child(back.on_click(cx.listener(|this, _, _, cx| {
            if this.background_idle() && this.controller.can_go_back() {
                this.controller.dispatch(AppAction::Back);
                this.route_changed(cx);
            }
        })));
        let forward = Self::icon_button(
            "forward",
            IconName::ArrowRight,
            s.forward.to_string(),
            idle && self.controller.can_go_forward(),
        );
        nav = nav.child(forward.on_click(cx.listener(|this, _, _, cx| {
            if this.background_idle() && this.controller.can_go_forward() {
                this.controller.dispatch(AppAction::Forward);
                this.route_changed(cx);
            }
        })));
        let up = Self::icon_button("up", IconName::ArrowUp, s.up.to_string(), idle && !at_root);
        nav = nav.child(up.on_click(cx.listener(|this, _, _, cx| {
            if this.background_idle() && !this.controller.state.current_dir.is_empty() {
                let parent = parent_of(&this.controller.state.current_dir);
                this.controller.dispatch(AppAction::Navigate(parent));
                this.route_changed(cx);
            }
        })));
        // The counts moved to the status bar, where a count belongs; the
        // breadcrumbs move up against the arrows, which is the only place a
        // path reads as "where you are" rather than as a right-hand caption.
        nav = nav.child(div().px_1().child(Separator::vertical().h(px(14.))));

        for (position, index) in shown.iter().enumerate() {
            if position > 0 {
                nav = nav.child(div().text_color(cx.theme().muted_foreground).child("/"));
            }
            if position == 1 && !hidden.is_empty() {
                // The folders that did not fit, behind the same kind of menu as
                // everything else. It carries its own copy of the names: the
                // menu is built while the shell is still borrowed.
                let folders: Vec<(String, String)> = hidden
                    .iter()
                    .map(|index| breadcrumbs[*index].clone())
                    .collect();
                let menu_owner = cx.entity().downgrade();
                nav = nav.child(
                    Button::new("crumb-more")
                        .label("…")
                        .accessibility_label(s.hidden_folders)
                        .tooltip(s.hidden_folders)
                        .ghost()
                        .compact()
                        .disabled(!idle)
                        .dropdown_menu(move |menu, _, _| {
                            let mut menu = menu;
                            for (name, path) in &folders {
                                let owner = menu_owner.clone();
                                let path = path.clone();
                                menu = menu.item(PopupMenuItem::new(name.clone()).on_click(
                                    move |_, _, cx| {
                                        let _ = owner.update(cx, |this, cx| {
                                            if !this.background_idle() {
                                                return;
                                            }
                                            this.controller
                                                .dispatch(AppAction::Navigate(path.clone()));
                                            this.route_changed(cx);
                                        });
                                    },
                                ));
                            }
                            menu
                        }),
                );
                nav = nav.child(div().text_color(cx.theme().muted_foreground).child("/"));
            }
            let (name, path) = &breadcrumbs[*index];
            if *index + 1 == breadcrumbs.len() {
                nav = nav.child(div().text_color(cx.theme().foreground).child(name.clone()));
            } else {
                let path = path.clone();
                let crumb = Self::button(
                    ("crumb", *index),
                    name,
                    fill(s.open_folder, &[("name", name)]),
                    idle,
                );
                nav = nav.child(crumb.on_click(cx.listener(move |this, _, _, cx| {
                    if !this.background_idle() {
                        return;
                    }
                    this.controller.dispatch(AppAction::Navigate(path.clone()));
                    this.route_changed(cx);
                })));
            }
        }

        let notice_color = if self.controller.state.error {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };
        let status = self.status();

        // The window is regions divided by hairlines, not strips floating in
        // padding: a toolbar bar, a navigation bar, the body, and a status bar
        // welded to the bottom edge. Padding lives inside each bar, so every
        // divider runs the full width and the list reaches both edges.
        let toolbar_bar = div()
            .id("toolbar-bar")
            .flex_none()
            .h(px(40.))
            .px_2()
            .flex()
            .items_center()
            .bg(cx.theme().title_bar)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(toolbar);
        let nav_bar = div()
            .id("nav-bar")
            .flex_none()
            .h(px(34.))
            .px_2()
            .flex()
            .items_center()
            .bg(cx.theme().background)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(nav);

        let mut status_view = div()
            .id("status")
            .aria_label(s.status_region)
            .text_xs()
            .text_color(notice_color)
            .truncate()
            .child(status);
        if !self.background_blocked() {
            status_view = status_view.role(if self.controller.state.error {
                Role::Alert
            } else {
                Role::Status
            });
        }

        // Everything that used to sit in the flow — the list, the empty states,
        // the progress row — now goes inside the content pane beside the
        // sidebar, so the sidebar runs the full height of the window.
        let mut content = div()
            .id("archive-content")
            .flex_1()
            .min_w(px(1.))
            .min_h(px(1.))
            .flex()
            .flex_col()
            .bg(cx.theme().background);

        // GPUI/AccessKit has no aria-hidden builder. The supported equivalent
        // is a role-less background subtree; every actionable descendant also
        // drops its role, tab stop, and listener while the sibling overlay
        // owns focus and input.
        // The name of what is open, where a title bar puts it. Nothing in here
        // can be pressed on purpose: the bar owns the drag, and a control
        // inside it would move the window when the hand wobbled on the way to
        // pressing it.
        let title_bar = TitleBar::new().child(
            div()
                .id("window-title")
                .role(Role::Heading)
                .flex()
                .items_center()
                .h_full()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .truncate()
                .child(self.controller.state.window_title.clone()),
        );

        let mut root = div()
            .id("arca-gpui-background")
            .on_action(cx.listener(Self::focus_filter))
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(title_bar)
            .child(toolbar_bar)
            .child(nav_bar);

        let drop_probe = {
            let view = cx.entity();
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let dropped = view.clone();
                    window.on_mouse_event(move |event: &gpui::FileDropEvent, _, window, app| {
                        let current_focus = window.focused(app);
                        dropped.update(app, |shell, cx| match event {
                            gpui::FileDropEvent::Entered { paths, .. } => {
                                shell.drop_paths =
                                    drop_paths_for_enter(paths.paths(), shell.background_idle());
                            }
                            gpui::FileDropEvent::Submit { .. } => {
                                let paths = std::mem::take(&mut shell.drop_paths);
                                if !paths.is_empty() && shell.background_idle() {
                                    // Keep the element that had focus before
                                    // the drop. The existing modal focus sync
                                    // will move into confirmation and return
                                    // here when it is answered.
                                    if let Some(focus) = current_focus {
                                        shell.dialog_return_focus = Some(focus);
                                    }
                                    shell.controller.dispatch(AppAction::Drop(paths));
                                    cx.notify();
                                }
                            }
                            gpui::FileDropEvent::Exited | gpui::FileDropEvent::Ended => {
                                shell.drop_paths.clear();
                            }
                            gpui::FileDropEvent::Pending { .. } => {}
                        });
                    });

                    // A column edge in hand, a band being pulled, and the list
                    // running after the pointer all have to keep working once
                    // the pointer has left the thing it started on, so the move
                    // and the release are watched on the window.
                    let dragging = view.clone();
                    window.on_mouse_event(
                        move |event: &gpui::MouseMoveEvent, phase, window, app| {
                            if !phase.bubble() {
                                return;
                            }
                            let leaving = dragging.update(app, |shell, cx| {
                                shell.pointer = event.position;
                                let mut moved = false;
                                if let Some((slot, from, width)) = shell.resizing {
                                    shell.set_column_width(
                                        slot,
                                        width + f32::from(event.position.x) - from,
                                    );
                                    moved = true;
                                }
                                moved |= shell.drag_band(event.position);
                                moved |= shell.wheel.is_some();
                                if moved {
                                    cx.notify();
                                }
                                shell.carrying && shell.left_the_list(event.position)
                            });
                            // Out of the list is out of the archive. Started here
                            // and not at the press, because the instant the native
                            // drag begins the system takes the pointer and there is
                            // no way back into the list to drop on a folder.
                            if leaving {
                                app.stop_active_drag(window);
                                dragging.update(app, |shell, cx| {
                                    shell.carrying = false;
                                    shell.controller.drag_out();
                                    cx.notify();
                                });
                            }
                        },
                    );
                    let pressed = view.clone();
                    window.on_mouse_event(move |event: &gpui::MouseDownEvent, phase, _, app| {
                        if !phase.bubble() {
                            return;
                        }
                        pressed.update(app, |shell, cx| {
                            match event.button {
                                // Pressing the wheel again puts it away, the way
                                // it does in a browser.
                                gpui::MouseButton::Middle => {
                                    shell.toggle_wheel(event.position);
                                    cx.notify();
                                }
                                // Any other button is somebody asking for
                                // something else.
                                _ if shell.wheel.is_some() => {
                                    shell.wheel = None;
                                    cx.notify();
                                }
                                gpui::MouseButton::Left => {
                                    shell.begin_band(
                                        event.position,
                                        event.modifiers.secondary(),
                                        event.modifiers.shift,
                                    );
                                }
                                _ => {}
                            }
                        });
                    });
                    let released = view.clone();
                    window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, _, app| {
                        if !phase.bubble() {
                            return;
                        }
                        released.update(app, |shell, cx| {
                            let mut changed = shell.band.take().is_some_and(|band| band.live);
                            shell.carrying = false;
                            if shell.resizing.take().is_some() {
                                // Written when the hand lets go rather than on
                                // the way, so pulling an edge across the window
                                // is one visit to the disk and not one a frame.
                                shell.controller.state.settings.save();
                                changed = true;
                            }
                            // Held down and pulled: the gesture ends where the
                            // hand lets go. Let go without having pulled and it
                            // stays on, waiting.
                            if event.button == gpui::MouseButton::Middle
                                && shell.wheel.as_ref().is_some_and(|wheel| wheel.moved)
                            {
                                shell.wheel = None;
                                changed = true;
                            }
                            if changed {
                                cx.notify();
                            }
                        });
                    });
                    // The wheel turning is somebody scrolling by hand, which is
                    // asking for something other than the list running itself.
                    let spun = view.clone();
                    window.on_mouse_event(move |_: &gpui::ScrollWheelEvent, phase, _, app| {
                        if !phase.bubble() {
                            return;
                        }
                        spun.update(app, |shell, cx| {
                            if shell.wheel.take().is_some() {
                                cx.notify();
                            }
                        });
                    });
                },
            )
            .size_0()
        };
        root = root
            .child(drop_probe)
            .child(
                div()
                    .id("drop-trigger-focus")
                    .track_focus(&self.drop_trigger_focus)
                    .size_0(),
            )
            .child(
                div()
                    .id("conflict-trigger-focus")
                    .track_focus(&self.conflict_trigger_focus)
                    .size_0(),
            )
            .child(
                div()
                    .id("add-start-focus")
                    .track_focus(&self.add_start_focus)
                    .size_0(),
            );

        if !self.drop_paths.is_empty() && self.background_idle() {
            let count = self.drop_paths.len();
            root = root.child(
                div()
                    .id("drop-feedback")
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(cx.theme().drop_target)
                    .border_2()
                    .border_color(cx.theme().drag_border)
                    .rounded(cx.theme().radius_lg)
                    .role(Role::Status)
                    .aria_label(format!("{} ({count})", s.dropped_word))
                    .child(format!("{} ({count})", s.dropped_word)),
            );
        }

        if self.controller.state.busy {
            use std::sync::atomic::Ordering;
            let total = self.controller.state.total_count;
            let done = self.controller.state.done_count;
            let fraction = if total == 0 {
                0.0
            } else {
                done as f64 / total as f64
            };
            let progress = match (total, self.controller.state.in_bytes) {
                (0, _) => format!("{}…", s.working_word),
                // A download counts bytes, not files, and nobody reads
                // "2481152 of 4627170".
                (total, true) => format!("{} / {}", human(done as u64), human(total as u64)),
                (total, false) => format!("{done} / {total}"),
            };
            let held = self.controller.state.hold.load(Ordering::Relaxed);
            let asked = self.controller.state.stop.load(Ordering::Relaxed);
            // The clock, and the guess of what is left made from how long the
            // part already done took. Only once enough of it is done for the
            // guess to be worth reading: at two per cent it would say an hour
            // and then a minute. A paused job is not going anywhere.
            let mut timing = self
                .controller
                .state
                .started
                .map(|started| {
                    let gone = started.elapsed().as_secs_f64();
                    let mut text = format!("{} {}", s.elapsed_word, super::clock(gone));
                    if !held && fraction > 0.05 {
                        text.push_str(&format!(
                            " · {} {}",
                            s.time_left,
                            super::clock(gone / fraction - gone)
                        ));
                    }
                    text
                })
                .unwrap_or_default();
            if asked {
                timing.push_str(&format!(" · {}", s.stopping));
            } else if held {
                timing.push_str(&format!(" · {}", s.paused_word));
            }
            // Pausing lets go at the end of an entry, not the end of a byte, so
            // a file that has started still has to finish.
            let hold_button = Self::button(
                "pause-job",
                if held { s.resume_word } else { s.pause_word },
                if held { s.resume_word } else { s.pause_word }.to_string(),
                !self.background_blocked() && !asked,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.background_blocked() {
                    this.controller.state.hold.store(!held, Ordering::Relaxed);
                    cx.notify();
                }
            }));
            let cancel = Self::button(
                "cancel-job",
                s.cancel,
                format!("{} · {}", s.cancel, s.progress_region),
                !self.background_blocked() && !asked,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.background_blocked() {
                    this.controller.dispatch(AppAction::CancelJob);
                    cx.notify();
                }
            }));
            // A strip across the top of the content pane rather than another
            // floating row: work happening to the archive belongs above the
            // archive, and it must not shove the list down a line when it
            // appears.
            // The bar owns the accessible name and the numeric value; the
            // strip around it is a status region whose text is read as it
            // changes. Naming both would announce the same thing twice.
            let mut progress_view = div()
                .id("progress")
                .flex_none()
                .h(px(30.))
                .px_2()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .bg(cx.theme().secondary)
                .border_b_1()
                .border_color(cx.theme().border)
                .child(progress)
                .child(
                    Progress::new("job-progress")
                        .accessibility_label(s.progress_region)
                        .value(fraction as f32 * 100.)
                        .loading(total == 0)
                        .w(px(96.))
                        .flex_none(),
                )
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(self.controller.state.current_file.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(cx.theme().muted_foreground)
                        .child(timing),
                )
                .child(hold_button)
                .child(cancel);
            if !self.background_blocked() {
                progress_view = progress_view.role(Role::Status);
            }
            content = content.child(progress_view);
        }

        if self.controller.state.busy && self.controller.state.entries.is_empty() {
            content = content.child({
                let mut loading = div()
                    .id("loading-state")
                    .aria_label(s.opening)
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{}…", s.opening));
                if !self.background_blocked() {
                    loading = loading.role(Role::Status);
                }
                loading
            });
        } else if !has_archive {
            content = content.child({
                let mut empty = div()
                    .id("empty-state")
                    .aria_label(if self.controller.state.error {
                        s.cannot_open
                    } else {
                        s.drop_here
                    })
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(if self.controller.state.error {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(if self.controller.state.error {
                        s.cannot_open
                    } else {
                        s.drop_here
                    });
                if !self.background_blocked() {
                    empty = empty.role(Role::Region);
                }
                empty
            });
        } else if visible == 0 {
            let message = if self.controller.state.error {
                s.cannot_open
            } else if self.controller.state.filter.trim().is_empty() {
                if self.controller.state.entries.is_empty() {
                    s.empty_archive
                } else {
                    s.empty_folder
                }
            } else {
                s.no_matches
            };
            content = content.child({
                let mut empty = div()
                    .id("empty-state")
                    .aria_label(empty_state_aria_label(
                        self.controller.state.error,
                        &self.controller.state.filter,
                        s,
                    ))
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(if self.controller.state.error {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(message);
                if !self.background_blocked() {
                    empty = empty.role(Role::Region);
                }
                empty
            });
        } else {
            content = content.child(self.file_table(rows, cx));
        }

        // Sidebar beside content, and only once there is an archive: an empty
        // folder pane next to an empty file list is two ways of saying nothing.
        let mut body = div()
            .id("archive-body")
            .flex_1()
            .min_h(px(1.))
            .flex()
            .flex_row();
        if has_archive {
            let sidebar = self.sidebar(cx);
            body = body.child(sidebar);
        }
        root = root.child(body.child(content)).child(
            StatusBar::new()
                .left(status_view)
                .right(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{visible} {}", s.visible_of))
                        .child(Separator::vertical().h(px(10.)))
                        .child(format!("{selected} {}", s.checked)),
                )
                .border_t_1()
                .border_color(cx.theme().border),
        );

        let background = root;
        let mut root = div()
            .id("arca-gpui-shell")
            .role(Role::Application)
            .aria_label("Arca")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(Self::copy_files_action))
            .on_action(cx.listener(Self::cut_files_action))
            .on_action(cx.listener(Self::paste_files_action))
            .on_key_down(cx.listener(Self::global_key_down))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(background);

        // The band, and the anchor the wheel dropped. Both are drawn over
        // everything rather than inside the list, because the hand is free to
        // wander off it while either gesture runs and a mark that vanished at
        // the edge would be worse than no mark at all.
        if let Some(band) = self.band.as_ref().filter(|band| band.live) {
            if let Some(view) = self.list_view(visible) {
                let (x0, x1) = minmax(band.origin.x, band.head.x);
                let (y0, y1) = minmax(band.origin.y, band.head.y);
                let top = y0.max(view.top);
                let bottom = y1.min(view.bottom);
                if bottom > top {
                    // A quarter of the selection ink, so the rows underneath
                    // stay readable while they are being swept: the band says
                    // what it is reaching, and a solid one would hide it.
                    // Nothing is occluded either, because the pointer has to
                    // keep being followed through it.
                    let mut fill = cx.theme().selection;
                    fill.a *= 0.25;
                    root = root.child(
                        div()
                            .id("selection-band")
                            .absolute()
                            .left(px(x0))
                            .top(px(top))
                            .w(px((x1 - x0).max(1.0)))
                            .h(px(bottom - top))
                            .bg(fill)
                            .border_1()
                            .border_color(cx.theme().ring),
                    );
                }
            }
        }
        if let Some(wheel) = &self.wheel {
            // A ring with a dot in it, left where the wheel went down: the mark
            // Windows leaves, so it reads as the same gesture rather than as
            // one of ours.
            root = root.child(
                div()
                    .id("wheel-anchor")
                    .absolute()
                    .left(px(f32::from(wheel.anchor.x) - 10.0))
                    .top(px(f32::from(wheel.anchor.y) - 10.0))
                    .size(px(20.))
                    .rounded_full()
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().muted_foreground)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .size(px(3.))
                            .rounded_full()
                            .bg(cx.theme().muted_foreground),
                    ),
            );
        }
        // The kit's stack is imperative and this shell's modal is derived, so
        // they are reconciled after the frame rather than during it.
        if self.modal_kind().filter(|kind| Self::kit_dialog(*kind)) != self.open_modal {
            let shell = cx.weak_entity();
            window.on_next_frame(move |window, cx| {
                if let Some(shell) = shell.upgrade() {
                    shell.update(cx, |this, cx| this.sync_dialog(window, cx));
                }
            });
        }
        root
    }
}

/// The window's first view under `Root`: the shell, plus the kit's own layers.
///
/// `Root` owns the dialog, sheet and notification stacks but does not mount
/// them, so somebody has to. It cannot be `GpuiShell` itself: a dialog body
/// reads the shell, and mounting the layer inside the shell's own `render`
/// runs that read while the shell is exclusively borrowed, which aborts the
/// process. As a sibling of the shell rather than a part of it, the borrow is
/// over by the time the layer builds.
struct Frame {
    shell: Entity<GpuiShell>,
}

impl Render for Frame {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(self.shell.clone())
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

/// The body of a kit dialog.
///
/// Free function taking the shell by entity: it runs from the dialog layer in
/// `Frame`, so it may read the shell, but it must never write it -- a write
/// here would repaint from inside a paint. The handlers below are fine,
/// because a click is dispatched between frames.
/// A setting picked from everything it can be, rather than cycled one press
/// at a time.
///
/// A button that cycles hides every other choice behind the one written on it,
/// and the only way back to the choice you just passed is round again. The
/// name of the setting goes to the accessible name, because the label is the
/// value.
fn pick<T: Copy + PartialEq + 'static>(
    id: &'static str,
    name: &'static str,
    options: Vec<(T, &'static str)>,
    current: T,
    enabled: bool,
    weak: WeakEntity<GpuiShell>,
    apply: fn(&mut GpuiShell, T),
) -> gpui::AnyElement {
    let label = options
        .iter()
        .find(|(candidate, _)| *candidate == current)
        .map(|(_, label)| *label)
        .unwrap_or_default();
    Button::new(id)
        .label(label)
        .accessibility_label(name)
        .dropdown_caret(true)
        .outline()
        .disabled(!enabled)
        .dropdown_menu(move |menu, _, _| {
            options.iter().fold(menu, |menu, (value, label)| {
                let (value, weak) = (*value, weak.clone());
                menu.item(
                    PopupMenuItem::new(*label)
                        .checked(value == current)
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                apply(this, value);
                                cx.notify();
                            });
                        }),
                )
            })
        })
        .into_any_element()
}

/// The three compression settings, shared by the settings dialog and the add
/// dialog so the two cannot drift apart.
fn format_pick(
    id: &'static str,
    shell: &Entity<GpuiShell>,
    weak: &WeakEntity<GpuiShell>,
    cx: &App,
) -> gpui::AnyElement {
    let s = shell.read(cx).controller.s();
    let options = [super::Format::Zip, super::Format::Tar, super::Format::TarGz]
        .map(|format| (format, format.label()))
        .to_vec();
    pick(
        id,
        s.format,
        options,
        shell.read(cx).controller.state.format,
        true,
        weak.clone(),
        |this, format| this.controller.state.format = format,
    )
}

fn codec_pick(
    id: &'static str,
    shell: &Entity<GpuiShell>,
    weak: &WeakEntity<GpuiShell>,
    enabled: bool,
    cx: &App,
) -> gpui::AnyElement {
    let this = shell.read(cx);
    let options = [
        super::Codec::Store,
        super::Codec::Deflate,
        super::Codec::Zstd,
    ]
    .map(|codec| (codec, this.controller.codec_name(codec)))
    .to_vec();
    pick(
        id,
        this.controller.s().compressor,
        options,
        this.controller.state.codec,
        enabled,
        weak.clone(),
        |this, codec| this.controller.state.codec = codec,
    )
}

fn level_pick(
    id: &'static str,
    shell: &Entity<GpuiShell>,
    weak: &WeakEntity<GpuiShell>,
    cx: &App,
) -> gpui::AnyElement {
    let this = shell.read(cx);
    let options = [
        super::Level::Store,
        super::Level::Fast,
        super::Level::Normal,
        super::Level::Best,
    ]
    .map(|level| (level, this.controller.level_name(level)))
    .to_vec();
    pick(
        id,
        this.controller.s().level,
        options,
        this.controller.state.level,
        true,
        weak.clone(),
        |this, level| this.controller.state.level = level,
    )
}

fn build_dialog(
    kind: ModalKind,
    shell: &Entity<GpuiShell>,
    dialog: Dialog,
    _window: &mut Window,
    cx: &mut App,
) -> Dialog {
    let s = shell.read(cx).controller.s();
    let weak = shell.downgrade();
    // Escape, the backdrop and the close button all leave the controller still
    // asking the question, and the next frame would reopen the dialog.
    // Answering for the user keeps the two in step -- but only while the state
    // is still on this question, because `on_close` also runs right after ok or
    // cancel already answered it.
    let dialog = dialog.on_close({
        let weak = weak.clone();
        move |_, _, cx| {
            let _ = weak.update(cx, |this, cx| {
                if this.modal_kind() == Some(kind) {
                    this.cancel_modal(kind);
                    cx.notify();
                }
            });
        }
    });
    // Same answer as the close button, for the people who reach for a labelled
    // button instead of an X.
    let cancel_button = |id: &'static str, label: &'static str| {
        let weak = weak.clone();
        Button::new(id)
            .label(label)
            .outline()
            .on_click(move |_, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.cancel_modal(kind);
                    cx.notify();
                });
            })
    };
    // Reveals what is being typed. Does not answer the dialog, so it never
    // closes it.
    let reveal_button = |id: &'static str| {
        let weak = weak.clone();
        let shown = shell.read(cx).controller.state.show_password;
        Button::new(id)
            .label(if shown { s.hide_word } else { s.show_password })
            .ghost()
            .on_click(move |_, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.controller
                        .dispatch(AppAction::TogglePasswordVisibility);
                    cx.notify();
                });
            })
    };
    match kind {
        ModalKind::Delete => {
            let names = shell
                .read(cx)
                .controller
                .state
                .confirm_delete
                .clone()
                .unwrap_or_default();
            let confirm = weak.clone();
            dialog
                .title(s.delete_word)
                .child(
                    DialogDescription::new()
                        .child(fill(s.confirm_delete, &[("n", &names.len().to_string())])),
                )
                // `button_props` alone draws nothing: the kit only renders
                // action buttons when a footer asks for them. `DialogClose`
                // and `DialogAction` are what route a press back into
                // `on_close` and `on_ok`.
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("delete-cancel").label(s.cancel).outline()),
                        )
                        .child(
                            DialogAction::new()
                                .child(Button::new("delete-confirm").label(s.delete_word).danger()),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = confirm.update(cx, |this, cx| {
                        this.controller.dispatch(AppAction::ConfirmDelete(true));
                        cx.notify();
                    });
                    true
                })
        }
        ModalKind::Conflict => {
            let path = shell
                .read(cx)
                .controller
                .state
                .conflict
                .clone()
                .unwrap_or_default();
            // Every choice closes the dialog the same way: it answers, the
            // controller drops the question and the next reconcile takes the
            // dialog down. Nothing here has to close it by hand.
            let choice = |id: &'static str, label: &'static str, answer: Answer| {
                let weak = weak.clone();
                Button::new(id).label(label).on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.answer_conflict(answer);
                        cx.notify();
                    });
                })
            };
            let enter = weak.clone();
            dialog
                .title(s.conflict_title)
                .w(px(560.))
                .child(DialogDescription::new().child(format!("{} {path}", s.already_there)))
                .footer(
                    DialogFooter::new()
                        .flex_wrap()
                        .child(choice("conflict-replace", s.yes, Answer::Replace).primary())
                        .child(choice(
                            "conflict-replace-all",
                            s.yes_all,
                            Answer::ReplaceAll,
                        ))
                        .child(choice("conflict-skip", s.no, Answer::Skip))
                        .child(choice("conflict-skip-all", s.no_all, Answer::SkipAll))
                        .child(choice("conflict-keep-both", s.rename, Answer::Rename))
                        .child(choice(
                            "conflict-keep-both-all",
                            s.rename_all,
                            Answer::RenameAll,
                        ))
                        .child(choice("conflict-cancel", s.cancel, Answer::Cancel).outline()),
                )
                // Enter keeps answering what it answered before the migration.
                .on_ok(move |_, _, cx| {
                    let _ = enter.update(cx, |this, cx| {
                        this.answer_conflict(Answer::Replace);
                        cx.notify();
                    });
                    true
                })
        }
        ModalKind::Drop => {
            let paths = shell
                .read(cx)
                .controller
                .state
                .confirm_drop
                .clone()
                .unwrap_or_default();
            let names = paths
                .iter()
                .take(8)
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let choice = |id: &'static str, label: &'static str, choice: DropChoice| {
                let weak = weak.clone();
                Button::new(id).label(label).on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.controller.dispatch(AppAction::AnswerDrop(choice));
                        cx.notify();
                    });
                })
            };
            let enter = weak.clone();
            dialog
                .title(s.drop_title)
                .child(DialogDescription::new().child(format!("{} {names}", s.dropped_word)))
                .footer(
                    DialogFooter::new()
                        .child(choice("drop-open", s.open_word, DropChoice::Open).primary())
                        .child(choice("drop-add", s.add_to_archive, DropChoice::Add))
                        .child(choice("drop-cancel", s.cancel, DropChoice::Cancel).outline()),
                )
                .on_ok(move |_, _, cx| {
                    let _ = enter.update(cx, |this, cx| {
                        this.controller
                            .dispatch(AppAction::AnswerDrop(DropChoice::Open));
                        cx.notify();
                    });
                    true
                })
        }
        ModalKind::Shortcuts => {
            // Keystrokes, not printed key names: the kit spells each one the
            // way the platform does, so the window stops claiming Supr on a
            // keyboard whose key says Delete.
            let left: [(&[&str], &str); 17] = [
                (&["ctrl-o"], s.open),
                (&["ctrl-n"], s.compress),
                (&["ctrl-e"], s.extract_all),
                (&["alt-w"], s.extract_here),
                (&["f3"], s.view_word),
                (&["ctrl-t"], s.test_word),
                (&["f5"], s.refresh_word),
                (&["ctrl-f"], s.find_word),
                (&["ctrl-z"], s.undo_word),
                (&["ctrl-p"], s.default_password),
                (&["ctrl-a"], s.select_all),
                (&["ctrl-i"], s.invert_selection),
                (&["escape"], s.clear_selection),
                (&["space"], s.toggle_word),
                (&["+", "-"], s.select_group),
                (&["f2"], s.rename_word),
                (&["delete"], s.delete_word),
            ];
            let right: [(&[&str], &str); 11] = [
                (&["f1"], s.shortcuts_title),
                (&["ctrl-c"], s.copy_word),
                (&["ctrl-x"], s.cut_word),
                (&["ctrl-v"], s.paste_word),
                (&["ctrl-shift-c"], s.copy_names),
                (&["enter"], s.open_word),
                (&["backspace"], s.up),
                (&["up", "down"], s.move_word),
                (&["home", "end"], s.move_word),
                (&["pageup", "pagedown"], s.move_word),
                (&["tab"], s.jump_word),
            ];
            let column = |rows: &[(&[&str], &str)]| {
                rows.iter()
                    .fold(div().flex().flex_col().gap_1(), |column, (keys, what)| {
                        column.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .text_sm()
                                .child(
                                    div()
                                        .w(px(130.))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .children(keys.iter().filter_map(|key| {
                                            gpui::Keystroke::parse(key).ok().map(Kbd::new)
                                        })),
                                )
                                .child(what.to_string()),
                        )
                    })
            };
            // No close button of its own: the kit's own close, escape and
            // backdrop are the way out of every dialog now.
            dialog.title(s.shortcuts_title).w(px(720.)).child(
                div()
                    .flex()
                    .gap_8()
                    .child(column(&left))
                    .child(column(&right)),
            )
        }
        ModalKind::Password => {
            let setting = matches!(
                shell.read(cx).controller.state.waiting_on_password,
                Some(Pending::CurrentPassword(_))
            );
            let field = shell.read(cx).password.clone();
            let submit = weak.clone();
            let submit_label = if setting { s.set_password } else { s.start };
            dialog
                .title(if setting {
                    s.set_password
                } else {
                    s.password_needed
                })
                .child(DialogDescription::new().child(if setting {
                    s.new_password
                } else {
                    s.password_hint
                }))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(Input::new(&field))
                        .child(reveal_button("password-visibility")),
                )
                .footer(
                    DialogFooter::new()
                        .child(cancel_button("password-cancel", s.cancel))
                        .child(
                            DialogAction::new().child(
                                Button::new("password-submit").label(submit_label).primary(),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = submit.update(cx, |this, cx| {
                        let password = this.password.read(cx).value().to_string();
                        this.controller
                            .dispatch(AppAction::SubmitPassword(password));
                        cx.notify();
                    });
                    true
                })
        }
        ModalKind::DefaultPassword => {
            let field = shell.read(cx).password.clone();
            let keep = weak.clone();
            let forget = weak.clone();
            let muted = cx.theme().muted_foreground;
            dialog
                .title(s.default_password)
                .child(DialogDescription::new().child(s.password_hint))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(Input::new(&field))
                        .child(reveal_button("default-password-visibility"))
                        .child(div().text_xs().text_color(muted).child(s.password_kept)),
                )
                .footer(
                    DialogFooter::new()
                        .child(cancel_button("default-password-cancel", s.cancel))
                        .child(
                            Button::new("default-password-forget")
                                .label(s.remove_password)
                                .on_click(move |_, _, cx| {
                                    let _ = forget.update(cx, |this, cx| {
                                        this.forget_default_password();
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            DialogAction::new().child(
                                Button::new("default-password-keep")
                                    .label(s.start)
                                    .primary(),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = keep.update(cx, |this, cx| {
                        this.keep_default_password();
                        cx.notify();
                    });
                    true
                })
        }
        ModalKind::Add => {
            let is_zip = shell.read(cx).controller.state.format == super::Format::Zip;
            let count = shell.read(cx).controller.state.pending_inputs.len();
            let output_name = shell.read(cx).output_name.clone();
            let add_password = shell.read(cx).add_password.clone();
            let labelled = |label: &'static str, control: gpui::AnyElement| {
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(label)
                    .child(control)
            };
            let start = weak.clone();
            let mut body = div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(s.output_name)
                        .child(Input::new(&output_name)),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(labelled(
                            s.format,
                            format_pick("add-format", shell, &weak, cx),
                        ))
                        .child(labelled(
                            s.compressor,
                            codec_pick("add-codec", shell, &weak, is_zip, cx),
                        ))
                        .child(labelled(s.level, level_pick("add-level", shell, &weak, cx))),
                );
            if is_zip {
                body = body.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(Input::new(&add_password))
                        .child(reveal_button("add-password-visibility")),
                );
            }
            dialog
                .title(s.add_to_archive)
                .w(px(600.))
                .child(DialogDescription::new().child(s.defaults_title))
                .child(body.child(format!("{count} {}", s.files_word)))
                .footer(
                    DialogFooter::new()
                        .child(cancel_button("add-cancel", s.cancel))
                        .child(
                            DialogAction::new()
                                .child(Button::new("add-start").label(s.start).primary()),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = start.update(cx, |this, cx| this.start_add(cx));
                    true
                })
        }
        ModalKind::Viewer => {
            // Field by field: `Viewed` is deliberately not `Clone`, because it
            // carries the split lines of the whole file.
            let Some((name, look, has_picture, bytes, lines)) = shell
                .read(cx)
                .controller
                .state
                .viewing
                .as_ref()
                .map(|view| {
                    (
                        view.name.clone(),
                        view.look,
                        view.picture,
                        view.bytes.clone(),
                        view.lines.clone(),
                    )
                })
            else {
                return dialog;
            };
            let scroll = shell.read(cx).viewer_scroll.clone();
            let muted = cx.theme().muted_foreground;
            let mut tabs = div().flex().items_center().gap_2();
            for (index, (candidate, label)) in [
                (super::Look::Text, s.as_text),
                (super::Look::Hex, s.as_hex),
                (super::Look::Picture, s.as_picture),
            ]
            .into_iter()
            .enumerate()
            {
                // Only where there is a picture to show. A tab that says
                // "picture" over a text file is a tab that lies.
                if candidate == super::Look::Picture && !has_picture {
                    continue;
                }
                let weak = weak.clone();
                tabs = tabs.child(
                    Button::new(("viewer-tab", index))
                        .label(label)
                        .selected(look == candidate)
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                if let Some(view) = &mut this.controller.state.viewing {
                                    view.look = candidate;
                                }
                                cx.notify();
                            });
                        }),
                );
            }
            tabs = tabs.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(human(bytes.len() as u64)),
            );
            let content = match look {
                super::Look::Picture => {
                    let image = shell
                        .read(cx)
                        .viewer_image
                        .as_ref()
                        .filter(|(cached, _)| cached == &name)
                        .map(|(_, image)| image.clone());
                    div()
                        .id("viewer-picture")
                        .flex_1()
                        .min_h(px(1.))
                        .overflow_scroll()
                        .children(image.map(gpui::img))
                        .into_any_element()
                }
                super::Look::Text => {
                    let total = lines.len();
                    uniform_list(
                        "viewer-text",
                        total,
                        move |range: Range<usize>, _window, _cx| {
                            range
                                .map(|index| {
                                    div()
                                        .font_family("monospace")
                                        .text_xs()
                                        .child(lines[index].clone())
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&scroll)
                    .size_full()
                    .into_any_element()
                }
                super::Look::Hex => {
                    let total = bytes.len().div_ceil(16);
                    uniform_list(
                        "viewer-hex",
                        total,
                        move |range: Range<usize>, _window, _cx| {
                            range
                                .map(|row| {
                                    let at = row * 16;
                                    let end = (at + 16).min(bytes.len());
                                    div()
                                        .font_family("monospace")
                                        .text_xs()
                                        .child(super::hex_line(at, &bytes[at..end]))
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&scroll)
                    .size_full()
                    .into_any_element()
                }
            };
            dialog
                .title(name)
                .w(px(800.))
                .child(DialogDescription::new().child(s.view_word))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .h(px(460.))
                        .child(tabs)
                        .child(Separator::horizontal())
                        .child(
                            // The bar hangs off the list's own scroll handle, so
                            // a file of a million lines says how far down it is
                            // instead of only answering the wheel.
                            div()
                                .id("viewer-content")
                                .relative()
                                .flex_1()
                                .min_h(px(1.))
                                .overflow_hidden()
                                .child(content)
                                .vertical_scrollbar(&scroll),
                        ),
                )
        }
        ModalKind::Settings => {
            let lang = shell.read(cx).controller.state.settings.lang;
            let theme = shell.read(cx).controller.state.settings.theme;
            let is_zip = shell.read(cx).controller.state.format == super::Format::Zip;
            let page = shell.read(cx).controller.state.settings.page;
            let pages = arca_zip::pages::Page::ALL
                .iter()
                .map(|(page, _, label)| (*page, *label))
                .collect::<Vec<_>>();
            let subfolder_on = shell.read(cx).controller.state.into_subfolder;
            let muted = cx.theme().muted_foreground;
            // Radios rather than a row of buttons where one looks pressed:
            // these are one-of-three choices, and the kit's radio says so to
            // the keyboard and to the screen reader without being told.
            let langs = [None, Some(super::Lang::En), Some(super::Lang::Es)];
            let language = {
                let weak = weak.clone();
                RadioGroup::horizontal("settings-language")
                    .selected_index(langs.iter().position(|candidate| *candidate == lang))
                    .children(vec![
                        s.theme_system,
                        super::Lang::En.label(),
                        super::Lang::Es.label(),
                    ])
                    .on_click(move |index, window, cx| {
                        let control = match index {
                            0 => SettingsControl::LangSystem,
                            1 => SettingsControl::LangEn,
                            _ => SettingsControl::LangEs,
                        };
                        let _ = weak.update(cx, |this, cx| {
                            this.settings_activate(control, window, cx);
                            cx.notify();
                        });
                    })
            };
            let themes = [
                super::ThemePreference::System,
                super::ThemePreference::Light,
                super::ThemePreference::Dark,
            ];
            let appearance = {
                let weak = weak.clone();
                RadioGroup::horizontal("settings-theme")
                    .selected_index(themes.iter().position(|candidate| *candidate == theme))
                    .children(vec![s.theme_system, s.theme_light, s.theme_dark])
                    .on_click(move |index, window, cx| {
                        let control = match index {
                            0 => SettingsControl::ThemeSystem,
                            1 => SettingsControl::ThemeLight,
                            _ => SettingsControl::ThemeDark,
                        };
                        let _ = weak.update(cx, |this, cx| {
                            this.settings_activate(control, window, cx);
                            cx.notify();
                        });
                    })
            };
            let row = |label: &'static str, control: gpui::AnyElement| {
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(div().w(px(110.)).flex_none().child(label))
                    .child(control)
            };
            let subfolder = {
                let weak = weak.clone();
                Switch::new("settings-subfolder")
                    .label(s.into_subfolder)
                    .checked(subfolder_on)
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.settings_activate(SettingsControl::Subfolder, window, cx);
                            cx.notify();
                        });
                    })
            };
            dialog
                .title(s.settings)
                .w(px(560.))
                .child(DialogDescription::new().child(s.defaults_title))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(row(s.language, language.into_any_element()))
                        .child(row(s.theme, appearance.into_any_element()))
                        .child(row(
                            s.name_encoding,
                            pick(
                                "settings-page",
                                s.name_encoding,
                                pages,
                                page,
                                true,
                                weak.clone(),
                                |this, page| this.controller.reread_names(page),
                            ),
                        ))
                        .child(Separator::horizontal())
                        .child(div().text_sm().text_color(muted).child(s.defaults_title))
                        .child(row(
                            s.format,
                            format_pick("settings-format", shell, &weak, cx),
                        ))
                        .child(row(
                            s.compressor,
                            codec_pick("settings-codec", shell, &weak, is_zip, cx),
                        ))
                        .child(row(s.level, level_pick("settings-level", shell, &weak, cx)))
                        .child(subfolder),
                )
        }
        ModalKind::NewFolder | ModalKind::Mask => {
            let (title, hint, confirm) = match kind {
                ModalKind::NewFolder => (s.new_folder, s.folder_name, s.new_folder),
                _ => {
                    let adding = shell
                        .read(cx)
                        .controller
                        .state
                        .picking_group
                        .unwrap_or(true);
                    (
                        if adding {
                            s.select_group
                        } else {
                            s.deselect_group
                        },
                        s.mask_hint,
                        s.start,
                    )
                }
            };
            let field = shell.read(cx).name_input.clone();
            let ok = weak.clone();
            dialog
                .title(title)
                .child(DialogDescription::new().child(hint))
                .child(Input::new(&field))
                .footer(
                    DialogFooter::new()
                        .child(cancel_button("name-cancel", s.cancel))
                        .child(
                            DialogAction::new()
                                .child(Button::new("name-ok").label(confirm).primary()),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let _ = ok.update(cx, |this, cx| this.confirm_name(kind, cx));
                    true
                })
        }
    }
}

/// The overflow entries that write to the archive, as a value rather than a
/// closure: the menu is built while the shell is still borrowed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OverflowAction {
    Release,
    AddFiles,
    NewFolder,
    Undo,
    SaveCopy,
    DefaultPassword,
}

/// A selection being drawn by pulling across the list.
struct Band {
    /// Where the button went down, in window coordinates.
    origin: gpui::Point<gpui::Pixels>,
    /// The row it went down on. Two row numbers rather than a rectangle: the
    /// list moves underneath while the drag happens, and a rectangle frozen
    /// where the button went down stops meaning anything the moment it does.
    anchor: usize,
    /// What was picked before it started, so that Ctrl adds to a selection and
    /// a plain drag replaces one.
    base: Vec<bool>,
    /// Where the pointer is now, which is the other end of the band.
    head: gpui::Point<gpui::Pixels>,
    /// The list as it was when the band began.
    ///
    /// Frozen for the length of the gesture rather than asked for on every
    /// pointer move: what the band picks cannot change the list it is picking
    /// from, and nothing else can change it either while a button is down.
    rows: Vec<super::Row>,
    /// Whether it has been pulled far enough to be a gesture rather than a
    /// click. A click is a drag of no distance, and under this it is left
    /// alone so clicking a row still means clicking a row.
    live: bool,
}

/// The anchor the wheel dropped, and whether the pointer has pulled away from
/// it yet. Letting the wheel go after it has ends the gesture; letting it go
/// before leaves it running until the next click, which is what makes
/// press-and-drag and click-and-go both work off the one button.
struct WheelPan {
    anchor: gpui::Point<gpui::Pixels>,
    moved: bool,
}

/// Where the list is on screen, how tall a row is and how far down it is
/// scrolled: everything the two pointer gestures need, read off the one scroll
/// handle rather than measured again.
struct ListView {
    top: f32,
    bottom: f32,
    left: f32,
    right: f32,
    row: f32,
    /// How far down the list is. GPUI keeps this as a negative offset; it is
    /// turned the right way up here so the arithmetic below reads like the
    /// list does.
    offset: f32,
    reach: f32,
}

/// How far a click may travel and still be a click.
///
/// Further than GPUI waits before calling a drag a drag, on purpose: a band
/// that appeared first would flash over the rows for the pixel or two between
/// the two thresholds every time a column was resized.
const DRAG_SLOP: f32 = 10.0;

/// The selection while it is in the air. An empty marker rather than the rows
/// themselves: what is carried is whatever is picked when it lands, and the
/// selection cannot change while the button is down.
struct DraggedRows;

/// What the kit's table needs to draw a frame of the archive.
///
/// It keeps its own copy rather than reading the shell, because the table is
/// drawn from inside the shell's own render and reading the shell there would
/// borrow it while it is already borrowed. The copy is refreshed once a frame,
/// just before the table is handed out.
struct FileTable {
    shell: WeakEntity<GpuiShell>,
    rows: Vec<super::Row>,
    columns: Vec<SortColumn>,
    widths: Vec<f32>,
    checked: Vec<bool>,
    /// Rows waiting to be moved by the clipboard, drawn in the muted ink.
    muted: Vec<bool>,
    cursor: Option<usize>,
    renaming: Option<usize>,
    rename_input: Entity<InputState>,
    order: (SortColumn, bool),
    strings: &'static Strings,
    idle: bool,
    writable: bool,
}

impl FileTable {
    fn row_is_checked(&self, index: usize) -> bool {
        self.checked.get(index).copied().unwrap_or(false)
    }
}

impl TableDelegate for FileTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let column = self.columns[col_ix];
        let slot = GpuiShell::column_slot(column);
        let mut definition = Column::new(
            gpui::SharedString::from(format!("{}", column as usize)),
            Columns::label(column, self.strings),
        )
        .sortable()
        .resizable(true)
        .min_width(px(Settings::least(slot)));
        // The name runs to the edge of the window: it is the column anybody
        // widens the window for, and a fixed one would leave a gutter.
        definition = if column == SortColumn::Name {
            definition.width(px(self.widths.get(slot).copied().unwrap_or(240.)))
        } else {
            definition.width(px(self.widths.get(slot).copied().unwrap_or(96.)))
        };
        if self.order.0 == column {
            definition = if self.order.1 {
                definition.ascending()
            } else {
                definition.descending()
            };
        }
        definition
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        _: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some(column) = self.columns.get(col_ix).copied() else {
            return;
        };
        let _ = self.shell.update(cx, |shell, cx| {
            if shell.background_idle() {
                shell.controller.dispatch(AppAction::Sort(column));
                cx.notify();
            }
        });
    }

    /// The header cell, named the way a header that sorts has to be named: the
    /// kit draws the arrow, but only the shell knows to say which way it points.
    ///
    /// The whole cell sorts, not just the kit's little arrow: pressing the name
    /// of a column is how a file list has sorted since before any of this.
    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(column) = self.columns.get(col_ix).copied() else {
            return div().id("header-cell");
        };
        let s = self.strings;
        let label = Columns::label(column, s);
        let role = Role::ColumnHeader;
        let accessible = if self.order.0 == column {
            let direction = if self.order.1 {
                s.ascending
            } else {
                s.descending
            };
            format!("{} {label} ({direction})", s.sort_by)
        } else {
            format!("{} {label}", s.sort_by)
        };
        div()
            .id(("header-cell", col_ix))
            .role(role)
            .aria_column_index(col_ix + 1)
            .aria_keyshortcuts("Enter")
            .size_full()
            .flex()
            .items_center()
            .cursor_pointer()
            .aria_label(accessible)
            .on_click(cx.listener(move |table, _, window, cx| {
                table
                    .delegate_mut()
                    .perform_sort(col_ix, ColumnSort::Default, window, cx);
            }))
            .child(label)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let cell = || div().id(("file-cell", row_ix * 16 + col_ix));
        let Some(row) = self.rows.get(row_ix) else {
            return cell();
        };
        let Some(column) = self.columns.get(col_ix).copied() else {
            return cell();
        };
        let muted = self.muted.get(row_ix).copied().unwrap_or(false);
        let ink = if muted {
            cx.theme().muted_foreground
        } else {
            cx.theme().foreground
        };
        if column != SortColumn::Name {
            return cell()
                .role(Role::Cell)
                .aria_column_index(col_ix + 1)
                .text_sm()
                .text_color(ink)
                .child(column_text(row, column, self.strings));
        }
        // Renaming happens here rather than in a dialog, so the name being
        // typed stays where the name is.
        if self.renaming == Some(row_ix) {
            return cell().child(Input::new(&self.rename_input));
        }
        cell()
            .role(Role::Cell)
            .aria_column_index(1)
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .text_color(ink)
            .child({
                let (icon, color) = GpuiShell::kind_icon(row.kind);
                div()
                    .w(px(18.))
                    .flex_none()
                    .child(Icon::new(icon).size(px(16.)).text_color(gpui::rgb(color)))
            })
            .child(div().flex_1().truncate().child(row.label.clone()))
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<gpui::Div> {
        let checked = self.row_is_checked(row_ix);
        // The banding is drawn here rather than by the kit, which stripes the
        // whole pane: rows that do not exist should not be drawn as rows.
        let mut item = div().id(("file-row", row_ix)).bg(if checked {
            cx.theme().table_active
        } else if row_ix % 2 == 1 {
            cx.theme().table_even
        } else {
            cx.theme().table
        });
        if self.cursor == Some(row_ix) {
            item = item.border_1().border_color(cx.theme().table_active_border);
        }
        let Some(row) = self.rows.get(row_ix).cloned() else {
            return item;
        };
        // Whether the row is marked is half of what this list is about, so it
        // is said out loud rather than left to the colour of the background.
        let s = self.strings;
        let mut description = format!("{} {}", s.col_name, row.label);
        for column in self.columns.iter().copied().skip(1) {
            let label = Columns::label(column, s);
            description.push_str(&format!("; {label} {}", column_text(&row, column, s)));
        }
        description.push_str("; ");
        description.push_str(if checked { s.checked } else { s.not_checked });
        item = item
            .aria_label(description)
            .role(Role::Row)
            // The header is row one, so the rows below it start at two.
            .aria_row_index(row_ix + 2);
        if self.idle {
            item = item.focusable();
            if self.cursor == Some(row_ix) {
                item = item.aria_active_descendant();
            }
        }
        // One row at a time is the kit's idea of a click; control, shift and a
        // plain click each mean something different to a file list, and that
        // stays the shell's to decide.
        if self.idle {
            let shell = self.shell.clone();
            item = item.on_click(move |event: &ClickEvent, window, cx| {
                let _ = shell.update(cx, |shell, cx| {
                    shell.select_row(row_ix, event, window, cx);
                });
            });
        }
        // A folder takes what is dropped on it and the entries move there,
        // which is a rewrite of the archive and not a copy out of it. Dropping
        // a folder into itself is not a move, so it is refused.
        if row.is_dir && self.idle {
            let target = row.path.clone();
            let shell = self.shell.clone();
            item = item.on_drop(move |_: &DraggedRows, _, cx| {
                let _ = shell.update(cx, |shell, cx| {
                    let carried = shell.controller.selected_roots();
                    let into_itself = carried.iter().any(|carried| {
                        carried.trim_end_matches('/') == target.trim_end_matches('/')
                    });
                    if !carried.is_empty() && !into_itself {
                        shell.controller.move_into(&carried, &target);
                    }
                    cx.notify();
                });
            });
        }
        // GPUI owns the threshold and the gesture's lifetime. Where the drag is
        // going is not decided here: a folder of this archive takes it as a
        // move, and leaving the list hands it to arca-drag's lazy IDataObject,
        // so no archive bytes are extracted merely to begin a drag.
        if checked && self.idle {
            let shell = self.shell.clone();
            item = item.on_drag(DraggedRows, move |_, _, _, app| {
                let _ = shell.update(app, |shell, _| shell.carrying = true);
                app.new(|_| gpui::Empty)
            });
        }
        item
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let is_file = self.rows.get(row_ix).is_some_and(|row| row.entry.is_some());
        let writable = self.writable;
        let strings = self.strings;
        let shell = self.shell.clone();
        let mut menu = menu;
        let mut drawn = false;
        for action in RowAction::ALL.into_iter().filter(|action| {
            action.offered()
                && (*action != RowAction::Rename || writable)
                && (*action != RowAction::View || is_file)
        }) {
            // Never as the first thing in the menu: a rule with nothing above
            // it is a line, not a grouping.
            if action.starts_group() && drawn {
                menu = menu.item(PopupMenuItem::separator());
            }
            drawn = true;
            let (label, keys) = action.label(strings);
            let shell = shell.clone();
            menu = menu.item(
                PopupMenuItem::element(move |_, cx| {
                    // Two children rather than one string with a tab in it:
                    // GPUI lays out text and a tab is nothing at all there.
                    let mut row = div().flex().w_full().gap_4().items_center();
                    row = row.child(div().flex_1().truncate().child(label));
                    if !keys.is_empty() {
                        row = row.child(
                            div()
                                .flex_none()
                                .text_color(cx.theme().muted_foreground)
                                .child(keys),
                        );
                    }
                    row
                })
                .on_click(move |_, window, cx| {
                    let _ = shell.update(cx, |shell, cx| {
                        shell.row_action(action, row_ix, window, cx);
                    });
                }),
            );
        }
        menu
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        match (self.rows.get(row_ix), self.columns.get(col_ix).copied()) {
            (Some(row), Some(column)) => column_text(row, column, self.strings),
            _ => String::new(),
        }
    }
}

/// What a key press means to the window, as opposed to the list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shortcut {
    Open,
    Compress,
    ExtractAll,
    ExtractHere,
    Test,
    Refresh,
    Invert,
    ClearSelection,
    CopyNames,
    Shortcuts,
    Undo,
    DefaultPassword,
    Rename,
    View,
    /// A mask that picks names, or one that drops them.
    PickGroup(bool),
}

/// Reading a key press, with no state and no side effects, so the table of
/// shortcuts can be checked without a window.
///
/// `typing` only silences the keys that carry no modifier: F5 inside a filter
/// box is a key, but Ctrl+O is never text.
fn shortcut_for(
    secondary: bool,
    shift: bool,
    alt: bool,
    key: &str,
    typing: bool,
) -> Option<Shortcut> {
    if secondary && shift {
        return (key == "c").then_some(Shortcut::CopyNames);
    }
    if secondary {
        return match key {
            "o" => Some(Shortcut::Open),
            "n" => Some(Shortcut::Compress),
            "e" => Some(Shortcut::ExtractAll),
            "t" => Some(Shortcut::Test),
            "i" => Some(Shortcut::Invert),
            "z" => Some(Shortcut::Undo),
            "p" => Some(Shortcut::DefaultPassword),
            _ => None,
        };
    }
    if alt {
        return (key == "w").then_some(Shortcut::ExtractHere);
    }
    if typing {
        return None;
    }
    match key {
        "f1" => Some(Shortcut::Shortcuts),
        "f2" => Some(Shortcut::Rename),
        "f3" => Some(Shortcut::View),
        "f5" => Some(Shortcut::Refresh),
        "escape" => Some(Shortcut::ClearSelection),
        // The keypad plus and minus, where WinRAR has kept picking a group by
        // name since before there were menus to put it in. Its third one, the
        // keypad star for inverting, cannot be told from any other asterisk by
        // a toolkit, so that one stays on Ctrl+I alone.
        "+" | "plus" | "add" => Some(Shortcut::PickGroup(true)),
        "-" | "minus" | "subtract" => Some(Shortcut::PickGroup(false)),
        _ => None,
    }
}

/// How tall one row is, from the height of everything and how many there are.
///
/// Not `last_item_size.item`, whatever that name suggests: that field holds the
/// size of the whole viewport. Reading it as a row height divided by four
/// hundred instead of by twenty-eight, which put every pointer on the first row
/// and left the band picking nothing.
fn row_height(contents: f32, count: usize) -> Option<f32> {
    if count == 0 || contents <= 0.0 {
        return None;
    }
    let row = contents / count as f32;
    (row > 0.0).then_some(row)
}

/// The two ends of a span, in the order they are drawn in.
fn minmax(a: gpui::Pixels, b: gpui::Pixels) -> (f32, f32) {
    let (a, b) = (f32::from(a), f32::from(b));
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn background_event_allowed(modal: bool, native_picker: bool) -> bool {
    !modal && !native_picker
}

fn clipboard_action_allowed(
    available: bool,
    idle: bool,
    has_archive: bool,
    selected: usize,
    needs_selection: bool,
) -> bool {
    available && idle && has_archive && (!needs_selection || selected > 0)
}

fn drop_paths_for_enter(paths: &[PathBuf], allowed: bool) -> Vec<PathBuf> {
    if allowed {
        paths.to_vec()
    } else {
        Vec::new()
    }
}

fn empty_state_aria_label(error: bool, filter: &str, s: &'static Strings) -> &'static str {
    if error {
        s.cannot_open
    } else if filter.trim().is_empty() {
        s.empty_folder
    } else {
        s.no_matches
    }
}

fn apply_startup(controller: &mut AppController, startup: Startup) {
    match startup {
        Startup::Browse(Some(path)) => controller.open(path),
        Startup::Browse(None) => {}
        Startup::Run(job) => controller.run_job(job),
        Startup::Add(files) => controller.dispatch(AppAction::PrepareCompress(files)),
    }
}

fn shell_size(compact: bool) -> (f32, f32) {
    if compact {
        COMPACT_SIZE
    } else {
        NORMAL_SIZE
    }
}

pub(crate) fn run() {
    let startup = super::parse_args();
    let compact = !matches!(startup, Startup::Browse(_));
    let (width, height) = shell_size(compact);

    // The kit's icons are SVG assets, not glyphs; without an asset source every
    // `IconName` resolves to nothing and the toolbar renders blank.
    application()
        .with_assets(gpui_kit_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_theme::init(cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-f", FocusFilter, None),
                KeyBinding::new("cmd-f", FocusFilter, None),
                KeyBinding::new("ctrl-c", CopyFiles, None),
                KeyBinding::new("cmd-c", CopyFiles, None),
                KeyBinding::new("ctrl-x", CutFiles, None),
                KeyBinding::new("cmd-x", CutFiles, None),
                KeyBinding::new("ctrl-v", PasteFiles, None),
                KeyBinding::new("cmd-v", PasteFiles, None),
            ]);
            let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
            let window = cx
                .open_window(
                    // The bar across the top is Arca's, not the system's: the
                    // kit's options make the native caption transparent and
                    // hand the dragging, the double click and the three window
                    // buttons to `TitleBar`, which is drawn from the same
                    // tokens as everything below it.
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(MINIMUM_SIZE.0), px(MINIMUM_SIZE.1))),
                        ..TitleBar::window_options()
                    },
                    |window, cx| {
                        // The kit's `Root` has to be the window's first view:
                        // dialogs, sheets and notifications are looked up
                        // through it, and every kit component that opens one
                        // panics without it.
                        let shell = cx.new(|cx| GpuiShell::new(window, cx, startup));
                        let shell_focus = shell.read(cx).focus_handle.clone();
                        window.focus(&shell_focus, cx);
                        window.set_window_title("Arca");
                        let frame = cx.new(|_| Frame { shell });
                        cx.new(|cx| Root::new(frame, window, cx))
                    },
                )
                .expect("open GPUI shell window");
            window
                .update(cx, |_, _, cx| {
                    cx.activate(true);
                })
                .expect("activate GPUI shell window");
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_keeps_compact_and_normal_startup_sizes() {
        assert_eq!(shell_size(true), COMPACT_SIZE);
        assert_eq!(shell_size(false), NORMAL_SIZE);
        assert!(MINIMUM_SIZE.0 <= NORMAL_SIZE.0);
        assert!(MINIMUM_SIZE.1 <= NORMAL_SIZE.1);
    }

    #[test]
    fn breadcrumbs_keep_root_and_nearest_folders() {
        assert_eq!(
            GpuiShell::visible_crumb_indices(4),
            ((0..4).collect(), Vec::new())
        );
        assert_eq!(
            GpuiShell::visible_crumb_indices(6),
            (vec![0, 3, 4, 5], vec![1, 2])
        );
    }

    #[test]
    fn conflict_actions_answer_every_supported_choice() {
        let answers = [
            Answer::Replace,
            Answer::ReplaceAll,
            Answer::Skip,
            Answer::SkipAll,
            Answer::Rename,
            Answer::RenameAll,
            Answer::Cancel,
        ];
        for answer in answers {
            let mut controller = AppController::new(Settings::default());
            let (tx, rx) = channel();
            controller.state.replies = Some(tx);
            controller.state.conflict = Some("already-there.txt".into());
            controller.dispatch(AppAction::AnswerConflict(answer));
            assert_eq!(rx.recv().unwrap(), answer);
            assert!(controller.state.conflict.is_none());
        }
    }

    #[test]
    fn background_events_are_blocked_by_gpui_modals_and_native_pickers() {
        assert!(background_event_allowed(false, false));
        assert!(!background_event_allowed(true, false));
        assert!(!background_event_allowed(false, true));
        assert!(!background_event_allowed(true, true));
    }

    #[test]
    fn clipboard_actions_require_capability_idle_archive_and_selection() {
        assert!(clipboard_action_allowed(true, true, true, 1, true));
        assert!(!clipboard_action_allowed(false, true, true, 1, true));
        assert!(!clipboard_action_allowed(true, false, true, 1, true));
        assert!(!clipboard_action_allowed(true, true, false, 1, true));
        assert!(!clipboard_action_allowed(true, true, true, 0, true));
        assert!(clipboard_action_allowed(true, true, true, 0, false));
        assert!(!clipboard_action_allowed(true, true, false, 0, false));
    }

    #[test]
    fn blocked_drop_enter_clears_pending_paths() {
        let paths = vec![PathBuf::from("queued.zip")];
        assert_eq!(drop_paths_for_enter(&paths, true), paths);
        assert!(drop_paths_for_enter(&paths, false).is_empty());
    }

    #[test]
    fn the_row_height_comes_from_the_content_and_not_from_the_viewport() {
        // A list showing fifty rows of twenty-eight pixels has fourteen hundred
        // pixels of content and a viewport of whatever the window left it. The
        // viewport is what `last_item_size.item` holds, so reading that as a
        // row height divided by four hundred instead of by twenty-eight: every
        // pointer landed on the first row and the band picked nothing.
        assert_eq!(row_height(1400.0, 50), Some(28.0));
        assert_eq!(row_height(1400.0, 0), None, "an empty list has no rows");
        assert_eq!(row_height(0.0, 50), None, "nor has one not laid out yet");
    }

    #[test]
    fn a_point_lands_on_the_row_that_is_drawn_under_it() {
        // The list is virtualized, so which row a pointer is over is arithmetic
        // on the scroll offset and not a rectangle anybody kept. Off by one row
        // here and a band would pick everything one place along.
        let view = ListView {
            top: 100.0,
            bottom: 360.0,
            left: 0.0,
            right: 800.0,
            row: 26.0,
            offset: 0.0,
            reach: 1000.0,
        };
        assert_eq!(GpuiShell::row_under(&view, 100.0, 50), Some(0));
        assert_eq!(GpuiShell::row_under(&view, 125.9, 50), Some(0));
        assert_eq!(GpuiShell::row_under(&view, 126.0, 50), Some(1));
        // Above the first row is no row at all, not the first one.
        assert_eq!(GpuiShell::row_under(&view, 99.0, 50), None);
        // And past the last there is nothing either, however far down it is.
        assert_eq!(GpuiShell::row_under(&view, 100.0, 0), None);
        assert_eq!(GpuiShell::row_under(&view, 100.0 + 26.0 * 3.0, 3), None);

        // Scrolled down by ten rows, the top of the list is row ten.
        let scrolled = ListView {
            offset: 260.0,
            ..view
        };
        assert_eq!(GpuiShell::row_under(&scrolled, 100.0, 50), Some(10));
        assert_eq!(GpuiShell::row_under(&scrolled, 126.0, 50), Some(11));
    }

    #[test]
    fn every_column_reads_its_width_from_its_own_slot() {
        // The widths vector is the one written to gui.conf: the name first,
        // then `Columns::ALL` in order. A slot off by one would hand a column
        // the width of its neighbour and the file would still load.
        assert_eq!(GpuiShell::column_slot(SortColumn::Name), 0);
        for (index, (column, _)) in Columns::ALL.iter().enumerate() {
            assert_eq!(GpuiShell::column_slot(*column), index + 1);
        }
        let widths = Settings::default_widths();
        assert_eq!(widths.len(), Columns::ALL.len() + 1);
        // A column cannot be pulled below what it needs to stay readable.
        assert!(Settings::least(0) > Settings::least(1));
    }

    #[test]
    fn the_row_menu_only_offers_the_clipboard_where_there_is_one() {
        // A menu entry that can never do anything is worse than no entry, so
        // copy, cut and paste are left out rather than greyed out.
        let offered: Vec<bool> = RowAction::ALL.iter().map(|a| a.offered()).collect();
        for action in [RowAction::Copy, RowAction::Cut, RowAction::Paste] {
            assert_eq!(offered[action as usize], clipboard::AVAILABLE);
        }
        assert!(offered[RowAction::Open as usize]);
        assert!(offered[RowAction::Delete as usize]);
        // The element id of each menu row is built from `action as usize`, so
        // an action added out of order would silently share an id.
        for (index, action) in RowAction::ALL.iter().enumerate() {
            assert_eq!(*action as usize, index);
        }
    }

    #[test]
    fn every_folder_of_the_tree_is_named_by_the_path_it_navigates_to() {
        // The id is what `Navigate` is dispatched with, so a nested folder
        // whose id is only its own name would send the window to the wrong
        // place -- or nowhere.
        let mut root = Folder::default();
        let src = root.kids.entry("src".to_string()).or_default();
        src.kids.entry("deep".to_string()).or_default();
        root.kids.entry("docs".to_string()).or_default();

        let items = folder_items(&root, "");
        let ids = items
            .iter()
            .map(|item| item.id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["docs/".to_string(), "src/".to_string()]);
        let nested = &items[1].children;
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].id.to_string(), "src/deep/");
        assert_eq!(nested[0].label.to_string(), "deep");
    }

    #[test]
    fn a_modifier_shortcut_still_works_while_a_text_field_has_the_keyboard() {
        // Ctrl+O is never text, so it must not wait behind the filter box; F5
        // is a key a text field could want, so it must.
        assert_eq!(
            shortcut_for(true, false, false, "o", true),
            Some(Shortcut::Open)
        );
        assert_eq!(
            shortcut_for(true, true, false, "c", true),
            Some(Shortcut::CopyNames)
        );
        assert_eq!(
            shortcut_for(false, false, true, "w", true),
            Some(Shortcut::ExtractHere)
        );
        assert_eq!(shortcut_for(false, false, false, "f5", true), None);
        assert_eq!(shortcut_for(false, false, false, "escape", true), None);
        assert_eq!(
            shortcut_for(false, false, false, "f5", false),
            Some(Shortcut::Refresh)
        );
        // Ctrl+Shift+C is the names as text, not the files.
        assert_eq!(shortcut_for(true, true, false, "o", false), None);
        assert_eq!(shortcut_for(false, false, false, "q", false), None);
        // The keypad's plus picks a group and its minus drops one; both are
        // bare keys, so both stand aside for a text field.
        assert_eq!(
            shortcut_for(false, false, false, "+", false),
            Some(Shortcut::PickGroup(true))
        );
        assert_eq!(
            shortcut_for(false, false, false, "minus", false),
            Some(Shortcut::PickGroup(false))
        );
        assert_eq!(shortcut_for(false, false, false, "+", true), None);
        assert_eq!(
            shortcut_for(true, false, false, "z", false),
            Some(Shortcut::Undo)
        );
        assert_eq!(
            shortcut_for(false, false, false, "f3", false),
            Some(Shortcut::View)
        );
    }

    #[test]
    fn every_settings_control_has_its_own_slot_in_draw_order() {
        // The dialog builds each control's element id from `control as
        // usize`, so a control added out of order would silently share an
        // id with another one.
        for (index, control) in SettingsControl::ALL.iter().enumerate() {
            assert_eq!(*control as usize, index);
        }
    }

    #[test]
    fn empty_folder_label_is_not_reported_as_archive_contents() {
        // Both languages, because the point of the label is that it says the
        // folder is empty rather than that the archive is unreadable, and a
        // translation that loses the difference is the same bug in Spanish.
        for lang in [super::super::Lang::En, super::super::Lang::Es] {
            let s = super::super::strings(lang);
            assert_eq!(empty_state_aria_label(false, "", s), s.empty_folder);
            assert_eq!(empty_state_aria_label(false, "  ", s), s.empty_folder);
            assert_eq!(empty_state_aria_label(false, "zip", s), s.no_matches);
            assert_eq!(empty_state_aria_label(true, "", s), s.cannot_open);
            assert_ne!(s.empty_folder, s.cannot_open);
        }
    }
}
