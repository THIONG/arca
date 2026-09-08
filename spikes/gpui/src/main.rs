#![allow(clippy::type_complexity)]

use std::{ops::Range, path::PathBuf};

use gpui::{
    App, Bounds, Context, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    FileDropEvent, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, Pixels, Role, ScrollStrategy, ShapedLine, Style, TextRun, UTF16Selection,
    Window, WindowBounds, WindowOptions, actions, canvas, div, img, point, prelude::*, px, rgb,
    size, uniform_list,
};
use gpui_platform::application;

const FIXTURE_ROWS: usize = 6_000;

#[derive(Clone)]
struct Row {
    number: usize,
    name: String,
    size: usize,
    packed: usize,
    kind: &'static str,
}

fn fixture_rows() -> Vec<Row> {
    (0..FIXTURE_ROWS)
        .map(|number| Row {
            number,
            name: format!("entry-{number:04}.txt"),
            size: 1024 + number * 37,
            packed: 768 + number * 29,
            kind: if number % 17 == 0 { "dir" } else { "file" },
        })
        .collect()
}

actions!(
    gpui_spike,
    [Backspace, SelectAll, MoveUp, MoveDown, Escape, ToggleModal]
);

struct FilterInput {
    focus_handle: FocusHandle,
    content: String,
    selected_range: Range<usize>,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
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

    fn selected_range_after_mark(
        replacement_start: usize,
        new_text: &str,
        selected_range_utf16: Range<usize>,
    ) -> Range<usize> {
        let selected = Self::utf8_range(new_text, selected_range_utf16);
        replacement_start + selected.start..replacement_start + selected.end
    }

    fn replace(&mut self, range: Range<usize>, value: &str, cx: &mut Context<Self>) {
        self.content.replace_range(range.clone(), value);
        let cursor = range.start + value.len();
        self.selected_range = cursor..cursor;
        self.marked_range = None;
        cx.notify();
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
        let range = range_utf16
            .map(|range| Self::utf8_range(&self.content, range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.content.replace_range(range.clone(), new_text);
        self.marked_range =
            (!new_text.is_empty()).then_some(range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .map(|selected| Self::selected_range_after_mark(range.start, new_text, selected))
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = Self::utf8_range(&self.content, range_utf16);
        Some(Bounds::from_corners(
            point(bounds.left() + line.x_for_index(range.start), bounds.top()),
            point(bounds.left() + line.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        if self.content.is_empty() {
            return Some(0);
        }
        let bounds = self.last_bounds?;
        let line = self.last_layout.as_ref()?;
        let local_point = bounds.localize(&point)?;
        let utf8_index = line.closest_index_for_x(local_point.x);
        Some(Self::utf16_from_utf8(&self.content, utf8_index))
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
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let value = if input.content.is_empty() {
            "Filter the 6,000-row fixture…".to_string()
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
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus_handle, cursor) = {
            let input = self.input.read(cx);
            (input.focus_handle.clone(), input.selected_range.end)
        };
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
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
            let cursor = Bounds::new(
                point(bounds.left() + cursor_x, bounds.top()),
                size(px(2.), bounds.size.height),
            );
            window.paint_quad(gpui::fill(cursor, rgb(0x2f80ed)));
        }
        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for FilterInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("filter-input")
            .key_context("FilterInput")
            .role(Role::TextInput)
            .aria_label("Filter")
            .aria_value(self.content.clone())
            .track_focus(&self.focus_handle)
            .focusable()
            .tab_stop(true)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::select_all))
            .border_1()
            .border_color(rgb(0x8a8f98))
            .rounded_sm()
            .px_2()
            .h(px(32.))
            .child(FilterElement { input: cx.entity() })
    }
}

impl Focusable for FilterInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

struct ResizeStart {
    index: usize,
    start_x: Pixels,
    start_width: Pixels,
}

struct ResizeDrag {
    index: usize,
}

struct GpuiSpike {
    rows: Vec<Row>,
    selected: Vec<usize>,
    cursor: usize,
    anchor: usize,
    columns: [Pixels; 4],
    resize_start: Option<ResizeStart>,
    filter: Entity<FilterInput>,
    focus_handle: FocusHandle,
    modal_focus_handle: FocusHandle,
    scroll_handle: gpui::UniformListScrollHandle,
    modal: bool,
    status: String,
}

impl GpuiSpike {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| FilterInput {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            selected_range: 0..0,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
        });
        let focus_handle = cx.focus_handle();
        let filter_focus = filter.read(cx).focus_handle.clone();
        window.focus(&filter_focus, cx);
        window.set_window_title("Arca — GPUI G1 spike");
        Self {
            rows: fixture_rows(),
            selected: vec![0],
            cursor: 0,
            anchor: 0,
            columns: [px(300.), px(110.), px(110.), px(120.)],
            resize_start: None,
            filter,
            focus_handle,
            modal_focus_handle: cx.focus_handle(),
            scroll_handle: gpui::UniformListScrollHandle::new(),
            modal: false,
            status: "G1: 6,000 fixture rows loaded and virtualized".into(),
        }
    }

    fn select_row(&mut self, index: usize, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.modifiers.shift {
            let start = self.anchor.min(index);
            let end = self.anchor.max(index);
            self.selected = (start..=end).collect();
        } else if event.modifiers.secondary() {
            if let Some(position) = self.selected.iter().position(|value| *value == index) {
                self.selected.remove(position);
            } else {
                self.selected.push(index);
            }
            self.anchor = index;
        } else {
            self.selected = vec![index];
            self.anchor = index;
        }
        self.cursor = index;
        self.status = format!(
            "selected {} row(s), cursor {} — Ctrl/Shift selection",
            self.selected.len(),
            index + 1
        );
        cx.notify();
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.rows.len().saturating_sub(1));
        self.selected = vec![self.cursor];
        self.anchor = self.cursor;
        self.scroll_handle
            .scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        self.status = format!("cursor {} — scroll-to-cursor seam", self.cursor + 1);
        cx.notify();
    }

