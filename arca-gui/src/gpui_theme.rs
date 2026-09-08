//! Arca's monochrome design tokens, written onto the GPUI Kit theme.
//!
//! Every colour in the GPUI window comes from here. `gpui-component` already
//! owns a full token vocabulary — `background`, `border`, `table_hover`,
//! `ring`, and a hundred more — and every component it ships reads them, so
//! Arca does not need a token layer of its own. What it needs is to *say what
//! those tokens are*, once, for light and for dark, and that is the whole file.
//!
//! The palette is monochrome by design, not by omission. There are six greys
//! and they are the same six greys inverted between the two modes. They are not
//! neutral: they carry a few percent of the night blue from `brand/BRAND.md`
//! (hue 225), which is the difference between a quiet window and a screenshot
//! of a terminal.
//!
//! There is no accent colour. Selection, the keyboard cursor and the focus ring
//! are all the *text* colour at different strengths, so contrast is guaranteed
//! by construction: anything that is legible as text is legible as a selection.
//!
//! The one exception is danger and warning. Those are not decoration; they are
//! the difference between "extracted" and "did not extract", and a user who
//! scans before reading has to be able to see it. They stay at the lowest
//! chroma that still reads as red or amber against both grounds, and they are
//! the only saturated pixels Arca paints.

use crate::ThemePreference;
use gpui::{px, App, Hsla, Rgba, Window};
use gpui_component::theme::{Theme, ThemeMode};

/// Corners. Small enough to read as a finish rather than as a shape.
const RADIUS: f32 = 4.0;
const RADIUS_LG: f32 = 6.0;

/// The body size of the window, and the slightly smaller size the columns of
/// numbers and dates are set in: a monospace face at the same size always looks
/// a size bigger.
const FONT_SIZE: f32 = 13.0;
const MONO_FONT_SIZE: f32 = 12.0;

/// The six greys, plus the two states that are allowed to have a hue.
///
/// `surface` sits on `background`, `raised` sits on `surface`. Three grounds is
/// as many as a window this size can tell apart; a fourth reads as noise.
struct Palette {
    background: u32,
    surface: u32,
    raised: u32,
    border: u32,
    text: u32,
    muted: u32,
    danger: u32,
    warning: u32,
}

/// Night, with the brand hue held at about 10% saturation.
const DARK: Palette = Palette {
    background: 0x0E1014,
    surface: 0x14161B,
    raised: 0x1B1E25,
    border: 0x262A33,
    text: 0xE4E6EB,
    muted: 0x8B909C,
    danger: 0xE5787C,
    warning: 0xD9A441,
};

/// The same palette turned over. Not `Visuals::light()` inverted by formula:
/// a light window needs its steps closer together or the chrome starts to
/// stripe.
const LIGHT: Palette = Palette {
    background: 0xFAFAFB,
    surface: 0xFFFFFF,
    raised: 0xF3F4F6,
    border: 0xE3E5EA,
    text: 0x14161B,
    muted: 0x666B76,
    danger: 0xB3262B,
    warning: 0x8A5A00,
};

/// A hex literal as GPUI sees colours.
fn hex(value: u32) -> Hsla {
    Rgba::from(gpui::rgb(value)).into()
}

/// The same colour at a fraction of its opacity.
///
/// This is what carries the whole monochrome scheme: a selected row is the text
/// colour at 12%, a hovered row is the text colour at 5%, and neither can drift
/// out of contrast with the text sitting on it because it *is* the text.
fn alpha(value: u32, a: f32) -> Hsla {
    let mut color = hex(value);
    color.a = a;
    color
}

/// Register GPUI Kit's theme machinery. Call once, before the first window.
pub fn init(cx: &mut App) {
    gpui_component::init(cx);
}

/// Resolve a stored preference against the desktop and paint the tokens.
///
/// `System` asks the window first and the app second, because on Linux the
/// app-level answer is unreliable while a window exists.
pub fn apply(preference: ThemePreference, window: Option<&mut Window>, cx: &mut App) {
    let mode = match preference {
        ThemePreference::Light => ThemeMode::Light,
        ThemePreference::Dark => ThemeMode::Dark,
        ThemePreference::System => window
            .as_ref()
            .map(|window| window.appearance())
            .unwrap_or_else(|| cx.window_appearance())
            .into(),
    };

    // `change` resets every colour from the bundled theme config, so the
    // palette has to go on afterwards, and `sync_base` has to go on after that
    // or the scrollbar keeps painting with the colours it was last given.
    Theme::change(mode, window, cx);
    paint(mode, cx);
    Theme::sync_base(cx);
}

/// Whether the theme currently in force is the dark one. The shell asks this to
/// pick between two icon sets, not to pick a colour.
pub fn is_dark(cx: &App) -> bool {
    Theme::global(cx).is_dark()
}

