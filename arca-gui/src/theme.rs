//! How Arca looks.
//!
//! Kept apart from the window because it is a different kind of decision. The
//! rest of the crate is about what happens when you press something; this is
//! about what it looks like before you do, and mixing the two means neither can
//! be changed without reading the other.
//!
//! The palette comes out of `brand/BRAND.md`: night blue, #05060A at one end of
//! the gradient and #1B2A4A at the other. Neither is usable as an accent on its
//! own -- one is nearly black, the other disappears into a dark panel -- so the
//! accent is that same hue carried up in lightness until it reads against both
//! grounds, and the greys are tinted towards it rather than being neutral. That
//! is what stops a dark window from looking like a screenshot of a terminal.

use eframe::egui::{self, Color32, Rounding, Stroke, Visuals};

const fn rgb(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

// The brand hue, lifted until it carries white text on a dark ground.
const ACCENT_DARK: Color32 = rgb(0x3D6FD6);
// The same hue taken the other way for a light ground, where the accent has to
// sit behind dark text instead of in front of it.
const ACCENT_LIGHT: Color32 = rgb(0x2A55B8);

// Corners. Small enough to read as a finish rather than as a shape.
const R_WIDGET: f32 = 4.0;
const R_WINDOW: f32 = 8.0;

/// Black and white, and the accent.
///
/// The tinted greys the light theme is built on were tried here too and the
/// window came out looking like every other dark window; black gives the list
/// the ground a page has, and on the screens people have now it is a colour in
/// its own right rather than the absence of one. The greys above it are
/// neutral to match: a blue-grey next to true black reads as a stain.
///
/// The ladder is deliberately short -- black, then three steps for the panels
/// and buttons -- because contrast is doing the work here that a hue does
/// elsewhere. What is picked is still the brand blue: a window with nothing
/// but white in it has no way of saying which of two white things matters.
pub fn dark() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = Color32::BLACK;
    // A shade off black, so that a dialog over the list reads as being over it
    // rather than cut out of it. The border does the rest.
    v.window_fill = rgb(0x0C0C0C);
    v.extreme_bg_color = Color32::BLACK;
    v.code_bg_color = rgb(0x0C0C0C);
    // Only just off the panel. A stripe you can name the colour of is a stripe
    // that competes with the selection.
    v.faint_bg_color = rgb(0x0C0C0C);
    v.window_stroke = Stroke::new(1.0_f32, rgb(0x333333));
    v.selection.bg_fill = rgb(0x2D5CB8);
    // Not an outline colour, whatever the name says: egui_extras reads this and
    // makes it the text colour of a picked row. A pale blue here left the sizes
    // and dates on a selected row dimmer than on an unpicked one, which is the
    // wrong way round.
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.hyperlink_color = rgb(0x6E9BEA);
    v.warn_fg_color = rgb(0xE0A85C);
    v.error_fg_color = rgb(0xE06C6C);

    let w = &mut v.widgets;
    // noninteractive.fg_stroke is the body text of the whole window, not a
    // disabled colour: egui reads `text_color()` straight out of it. White,
    // since that is the whole point of a black window; the rules and edges
    // that share this group get their own dark grey below.
    w.noninteractive.bg_fill = rgb(0x0C0C0C);
    w.noninteractive.weak_bg_fill = rgb(0x0C0C0C);
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, rgb(0x262626));
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

    w.inactive.bg_fill = rgb(0x161616);
    w.inactive.weak_bg_fill = rgb(0x161616);
    w.inactive.bg_stroke = Stroke::new(1.0_f32, rgb(0x2E2E2E));
    w.inactive.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

    w.hovered.bg_fill = rgb(0x232323);
    w.hovered.weak_bg_fill = rgb(0x232323);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT_DARK);
    w.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

    // `active` is the pressed state and also where egui takes `strong` text
    // from, so its foreground has to be the brightest thing here rather than
    // whatever happens to look right on a pressed button.
    w.active.bg_fill = ACCENT_DARK;
    w.active.weak_bg_fill = ACCENT_DARK;
    w.active.bg_stroke = Stroke::new(1.0_f32, rgb(0x6E9BEA));
    w.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

    w.open.bg_fill = rgb(0x232323);
    w.open.weak_bg_fill = rgb(0x232323);
    w.open.bg_stroke = Stroke::new(1.0_f32, rgb(0x2E2E2E));
    w.open.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

    round(&mut v);
    v
}

