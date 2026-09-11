//! Text input adapter and native dialog results for the GPUI shell.

use super::{AppAction, Backspace, GpuiShell, SelectAll};
use crate::Strings;
use gpui::{
    div, point, prelude::*, px, size, App, Bounds, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, LayoutId, Role, ShapedLine, Style,
    TextRun, UTF16Selection, WeakEntity, Window,
};
use gpui_component::ActiveTheme;
use std::ops::Range;
use std::path::PathBuf;

pub(super) enum DialogResult {
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
pub(super) enum TextFieldKind {
    Password,
    OutputName,
    AddPassword,
    /// The one field shared by the dialogs that ask for a piece of text: a new
    /// folder, a new name, a mask. They are modal and mutually exclusive, so
    /// one field with the name the open dialog gives it is one field, not
    /// three that are always empty.
    Name,
}

pub(super) struct FilterInput {
    pub(super) owner: WeakEntity<GpuiShell>,
    pub(super) kind: TextFieldKind,
    /// The field reads its own name out to a screen reader, so it needs the
    /// language too. The shell pushes it in on every frame, because the
    /// settings dialog can change it while the window is up.
    pub(super) strings: &'static Strings,
    /// What a screen reader calls this field. Set by the shell each frame,
    /// because the shared `Name` field is a folder name in one dialog and a
    /// mask in another.
    pub(super) label: &'static str,
    pub(super) masked: bool,
    pub(super) focus_handle: FocusHandle,
    pub(super) enabled: bool,
    pub(super) content: String,
    pub(super) selected_range: Range<usize>,
    pub(super) marked_range: Option<Range<usize>>,
    pub(super) last_layout: Option<ShapedLine>,
    pub(super) last_bounds: Option<Bounds<gpui::Pixels>>,
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

    pub(super) fn sync_from_state(&mut self, value: &str) {
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