fn paint(mode: ThemeMode, cx: &mut App) {
    let p = if mode.is_dark() { DARK } else { LIGHT };
    let theme = Theme::global_mut(cx);

    theme.radius = px(RADIUS);
    theme.radius_lg = px(RADIUS_LG);
    theme.font_size = px(FONT_SIZE);
    theme.mono_font_size = px(MONO_FONT_SIZE);
    theme.font_family = system_ui().into();
    theme.mono_font_family = system_mono().into();

    // Grounds and ink.
    theme.background = hex(p.background);
    theme.foreground = hex(p.text);
    theme.border = hex(p.border);
    theme.muted = hex(p.raised);
    theme.muted_foreground = hex(p.muted);
    theme.transparent = alpha(p.background, 0.0);

    // Focus and selection: the text colour, weakened. No accent exists.
    theme.ring = alpha(p.text, 0.70);
    theme.selection = alpha(p.text, 0.18);
    theme.caret = hex(p.text);

    // Primary is the inversion — ink where the window is ground. In a
    // monochrome scheme that is the only way a button can be louder than the
    // one beside it.
    theme.primary = hex(p.text);
    theme.primary_foreground = hex(p.background);
    theme.primary_hover = alpha(p.text, 0.88);
    theme.primary_active = alpha(p.text, 0.76);

    theme.secondary = hex(p.surface);
    theme.secondary_foreground = hex(p.text);
    theme.secondary_hover = hex(p.raised);
    theme.secondary_active = hex(p.border);

    theme.button = hex(p.surface);
    theme.button_foreground = hex(p.text);
    theme.button_hover = hex(p.raised);
    theme.button_active = hex(p.border);
    theme.button_primary = theme.primary;
    theme.button_primary_foreground = theme.primary_foreground;
    theme.button_primary_hover = theme.primary_hover;
    theme.button_primary_active = theme.primary_active;
    theme.button_secondary = theme.secondary;
    theme.button_secondary_foreground = theme.secondary_foreground;
    theme.button_secondary_hover = theme.secondary_hover;
    theme.button_secondary_active = theme.secondary_active;

    // `accent` is what a menu item or a list item turns when the pointer is on
    // it. Weak on purpose: it has to be visible without competing with the
    // selection, which is the same colour twice as strong.
    theme.accent = alpha(p.text, 0.06);
    theme.accent_foreground = hex(p.text);

    theme.popover = hex(p.surface);
    theme.popover_foreground = hex(p.text);
    theme.overlay = alpha(0x000000, 0.45);

    theme.input = hex(p.border);

    // The list of files. `table_row_border` is transparent because the rows are
    // already separated by the alternating tint, and drawing both turns the
    // list back into the spreadsheet it stopped being.
    theme.table = hex(p.background);
    theme.table_head = hex(p.surface);
    theme.table_head_foreground = hex(p.muted);
    theme.table_foot = hex(p.surface);
    theme.table_foot_foreground = hex(p.muted);
    theme.table_even = alpha(p.text, 0.02);
    theme.table_hover = alpha(p.text, 0.05);
    theme.table_active = alpha(p.text, 0.12);
    theme.table_active_border = hex(p.text);
    theme.table_row_border = alpha(p.text, 0.0);

    // `Theme::list` is the list *settings* struct, not a colour; the colour of
    // the same name lives one level down and is only reachable spelled out.
    theme.colors.list = hex(p.background);
    theme.list_head = hex(p.surface);
    theme.list_even = alpha(p.text, 0.02);
    theme.list_hover = alpha(p.text, 0.05);
    theme.list_active = alpha(p.text, 0.12);
    theme.list_active_border = hex(p.text);

    theme.scrollbar = alpha(p.background, 0.0);
    theme.scrollbar_thumb = alpha(p.text, 0.16);
    theme.scrollbar_thumb_hover = alpha(p.text, 0.28);

    theme.title_bar = hex(p.surface);
    theme.title_bar_border = hex(p.border);
    theme.status_bar = hex(p.surface);
    theme.status_bar_border = hex(p.border);
    theme.window_border = hex(p.border);

    theme.sidebar = hex(p.surface);
    theme.sidebar_foreground = hex(p.text);
    theme.sidebar_border = hex(p.border);
    theme.sidebar_accent = alpha(p.text, 0.06);
    theme.sidebar_accent_foreground = hex(p.text);
    theme.sidebar_primary = hex(p.text);
    theme.sidebar_primary_foreground = hex(p.background);

    theme.tab = hex(p.surface);
    theme.tab_bar = hex(p.surface);
    theme.tab_bar_segmented = hex(p.raised);
    theme.tab_foreground = hex(p.muted);
    theme.tab_active = hex(p.background);
    theme.tab_active_foreground = hex(p.text);

    theme.accordion = hex(p.surface);
    theme.group_box = hex(p.surface);
    theme.group_box_foreground = hex(p.text);
    theme.description_list_label = hex(p.surface);
    theme.description_list_label_foreground = hex(p.muted);
    theme.skeleton = hex(p.raised);
    theme.tiles = hex(p.surface);

    theme.switch = hex(p.border);
    theme.switch_thumb = hex(p.surface);
    theme.slider_bar = hex(p.border);
    theme.slider_thumb = hex(p.text);
    theme.progress_bar = hex(p.text);

    // A file dragged over the window. The border is the text colour so it reads
    // at a glance; the fill is weak so the list underneath stays readable,
    // because what is underneath is what you are about to drop onto.
    theme.drag_border = hex(p.text);
    theme.drop_target = alpha(p.text, 0.08);

    theme.link = hex(p.text);
    theme.link_hover = hex(p.muted);
    theme.link_active = hex(p.muted);

    // Neutral where the vocabulary demands a colour but Arca has nothing to
    // say with one.
    theme.info = hex(p.text);
    theme.info_foreground = hex(p.background);
    theme.info_hover = alpha(p.text, 0.88);
    theme.info_active = alpha(p.text, 0.76);
    theme.success = hex(p.text);
    theme.success_foreground = hex(p.background);
    theme.success_hover = alpha(p.text, 0.88);
    theme.success_active = alpha(p.text, 0.76);

    // The two states that keep a hue.
    theme.danger = hex(p.danger);
    theme.danger_foreground = hex(0xFFFFFF);
    theme.danger_hover = alpha(p.danger, 0.88);
    theme.danger_active = alpha(p.danger, 0.76);
    theme.button_danger = theme.danger;
    theme.button_danger_foreground = theme.danger_foreground;
    theme.button_danger_hover = theme.danger_hover;
    theme.button_danger_active = theme.danger_active;

    theme.warning = hex(p.warning);
    theme.warning_foreground = hex(p.background);
    theme.warning_hover = alpha(p.warning, 0.88);
    theme.warning_active = alpha(p.warning, 0.76);
}

