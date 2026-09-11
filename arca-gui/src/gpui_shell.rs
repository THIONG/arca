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
    canvas, div, prelude::*, px, size, uniform_list, App, Bounds, ClickEvent, Context, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable,
    GlobalElementId, KeyBinding, KeyDownEvent, LayoutId, Role, ScrollStrategy, ShapedLine,
    Stateful, Style, TextRun, UTF16Selection, UniformListScrollHandle, WeakEntity, Window,
    WindowBounds, WindowOptions,
};
use gpui_component::separator::Separator;
use gpui_component::sidebar::{SidebarItem, SidebarMenu, SidebarMenuItem};
use gpui_component::status_bar::StatusBar;
use gpui_component::{ActiveTheme, Icon, IconName};
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

/// Where the recently opened archives start in `overflow_item_focus`, and how
/// many of them the menu keeps room for. Ten is about as many as anybody scans
/// before giving up and going to the folder instead.
const RECENT_SLOT: usize = 12;
const RECENT_MAX: usize = 10;

/// The offer of a newer Arca, which is only drawn when there is one. Parked
/// past the columns rather than at the front, so that adding it does not move
/// every other slot along.
const RELEASE_SLOT: usize = RECENT_SLOT + RECENT_MAX + 4 + Columns::ALL.len();

actions!(
    arca_gpui,
    [
        Backspace,
        SelectAll,
        FocusFilter,
        CopyFiles,
        CutFiles,
        PasteFiles
    ]
);

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

/// The smallest native text input GPUI needs for this surface. It follows the
/// same UTF-16 contract used by platform IMEs, while the controller stores UTF-8.
#[derive(Clone, Copy)]
enum TextFieldKind {
    Filter,
    Password,
    OutputName,
    AddPassword,
    /// The one field shared by the dialogs that ask for a piece of text: a new
    /// folder, a new name, a mask. They are modal and mutually exclusive, so
    /// one field with the name the open dialog gives it is one field, not
    /// three that are always empty.
    Name,
}

struct FilterInput {
    owner: WeakEntity<GpuiShell>,
    kind: TextFieldKind,
    /// The field reads its own name out to a screen reader, so it needs the
    /// language too. The shell pushes it in on every frame, because the
    /// settings dialog can change it while the window is up.
    strings: &'static Strings,
    /// What a screen reader calls this field. Set by the shell each frame,
    /// because the shared `Name` field is a folder name in one dialog and a
    /// mask in another.
    label: &'static str,
    masked: bool,
    focus_handle: FocusHandle,
    enabled: bool,
    content: String,
    selected_range: Range<usize>,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<gpui::Pixels>>,
}

impl FilterInput {
    fn utf8_from_utf16(text: &str, offset: usize) -> usize {
        let mut utf16 = 0;
        for (index, character) in text.char_indices() {
            if offset <= utf16 {
                return index;
            }
            utf16 += character.len_utf16();
            if offset < utf16 {
                return index;
            }
        }
        text.len()
    }