pub fn light() -> Visuals {
    let mut v = Visuals::light();
    v.panel_fill = rgb(0xF2F4F8);
    v.window_fill = rgb(0xFFFFFF);
    v.extreme_bg_color = rgb(0xFFFFFF);
    v.code_bg_color = rgb(0xF2F4F8);
    v.faint_bg_color = rgb(0xE9EDF4);
    v.window_stroke = Stroke::new(1.0_f32, rgb(0xD2D8E4));
    // Pale, because egui paints this behind text that keeps its own colour: a
    // saturated blue here would leave a selected row unreadable.
    v.selection.bg_fill = rgb(0xCBDCF7);
    // The text of a picked row, as above. Dark, because the fill is pale.
    v.selection.stroke = Stroke::new(1.0_f32, rgb(0x10141B));
    v.hyperlink_color = ACCENT_LIGHT;
    v.warn_fg_color = rgb(0xA1660D);
    v.error_fg_color = rgb(0xC03A3A);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = rgb(0xFFFFFF);
    w.noninteractive.weak_bg_fill = rgb(0xFFFFFF);
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, rgb(0xE3E7EF));
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, rgb(0x1B2129));

    w.inactive.bg_fill = rgb(0xFFFFFF);
    w.inactive.weak_bg_fill = rgb(0xFFFFFF);
    w.inactive.bg_stroke = Stroke::new(1.0_f32, rgb(0xCFD6E2));
    w.inactive.fg_stroke = Stroke::new(1.0_f32, rgb(0x1B2129));

    w.hovered.bg_fill = rgb(0xEDF2FB);
    w.hovered.weak_bg_fill = rgb(0xEDF2FB);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT_LIGHT);
    w.hovered.fg_stroke = Stroke::new(1.0_f32, rgb(0x10141B));

    // A pale tint rather than the accent itself: this is also `strong` text,
    // and strong text has to stay dark on a white window.
    w.active.bg_fill = rgb(0xD8E3F8);
    w.active.weak_bg_fill = rgb(0xD8E3F8);
    w.active.bg_stroke = Stroke::new(1.0_f32, ACCENT_LIGHT);
    w.active.fg_stroke = Stroke::new(1.0_f32, rgb(0x10141B));

    w.open.bg_fill = rgb(0xEDF2FB);
    w.open.weak_bg_fill = rgb(0xEDF2FB);
    w.open.bg_stroke = Stroke::new(1.0_f32, rgb(0xCFD6E2));
    w.open.fg_stroke = Stroke::new(1.0_f32, rgb(0x1B2129));

    round(&mut v);
    v
}

fn round(v: &mut Visuals) {
    for s in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        s.rounding = Rounding::same(R_WIDGET);
        // egui grows a widget by a pixel when the pointer is over it. On a row
        // in a table that reads as a twitch, and the colour already says it.
        s.expansion = 0.0;
    }
    v.window_rounding = Rounding::same(R_WINDOW);
    v.menu_rounding = Rounding::same(R_WINDOW);
}

/// The colour that marks the row the keyboard is on.
///
/// Deliberately not `selection.stroke`, which would be the obvious place: that
/// one is spoken for as the text colour of a picked row. Where the keyboard is
/// and what is picked are two different things and they need two colours, or
/// moving the cursor onto a picked row makes both of them disappear.
pub fn cursor(v: &Visuals) -> Stroke {
    let color = if v.dark_mode {
        rgb(0x7FA6F0)
    } else {
        ACCENT_LIGHT
    };
    Stroke::new(1.0_f32, color)
}

/// The ground the column headings stand on.
///
/// A shade off the list, so that the row of names reads as the lid of the list
/// rather than as its first row. It is the one place in the window where a
/// panel is allowed to be a different colour from the panel next to it: what is
/// above the line is the handle and what is below it is the contents.
pub fn header(v: &Visuals) -> Color32 {
    if v.dark_mode {
        rgb(0x141414)
    } else {
        rgb(0xE7EBF2)
    }
}