    fn resize_column(&mut self, index: usize, width: Pixels, cx: &mut Context<Self>) {
        if let Some(column) = self.columns.get_mut(index) {
            *column = width.max(px(60.));
            self.status = format!("column {} resized — relative drag delta", index + 1);
            cx.notify();
        }
    }

    fn open_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal {
            return;
        }
        self.modal = true;
        window.focus(&self.modal_focus_handle, cx);
        cx.notify();
    }

    fn toggle_modal(&mut self, _: &ToggleModal, window: &mut Window, cx: &mut Context<Self>) {
        self.open_modal(window, cx);
    }

    fn escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal {
            self.modal = false;
            let filter_focus = self.filter.read(cx).focus_handle.clone();
            window.focus(&filter_focus, cx);
            cx.notify();
        }
    }
}

impl Focusable for GpuiSpike {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GpuiSpike {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let list_view = view.clone();
        let list = uniform_list("entries", self.rows.len(), move |range, _, app| {
            let state = list_view.read(app);
            range
                .map(|index| {
                    let row = &state.rows[index];
                    let selected = state.selected.contains(&index);
                    let row_view = list_view.clone();
                    div()
                        .id(("entry", index))
                        .role(Role::ListItem)
                        .aria_label(format!("{} {}", row.kind, row.name))
                        .aria_selected(selected)
                        .aria_position_in_set(index + 1)
                        .aria_size_of_set(state.rows.len())
                        .flex()
                        .items_center()
                        .h(px(30.))
                        .px_2()
                        .when(selected, |this| this.bg(rgb(0xdbeafe)))
                        .on_mouse_down(MouseButton::Left, move |event, _, app| {
                            row_view.update(app, |this, cx| this.select_row(index, event, cx));
                        })
                        .children(
                            [
                                format!("{:04}  {}", row.number, row.name),
                                row.size.to_string(),
                                row.packed.to_string(),
                                row.kind.to_string(),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(column, value)| {
                                div()
                                    .w(state.columns[column])
                                    .px_2()
                                    .overflow_hidden()
                                    .child(value)
                            }),
                        )
                })
                .collect()
        })
        .h_full()
        .track_scroll(&self.scroll_handle);

        let header = ["Name", "Size", "Packed", "Kind"]
            .into_iter()
            .enumerate()
            .map(|(index, label)| {
                let width = self.columns[index];
                let view = view.clone();
                div()
                    .flex()
                    .items_center()
                    .w(width)
                    .h(px(30.))
                    .px_2()
                    .border_r_1()
                    .border_color(rgb(0xb7bec8))
                    .child(label)
                    .child({
                        let drag_view = view.clone();
                        let start_width = self.columns[index];
                        div()
                            .id(("resize", index))
                            .ml_auto()
                            .w(px(6.))
                            .h_full()
                            .cursor_col_resize()
                            .on_drag(ResizeDrag { index }, move |drag, position, _, app| {
                                drag_view.update(app, |this, cx| {
                                    this.resize_start = Some(ResizeStart {
                                        index: drag.index,
                                        start_x: position.x,
                                        start_width,
                                    });
                                    cx.notify();
                                });
                                app.new(|_| ResizeGhost)
                            })
                            .on_drag_move(move |event: &gpui::DragMoveEvent<ResizeDrag>, _, app| {
                                let index = event.drag(app).index;
                                view.update(app, |this, cx| {
                                    if let Some(start) = &this.resize_start
                                        && start.index == index
                                    {
                                        let delta = event.event.position.x - start.start_x;
                                        this.resize_column(index, start.start_width + delta, cx);
                                    }
                                });
                            })
                    })
            })
            .collect::<Vec<_>>();