    fn utf16_from_utf8(text: &str, offset: usize) -> usize {
        text[..offset.min(text.len())]
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    fn utf8_range(text: &str, range: Range<usize>) -> Range<usize> {
        Self::utf8_from_utf16(text, range.start)..Self::utf8_from_utf16(text, range.end)
    }

    fn utf16_range(text: &str, range: Range<usize>) -> Range<usize> {
        Self::utf16_from_utf8(text, range.start)..Self::utf16_from_utf8(text, range.end)
    }

    fn sync_from_state(&mut self, value: &str) {
        self.content.clear();
        self.content.push_str(value);
        let cursor = self.content.len();
        self.selected_range = cursor..cursor;
        self.marked_range = None;
    }

    fn replace(&mut self, range: Range<usize>, value: &str, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        self.content.replace_range(range.clone(), value);
        let cursor = range.start + value.len();
        self.selected_range = cursor..cursor;
        self.marked_range = None;
        self.push_to_owner(cx);
        cx.notify();
    }

    /// Hands what was typed back to whoever owns it.
    ///
    /// One copy, not one per entry point: a plain keystroke, an IME commit and
    /// a marked-text edit all end here, and three copies of the same match is
    /// three places for a new field to be forgotten in.
    fn push_to_owner(&self, cx: &mut Context<Self>) {
        let content = self.content.clone();
        let kind = self.kind;
        let _ = self.owner.update(cx, |shell, cx| {
            match kind {
                TextFieldKind::Filter => shell.controller.dispatch(AppAction::SetFilter(content)),
                TextFieldKind::Password => shell
                    .controller
                    .dispatch(AppAction::SetPasswordInput(content)),
                TextFieldKind::OutputName => shell.controller.state.output_name = content,
                TextFieldKind::AddPassword => shell.controller.state.add_password = content,
                TextFieldKind::Name => shell.name_value = content,
            }
            cx.notify();
        });
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected_range.is_empty() {
            let Some((start, _)) = self.content[..self.selected_range.start]
                .char_indices()
                .next_back()
            else {
                window.play_system_bell();
                return;
            };
            start..self.selected_range.end
        } else {
            self.selected_range.clone()
        };
        self.replace(range, "", cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        cx.notify();
    }
}

impl EntityInputHandler for FilterInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = Self::utf8_range(&self.content, range_utf16);
        actual_range.replace(Self::utf16_range(&self.content, range.clone()));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: Self::utf16_range(&self.content, self.selected_range.clone()),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| Self::utf16_range(&self.content, range.clone()))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        let range = range_utf16
            .map(|range| Self::utf8_range(&self.content, range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.replace(range, new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        let range = range_utf16
            .map(|range| Self::utf8_range(&self.content, range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.content.replace_range(range.clone(), new_text);
        self.marked_range =
            (!new_text.is_empty()).then_some(range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .map(|selected| {
                let selected = Self::utf8_range(new_text, selected);
                range.start + selected.start..range.start + selected.end
            })
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.push_to_owner(cx);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<gpui::Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<gpui::Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = Self::utf8_range(&self.content, range_utf16);
        Some(Bounds::from_corners(
            point(bounds.left() + line.x_for_index(range.start), bounds.top()),
            point(bounds.left() + line.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<gpui::Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if self.content.is_empty() {
            return Some(0);
        }
        let bounds = self.last_bounds?;
        let line = self.last_layout.as_ref()?;
        let local_point = bounds.localize(&point)?;
        Some(Self::utf16_from_utf8(
            &self.content,
            line.closest_index_for_x(local_point.x),
        ))
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(Self::utf16_from_utf8(&self.content, self.content.len()))
    }
}

struct FilterElement {
    input: Entity<FilterInput>,
}

struct FilterPrepaint {
    line: Option<ShapedLine>,
}

impl IntoElement for FilterElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for FilterElement {
    type RequestLayoutState = ();
    type PrepaintState = FilterPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<gpui::Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let value = if input.content.is_empty() {
            match input.kind {
                TextFieldKind::Filter => input.strings.filter_hint.to_string(),
                TextFieldKind::Password => input.strings.password_hint.to_string(),
                TextFieldKind::OutputName => "archive.zip".to_string(),
                TextFieldKind::AddPassword => input.strings.password_optional.to_string(),
                TextFieldKind::Name => input.label.to_string(),
            }
        } else if matches!(
            input.kind,
            TextFieldKind::Password | TextFieldKind::AddPassword
        ) && input.masked
        {
            "•".repeat(input.content.chars().count())
        } else {
            input.content.clone()
        };
        let style = window.text_style();
        let line = window.text_system().shape_line(
            value.clone().into(),
            style.font_size.to_pixels(window.rem_size()),
            &[TextRun {
                len: value.len(),
                font: style.font(),
                color: style.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        FilterPrepaint { line: Some(line) }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<gpui::Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus_handle, cursor) = {
            let input = self.input.read(cx);
            (input.focus_handle.clone(), input.selected_range.end)
        };
        if self.input.read(cx).enabled {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }
        let line = prepaint.line.take().expect("filter line");
        let cursor_x = line.x_for_index(cursor);
        line.paint(
            bounds.origin,
            window.line_height(),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
        if focus_handle.is_focused(window) {
            window.paint_quad(gpui::fill(
                Bounds::new(
                    point(bounds.left() + cursor_x, bounds.top()),
                    size(px(2.), bounds.size.height),
                ),
                cx.theme().caret,
            ));
        }
        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for FilterInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut input = div()
            .id(match self.kind {
                TextFieldKind::Filter => "filter-input",
                TextFieldKind::Password => "password-input",
                TextFieldKind::OutputName => "output-name-input",
                TextFieldKind::AddPassword => "add-password-input",
                TextFieldKind::Name => "name-input",
            })
            .key_context("FilterInput")
            .aria_label(self.label)
            .aria_value(
                if matches!(
                    self.kind,
                    TextFieldKind::Password | TextFieldKind::AddPassword
                ) {
                    self.strings.password_word.into()
                } else {
                    self.content.clone()
                },
            )
            .track_focus(&self.focus_handle)
            .tab_stop(self.enabled)
            .border_1()
            .border_color(cx.theme().input)
            .bg(cx.theme().input_background())
            .rounded(cx.theme().radius)
            .px_2()
            .flex()
            .items_center()
            .h(px(26.))
            .w_full()
            .text_xs()
            .child(FilterElement { input: cx.entity() });
        if self.enabled {
            input = input
                .role(Role::TextInput)
                .focusable()
                .on_action(cx.listener(Self::backspace))
                .on_action(cx.listener(Self::select_all));
        }
        input
    }
}

impl Focusable for FilterInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

struct GpuiShell {
    controller: AppController,
    filter: Entity<FilterInput>,
    password: Entity<FilterInput>,
    output_name: Entity<FilterInput>,
    add_password: Entity<FilterInput>,
    name_input: Entity<FilterInput>,
    focus_handle: FocusHandle,
    list_focus: FocusHandle,
    list_scroll: UniformListScrollHandle,
    dialog: Option<Receiver<DialogResult>>,
    overflow_open: bool,
    breadcrumbs_open: bool,
    overflow_trigger_focus: FocusHandle,
    breadcrumbs_trigger_focus: FocusHandle,
    open_trigger_focus: FocusHandle,
    compress_trigger_focus: FocusHandle,
    extract_all_trigger_focus: FocusHandle,
    extract_selected_trigger_focus: FocusHandle,
    password_trigger_focus: FocusHandle,
    delete_trigger_focus: FocusHandle,
    drop_trigger_focus: FocusHandle,
    conflict_trigger_focus: FocusHandle,
    overflow_menu_focus: FocusHandle,
    breadcrumbs_menu_focus: FocusHandle,
    overflow_item_focus: Vec<FocusHandle>,
    breadcrumbs_item_focus: Vec<FocusHandle>,
    dialog_return_focus: FocusHandle,
    modal_seen: Option<ModalKind>,
    password_toggle_focus: FocusHandle,
    dialog_primary_focus: FocusHandle,
    dialog_secondary_focus: FocusHandle,
    dialog_tertiary_focus: FocusHandle,
    dialog_quaternary_focus: FocusHandle,
    dialog_rename_focus: FocusHandle,
    dialog_rename_all_focus: FocusHandle,
    dialog_cancel_focus: FocusHandle,
    add_format_focus: FocusHandle,
    add_codec_focus: FocusHandle,
    add_level_focus: FocusHandle,
    add_password_toggle_focus: FocusHandle,
    add_start_focus: FocusHandle,
    add_cancel_focus: FocusHandle,
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
    /// Which column edge is in hand: its slot in `Settings::widths`, where the
    /// pointer was when it was grabbed, and how wide the column was then.
    /// Kept as the state at the grab rather than as a running delta, so a
    /// dropped mouse-move event cannot make the column drift.
    resizing: Option<(usize, f32, f32)>,
    row_menu: Option<(usize, gpui::Point<gpui::Pixels>)>,
    row_menu_focus: FocusHandle,
    row_menu_item_focus: Vec<FocusHandle>,
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

    /// Copy, cut and paste are left out rather than greyed out where the shell
    /// has nowhere to put them: a menu entry that can never do anything is
    /// worse than no entry.
    fn offered(self) -> bool {
        !matches!(self, RowAction::Copy | RowAction::Cut | RowAction::Paste)
            || clipboard::AVAILABLE
    }
}

/// A control in the settings dialog, in draw order. The index into
/// `settings_focus` is `control as usize`, so the keyboard and the mouse reach
/// the same code instead of two copies of it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsControl {
    LangSystem,
    LangEn,
    LangEs,
    ThemeSystem,
    ThemeLight,
    ThemeDark,
    Format,
    Codec,
    Level,
    Subfolder,
    /// Which code page an unflagged zip has its names written in. Only the
    /// person looking at the archive can know, so it is a choice, and a choice
    /// that is remembered is a setting.
    NamePage,
    Close,
}

impl SettingsControl {
    const ALL: [SettingsControl; 12] = [
        SettingsControl::LangSystem,
        SettingsControl::LangEn,
        SettingsControl::LangEs,
        SettingsControl::ThemeSystem,
        SettingsControl::ThemeLight,
        SettingsControl::ThemeDark,
        SettingsControl::Format,
        SettingsControl::Codec,
        SettingsControl::Level,
        SettingsControl::Subfolder,
        SettingsControl::NamePage,
        SettingsControl::Close,
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
    Rename,
    Mask,
    DefaultPassword,
    Settings,
    Shortcuts,
}

impl GpuiShell {
    fn new(window: &mut Window, cx: &mut Context<Self>, startup: Startup) -> Self {
        let owner = cx.weak_entity();
        let strings = super::strings(Settings::load().effective_lang());
        let filter = cx.new(|cx| FilterInput {
            owner: owner.clone(),
            strings,
            label: strings.find_word,
            focus_handle: cx.focus_handle(),
            enabled: true,
            kind: TextFieldKind::Filter,
            masked: false,
            content: String::new(),
            selected_range: 0..0,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
        });
        let password = cx.new(|cx| FilterInput {
            owner: owner.clone(),
            strings,
            label: strings.password_word,
            focus_handle: cx.focus_handle(),
            enabled: false,
            kind: TextFieldKind::Password,
            masked: true,
            content: String::new(),
            selected_range: 0..0,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
        });
        let output_name = cx.new(|cx| FilterInput {
            owner: owner.clone(),
            strings,
            label: strings.output_name,
            focus_handle: cx.focus_handle(),
            enabled: false,
            kind: TextFieldKind::OutputName,
            masked: false,
            content: String::new(),
            selected_range: 0..0,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
        });
        let add_password = cx.new(|cx| FilterInput {
            owner: owner.clone(),
            strings,
            label: strings.password_optional,
            focus_handle: cx.focus_handle(),
            enabled: false,
            kind: TextFieldKind::AddPassword,
            masked: true,
            content: String::new(),
            selected_range: 0..0,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
        });
        let name_input = cx.new(|cx| FilterInput {
            owner,
            strings,
            label: strings.folder_name,
            focus_handle: cx.focus_handle(),
            enabled: false,
            kind: TextFieldKind::Name,
            masked: false,
            content: String::new(),
            selected_range: 0..0,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
        });
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
            password,
            output_name,
            add_password,
            name_input,
            focus_handle: cx.focus_handle(),
            list_focus: cx.focus_handle(),
            list_scroll: UniformListScrollHandle::new(),
            dialog: None,
            overflow_open: false,
            breadcrumbs_open: false,
            overflow_trigger_focus: cx.focus_handle(),
            breadcrumbs_trigger_focus: cx.focus_handle(),
            open_trigger_focus: cx.focus_handle().tab_stop(true),
            compress_trigger_focus: cx.focus_handle().tab_stop(true),
            extract_all_trigger_focus: cx.focus_handle().tab_stop(true),
            extract_selected_trigger_focus: cx.focus_handle().tab_stop(true),
            password_trigger_focus: cx.focus_handle().tab_stop(true),
            delete_trigger_focus: cx.focus_handle(),
            drop_trigger_focus: cx.focus_handle(),
            conflict_trigger_focus: cx.focus_handle(),
            overflow_menu_focus: cx.focus_handle(),
            breadcrumbs_menu_focus: cx.focus_handle(),
            // Test/select/invert/clear, copy/cut/paste, the five that change
            // the archive, the recent list and its broom, flat view,
            // shortcuts, settings, then the columns.
            overflow_item_focus: (0..(RELEASE_SLOT + 1))
                .map(|_| cx.focus_handle().tab_stop(true))
                .collect(),
            breadcrumbs_item_focus: Vec::new(),
            dialog_return_focus: cx.focus_handle(),
            modal_seen: None,
            password_toggle_focus: cx.focus_handle().tab_stop(true),
            dialog_primary_focus: cx.focus_handle().tab_stop(true),
            dialog_secondary_focus: cx.focus_handle().tab_stop(true),
            dialog_tertiary_focus: cx.focus_handle().tab_stop(true),
            dialog_quaternary_focus: cx.focus_handle().tab_stop(true),
            dialog_rename_focus: cx.focus_handle().tab_stop(true),
            dialog_rename_all_focus: cx.focus_handle().tab_stop(true),
            dialog_cancel_focus: cx.focus_handle().tab_stop(true),
            add_format_focus: cx.focus_handle().tab_stop(true),
            add_codec_focus: cx.focus_handle().tab_stop(true),
            add_level_focus: cx.focus_handle().tab_stop(true),
            add_password_toggle_focus: cx.focus_handle().tab_stop(true),
            add_start_focus: cx.focus_handle().tab_stop(true),
            add_cancel_focus: cx.focus_handle().tab_stop(true),
            settings_focus: SettingsControl::ALL
                .iter()
                .map(|_| cx.focus_handle().tab_stop(true))
                .collect(),
            band: None,
            wheel: None,
            pointer: point(px(0.), px(0.)),
            resizing: None,
            row_menu: None,
            row_menu_focus: cx.focus_handle(),
            row_menu_item_focus: RowAction::ALL
                .iter()
                .map(|_| cx.focus_handle().tab_stop(true))
                .collect(),
            name_value: String::new(),
            viewer_focus: (0..3).map(|_| cx.focus_handle().tab_stop(true)).collect(),
            viewer_scroll: UniformListScrollHandle::new(),
            viewer_image: None,
            drop_paths: Vec::new(),
        }
    }

    /// One folder and everything under it, as a nestable sidebar item.
    ///
    /// The branch leading to the folder you are in opens itself, so opening an
    /// archive three levels down does not present a closed tree you have to
    /// re-walk by hand. Everything else stays shut, because an archive of a
    /// source tree fully expanded is not a sidebar, it is a second file list.
    fn folder_item(
        folder: &Folder,
        label: &str,
        path: String,
        current: &str,
        cx: &mut Context<Self>,
    ) -> SidebarMenuItem {
        let on_path = current.starts_with(path.as_str());
        SidebarMenuItem::new(label.to_string())
            .icon(if on_path {
                IconName::FolderOpen
            } else {
                IconName::Folder
            })
            .active(current == path)
            .default_open(on_path)
            // Clicking the label navigates; the disclosure chevron is what
            // opens a branch. Merging the two would make it impossible to look
            // inside a folder without leaving the one you are in.
            .click_to_open(false)
            .children(
                folder
                    .kids
                    .iter()
                    .map(|(child_label, child)| {
                        Self::folder_item(
                            child,
                            child_label,
                            format!("{path}{child_label}/"),
                            current,
                            cx,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.background_idle() {
                    this.controller.dispatch(AppAction::Navigate(path.clone()));
                    this.route_changed(cx);
                }
            }))
    }

    /// The archive's folders, down the left edge.
    ///
    /// Breadcrumbs say where you are; this says what else there is. A deep
    /// archive was previously only navigable by descending one double-click at
    /// a time and reversing back out.
    fn sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let current = self.controller.state.current_dir.clone();
        let at_root = current.is_empty();
        let root_item = SidebarMenuItem::new(self.controller.s().archive_root.to_string())
            .icon(IconName::Inbox)
            .active(at_root)
            .on_click(cx.listener(|this, _, _, cx| {
                if this.background_idle() {
                    this.controller.dispatch(AppAction::Navigate(String::new()));
                    this.route_changed(cx);
                }
            }));
        let mut items = vec![root_item];
        let tree = &self.controller.state.folders;
        items.extend(
            tree.kids
                .iter()
                .map(|(label, folder)| {
                    Self::folder_item(folder, label, format!("{label}/"), &current, cx)
                })
                .collect::<Vec<_>>(),
        );
        let menu = SidebarMenu::new().children(items);
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
            .child(menu.render("archive-folder-menu", window, cx))
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
        let return_focus = self.dialog_return_focus.clone();
        window.on_next_frame(move |window, cx| window.focus(&return_focus, cx));
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
        let handle = self.filter.read(cx).focus_handle.clone();
        window.focus(&handle, cx);
    }

    fn menu_key_down(
        event: &KeyDownEvent,
        items: &[FocusHandle],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if key == "tab" {
            Self::trap_focus(items, event.keystroke.modifiers.shift, window, cx);
            return;
        }
        if !matches!(key, "down" | "up") {
            return;
        }
        let Some(current) = items.iter().position(|item| item.is_focused(window)) else {
            cx.stop_propagation();
            return;
        };
        let Some(next) = menu_target(current, key, items.len()) else {
            cx.stop_propagation();
            return;
        };
        items[next].focus(window, cx);
        cx.stop_propagation();
    }

    fn overflow_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.overflow_open = false;
            self.overflow_trigger_focus.focus(window, cx);
            cx.stop_propagation();
            cx.notify();
        } else {
            let mut items = Vec::new();
            if self.controller.state.archive.is_some() && self.menu_enabled() {
                items.extend(self.overflow_item_focus[..4].iter().cloned());
                if self.can_copy_files() {
                    items.push(self.overflow_item_focus[4].clone());
                    items.push(self.overflow_item_focus[5].clone());
                }
            }
            if self.can_paste_files() {
                items.push(self.overflow_item_focus[6].clone());
            }
            if self.menu_enabled() {
                items.extend(self.overflow_item_focus[7..RECENT_SLOT].iter().cloned());
                // Only the recent slots that have an archive behind them: an
                // empty slot is a stop on the way down that lands on nothing.
                let recent = self.recent_shown();
                items.extend(
                    self.overflow_item_focus[RECENT_SLOT..RECENT_SLOT + recent]
                        .iter()
                        .cloned(),
                );
                items.extend(
                    self.overflow_item_focus[RECENT_SLOT + RECENT_MAX..RELEASE_SLOT]
                        .iter()
                        .cloned(),
                );
                if self.controller.state.update.is_some() {
                    items.push(self.overflow_item_focus[RELEASE_SLOT].clone());
                }
            }
            Self::menu_key_down(event, &items, window, cx);
        }
    }

    fn breadcrumbs_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.breadcrumbs_open = false;
            self.breadcrumbs_trigger_focus.focus(window, cx);
            cx.stop_propagation();
            cx.notify();
        } else {
            let hidden_len = Self::visible_crumb_indices(self.crumbs().len()).1.len();
            let visible_items = visible_menu_items(&self.breadcrumbs_item_focus, hidden_len);
            Self::menu_key_down(event, visible_items, window, cx);
        }
    }

    fn sync_breadcrumb_item_focus(&mut self, cx: &mut Context<Self>) {
        let hidden_len = Self::visible_crumb_indices(self.crumbs().len()).1.len();
        self.breadcrumbs_item_focus.truncate(hidden_len);
        while self.breadcrumbs_item_focus.len() < hidden_len {
            self.breadcrumbs_item_focus
                .push(cx.focus_handle().tab_stop(true));
        }
    }

    fn route_changed(&mut self, cx: &mut Context<Self>) {
        self.sync_breadcrumb_item_focus(cx);
        cx.notify();
    }

    fn button(
        id: impl Into<ElementId>,
        label: impl Into<gpui::SharedString>,
        accessible_name: String,
        enabled: bool,
        cx: &App,
    ) -> Stateful<gpui::Div> {
        // Borderless, with the ground only appearing under the pointer. A row
        // of outlined boxes reads as seven competing things; the same row
        // without outlines reads as one toolbar, and the hover tint says which
        // one you are about to press. Disabled loses the ink, not the space,
        // so nothing shifts when an action becomes available.
        let mut button = div()
            .id(id)
            .aria_label(accessible_name)
            .h(px(26.))
            .px_2()
            .flex()
            .items_center()
            .flex_none()
            .rounded(cx.theme().radius)
            .focus_visible(focus_ring(cx))
            .text_xs()
            .child(label.into());
        if enabled {
            button = button
                .role(Role::Button)
                .focusable()
                .tab_stop(true)
                .cursor_pointer()
                .text_color(cx.theme().foreground)
                .hover(|style| style.bg(cx.theme().accent))
                .active(|style| style.bg(cx.theme().secondary_active));
        } else {
            button = button
                .tab_stop(false)
                .text_color(cx.theme().muted_foreground);
        }
        button
    }

    /// A square button carrying one of the kit's icons instead of a word.
    ///
    /// Only for the controls whose meaning is a direction — back, forward, up.
    /// A named action stays a word, because an icon that needs a tooltip to be
    /// understood has cost a word and bought nothing. The accessible name is
    /// still the word, so nothing changes for a screen reader.
    fn icon_button(
        id: impl Into<ElementId>,
        icon: IconName,
        accessible_name: String,
        enabled: bool,
        cx: &App,
    ) -> Stateful<gpui::Div> {
        let mut button = div()
            .id(id)
            .aria_label(accessible_name)
            .size(px(26.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(cx.theme().radius)
            .focus_visible(focus_ring(cx))
            .child(Icon::new(icon).size_4().text_color(if enabled {
                cx.theme().foreground
            } else {
                cx.theme().muted_foreground
            }));
        if enabled {
            button = button
                .role(Role::Button)
                .focusable()
                .tab_stop(true)
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().accent));
        } else {
            button = button.tab_stop(false);
        }
        button
    }

    fn menu_item(
        id: impl Into<ElementId>,
        label: impl Into<gpui::SharedString>,
        accessible_name: String,
        enabled: bool,
        cx: &App,
    ) -> Stateful<gpui::Div> {
        // A menu item is a full-width row, not a button that happens to be in a
        // menu: it takes the whole popover so the hover tint reaches both edges.
        let mut item = Self::button(id, label, accessible_name, enabled, cx)
            .w_full()
            .justify_start();
        if enabled {
            item = item.role(Role::MenuItem);
        }
        item
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
        } else if self.controller.state.renaming.is_some() {
            Some(ModalKind::Rename)
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

    fn modal_focus_targets(&self, kind: ModalKind, cx: &mut Context<Self>) -> Vec<FocusHandle> {
        match kind {
            ModalKind::Password => vec![
                self.password.read(cx).focus_handle.clone(),
                self.password_toggle_focus.clone(),
                self.dialog_primary_focus.clone(),
                self.dialog_cancel_focus.clone(),
            ],
            ModalKind::Conflict => vec![
                self.dialog_primary_focus.clone(),
                self.dialog_secondary_focus.clone(),
                self.dialog_tertiary_focus.clone(),
                self.dialog_quaternary_focus.clone(),
                self.dialog_rename_focus.clone(),
                self.dialog_rename_all_focus.clone(),
                self.dialog_cancel_focus.clone(),
            ],
            ModalKind::Delete => vec![
                self.dialog_primary_focus.clone(),
                self.dialog_cancel_focus.clone(),
            ],
            ModalKind::Drop => vec![
                self.dialog_primary_focus.clone(),
                self.dialog_secondary_focus.clone(),
                self.dialog_cancel_focus.clone(),
            ],
            ModalKind::Add => self.add_focus_targets(cx),
            ModalKind::Viewer => self.viewer_focus.clone(),
            ModalKind::NewFolder | ModalKind::Rename | ModalKind::Mask => vec![
                self.name_input.read(cx).focus_handle.clone(),
                self.dialog_primary_focus.clone(),
                self.dialog_cancel_focus.clone(),
            ],
            ModalKind::DefaultPassword => vec![
                self.password.read(cx).focus_handle.clone(),
                self.password_toggle_focus.clone(),
                self.dialog_primary_focus.clone(),
                self.dialog_secondary_focus.clone(),
                self.dialog_cancel_focus.clone(),
            ],
            ModalKind::Settings => self.settings_focus.clone(),
            ModalKind::Shortcuts => vec![self.dialog_cancel_focus.clone()],
        }
    }

    fn add_focus_targets(&self, cx: &mut Context<Self>) -> Vec<FocusHandle> {
        let mut targets = vec![
            self.output_name.read(cx).focus_handle.clone(),
            self.add_format_focus.clone(),
        ];
        if self.controller.state.format == super::Format::Zip {
            targets.push(self.add_codec_focus.clone());
        }
        targets.push(self.add_level_focus.clone());
        if self.controller.state.format == super::Format::Zip {
            targets.push(self.add_password.read(cx).focus_handle.clone());
            targets.push(self.add_password_toggle_focus.clone());
        }
        targets.extend([self.add_start_focus.clone(), self.add_cancel_focus.clone()]);
        targets
    }

    fn trap_focus(
        targets: &[FocusHandle],
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if targets.is_empty() {
            cx.stop_propagation();
            return;
        }
        let current = targets.iter().position(|target| target.is_focused(window));
        if let Some(index) = focus_cycle_index(current, reverse, targets.len()) {
            window.focus(&targets[index], cx);
        }
        cx.stop_propagation();
    }

    fn remember_background_focus(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.modal_seen.is_none()
            && self.modal_kind().is_none()
            && self.dialog.is_none()
            && !self.overflow_open
            && !self.breadcrumbs_open
        {
            if let Some(focus) = window.focused(cx) {
                self.dialog_return_focus = focus;
            }
        }
    }

    fn remember_conflict_focus(&mut self) {
        self.dialog_return_focus = self.conflict_trigger_focus.clone();
    }

    fn sync_modal_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.modal_kind();
        if current == self.modal_seen {
            return;
        }
        if current == Some(ModalKind::Conflict) {
            self.remember_conflict_focus();
        }
        self.modal_seen = current;
        let target = match current {
            Some(ModalKind::Password) => self.password.read(cx).focus_handle.clone(),
            Some(ModalKind::Conflict | ModalKind::Delete | ModalKind::Drop) => {
                self.dialog_primary_focus.clone()
            }
            Some(ModalKind::Add) => self.output_name.read(cx).focus_handle.clone(),
            Some(ModalKind::Viewer) => self.viewer_focus[0].clone(),
            Some(ModalKind::NewFolder | ModalKind::Rename | ModalKind::Mask) => {
                self.name_input.read(cx).focus_handle.clone()
            }
            Some(ModalKind::DefaultPassword) => self.password.read(cx).focus_handle.clone(),
            Some(ModalKind::Settings) => self.settings_focus[0].clone(),
            Some(ModalKind::Shortcuts) => self.dialog_cancel_focus.clone(),
            None => self.dialog_return_focus.clone(),
        };
        window.on_next_frame(move |window, cx| window.focus(&target, cx));
    }

    fn answer_conflict(&mut self, answer: Answer) {
        self.remember_conflict_focus();
        self.controller.dispatch(AppAction::AnswerConflict(answer));
    }

    fn modal_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let Some(kind) = self.modal_kind() else {
            return;
        };
        if key == "tab" {
            let targets = self.modal_focus_targets(kind, cx);
            Self::trap_focus(&targets, event.keystroke.modifiers.shift, window, cx);
            return;
        }
        if key == "escape" {
            match kind {
                ModalKind::Password => self.controller.dispatch(AppAction::CancelPassword),
                ModalKind::Conflict => self.answer_conflict(Answer::Cancel),
                ModalKind::Delete => self.controller.dispatch(AppAction::ConfirmDelete(false)),
                ModalKind::Drop => self
                    .controller
                    .dispatch(AppAction::AnswerDrop(DropChoice::Cancel)),
                ModalKind::Add => self.controller.state.view = View::Browse,
                ModalKind::Viewer => self.controller.state.viewing = None,
                ModalKind::NewFolder | ModalKind::Rename | ModalKind::Mask => self.close_name(),
                ModalKind::DefaultPassword => {
                    self.controller.state.asking_default_password = false;
                    self.controller.state.password_input.clear();
                }
                ModalKind::Settings => self.controller.state.show_settings = false,
                ModalKind::Shortcuts => self.controller.state.show_shortcuts = false,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key != "enter" {
            return;
        }
        self.modal_enter(kind, window, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn modal_enter(&mut self, kind: ModalKind, window: &mut Window, cx: &mut Context<Self>) {
        let focused = |handle: &FocusHandle| handle.is_focused(window);
        match kind {
            ModalKind::Password => {
                if focused(&self.password_toggle_focus) {
                    self.controller
                        .dispatch(AppAction::TogglePasswordVisibility);
                } else if focused(&self.dialog_cancel_focus) {
                    self.controller.dispatch(AppAction::CancelPassword);
                } else {
                    let password = self.password.read(cx).content.clone();
                    self.controller
                        .dispatch(AppAction::SubmitPassword(password));
                }
            }
            ModalKind::Conflict => {
                let answer = if focused(&self.dialog_secondary_focus) {
                    Answer::ReplaceAll
                } else if focused(&self.dialog_tertiary_focus) {
                    Answer::Skip
                } else if focused(&self.dialog_quaternary_focus) {
                    Answer::SkipAll
                } else if focused(&self.dialog_rename_focus) {
                    Answer::Rename
                } else if focused(&self.dialog_rename_all_focus) {
                    Answer::RenameAll
                } else if focused(&self.dialog_cancel_focus) {
                    Answer::Cancel
                } else {
                    Answer::Replace
                };
                self.answer_conflict(answer);
            }
            ModalKind::Delete => self.controller.dispatch(AppAction::ConfirmDelete(!focused(
                &self.dialog_cancel_focus,
            ))),
            ModalKind::Drop => {
                let choice = if focused(&self.dialog_secondary_focus) {
                    DropChoice::Add
                } else if focused(&self.dialog_cancel_focus) {
                    DropChoice::Cancel
                } else {
                    DropChoice::Open
                };
                self.controller.dispatch(AppAction::AnswerDrop(choice));
            }
            ModalKind::Add => {
                if focused(&self.add_cancel_focus) {
                    self.controller.state.view = View::Browse;
                } else if focused(&self.add_format_focus) {
                    self.cycle_format();
                } else if focused(&self.add_codec_focus) {
                    self.cycle_codec();
                } else if focused(&self.add_level_focus) {
                    self.cycle_level();
                } else if focused(&self.add_password_toggle_focus) {
                    self.controller.state.show_password = !self.controller.state.show_password;
                } else {
                    self.start_add(cx);
                }
            }
            ModalKind::NewFolder | ModalKind::Rename | ModalKind::Mask => {
                if focused(&self.dialog_cancel_focus) {
                    self.close_name();
                } else {
                    self.confirm_name(kind, cx);
                }
            }
            ModalKind::DefaultPassword => {
                if focused(&self.password_toggle_focus) {
                    self.controller.state.show_password = !self.controller.state.show_password;
                } else if focused(&self.dialog_secondary_focus) {
                    self.forget_default_password();
                } else if focused(&self.dialog_cancel_focus) {
                    self.controller.state.asking_default_password = false;
                    self.controller.state.password_input.clear();
                } else {
                    self.keep_default_password();
                }
            }
            ModalKind::Viewer => {
                if let Some(look) = [super::Look::Text, super::Look::Hex, super::Look::Picture]
                    .into_iter()
                    .enumerate()
                    .find(|(index, _)| focused(&self.viewer_focus[*index]))
                    .map(|(_, look)| look)
                {
                    if let Some(view) = &mut self.controller.state.viewing {
                        view.look = look;
                    }
                } else {
                    self.controller.state.viewing = None;
                }
            }
            ModalKind::Shortcuts => self.controller.state.show_shortcuts = false,
            ModalKind::Settings => {
                if let Some(control) = SettingsControl::ALL
                    .iter()
                    .copied()
                    .find(|control| focused(&self.settings_focus[*control as usize]))
                {
                    self.settings_activate(control, window, cx);
                }
            }
        }
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
            SettingsControl::ThemeSystem => self.set_theme(super::ThemePreference::System, window, cx),
            SettingsControl::ThemeLight => self.set_theme(super::ThemePreference::Light, window, cx),
            SettingsControl::ThemeDark => self.set_theme(super::ThemePreference::Dark, window, cx),
            SettingsControl::Format => self.cycle_format(),
            SettingsControl::Codec => self.cycle_codec(),
            SettingsControl::Level => self.cycle_level(),
            SettingsControl::Subfolder => {
                self.controller.state.into_subfolder = !self.controller.state.into_subfolder;
            }
            SettingsControl::NamePage => {
                let pages = arca_zip::pages::Page::ALL;
                let at = pages
                    .iter()
                    .position(|(page, _, _)| *page == self.controller.state.settings.page)
                    .unwrap_or(0);
                self.controller
                    .reread_names(pages[(at + 1) % pages.len()].0);
            }
            SettingsControl::Close => self.controller.state.show_settings = false,
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
    fn viewer_dialog(&mut self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let s = self.controller.s();
        let Some(view) = &self.controller.state.viewing else {
            return div().id("viewer-missing");
        };
        let name = view.name.clone();
        let size = human(view.bytes.len() as u64);
        let look = view.look;
        let picture = view.picture;
        let bytes = view.bytes.clone();
        let lines = view.lines.clone();

        let mut tabs = div().flex().items_center().gap_2();
        for (index, (candidate, label)) in [
            (super::Look::Text, s.as_text),
            (super::Look::Hex, s.as_hex),
            (super::Look::Picture, s.as_picture),
        ]
        .into_iter()
        .enumerate()
        {
            // Only where there is a picture to show. A tab that says "picture"
            // over a text file is a tab that lies.
            if candidate == super::Look::Picture && !picture {
                continue;
            }
            tabs = tabs.child(
                Self::dialog_button(
                    ("viewer-tab", index),
                    label,
                    &self.viewer_focus[index],
                    look == candidate,
                    cx,
                )
                .aria_selected(look == candidate)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(view) = &mut this.controller.state.viewing {
                        view.look = candidate;
                    }
                    cx.notify();
                })),
            );
        }
        tabs = tabs.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(size),
        );

        let content = match look {
            super::Look::Picture => {
                let image = self.viewer_picture(&name, &bytes);
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
                .track_scroll(&self.viewer_scroll)
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
                .track_scroll(&self.viewer_scroll)
                .size_full()
                .into_any_element()
            }
        };

        let body = div()
            .id("viewer-dialog-body")
            .flex()
            .flex_col()
            .gap_3()
            .w(px(760.))
            .h(px(460.))
            .child(tabs)
            .child(Separator::horizontal())
            .child(
                div()
                    .id("viewer-content")
                    .flex_1()
                    .min_h(px(1.))
                    .overflow_hidden()
                    .child(content),
            );
        self.dialog_overlay(ModalKind::Viewer, name, s.view_word, body, cx)
    }

    /// The decoded picture for the entry being looked at, decoded once.
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
            ModalKind::Rename => {
                let Some((path, _)) = self.controller.state.renaming.clone() else {
                    self.close_name();
                    return;
                };
                let rows = self.controller.visible_rows();
                self.close_name();
                self.controller.rename_to(&rows, &path, name.trim());
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

    /// The dialogs that are one text field and two buttons: a new folder, a
    /// new name, a mask.
    fn name_dialog(&mut self, kind: ModalKind, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let s = self.controller.s();
        let (title, hint, confirm) = match kind {
            ModalKind::NewFolder => (s.new_folder, s.folder_name, s.new_folder),
            ModalKind::Rename => (s.rename_word, s.rename_word, s.rename_word),
            _ => {
                let adding = self.controller.state.picking_group.unwrap_or(true);
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
        let ok = Self::dialog_button("name-ok", confirm, &self.dialog_primary_focus, true, cx)
            .on_click(cx.listener(move |this, _, _, cx| this.confirm_name(kind, cx)));
        let cancel = Self::dialog_button("name-cancel", s.cancel, &self.dialog_cancel_focus, false, cx)
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_name();
                cx.notify();
            }));
        let body = div()
            .id("name-dialog-body")
            .flex()
            .flex_col()
            .gap_3()
            .child(self.name_input.clone())
            .child(div().flex().gap_2().child(ok).child(cancel));
        self.dialog_overlay(kind, title, hint, body, cx)
    }

    fn default_password_dialog(&mut self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let s = self.controller.s();
        let show = !self.controller.state.show_password;
        let toggle = Self::dialog_button(
            "default-password-visibility",
            if show { s.show_password } else { s.hide_word },
            &self.password_toggle_focus,
            false,
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.controller
                .dispatch(AppAction::TogglePasswordVisibility);
            cx.notify();
        }));
        let keep = Self::dialog_button(
            "default-password-keep",
            s.start,
            &self.dialog_primary_focus,
            true,
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.keep_default_password();
            cx.notify();
        }));
        let forget = Self::dialog_button(
            "default-password-forget",
            s.remove_password,
            &self.dialog_secondary_focus,
            false,
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.forget_default_password();
            cx.notify();
        }));
        let cancel = Self::dialog_button(
            "default-password-cancel",
            s.cancel,
            &self.dialog_cancel_focus,
            false,
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.controller.state.asking_default_password = false;
            this.controller.state.password_input.clear();
            cx.notify();
        }));
        let body = div()
            .id("default-password-dialog-body")
            .flex()
            .flex_col()
            .gap_3()
            .child(self.password.clone())
            .child(toggle)
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(s.password_kept),
            )
            .child(div().flex().gap_2().child(keep).child(forget).child(cancel));
        self.dialog_overlay(
            ModalKind::DefaultPassword,
            s.default_password,
            s.password_hint,
            body,
            cx,
        )
    }

    /// Language, theme, and the defaults a new archive is made with.
    ///
    /// Every choice is a row of buttons with the current one filled in, not a
    /// dropdown: there are three of each at most, and a list that short costs
    /// more to open than to read.
    fn settings_dialog(&mut self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let s = self.controller.s();
        let lang = self.controller.state.settings.lang;
        let theme = self.controller.state.settings.theme;
        let is_zip = self.controller.state.format == super::Format::Zip;
        let choice = |this: &Self,
                      control: SettingsControl,
                      label: &'static str,
                      active: bool,
                      enabled: bool,
                      cx: &mut Context<Self>| {
            let mut button = Self::dialog_button(
                ("settings", control as usize),
                label,
                &this.settings_focus[control as usize],
                active,
                cx,
            )
            .aria_selected(active);
            if enabled {
                button = button.on_click(cx.listener(move |this, _, window, cx| {
                    this.settings_activate(control, window, cx);
                    cx.notify();
                }));
            }
            button
        };
        let row = |label: &'static str, children: Vec<Stateful<gpui::Div>>| {
            div()
                .flex()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(div().w(px(110.)).flex_none().child(label))
                .children(children)
        };
        let languages = row(
            s.language,
            vec![
                choice(
                    self,
                    SettingsControl::LangSystem,
                    s.theme_system,
                    lang.is_none(),
                    true,
                    cx,
                ),
                choice(
                    self,
                    SettingsControl::LangEn,
                    super::Lang::En.label(),
                    lang == Some(super::Lang::En),
                    true,
                    cx,
                ),
                choice(
                    self,
                    SettingsControl::LangEs,
                    super::Lang::Es.label(),
                    lang == Some(super::Lang::Es),
                    true,
                    cx,
                ),
            ],
        );
        let themes = row(
            s.theme,
            vec![
                choice(
                    self,
                    SettingsControl::ThemeSystem,
                    s.theme_system,
                    theme == super::ThemePreference::System,
                    true,
                    cx,
                ),
                choice(
                    self,
                    SettingsControl::ThemeLight,
                    s.theme_light,
                    theme == super::ThemePreference::Light,
                    true,
                    cx,
                ),
                choice(
                    self,
                    SettingsControl::ThemeDark,
                    s.theme_dark,
                    theme == super::ThemePreference::Dark,
                    true,
                    cx,
                ),
            ],
        );
        let defaults = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(s.defaults_title),
            )
            .child(row(
                s.format,
                vec![choice(
                    self,
                    SettingsControl::Format,
                    self.controller.state.format.label(),
                    false,
                    true,
                    cx,
                )],
            ))
            .child(row(
                s.compressor,
                vec![choice(
                    self,
                    SettingsControl::Codec,
                    self.controller.codec_name(self.controller.state.codec),
                    false,
                    is_zip,
                    cx,
                )],
            ))
            .child(row(
                s.level,
                vec![choice(
                    self,
                    SettingsControl::Level,
                    self.controller.level_name(self.controller.state.level),
                    false,
                    true,
                    cx,
                )],
            ));
        let subfolder = choice(
            self,
            SettingsControl::Subfolder,
            s.into_subfolder,
            self.controller.state.into_subfolder,
            true,
            cx,
        );
        let page = arca_zip::pages::Page::ALL
            .iter()
            .find(|(page, _, _)| *page == self.controller.state.settings.page)
            .map(|(_, _, label)| *label)
            .unwrap_or("");
        let name_page = row(
            s.name_encoding,
            vec![choice(
                self,
                SettingsControl::NamePage,
                page,
                false,
                true,
                cx,
            )],
        );
        let close = choice(self, SettingsControl::Close, s.close, true, true, cx);
        let body = div()
            .id("settings-dialog-body")
            .flex()
            .flex_col()
            .gap_3()
            .child(languages)
            .child(themes)
            .child(name_page)
            .child(Separator::horizontal())
            .child(defaults)
            .child(subfolder)
            .child(div().flex().gap_2().child(close));
        self.dialog_overlay(ModalKind::Settings, s.settings, s.defaults_title, body, cx)
    }

    fn dialog_button(
        id: impl Into<ElementId>,
        label: impl Into<gpui::SharedString>,
        focus: &FocusHandle,
        primary: bool,
        cx: &App,
    ) -> Stateful<gpui::Div> {
        let label: gpui::SharedString = label.into();
        let mut button = div()
            .id(id)
            .role(Role::Button)
            .aria_label(label.clone())
            .focusable()
            .tab_stop(true)
            .track_focus(focus)
            .cursor_pointer()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(if primary {
                cx.theme().primary
            } else {
                cx.theme().border
            })
            .focus_visible(focus_ring(cx));
        if primary {
            button = button
                .bg(cx.theme().primary)
                .text_color(cx.theme().primary_foreground)
                .hover(|style| style.bg(cx.theme().primary_hover))
                .active(|style| style.bg(cx.theme().primary_active));
        } else {
            button = button
                .bg(cx.theme().button)
                .text_color(cx.theme().button_foreground)
                .hover(|style| style.bg(cx.theme().button_hover))
                .active(|style| style.bg(cx.theme().button_active));
        }
        button.child(label)
    }

    fn dialog_overlay(
        &mut self,
        kind: ModalKind,
        title: impl Into<gpui::SharedString>,
        description: impl Into<gpui::SharedString>,
        body: Stateful<gpui::Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let title: gpui::SharedString = title.into();
        let description: gpui::SharedString = description.into();
        div()
            .id(("gpui-dialog", kind as usize))
            .absolute()
            .inset_0()
            .bg(cx.theme().overlay)
            .role(Role::Dialog)
            .aria_label(title.clone())
            .aria_keyshortcuts("Escape Enter")
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, _| {})
            .capture_key_down(cx.listener(Self::modal_key_down))
            .child(
                div()
                    .id(("gpui-dialog-card", kind as usize))
                    .m_8()
                    // Wide enough for the viewer, which is the only dialog that
                    // holds content rather than a question. The rest are sized
                    // by what is in them and never reach it.
                    .max_w(px(920.))
                    .p_5()
                    .gap_3()
                    .flex()
                    .flex_col()
                    .tab_group()
                    .bg(cx.theme().popover)
                    .text_color(cx.theme().popover_foreground)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .child(
                        div()
                            .id(("dialog-title", kind as usize))
                            .role(Role::Heading)
                            .text_lg()
                            .child(title.clone()),
                    )
                    .child(
                        div()
                            .id(("dialog-description", kind as usize))
                            .role(Role::Note)
                            .aria_label("Dialog description")
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(description),
                    )
                    .child(body),
            )
    }

    fn cycle_format(&mut self) {
        self.controller.state.format = match self.controller.state.format {
            super::Format::Zip => super::Format::Tar,
            super::Format::Tar => super::Format::TarGz,
            super::Format::TarGz => super::Format::Zip,
        };
    }

    fn cycle_codec(&mut self) {
        if self.controller.state.format == super::Format::Zip {
            self.controller.state.codec = match self.controller.state.codec {
                super::Codec::Store => super::Codec::Deflate,
                super::Codec::Deflate => super::Codec::Zstd,
                super::Codec::Zstd => super::Codec::Store,
            };
        }
    }

    fn cycle_level(&mut self) {
        self.controller.state.level = match self.controller.state.level {
            super::Level::Store => super::Level::Fast,
            super::Level::Fast => super::Level::Normal,
            super::Level::Normal => super::Level::Best,
            super::Level::Best => super::Level::Store,
        };
    }

    fn start_add(&mut self, cx: &mut Context<Self>) {
        let Some(first) = self.controller.state.pending_inputs.first() else {
            return;
        };
        self.dialog_return_focus = self.add_start_focus.clone();
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

    fn add_dialog(&mut self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let s = self.controller.s();
        let format = self.controller.state.format;
        let is_zip = format == super::Format::Zip;
        let format_button = Self::dialog_button(
            "add-format",
            format.label(),
            &self.add_format_focus,
            false,
            cx,
        )
        .aria_label(s.format)
        .on_click(cx.listener(|this, _, _, cx| {
            this.cycle_format();
            cx.notify();
        }));
        let codec_button = Self::button(
            "add-codec",
            self.controller.codec_name(self.controller.state.codec),
            s.compressor.to_string(),
            is_zip,
            cx,
        )
        .track_focus(&self.add_codec_focus)
        .on_click(cx.listener(|this, _, _, cx| {
            this.cycle_codec();
            cx.notify();
        }));
        let level_button = Self::dialog_button(
            "add-level",
            self.controller.level_name(self.controller.state.level),
            &self.add_level_focus,
            false,
            cx,
        )
        .aria_label(s.level)
        .on_click(cx.listener(|this, _, _, cx| {
            this.cycle_level();
            cx.notify();
        }));
        let password = if is_zip {
            let show = self.controller.state.show_password;
            let toggle = Self::dialog_button(
                "add-password-visibility",
                if show { s.hide_word } else { s.show_password },
                &self.add_password_toggle_focus,
                false,
                cx,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.controller.state.show_password = !this.controller.state.show_password;
                cx.notify();
            }));
            Some(
                div()
                    .flex()
                    .gap_2()
                    .child(self.add_password.clone())
                    .child(toggle),
            )
        } else {
            None
        };
        let start = Self::dialog_button("add-start", s.start, &self.add_start_focus, true, cx)
            .on_click(cx.listener(|this, _, _, cx| this.start_add(cx)));
        let cancel = Self::dialog_button("add-cancel", s.cancel, &self.add_cancel_focus, false, cx)
            .on_click(cx.listener(|this, _, _, cx| {
                this.controller.state.view = View::Browse;
                cx.notify();
            }));
        let count = self.controller.state.pending_inputs.len();
        let options = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(s.format)
                    .child(format_button),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(s.compressor)
                    .child(codec_button),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(s.level)
                    .child(level_button),
            );
        let mut body = div()
            .id("add-dialog-body")
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(s.output_name)
                    .child(self.output_name.clone()),
            )
            .child(options);
        if let Some(password) = password {
            body = body.child(password);
        }
        body = body
            .child(format!("{count} {}", s.files_word))
            .child(div().flex().gap_2().child(start).child(cancel));
        self.dialog_overlay(
            ModalKind::Add,
            s.add_to_archive,
            s.defaults_title,
            body,
            cx,
        )
    }

    fn dialogs(&mut self, cx: &mut Context<Self>) -> Option<Stateful<gpui::Div>> {
        let s = self.controller.s();
        if matches!(self.controller.state.view, View::Add) {
            return Some(self.add_dialog(cx));
        }
        match self.modal_kind()? {
            ModalKind::Password => {
                let setting = matches!(
                    self.controller.state.waiting_on_password,
                    Some(Pending::NewPassword(_) | Pending::CurrentPassword(_))
                );
                let title = if setting {
                    s.set_password
                } else {
                    s.password_needed
                };
                let hint = if setting {
                    s.new_password
                } else {
                    s.password_hint
                };
                let password = self.password.clone();
                let show = !self.controller.state.show_password;
                let toggle = Self::dialog_button(
                    "password-visibility",
                    if show { s.show_password } else { s.hide_word },
                    &self.password_toggle_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller
                        .dispatch(AppAction::TogglePasswordVisibility);
                    cx.notify();
                }));
                let submit = Self::dialog_button(
                    "password-submit",
                    if setting { s.set_password } else { s.start },
                    &self.dialog_primary_focus,
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let password = this.password.read(cx).content.clone();
                    this.controller
                        .dispatch(AppAction::SubmitPassword(password));
                    cx.notify();
                }));
                let cancel = Self::dialog_button(
                    "password-cancel",
                    s.cancel,
                    &self.dialog_cancel_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.dispatch(AppAction::CancelPassword);
                    cx.notify();
                }));
                Some(
                    self.dialog_overlay(
                        ModalKind::Password,
                        title,
                        hint,
                        div()
                            .id("password-dialog-body")
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(password)
                            .child(toggle)
                            .child(div().flex().gap_2().child(submit).child(cancel)),
                        cx,
                    ),
                )
            }
            ModalKind::Conflict => {
                let path = self.controller.state.conflict.clone().unwrap_or_default();
                let overwrite = Self::dialog_button(
                    "conflict-replace",
                    s.yes,
                    &self.dialog_primary_focus,
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.answer_conflict(Answer::Replace);
                    cx.notify();
                }));
                let overwrite_all = Self::dialog_button(
                    "conflict-replace-all",
                    s.yes_all,
                    &self.dialog_secondary_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.answer_conflict(Answer::ReplaceAll);
                    cx.notify();
                }));
                let skip = Self::dialog_button(
                    "conflict-skip",
                    s.no,
                    &self.dialog_tertiary_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.answer_conflict(Answer::Skip);
                    cx.notify();
                }));
                let skip_all = Self::dialog_button(
                    "conflict-skip-all",
                    s.no_all,
                    &self.dialog_quaternary_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.answer_conflict(Answer::SkipAll);
                    cx.notify();
                }));
                let keep = Self::dialog_button(
                    "conflict-keep-both",
                    s.rename,
                    &self.dialog_rename_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.answer_conflict(Answer::Rename);
                    cx.notify();
                }));
                let rename_all = Self::dialog_button(
                    "conflict-keep-both-all",
                    s.rename_all,
                    &self.dialog_rename_all_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.answer_conflict(Answer::RenameAll);
                    cx.notify();
                }));
                let cancel = Self::dialog_button(
                    "conflict-cancel",
                    s.cancel,
                    &self.dialog_cancel_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.answer_conflict(Answer::Cancel);
                    cx.notify();
                }));
                Some(
                    self.dialog_overlay(
                        ModalKind::Conflict,
                        s.conflict_title,
                        format!("{} {path}", s.already_there),
                        div()
                            .id("conflict-dialog-body")
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .children([
                                overwrite,
                                overwrite_all,
                                skip,
                                skip_all,
                                keep,
                                rename_all,
                                cancel,
                            ]),
                        cx,
                    ),
                )
            }
            ModalKind::Delete => {
                let names = self
                    .controller
                    .state
                    .confirm_delete
                    .clone()
                    .unwrap_or_default();
                let confirm = Self::dialog_button(
                    "delete-confirm",
                    s.delete_word,
                    &self.dialog_primary_focus,
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.dispatch(AppAction::ConfirmDelete(true));
                    cx.notify();
                }));
                let cancel = Self::dialog_button(
                    "delete-cancel",
                    s.cancel,
                    &self.dialog_cancel_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.dispatch(AppAction::ConfirmDelete(false));
                    cx.notify();
                }));
                Some(
                    self.dialog_overlay(
                        ModalKind::Delete,
                        s.delete_word,
                        fill(s.confirm_delete, &[("n", &names.len().to_string())]),
                        div()
                            .id("delete-dialog-body")
                            .flex()
                            .gap_2()
                            .children([confirm, cancel]),
                        cx,
                    ),
                )
            }
            ModalKind::Drop => {
                let paths = self
                    .controller
                    .state
                    .confirm_drop
                    .clone()
                    .unwrap_or_default();
                let open =
                    Self::dialog_button("drop-open", s.open_word, &self.dialog_primary_focus, true, cx)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller
                                .dispatch(AppAction::AnswerDrop(DropChoice::Open));
                            cx.notify();
                        }));
                let add = Self::dialog_button(
                    "drop-add",
                    s.add_to_archive,
                    &self.dialog_secondary_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                            this.controller
                                .dispatch(AppAction::AnswerDrop(DropChoice::Add));
                            cx.notify();
                        }));
                let cancel = Self::dialog_button(
                    "drop-cancel",
                    s.cancel,
                    &self.dialog_cancel_focus,
                    false,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller
                        .dispatch(AppAction::AnswerDrop(DropChoice::Cancel));
                    cx.notify();
                }));
                let names = paths
                    .iter()
                    .take(8)
                    .filter_map(|path| path.file_name())
                    .map(|name| name.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(
                    self.dialog_overlay(
                        ModalKind::Drop,
                        s.drop_title,
                        format!("{} {names}", s.dropped_word),
                        div()
                            .id("drop-dialog-body")
                            .flex()
                            .gap_2()
                            .children([open, add, cancel]),
                        cx,
                    ),
                )
            }
            kind @ (ModalKind::NewFolder | ModalKind::Rename | ModalKind::Mask) => {
                Some(self.name_dialog(kind, cx))
            }
            ModalKind::Viewer => Some(self.viewer_dialog(cx)),
            ModalKind::DefaultPassword => Some(self.default_password_dialog(cx)),
            ModalKind::Settings => Some(self.settings_dialog(cx)),
            ModalKind::Shortcuts => Some(self.shortcuts_dialog(cx)),
            ModalKind::Add => unreachable!("add dialog is rendered above"),
        }
    }

    /// Runs what the row menu was asked for and shuts it.
    fn row_action(
        &mut self,
        action: RowAction,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.row_menu = None;
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
                    if self.controller.state.format == super::Format::Zip {
                        self.name_value = row.label.clone();
                        self.controller.state.renaming = Some((row.path, row.label));
                    }
                }
            }
            RowAction::Delete => {
                self.dialog_return_focus = self.delete_trigger_focus.clone();
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

    fn row_menu_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.row_menu = None;
            self.list_focus.focus(window, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let writable = self.controller.state.format == super::Format::Zip;
        let items: Vec<FocusHandle> = RowAction::ALL
            .iter()
            .filter(|action| action.offered() && (**action != RowAction::Rename || writable))
            .map(|action| self.row_menu_item_focus[*action as usize].clone())
            .collect();
        Self::menu_key_down(event, &items, window, cx);
    }

    /// The menu the right button opens on a row, floating where the pointer is.
    fn row_menu_view(
        &mut self,
        index: usize,
        at: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let s = self.controller.s();
        let mut menu = div()
            .id("row-menu")
            .role(Role::Menu)
            .aria_label(s.archive_contents)
            .absolute()
            .left(at.x)
            .top(at.y)
            .w(px(240.))
            .flex()
            .flex_col()
            .gap_px()
            .p_1()
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_lg)
            .shadow_lg()
            .occlude()
            .track_focus(&self.row_menu_focus)
            .tab_group()
            .focus_visible(focus_ring(cx))
            .on_key_down(cx.listener(Self::row_menu_key_down));
        let writable = self.controller.state.format == super::Format::Zip;
        // Only a file has anything to look at, so a folder is not offered a
        // viewer it would refuse.
        let is_file = self
            .controller
            .visible_rows()
            .get(index)
            .is_some_and(|row| row.entry.is_some());
        for action in RowAction::ALL.into_iter().filter(|a| {
            a.offered()
                && (*a != RowAction::Rename || writable)
                && (*a != RowAction::View || is_file)
        }) {
            let (label, keys) = action.label(s);
            let item_focus = self.row_menu_item_focus[action as usize].clone();
            let item = Self::menu_item(
                ("row-menu-item", action as usize),
                if keys.is_empty() {
                    label.to_string()
                } else {
                    format!("{label}\t{keys}")
                },
                label.to_string(),
                true,
                cx,
            )
            .track_focus(&item_focus)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.row_action(action, index, window, cx);
            }));
            menu = menu.child(item);
        }
        menu
    }

    /// The list's geometry, or nothing before it has been laid out once.
    fn list_view(&self) -> Option<ListView> {
        let state = self.list_scroll.0.borrow();
        let row = f32::from(state.last_item_size?.item.height);
        if row <= 0.0 {
            return None;
        }
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
        let Some(view) = self.list_view() else { return };
        let (x, y) = (f32::from(at.x), f32::from(at.y));
        if y < view.top || y > view.bottom || x < view.left || x > view.right {
            return;
        }
        let rows = self.controller.visible_rows();
        let anchor = Self::row_under(&view, y, rows.len());
        if let Some(index) = anchor {
            if !secondary && self.controller.is_checked(&rows[index]) {
                return;
            }
        }
        self.band = Some(Band {
            origin: at,
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
        let Some(band) = &mut self.band else {
            return false;
        };
        band.head = at;
        let travelled = (f32::from(at.x) - f32::from(band.origin.x)).hypot(
            f32::from(at.y) - f32::from(band.origin.y),
        );
        if !band.live && travelled < DRAG_SLOP {
            return false;
        }
        band.live = true;
        let anchor = band.anchor;
        let base = band.base.clone();
        let Some(view) = self.list_view() else {
            return false;
        };
        let rows = self.controller.visible_rows();
        if rows.is_empty() {
            return false;
        }
        let y = f32::from(at.y);
        let head = Self::row_under(&view, y.clamp(view.top, view.bottom), rows.len())
            .unwrap_or(rows.len() - 1);
        let (lo, hi) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        self.controller.state.checked.clone_from(&base);
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
        true
    }

    /// Drops the anchor, or picks it back up.
    fn toggle_wheel(&mut self, at: gpui::Point<gpui::Pixels>) {
        if self.wheel.is_some() || !self.background_idle() {
            self.wheel = None;
            return;
        }
        let Some(view) = self.list_view() else { return };
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
        let Some(view) = self.list_view() else {
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
        [&self.filter, &self.password, &self.output_name, &self.add_password]
            .iter()
            .any(|input| input.read(cx).focus_handle.is_focused(window))
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
                match cursor.and_then(|index| rows.get(index)) {
                    Some(row) if self.controller.state.format == super::Format::Zip => {
                        self.name_value = row.label.clone();
                        self.controller.state.renaming =
                            Some((row.path.clone(), row.label.clone()));
                    }
                    _ => return,
                }
            }
            Shortcut::PickGroup(adding) if archive.is_some() => {
                self.name_value.clear();
                self.controller.state.picking_group = Some(adding);
            }
            Shortcut::View if archive.is_some() => {
                let cursor = self.controller.state.cursor;
                let rows = self.controller.visible_rows();
                match cursor.and_then(|index| rows.get(index)).and_then(|row| row.entry) {
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
    fn shortcuts_dialog(&mut self, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let s = self.controller.s();
        let left = [
            ("Ctrl+O", s.open),
            ("Ctrl+N", s.compress),
            ("Ctrl+E", s.extract_all),
            ("Alt+W", s.extract_here),
            ("F3", s.view_word),
            ("Ctrl+T", s.test_word),
            ("F5", s.refresh_word),
            ("Ctrl+F", s.find_word),
            ("", ""),
            ("Ctrl+Z", s.undo_word),
            ("Ctrl+P", s.default_password),
            ("Ctrl+A", s.select_all),
            ("Ctrl+I", s.invert_selection),
            ("Esc", s.clear_selection),
            ("Space", s.toggle_word),
            ("Num +  -", s.select_group),
            ("F2", s.rename_word),
            ("Supr", s.delete_word),
            ("F1", s.shortcuts_title),
        ];
        let right = [
            ("Ctrl+C", s.copy_word),
            ("Ctrl+X", s.cut_word),
            ("Ctrl+V", s.paste_word),
            ("Ctrl+Shift+C", s.copy_names),
            ("", ""),
            ("Enter", s.open_word),
            ("Backspace", s.up),
            ("\u{2191} \u{2193}", s.move_word),
            ("Home  End", s.move_word),
            ("PageUp  PageDown", s.move_word),
            ("Tab", s.jump_word),
        ];
        let column = |rows: &[(&str, &str)]| {
            rows.iter()
                .filter(|(key, _)| !key.is_empty())
                .fold(div().flex().flex_col().gap_1(), |column, (key, what)| {
                    column.child(
                        div()
                            .flex()
                            .gap_3()
                            .text_sm()
                            .child(
                                div()
                                    .w(px(130.))
                                    .flex_none()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(key.to_string()),
                            )
                            .child(what.to_string()),
                    )
                })
        };
        let close = Self::dialog_button(
            "shortcuts-close",
            s.close,
            &self.dialog_cancel_focus,
            true,
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.controller.state.show_shortcuts = false;
            cx.notify();
        }));
        let body = div()
            .id("shortcuts-dialog-body")
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .gap_8()
                    .child(column(&left))
                    .child(column(&right)),
            )
            .child(close);
        self.dialog_overlay(
            ModalKind::Shortcuts,
            s.shortcuts_title,
            s.shortcuts_title,
            body,
            cx,
        )
    }

    fn background_idle(&self) -> bool {
        !self.controller.state.busy
            && self.modal_kind().is_none()
            && self.dialog.is_none()
            && !self.overflow_open
            && !self.breadcrumbs_open
            && self.row_menu.is_none()
    }

    /// How many of the recent archives the menu actually draws.
    fn recent_shown(&self) -> usize {
        self.controller.state.settings.recent.len().min(RECENT_MAX)
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
        !self.filter.read(cx).focus_handle.is_focused(window) && self.modal_kind().is_none()
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

    fn column_width(&self, column: SortColumn) -> f32 {
        let slot = Self::column_slot(column);
        self.controller
            .state
            .settings
            .widths
            .get(slot)
            .copied()
            .unwrap_or_else(|| Settings::default_widths()[slot])
    }

    /// What a column would have to be to hold what is in it.
    ///
    /// ponytail: counted in characters against a nominal advance rather than
    /// shaped through the text system, which is not reachable from a mouse
    /// handler. Fit the real shaped width if a proportional face ever makes
    /// this visibly wrong.
    fn natural_width(&self, column: SortColumn, rows: &[super::Row]) -> f32 {
        let head = Columns::label(column, self.controller.s()).chars().count();
        let widest = rows
            .iter()
            .map(|row| self.column_text(row, column).chars().count())
            .max()
            .unwrap_or(0);
        let slot = Self::column_slot(column);
        (widest.max(head) as f32 * 7.2 + 24.0).clamp(Settings::least(slot), 640.0)
    }

    fn column_header(
        &self,
        column: SortColumn,
        label: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let s = self.controller.s();
        let active = self.controller.state.order.0 == column;
        let ascending = self.controller.state.order.1;
        let direction = if ascending { s.ascending } else { s.descending };
        let text = if active {
            format!("{} {}", label, if ascending { "↑" } else { "↓" })
        } else {
            label.to_string()
        };
        let accessible = if active {
            format!("{} {label} ({direction})", s.sort_by)
        } else {
            format!("{} {label}", s.sort_by)
        };
        let mut cell = div()
            .id(label)
            .aria_label(accessible)
            .aria_keyshortcuts("Enter")
            .tab_stop(enabled)
            .focus_visible(focus_ring(cx))
            .px_2()
            .items_center()
            .flex()
            .text_sm()
            .text_color(if active {
                cx.theme().foreground
            } else {
                cx.theme().table_head_foreground
            })
            .child(text);
        if enabled {
            cell = cell.role(Role::Button).focusable();
        }
        if column == SortColumn::Name {
            cell = cell.flex_1();
        } else {
            cell = cell.w(px(self.column_width(column))).flex_none();
        }
        if enabled {
            cell = cell.on_click(cx.listener(move |this, _, _, cx| {
                if this.background_idle() {
                    this.controller.dispatch(AppAction::Sort(column));
                    cx.notify();
                }
            }));
        }
        cell
    }

    /// The grab strip down the right edge of a header cell.
    ///
    /// Absolute inside the cell rather than an element of its own in the flex
    /// row: a divider with a width would push every header a few pixels off
    /// the column it names.
    fn column_edge(&self, column: SortColumn, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let slot = Self::column_slot(column);
        let width = self.column_width(column);
        div()
            .id(("column-edge", column as usize))
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(-3.))
            .w(px(6.))
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .hover(|style| style.bg(cx.theme().ring))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        // Fitting the column to what is in it, which is what the
                        // same gesture does in WinRAR and in the Explorer.
                        let rows = this.controller.visible_rows();
                        let fitted = this.natural_width(column, &rows);
                        this.set_column_width(slot, fitted);
                        this.controller.state.settings.save();
                    } else {
                        this.resizing = Some((slot, f32::from(event.position.x), width));
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
    }

    fn set_column_width(&mut self, slot: usize, width: f32) {
        if self.controller.state.settings.widths.len() <= slot {
            self.controller.state.settings.widths = Settings::default_widths();
        }
        self.controller.state.settings.widths[slot] = width.max(Settings::least(slot));
    }

    fn kind_mark(kind: Kind) -> &'static str {
        match kind {
            Kind::Dir => "▰",
            Kind::Image => "▧",
            Kind::Text => "▤",
            Kind::Archive => "▱",
            Kind::Audio => "♫",
            Kind::Video => "▶",
            Kind::Other => "□",
        }
    }

    fn text_cell(
        text: impl Into<gpui::SharedString>,
        column: SortColumn,
        width: f32,
        index: usize,
        column_index: usize,
        accessible: bool,
    ) -> Stateful<gpui::Div> {
        let mut cell = div()
            .id(("file-cell", index * 10 + column as usize))
            .w(px(width))
            .flex_none()
            .px_2()
            .items_center()
            .flex()
            .text_sm()
            .child(text.into());
        if accessible {
            cell = cell.role(Role::Cell).aria_column_index(column_index);
        }
        cell
    }

    fn column_text(&self, row: &super::Row, column: SortColumn) -> String {
        match column {
            SortColumn::Name => row.label.clone(),
            SortColumn::Size => human(row.size),
            SortColumn::Packed => human(row.packed),
            SortColumn::Method => {
                if row.is_dir {
                    format!("{} {}", row.count, self.controller.s().items_word)
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

    fn file_row(
        &self,
        index: usize,
        row: &super::Row,
        columns: &[SortColumn],
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let selected = self.controller.is_checked(row);
        let cursor = self.controller.state.cursor == Some(index);
        let muted = row.entry.is_some_and(|entry| {
            self.controller
                .state
                .cut_names
                .contains(&self.controller.state.entries[entry].name)
        });
        let s = self.controller.s();
        let mut description = format!("{} {}", s.col_name, row.label);
        for column in columns.iter().copied().skip(1) {
            let label = Columns::label(column, s);
            description.push_str(&format!("; {label} {}", self.column_text(row, column)));
        }
        description.push_str("; ");
        description.push_str(if selected { s.checked } else { s.not_checked });
        let accessible = self.background_idle();
        // A cut entry is still there until it lands somewhere; it is drawn in
        // the muted ink so it reads as "about to leave" rather than as gone.
        let name_color = if muted {
            cx.theme().muted_foreground
        } else {
            cx.theme().foreground
        };
        let mut name = div()
            .id(("file-name-cell", index))
            .flex_1()
            .px_2()
            .items_center()
            .flex()
            .gap_2()
            .min_w(px(140.))
            .text_sm()
            .text_color(name_color)
            .child(
                div()
                    .w(px(18.))
                    .flex_none()
                    // The type mark is the one place a folder is allowed to
                    // out-shout a file, and in a monochrome window that is done
                    // with weight, not hue: full ink for a folder, muted for
                    // everything else.
                    .text_color(if row.is_dir {
                        name_color
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(Self::kind_mark(row.kind)),
            )
            .child(div().flex_1().truncate().child(row.label.clone()));
        if accessible {
            name = name.role(Role::Cell).aria_column_index(1);
        }

        let mut item = div()
            .id(("file-row", index))
            .aria_label(description)
            .aria_selected(selected)
            .aria_row_index(index + 2)
            .h(px(26.))
            .w_full()
            .px_1()
            .flex()
            .items_center()
            // Where the keyboard is and what is picked are two different
            // things, so they get two strengths of the same ink rather than two
            // colours: moving the cursor onto a picked row has to leave both
            // still visible.
            .border_1()
            .border_color(if cursor {
                cx.theme().table_active_border
            } else {
                cx.theme().table_row_border
            })
            .bg(if selected {
                cx.theme().table_active
            } else if index % 2 == 1 {
                cx.theme().table_even
            } else {
                cx.theme().table
            })
            .hover(|style| style.bg(cx.theme().table_hover))
            .tab_stop(false)
            .focus_visible(focus_ring(cx))
            .child(name);
        if accessible {
            item = item.role(Role::Row).focusable();
            if cursor {
                item = item.aria_active_descendant();
            }
        }
        for (offset, column) in columns.iter().copied().skip(1).enumerate() {
            item = item.child(Self::text_cell(
                self.column_text(row, column),
                column,
                self.column_width(column),
                index,
                offset + 2,
                accessible,
            ));
        }
        if accessible {
            item = item.on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select_row(index, event, window, cx);
            }));

            // Right clicking something that is not picked picks it, which is
            // what every file list does; right clicking inside a selection
            // leaves the selection alone.
            item = item.on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if !this.background_idle() {
                        return;
                    }
                    if !selected {
                        if let Some(row) = this.controller.visible_rows().get(index) {
                            this.controller.dispatch(AppAction::SetChecked {
                                row: row.clone(),
                                value: true,
                            });
                        }
                    }
                    this.controller.state.cursor = Some(index);
                    this.row_menu = Some((index, event.position));
                    let menu_focus = this.row_menu_item_focus[0].clone();
                    window.on_next_frame(move |window, cx| window.focus(&menu_focus, cx));
                    cx.stop_propagation();
                    cx.notify();
                }),
            );

            // A folder takes what is dropped on it and the entries move there,
            // which is a rewrite of the archive and not a copy out of it.
            // Dropping a folder into itself is not a move, so it is refused.
            if row.is_dir {
                let target = row.path.clone();
                item = item.on_drop(cx.listener(
                    move |this: &mut Self, _: &DraggedRows, _window, cx| {
                        let carried = this.controller.selected_roots();
                        let into_itself = carried.iter().any(|carried| {
                            carried.trim_end_matches('/') == target.trim_end_matches('/')
                        });
                        if !carried.is_empty() && !into_itself {
                            this.controller.move_into(&carried, &target);
                        }
                        cx.notify();
                    },
                ));
            }

            // GPUI owns the threshold and gesture lifetime. Where the drag is
            // going is not decided here: a folder of this archive takes it as a
            // move, and everything else on Windows hands it to arca-drag's lazy
            // IDataObject, so no archive bytes are extracted merely to begin a
            // drag. On other platforms there is no drag out, because arca-drag
            // has no backend there, but a move inside the archive still works.
            if selected {
                item = item.on_drag(DraggedRows, move |_, _, _window, app| {
                    app.new(|_| gpui::Empty)
                });
                #[cfg(windows)]
                {
                    let shell = cx.entity();
                    item = item.on_drag_move(move |event: &gpui::DragMoveEvent<DraggedRows>, window, app| {
                        // Out of the list is out of the archive. Started here
                        // rather than at the press, because the instant the
                        // native drag begins the system takes the pointer and
                        // there is no way back into the list.
                        if event.bounds.contains(&event.event.position) {
                            return;
                        }
                        let _ = shell.update(app, |shell, cx| {
                            shell.controller.drag_out();
                            cx.notify();
                        });
                        window.on_next_frame(|window, app| {
                            app.stop_active_drag(window);
                        });
                    });
                }
            }
        }
        item
    }

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
            self.dialog_return_focus = self.delete_trigger_focus.clone();
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

    fn file_table(&self, rows: Vec<super::Row>, cx: &mut Context<Self>) -> Stateful<gpui::Div> {
        let enabled = self.background_idle();
        let columns = self.shown_columns();
        let strings = self.controller.s();
        let labels: Vec<(&'static str, SortColumn)> = columns
            .iter()
            .map(|column| (Columns::label(*column, strings), *column))
            .collect();
        let mut header = labels.iter().enumerate().fold(
            div()
                .id("file-header")
                .aria_row_index(1)
                .h(px(32.))
                .w_full()
                .flex()
                .items_center()
                .px_1()
                .bg(cx.theme().table_head)
                .border_b_1()
                .border_color(cx.theme().border),
            |header, (position, (label, column))| {
                let mut cell = div()
                    .id(("header-cell", *column as usize))
                    .aria_label(*label)
                    .aria_column_index(position + 1)
                    .h_full();
                if *column == SortColumn::Name {
                    cell = cell.flex_1();
                } else {
                    cell = cell.w(px(self.column_width(*column))).flex_none();
                    // The rule that pulls the column wider, sitting in the gap
                    // between two cells rather than taking a place in the row,
                    // so the header and the rows below it stay lined up.
                    cell = cell.relative().child(self.column_edge(*column, cx));
                }
                if enabled {
                    cell = cell
                        .role(Role::ColumnHeader)
                        .aria_label(*label)
                        .aria_column_index(position + 1);
                }
                header.child(cell.child(self.column_header(*column, label, enabled, cx)))
            },
        );
        if enabled {
            header = header.role(Role::Row).aria_row_index(1);
        }
        let total = rows.len();
        let column_count = columns.len();
        let row_data = rows;
        let row_columns = columns;
        let list = uniform_list(
            "file-rows",
            total,
            cx.processor(move |this, range: Range<usize>, _window, row_cx| {
                range
                    .map(|index| this.file_row(index, &row_data[index], &row_columns, row_cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.list_scroll)
        .size_full();
        let mut table = div()
            .id("file-table")
            .aria_label(strings.archive_contents)
            .aria_row_count(total + 1)
            .aria_column_count(column_count)
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
            .child(header)
            .child(div().id("file-list").flex_1().min_h(px(1.)).child(list))
            .child(
                div()
                    .id("delete-trigger-focus")
                    .track_focus(&self.delete_trigger_focus)
                    .size_0(),
            );
        if enabled {
            table = table
                .role(Role::Table)
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
    Extract { only_checked: bool },
    /// Files to put inside the archive that is already open.
    AddFiles,
    /// A copy of the open archive under another name, which is the thing to do
    /// before a change nobody is sure about.
    SaveCopy { name: String, directory: PathBuf },
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
        let s = self.controller.s();
        let modal = self.modal_kind();
        let idle = self.background_idle();
        self.sync_modal_focus(window, cx);
        let password_value = self.controller.state.password_input.clone();
        let add_password_value = self.controller.state.add_password.clone();
        let password_masked = !self.controller.state.show_password;
        self.password.update(cx, |input, _| {
            input.strings = s;
            input.label = s.password_word;
            input.enabled = matches!(
                modal,
                Some(ModalKind::Password | ModalKind::DefaultPassword)
            );
            input.masked = password_masked;
            if input.content != password_value {
                input.sync_from_state(&password_value);
            }
        });
        // The shared text field takes the name of whichever dialog is asking.
        let name_label = match modal {
            Some(ModalKind::Rename) => s.rename_word,
            Some(ModalKind::Mask) => s.mask_hint,
            _ => s.folder_name,
        };
        let name_value = self.name_value.clone();
        self.name_input.update(cx, |input, _| {
            input.strings = s;
            input.label = name_label;
            input.enabled = matches!(
                modal,
                Some(ModalKind::NewFolder | ModalKind::Rename | ModalKind::Mask)
            );
            if input.content != name_value {
                input.sync_from_state(&name_value);
            }
        });
        self.output_name.update(cx, |input, _| {
            input.strings = s;
            input.label = s.output_name;
            input.enabled = matches!(modal, Some(ModalKind::Add));
            if input.content != self.controller.state.output_name {
                input.sync_from_state(&self.controller.state.output_name);
            }
        });
        self.add_password.update(cx, |input, _| {
            input.strings = s;
            input.label = s.password_optional;
            input.enabled = matches!(modal, Some(ModalKind::Add))
                && self.controller.state.format == super::Format::Zip;
            input.masked = password_masked;
            if input.content != add_password_value {
                input.sync_from_state(&add_password_value);
            }
        });
        let state_filter = self.controller.state.filter.clone();
        self.filter.update(cx, |input, _| {
            input.strings = s;
            input.label = s.find_word;
            input.enabled = idle;
        });
        if let Some(value) = filter_value_to_sync(&self.filter.read(cx).content, &state_filter) {
            self.filter
                .update(cx, |input, _| input.sync_from_state(value));
        }
        let has_archive = self.controller.state.archive.is_some();
        let selected = self.selected_count();
        let rows = self.controller.visible_rows();
        let visible = rows.len();
        let can_extract = has_archive && idle;
        let can_extract_selected = can_extract && selected > 0;
        let password_available = can_extract && self.controller.state.format == super::Format::Zip;
        let breadcrumbs = self.crumbs();
        let (shown, hidden) = Self::visible_crumb_indices(breadcrumbs.len());
        self.sync_breadcrumb_item_focus(cx);

        let mut toolbar = div()
            .id("toolbar")
            .aria_label(s.toolbar_region)
            .w_full()
            .flex()
            .items_center()
            .gap_1();
        if !self.background_blocked() {
            toolbar = toolbar.role(Role::Toolbar);
        }

        let open = Self::button("open", s.open, format!("{} (Ctrl+O)", s.open), idle, cx)
            .aria_keyshortcuts("Control+O")
            .track_focus(&self.open_trigger_focus);
        toolbar = toolbar.child(open.on_click(cx.listener(|this, _, window, cx| {
            if this.background_idle() {
                this.dialog_return_focus = this.open_trigger_focus.clone();
                this.begin_dialog(DialogKind::Open, cx);
                window.focus(&this.open_trigger_focus, cx);
            }
        })));
        let compress = Self::button(
            "compress",
            s.compress,
            format!("{} (Ctrl+N)", s.compress),
            idle,
            cx,
        )
        .track_focus(&self.compress_trigger_focus);
        toolbar = toolbar.child(compress.on_click(cx.listener(|this, _, window, cx| {
            if this.background_idle() {
                this.dialog_return_focus = this.compress_trigger_focus.clone();
                this.begin_dialog(DialogKind::Compress, cx);
                window.focus(&this.compress_trigger_focus, cx);
            }
        })));
        toolbar = toolbar.child(div().px_1().child(Separator::vertical().h(px(16.))));

        let extract_all = Self::button(
            "extract-all",
            s.extract_all,
            format!("{} (Ctrl+E)", s.extract_all),
            can_extract,
            cx,
        )
        .track_focus(&self.extract_all_trigger_focus);
        toolbar = toolbar.child(extract_all.on_click(cx.listener(|this, _, window, cx| {
            if !this.background_idle() {
                return;
            }
            this.dialog_return_focus = this.extract_all_trigger_focus.clone();
            window.focus(&this.extract_all_trigger_focus, cx);
            this.begin_dialog(
                DialogKind::Extract {
                    only_checked: false,
                },
                cx,
            );
        })));
        let extract_selected = Self::button(
            "extract-selected",
            s.extract_selected,
            s.extract_selected.to_string(),
            can_extract_selected,
            cx,
        )
        .track_focus(&self.extract_selected_trigger_focus);
        toolbar = toolbar.child(
            extract_selected.on_click(cx.listener(|this, _, window, cx| {
                if !this.background_idle() {
                    return;
                }
                this.dialog_return_focus = this.extract_selected_trigger_focus.clone();
                window.focus(&this.extract_selected_trigger_focus, cx);
                this.begin_dialog(DialogKind::Extract { only_checked: true }, cx);
            })),
        );
        let password = Self::button(
            "password",
            s.password_word,
            format!("{} / {}", s.set_password, s.remove_password),
            password_available,
            cx,
        )
        .track_focus(&self.password_trigger_focus);
        toolbar = toolbar.child(password.on_click(cx.listener(|this, _, window, cx| {
            if this.background_idle() {
                this.dialog_return_focus = this.password_trigger_focus.clone();
                window.focus(&this.password_trigger_focus, cx);
                this.controller.dispatch(AppAction::BeginPasswordChange);
                cx.notify();
            }
        })));
        toolbar = toolbar.child(div().px_1().child(Separator::vertical().h(px(16.))));
        let overflow = Self::button("overflow", s.more_word, s.more_word.to_string(), idle, cx)
            .track_focus(&self.overflow_trigger_focus)
            .aria_expanded(self.overflow_open);
        toolbar = toolbar.child(overflow.on_click(cx.listener(|this, _, window, cx| {
            if this.menu_enabled() {
                this.overflow_open = !this.overflow_open;
                this.breadcrumbs_open = false;
                if this.overflow_open {
                    let menu_focus = this.overflow_item_focus[0].clone();
                    window.on_next_frame(move |window, cx| window.focus(&menu_focus, cx));
                }
                cx.notify();
            }
        })));

        // The filter sits at the far end of the bar, the way a search field
        // does in every file manager on the desktop, instead of stretching
        // across whatever room the buttons left over.
        let filter_input = self.filter.clone();
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
            cx,
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
            cx,
        );
        nav = nav.child(forward.on_click(cx.listener(|this, _, _, cx| {
            if this.background_idle() && this.controller.can_go_forward() {
                this.controller.dispatch(AppAction::Forward);
                this.route_changed(cx);
            }
        })));
        let up = Self::icon_button(
            "up",
            IconName::ArrowUp,
            s.up.to_string(),
            idle && !at_root,
            cx,
        );
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
                let more =
                    Self::button("crumb-more", "…", s.hidden_folders.to_string(), idle, cx)
                    .track_focus(&self.breadcrumbs_trigger_focus)
                    .aria_expanded(self.breadcrumbs_open);
                nav = nav.child(more.on_click(cx.listener(|this, _, window, cx| {
                    if !this.background_idle() {
                        return;
                    }
                    this.sync_breadcrumb_item_focus(cx);
                    this.breadcrumbs_open = !this.breadcrumbs_open;
                    this.overflow_open = false;
                    if this.breadcrumbs_open {
                        let menu_focus = this.breadcrumbs_item_focus[0].clone();
                        window.on_next_frame(move |window, cx| window.focus(&menu_focus, cx));
                    }
                    cx.notify();
                })));
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
                    cx,
                );
                nav = nav.child(crumb.on_click(cx.listener(move |this, _, _, cx| {
                    if !this.background_idle() {
                        return;
                    }
                    this.controller.dispatch(AppAction::Navigate(path.clone()));
                    this.breadcrumbs_open = false;
                    this.route_changed(cx);
                })));
            }
        }

        if self.breadcrumbs_open && !hidden.is_empty() {
            let mut hidden_menu = div()
                .id("hidden-breadcrumbs")
                .role(Role::Menu)
                .aria_label(s.hidden_folders)
                .absolute()
                .top(px(30.))
                .left(px(70.))
                .w(px(220.))
                .flex()
                .flex_col()
                .gap_px()
                .p_1()
                .bg(cx.theme().popover)
                .text_color(cx.theme().popover_foreground)
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius_lg)
                .shadow_lg()
                .track_focus(&self.breadcrumbs_menu_focus)
                .tab_group()
                .focus_visible(focus_ring(cx))
                .on_key_down(cx.listener(Self::breadcrumbs_key_down));
            for (position, index) in hidden.into_iter().enumerate() {
                let (name, path) = &breadcrumbs[index];
                let path = path.clone();
                let item_focus = self.breadcrumbs_item_focus[position].clone();
                hidden_menu = hidden_menu.child(
                    Self::menu_item(
                        ("hidden-crumb", index),
                        name,
                        fill(s.open_folder, &[("name", name)]),
                        true,
                        cx,
                    )
                    .track_focus(&item_focus)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.background_idle() {
                            return;
                        }
                        this.controller.dispatch(AppAction::Navigate(path.clone()));
                        this.breadcrumbs_open = false;
                        this.route_changed(cx);
                    })),
                );
            }
            nav = nav.child(hidden_menu);
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
        let mut root = div()
            .id("arca-gpui-background")
            .on_action(cx.listener(Self::focus_filter))
            // Pressing anywhere that is not the menu shuts the menu. The menus
            // themselves are `occlude`d, so their own clicks never arrive here.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.row_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
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
                                        shell.dialog_return_focus = focus;
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
                    window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, app| {
                        if !phase.bubble() {
                            return;
                        }
                        dragging.update(app, |shell, cx| {
                            shell.pointer = event.position;
                            let mut moved = false;
                            if let Some((slot, from, width)) = shell.resizing {
                                shell
                                    .set_column_width(slot, width + f32::from(event.position.x) - from);
                                moved = true;
                            }
                            moved |= shell.drag_band(event.position);
                            moved |= shell.wheel.is_some();
                            if moved {
                                cx.notify();
                            }
                        });
                    });
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
                cx,
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
                cx,
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
            let mut progress_view = div()
                .id("progress")
                .aria_label(s.progress_region)
                .aria_value(progress.clone())
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
            let sidebar = self.sidebar(window, cx);
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

        if self.overflow_open && !self.background_blocked() {
            let menu_enabled = self.menu_enabled();
            let has = has_archive && menu_enabled;
            // Floating under the button that opened it, instead of being laid
            // out in the column and shoving the whole window down half a page,
            // which is what a menu in the flow did.
            let mut menu = div()
                .id("overflow-menu")
                .role(Role::Menu)
                .aria_label(s.more_word)
                .absolute()
                .top(px(38.))
                .right(px(232.))
                .w(px(210.))
                .max_h(px(420.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_px()
                .p_1()
                .bg(cx.theme().popover)
                .text_color(cx.theme().popover_foreground)
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius_lg)
                .shadow_lg()
                .track_focus(&self.overflow_menu_focus)
                .tab_group()
                .focus_visible(focus_ring(cx))
                .on_key_down(cx.listener(Self::overflow_key_down));
            let test_focus = self.overflow_item_focus[0].clone().tab_stop(has);
            let test = Self::menu_item("test", s.test_word, s.test_word.to_string(), has, cx)
                .track_focus(&test_focus);
            menu = menu.child(test.on_click(cx.listener(|this, _, _, cx| {
                if this.menu_enabled() {
                    if let Some(archive) = this.controller.state.archive.clone() {
                        this.controller.dispatch(AppAction::Run(Job::Test {
                            archive,
                            only: None,
                        }));
                    }
                }
                this.overflow_open = false;
                cx.notify();
            })));
            let select_focus = self.overflow_item_focus[1].clone().tab_stop(has);
            let select = Self::menu_item(
                "select-all",
                s.select_all,
                s.select_all.to_string(),
                has,
                cx,
            )
            .track_focus(&select_focus);
            menu = menu.child(select.on_click(cx.listener(|this, _, _, cx| {
                if this.menu_enabled() && this.controller.state.archive.is_some() {
                    this.controller.dispatch(AppAction::SelectAllVisible);
                }
                this.overflow_open = false;
                cx.notify();
            })));
            let invert_focus = self.overflow_item_focus[2].clone().tab_stop(has);
            let invert = Self::menu_item(
                "invert",
                s.invert_selection,
                s.invert_selection.to_string(),
                has,
                cx,
            )
            .track_focus(&invert_focus);
            menu = menu.child(invert.on_click(cx.listener(|this, _, _, cx| {
                if this.menu_enabled() && this.controller.state.archive.is_some() {
                    this.controller.dispatch(AppAction::InvertVisible);
                }
                this.overflow_open = false;
                cx.notify();
            })));
            let clear_focus = self.overflow_item_focus[3].clone().tab_stop(has);
            let clear = Self::menu_item(
                "clear",
                s.clear_selection,
                s.clear_selection.to_string(),
                has,
                cx,
            )
            .track_focus(&clear_focus);
            menu = menu.child(clear.on_click(cx.listener(|this, _, _, cx| {
                if this.menu_enabled() && this.controller.state.archive.is_some() {
                    this.controller.dispatch(AppAction::ClearSelection);
                }
                this.overflow_open = false;
                cx.notify();
            })));
            let copy_enabled = self.can_copy_files();
            let copy_focus = self.overflow_item_focus[4].clone().tab_stop(copy_enabled);
            let copy = Self::menu_item(
                "copy-files",
                format!("{}\tCtrl/Cmd+C", s.copy_word),
                s.copy_word.to_string(),
                copy_enabled,
                cx,
            )
            .aria_keyshortcuts("Control+C Meta+C")
            .track_focus(&copy_focus);
            menu = menu.child(copy.on_click(cx.listener(|this, _, window, cx| {
                if this.can_copy_files() {
                    this.dispatch_clipboard(false, window, cx);
                }
                this.overflow_open = false;
                cx.notify();
            })));

            let cut_focus = self.overflow_item_focus[5].clone().tab_stop(copy_enabled);
            let cut = Self::menu_item(
                "cut-files",
                format!("{}\tCtrl/Cmd+X", s.cut_word),
                s.cut_word.to_string(),
                copy_enabled,
                cx,
            )
            .aria_keyshortcuts("Control+X Meta+X")
            .track_focus(&cut_focus);
            menu = menu.child(cut.on_click(cx.listener(|this, _, window, cx| {
                if this.can_copy_files() {
                    this.dispatch_clipboard(true, window, cx);
                }
                this.overflow_open = false;
                cx.notify();
            })));

            let paste_enabled = self.can_paste_files();
            let paste_focus = self.overflow_item_focus[6].clone().tab_stop(paste_enabled);
            let paste = Self::menu_item(
                "paste-files",
                format!("{}\tCtrl/Cmd+V", s.paste_word),
                s.paste_word.to_string(),
                paste_enabled,
                cx,
            )
            .aria_keyshortcuts("Control+V Meta+V")
            .track_focus(&paste_focus);
            menu = menu.child(paste.on_click(cx.listener(|this, _, window, cx| {
                if this.can_paste_files() {
                    this.dispatch_paste(window, cx);
                }
                this.overflow_open = false;
                cx.notify();
            })));

            // Only when there is one, and at the top, where something that was
            // not there yesterday belongs.
            if let Some(release) = self.controller.state.update.clone() {
                let release_focus = self.overflow_item_focus[RELEASE_SLOT]
                    .clone()
                    .tab_stop(menu_enabled);
                let label = fill(s.update_ready, &[("version", &release.tag)]);
                let item = Self::menu_item(
                    "release",
                    label.clone(),
                    label,
                    menu_enabled,
                    cx,
                )
                .track_focus(&release_focus);
                menu = menu.child(item.on_click(cx.listener(|this, _, _, cx| {
                    if this.menu_enabled() {
                        this.overflow_action(OverflowAction::Release, cx);
                    }
                    this.overflow_open = false;
                    cx.notify();
                })));
            }

            let writable = has && self.controller.state.format == super::Format::Zip;
            let can_undo = has && self.controller.state.undo.is_some();
            for (slot, id, label, enabled, action) in [
                (
                    7usize,
                    "add-files",
                    s.add_to_archive,
                    writable,
                    OverflowAction::AddFiles,
                ),
                (
                    8,
                    "new-folder",
                    s.new_folder,
                    writable,
                    OverflowAction::NewFolder,
                ),
                (9, "undo", s.undo_word, can_undo, OverflowAction::Undo),
                (10, "save-copy", s.save_copy, has, OverflowAction::SaveCopy),
                (
                    11,
                    "default-password",
                    s.default_password,
                    menu_enabled,
                    OverflowAction::DefaultPassword,
                ),
            ] {
                let item_focus = self.overflow_item_focus[slot].clone().tab_stop(enabled);
                let item = Self::menu_item(id, label, label.to_string(), enabled, cx)
                    .track_focus(&item_focus);
                menu = menu.child(item.on_click(cx.listener(move |this, _, _, cx| {
                    if this.menu_enabled() {
                        this.overflow_action(action, cx);
                    }
                    this.overflow_open = false;
                    cx.notify();
                })));
            }

            // The archives opened lately, newest first, by path rather than by
            // name so that two called the same thing are told apart.
            let recent: Vec<String> = self
                .controller
                .state
                .settings
                .recent
                .iter()
                .take(RECENT_MAX)
                .cloned()
                .collect();
            for (position, path) in recent.iter().enumerate() {
                let leaf = std::path::Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                let item_focus = self.overflow_item_focus[RECENT_SLOT + position]
                    .clone()
                    .tab_stop(menu_enabled);
                let target = PathBuf::from(path);
                let item = Self::menu_item(
                    ("recent", position),
                    leaf,
                    format!("{} {path}", s.recent_word),
                    menu_enabled,
                    cx,
                )
                .track_focus(&item_focus);
                menu = menu.child(item.on_click(cx.listener(move |this, _, _, cx| {
                    if this.menu_enabled() {
                        this.controller.dispatch(AppAction::Open(target.clone()));
                    }
                    this.overflow_open = false;
                    cx.notify();
                })));
            }
            let clear_history_enabled = menu_enabled && !recent.is_empty();
            let clear_history_focus = self.overflow_item_focus[RECENT_SLOT + RECENT_MAX]
                .clone()
                .tab_stop(clear_history_enabled);
            let clear_history = Self::menu_item(
                "clear-history",
                s.clear_history,
                s.clear_history.to_string(),
                clear_history_enabled,
                cx,
            )
            .track_focus(&clear_history_focus);
            menu = menu.child(clear_history.on_click(cx.listener(|this, _, _, cx| {
                if this.menu_enabled() {
                    this.controller.state.settings.recent.clear();
                    this.controller.state.settings.save();
                }
                this.overflow_open = false;
                cx.notify();
            })));

            let flat = self.controller.state.settings.flat;
            let flat_focus = self.overflow_item_focus[RECENT_SLOT + RECENT_MAX + 1]
                .clone()
                .tab_stop(has);
            let flat_item = Self::menu_item(
                "flat-view",
                if flat {
                    format!("{} ✓", s.flat_view)
                } else {
                    s.flat_view.to_string()
                },
                s.flat_view.to_string(),
                has,
                cx,
            )
            .aria_selected(flat)
            .track_focus(&flat_focus);
            menu = menu.child(flat_item.on_click(cx.listener(|this, _, _, cx| {
                if this.menu_enabled() && this.controller.state.archive.is_some() {
                    let settings = &mut this.controller.state.settings;
                    settings.flat = !settings.flat;
                    // A flat list is a list of names with no folder over them,
                    // so the folder each one came from has to go somewhere. It
                    // is left on afterwards: turning the view off and on again
                    // should not keep undoing a column since arranged by hand.
                    if settings.flat && !settings.columns.on(SortColumn::Path) {
                        settings.columns.set(SortColumn::Path, true);
                    }
                    settings.save();
                    this.controller.dispatch(AppAction::ClearSelection);
                }
                this.overflow_open = false;
                cx.notify();
            })));

            let shortcuts_focus = self.overflow_item_focus[RECENT_SLOT + RECENT_MAX + 2]
                .clone()
                .tab_stop(menu_enabled);
            let shortcuts_item = Self::menu_item(
                "shortcuts",
                format!("{}\tF1", s.shortcuts_title),
                s.shortcuts_title.to_string(),
                menu_enabled,
                cx,
            )
            .aria_keyshortcuts("F1")
            .track_focus(&shortcuts_focus);
            menu = menu.child(shortcuts_item.on_click(cx.listener(|this, _, _, cx| {
                if this.menu_enabled() {
                    this.controller.state.show_shortcuts = true;
                }
                this.overflow_open = false;
                cx.notify();
            })));

            let settings_focus = self.overflow_item_focus[RECENT_SLOT + RECENT_MAX + 3]
                .clone()
                .tab_stop(menu_enabled);
            let settings_item = Self::menu_item(
                "settings",
                s.settings,
                s.settings.to_string(),
                menu_enabled,
                cx,
            )
            .track_focus(&settings_focus);
            menu = menu.child(settings_item.on_click(cx.listener(|this, _, _, cx| {
                if this.menu_enabled() {
                    this.controller.state.show_settings = true;
                }
                this.overflow_open = false;
                cx.notify();
            })));

            let columns_available = menu_enabled;
            for (position, (column, _)) in Columns::ALL.iter().enumerate() {
                let column = *column;
                let label = Columns::label(column, s);
                let shown = self.controller.state.settings.columns.on(column);
                let action = if shown { s.hide_word } else { s.show_word };
                let item_focus = self.overflow_item_focus
                    [position + RECENT_SLOT + RECENT_MAX + 4]
                    .clone()
                    .tab_stop(columns_available);
                let item = Self::menu_item(
                    ("column", position),
                    format!("{action} {label}"),
                    format!("{action} {label}"),
                    columns_available,
                    cx,
                )
                .track_focus(&item_focus);
                menu = menu.child(item.on_click(cx.listener(move |this, _, _, cx| {
                    if this.menu_enabled() {
                        this.controller.dispatch(AppAction::ToggleColumn(column));
                    }
                    this.overflow_open = false;
                    cx.notify();
                })));
            }
            root = root.child(menu);
        }
        // The band, and the anchor the wheel dropped. Both are drawn over
        // everything rather than inside the list, because the hand is free to
        // wander off it while either gesture runs and a mark that vanished at
        // the edge would be worse than no mark at all.
        if let Some(band) = self.band.as_ref().filter(|band| band.live) {
            if let Some(view) = self.list_view() {
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
        if let Some((index, at)) = self.row_menu {
            root = root.child(self.row_menu_view(index, at, cx));
        }
        if self.dialog.is_some() {
            root = root.child(
                div()
                    .id("native-picker-overlay")
                    .absolute()
                    .inset_0()
                    .bg(cx.theme().overlay)
                    .role(Role::Dialog)
                    .aria_label(s.waiting_picker)
                    .occlude()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, _| {})
                    .child(
                        div()
                            .m_8()
                            .p_4()
                            .bg(cx.theme().popover)
                            .text_color(cx.theme().popover_foreground)
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(cx.theme().radius_lg)
                            .shadow_lg()
                            .child(s.waiting_picker),
                    ),
            );
        }
        if let Some(dialog) = self.dialogs(cx) {
            root = root.child(dialog);
        }
        root
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

/// The two ends of a span, in the order they are drawn in.
fn minmax(a: gpui::Pixels, b: gpui::Pixels) -> (f32, f32) {
    let (a, b) = (f32::from(a), f32::from(b));
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn visible_menu_items<T>(items: &[T], rendered_len: usize) -> &[T] {
    &items[..items.len().min(rendered_len)]
}

fn focus_cycle_index(current: Option<usize>, reverse: bool, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match (current, reverse) {
        (None, false) => Some(0),
        (None, true) => Some(len - 1),
        (Some(index), false) => Some((index + 1) % len),
        (Some(index), true) => Some(if index == 0 { len - 1 } else { index - 1 }),
    }
}

fn menu_target(current: usize, key: &str, len: usize) -> Option<usize> {
    match key {
        "down" if current + 1 < len => Some(current + 1),
        "up" if current > 0 => Some(current - 1),
        _ => None,
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

fn filter_value_to_sync<'a>(input: &str, state: &'a str) -> Option<&'a str> {
    (input != state).then_some(state)
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
                KeyBinding::new("backspace", Backspace, Some("FilterInput")),
                KeyBinding::new("ctrl-a", SelectAll, Some("FilterInput")),
                KeyBinding::new("cmd-a", SelectAll, Some("FilterInput")),
            ]);
            let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(MINIMUM_SIZE.0), px(MINIMUM_SIZE.1))),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("Arca".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    |window, cx| cx.new(|cx| GpuiShell::new(window, cx, startup)),
                )
                .expect("open GPUI shell window");
            window
                .update(cx, |shell, window, cx| {
                    let filter_focus = shell.filter.read(cx).focus_handle.clone();
                    window.focus(&filter_focus, cx);
                    cx.activate(true);
                    window.set_window_title("Arca");
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
    fn breadcrumb_navigation_drops_stale_handles_when_route_shrinks() {
        let route_with_several_hidden = GpuiShell::visible_crumb_indices(10).1.len();
        let route_with_fewer_hidden = GpuiShell::visible_crumb_indices(6).1.len();
        let handles: Vec<_> = (0..route_with_several_hidden).collect();
        let visible = visible_menu_items(&handles, route_with_fewer_hidden);

        assert_eq!(visible, &[0, 1]);
        assert_eq!(menu_target(0, "down", visible.len()), Some(1));
        assert_eq!(menu_target(1, "up", visible.len()), Some(0));
        assert_eq!(menu_target(0, "up", visible.len()), None);
        assert_eq!(menu_target(1, "down", visible.len()), None);
    }

    #[test]
    fn menu_navigation_stays_within_the_menu() {
        assert_eq!(menu_target(0, "down", 4), Some(1));
        assert_eq!(menu_target(1, "up", 4), Some(0));
        assert_eq!(menu_target(0, "up", 4), None);
        assert_eq!(menu_target(3, "down", 4), None);
    }

    #[test]
    fn focus_trap_wraps_in_both_directions_and_handles_no_focus() {
        assert_eq!(focus_cycle_index(Some(0), false, 3), Some(1));
        assert_eq!(focus_cycle_index(Some(2), false, 3), Some(0));
        assert_eq!(focus_cycle_index(Some(0), true, 3), Some(2));
        assert_eq!(focus_cycle_index(Some(2), true, 3), Some(1));
        assert_eq!(focus_cycle_index(None, false, 3), Some(0));
        assert_eq!(focus_cycle_index(None, true, 3), Some(2));
        assert_eq!(focus_cycle_index(None, false, 0), None);
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
    fn filter_sync_only_replaces_stale_input() {
        assert_eq!(filter_value_to_sync("old", "new"), Some("new"));
        assert_eq!(filter_value_to_sync("same", "same"), None);
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
        // Same index contract as the settings dialog: the focus vector is
        // indexed by `action as usize`.
        for (index, action) in RowAction::ALL.iter().enumerate() {
            assert_eq!(*action as usize, index);
        }
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
        // The dialog indexes `settings_focus` by `control as usize`, so a
        // control added out of order would silently take another one's focus
        // handle and Enter would activate the wrong preference.
        for (index, control) in SettingsControl::ALL.iter().enumerate() {
            assert_eq!(*control as usize, index);
        }
    }

    #[test]
    fn empty_folder_label_is_not_reported_as_archive_contents() {
        // Both languages, because the point of the label is that it says the
        // folder is empty rather than that the archive is unreadable, and a
        // translation that loses the difference is the same bug in Spanish.
        for lang in super::super::Lang::ALL {
            let s = super::super::strings(lang);
            assert_eq!(empty_state_aria_label(false, "", s), s.empty_folder);
            assert_eq!(empty_state_aria_label(false, "  ", s), s.empty_folder);
            assert_eq!(empty_state_aria_label(false, "zip", s), s.no_matches);
            assert_eq!(empty_state_aria_label(true, "", s), s.cannot_open);
            assert_ne!(s.empty_folder, s.cannot_open);
        }
    }
}