/// The same, for the column the list is sorted by.
///
/// One step further from the list than its neighbours, which is enough to pick
/// it out down the whole height of the window without a second colour.
pub fn header_sorted(v: &Visuals) -> Color32 {
    if v.dark_mode {
        rgb(0x1F1F1F)
    } else {
        rgb(0xD9E0EC)
    }
}

/// The mark that says which way a column is sorted.
///
/// Blue, and only ever blue: it is the one thing in the header that is not a
/// word, and in the colour of the text it read as a speck of dust on the
/// screen. This is the colour that says the list is being held a particular way
/// round, and nothing else in the window is allowed to use it.
pub fn mark(v: &Visuals) -> Color32 {
    if v.dark_mode {
        rgb(0x4C8DFF)
    } else {
        ACCENT_LIGHT
    }
}

/// Sizes and spacing, which are the same whichever way the theme goes.
pub fn style(style: &mut egui::Style) {
    use egui::{FontFamily, FontId, TextStyle};

    style.text_styles = [
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional)),
        // The columns of numbers and dates. Slightly smaller than the body:
        // a monospace face at the same size always looks a size bigger.
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.menu_margin = egui::Margin::same(6.0);
    style.spacing.window_margin = egui::Margin::same(12.0);
    style.spacing.interact_size.y = 24.0;
    // Wide enough to grab a column edge without hitting the text beside it.
    style.interaction.resize_grab_radius_side = 6.0;
}

/// The letters the rest of the desktop is written in.
///
/// egui ships Ubuntu-Light and Hack. They are good fonts and they look like
/// nothing else on Windows: a window sitting next to the Explorer that does not
/// use the Explorer's letters reads as foreign before you have looked at
/// anything in it. Whatever is missing falls back to what egui brought, so a
/// machine without these still gets a window.
/// The family the toolbar's system icons are drawn from, when there is one.
pub const ICONS: &str = "icons";

static HAS_ICONS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Whether the system icon font was there to load. Buttons ask before reaching
/// for a codepoint out of it, and fall back to the painted shapes when it is
/// not, which is every platform that is not Windows.
pub fn icons_available() -> bool {
    *HAS_ICONS.get().unwrap_or(&false)
}

#[cfg(windows)]
pub fn fonts() -> egui::FontDefinitions {
    let mut defs = egui::FontDefinitions::default();
    let dir = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("C:\\Windows"))
        .join("Fonts");
    for (name, file, family) in [
        ("segoe-ui", "segoeui.ttf", egui::FontFamily::Proportional),
        ("consolas", "consola.ttf", egui::FontFamily::Monospace),
    ] {
        let Ok(bytes) = std::fs::read(dir.join(file)) else {
            continue;
        };
        defs.font_data
            .insert(name.to_owned(), egui::FontData::from_owned(bytes));
        // In front of what is already there, not instead of it: the fallbacks
        // are what draws a glyph these two have not got.
        defs.families
            .entry(family)
            .or_default()
            .insert(0, name.to_owned());
    }

    // Windows ships the icons its own programs are drawn with. A padlock and a
    // cogwheel out of that file are the ones the rest of the desktop uses and
    // are drawn by people who do this for a living; the pair painted by hand
    // here came out as a handbag and an asterisk. Segoe Fluent Icons on
    // Windows 11, Segoe MDL2 Assets before it, and neither is fatal.
    let icons = ["SegoeIcons.ttf", "segmdl2.ttf"]
        .iter()
        .find_map(|f| std::fs::read(dir.join(f)).ok());
    if let Some(bytes) = icons {
        defs.font_data
            .insert(ICONS.to_owned(), egui::FontData::from_owned(bytes));
        defs.families
            .insert(egui::FontFamily::Name(ICONS.into()), vec![ICONS.to_owned()]);
        let _ = HAS_ICONS.set(true);
    }
    defs
}

#[cfg(not(windows))]
pub fn fonts() -> egui::FontDefinitions {
    egui::FontDefinitions::default()
}