/// The letters the rest of the desktop is written in.
///
/// A window sitting next to the Explorer that does not use the Explorer's
/// letters reads as foreign before you have looked at anything in it. Every
/// other platform gets the name GPUI resolves to the system UI face.
fn system_ui() -> &'static str {
    if cfg!(windows) {
        "Segoe UI"
    } else {
        ".SystemUIFont"
    }
}

fn system_mono() -> &'static str {
    if cfg!(windows) {
        "Consolas"
    } else {
        "monospace"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative luminance, WCAG 2.x. Only the sRGB channels are needed here,
    /// so this stays a dozen lines rather than a colour-science dependency.
    fn luminance(value: u32) -> f32 {
        let channel = |shift: u32| {
            let c = ((value >> shift) & 0xFF) as f32 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    fn contrast(a: u32, b: u32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The palette is the accessibility story: there is no accent to fall back
    /// on, so if these ratios slip there is nothing else holding the window up.
    #[test]
    fn every_ground_carries_its_text() {
        for (name, p) in [("dark", DARK), ("light", LIGHT)] {
            for (ground_name, ground) in [
                ("background", p.background),
                ("surface", p.surface),
                ("raised", p.raised),
            ] {
                // WCAG AA for body text.
                let body = contrast(p.text, ground);
                assert!(
                    body >= 4.5,
                    "{name}: text on {ground_name} is {body:.2}:1, below AA"
                );
                // AA for large/secondary text, which is all `muted` is used for.
                let secondary = contrast(p.muted, ground);
                assert!(
                    secondary >= 3.0,
                    "{name}: muted on {ground_name} is {secondary:.2}:1, below AA large"
                );
            }
            // A border nobody can see is not a border.
            let edge = contrast(p.border, p.background);
            assert!(
                edge >= 1.2,
                "{name}: border on background is {edge:.2}:1, invisible"
            );
            // Danger has to be readable as text, not just present as a hue.
            let danger = contrast(p.danger, p.background);
            assert!(
                danger >= 4.0,
                "{name}: danger on background is {danger:.2}:1"
            );
        }
    }

    /// Two greys that are the same grey, and a mode that is not the other
    /// mode's inverse. Both are copy-paste slips, both survive a compile, and
    /// both are invisible in a diff of thirty hex literals.
    ///
    /// The step sizes are deliberately not asserted: elevation does not run the
    /// same way in both modes — a light window raises a panel *towards* white
    /// and tints a hover *away* from it — so any threshold here would be a
    /// number tuned until it passed rather than a rule.
    #[test]
    fn the_palettes_are_distinct_and_opposed() {
        for (name, p) in [("dark", DARK), ("light", LIGHT)] {
            let greys = [
                ("background", p.background),
                ("surface", p.surface),
                ("raised", p.raised),
                ("border", p.border),
                ("text", p.text),
                ("muted", p.muted),
            ];
            for (i, (a_name, a)) in greys.iter().enumerate() {
                for (b_name, b) in greys.iter().skip(i + 1) {
                    assert_ne!(a, b, "{name}: {a_name} and {b_name} are the same colour");
                }
            }
        }

        // Ink and ground swap places between the modes. If they ever stop
        // doing that, one of the two windows has gone grey-on-grey.
        assert!(
            luminance(DARK.text) > luminance(DARK.background),
            "dark: text is not lighter than its ground"
        );
        assert!(
            luminance(LIGHT.text) < luminance(LIGHT.background),
            "light: text is not darker than its ground"
        );
    }
}