        let drop_probe = {
            let view = view.clone();
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let view = view.clone();
                    window.on_mouse_event(move |event: &FileDropEvent, _, _, app| {
                        if matches!(
                            event,
                            FileDropEvent::Entered { .. } | FileDropEvent::Submit { .. }
                        ) {
                            view.update(app, |this, cx| {
                                this.status = "GPUI file-drop event received".into();
                                cx.notify();
                            });
                        }
                    });
                },
            )
            .size_0()
        };

        let mut root = div()
            .id("gpui-spike-root")
            .role(Role::Application)
            .aria_label("Arca GPUI G1 spike")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_modal))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if !this.modal {
                    this.move_cursor(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if !this.modal {
                    this.move_cursor(1, cx);
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(rgb(0xf6f7f9))
            .text_color(rgb(0x1f2937));

        if !self.modal {
            root = root
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(self.filter.clone())
                        .child(
                            div()
                                .id("modal-button")
                                .role(Role::Button)
                                .focusable()
                                .tab_stop(true)
                                .cursor_pointer()
                                .px_3()
                                .py_2()
                                .bg(rgb(0x2563eb))
                                .text_color(rgb(0xffffff))
                                .rounded_sm()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_modal(window, cx);
                                }))
                                .child("Open blocking modal"),
                        )
                        .child(
                            img(PathBuf::from(concat!(
                                env!("CARGO_MANIFEST_DIR"),
                                "/fixtures/icon-rgba.png"
                            )))
                            .size_6(),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .text_sm()
                        .text_color(rgb(0x4b5563))
                        .child(self.status.clone()),
                )
                .child(div().flex().bg(rgb(0xe5e7eb)).children(header))
                .child(list)
                .child(drop_probe);
        } else {
            root = root.child(
                div()
                    .id("modal-shield")
                    .absolute()
                    .inset_0()
                    .bg(rgb(0x111827).opacity(0.45))
                    .on_mouse_down(MouseButton::Left, |_, _, _| {})
                    .child(
                        div()
                            .id("modal")
                            .role(Role::Dialog)
                            .aria_label("Blocking GPUI spike modal")
                            .track_focus(&self.modal_focus_handle)
                            .focusable()
                            .tab_stop(true)
                            .m_10()
                            .p_5()
                            .bg(rgb(0xffffff))
                            .border_1()
                            .border_color(rgb(0x374151))
                            .rounded_md()
                            .child("Background input is blocked. Press Escape to close."),
                    ),
            );
        }
        root
    }
}

struct ResizeGhost;

impl Render for ResizeGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_0()
    }
}

#[cfg(all(feature = "platform-probes", target_os = "windows"))]
fn platform_probes_compile() {
    let _file_dialog = rfd::FileDialog::new();
    let _clipboard_open: fn() -> clipboard_win::SysResult<clipboard_win::Clipboard> =
        clipboard_win::Clipboard::new;
    let _drag: fn(
        Vec<arca_drag::Item>,
        Box<dyn Fn(usize) -> Option<PathBuf>>,
        bool,
    ) -> arca_drag::Effect = arca_drag::drag;
}

fn main() {
    #[cfg(all(feature = "platform-probes", target_os = "windows"))]
    platform_probes_compile();
    application().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, Some("FilterInput")),
            KeyBinding::new("cmd-a", SelectAll, Some("FilterInput")),
            KeyBinding::new("ctrl-a", SelectAll, Some("FilterInput")),
            KeyBinding::new("up", MoveUp, None),
            KeyBinding::new("down", MoveDown, None),
            KeyBinding::new("escape", Escape, None),
        ]);
        let bounds = Bounds::centered(None, size(px(920.), px(640.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| GpuiSpike::new(window, cx)),
            )
            .expect("open GPUI spike window");
        window
            .update(cx, |view, window, cx| {
                let filter_focus = view.filter.read(cx).focus_handle.clone();
                window.focus(&filter_focus, cx);
                cx.activate(true);
            })
            .expect("focus GPUI spike filter");
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_has_the_g0_shape() {
        let rows = fixture_rows();
        let fixture = include_str!("../fixtures/entries-6000.txt");
        assert_eq!(rows.len(), FIXTURE_ROWS);
        assert_eq!(fixture.lines().count(), FIXTURE_ROWS);
        assert!(fixture.lines().next().unwrap().contains("entry-0000.txt"));
        assert!(fixture.lines().last().unwrap().contains("entry-5999.txt"));
    }

    #[test]
    fn utf16_ranges_handle_accent_emoji_and_nonzero_offsets() {
        let text = "pre-é🙂-post";
        assert_eq!(FilterInput::utf8_range(text, 4..7), 4..10);
        assert_eq!(FilterInput::utf16_range(text, 4..10), 4..7);
        assert_eq!(FilterInput::utf8_from_utf16(text, 5), 6);
        assert_eq!(FilterInput::utf16_from_utf8(text, 10), 7);
    }

    #[test]
    fn marked_selection_uses_offsets_in_the_new_text() {
        let selected = FilterInput::selected_range_after_mark(6, "🙂é", 2..3);
        assert_eq!(selected, 10..12);
    }

    #[test]
    fn shift_selection_is_contiguous() {
        let mut selected = vec![2, 3, 4];
        selected.extend(5..=7);
        assert_eq!(selected, vec![2, 3, 4, 5, 6, 7]);
    }
}
