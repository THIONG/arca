#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// The GPUI feature shares the controller with the legacy egui implementation;
// the latter remains compiled for the transition but is not called by GPUI.
#![cfg_attr(feature = "gpui", allow(dead_code))]

mod clipboard;
mod glyphs;
#[cfg(feature = "gpui")]
mod gpui_shell;
#[cfg(feature = "gpui")]
mod gpui_theme;
mod i18n;
mod theme;
mod tree;

use arca_core::{Codec, Entry, Level};
use arca_tar::{TarReader, TarWriter};
use arca_zip::{ZipArchive, ZipWriter};
use eframe::egui;
use egui::ThemePreference;
use egui_extras::{Column, TableBuilder};
use i18n::{strings, Lang, Strings};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;
use tree::{children_of, draw_icon, draw_icon_at, entries_under, kind_of, parent_of, Kind, Row};

const BUF: usize = 256 * 1024;
const ROW_HEIGHT: f32 = 29.0;

// How wide a column starts out and the least it can be pulled down to. The
// name gets the room because it is the thing being read; the rest hold a
// number or a word and are sized for it.
const NAME_WIDE: f32 = 320.0;
const NAME_LEAST: f32 = 140.0;
const CELL_WIDE: f32 = 95.0;
const CELL_LEAST: f32 = 60.0;
// A second click on the same row within this opens it. Half a second, which is
// what Windows uses for the same gesture by default.
const DOUBLE_CLICK: f64 = 0.5;
const ICON_PNG: &[u8] = include_bytes!("../../brand/arca-256.png");

#[derive(PartialEq, Eq, Clone, Copy)]
enum Format {
    Zip,
    Tar,
    TarGz,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Format::Zip => "ZIP",
            Format::Tar => "TAR",
            Format::TarGz => "TAR.GZ",
        }
    }
}

fn detect(p: &Path) -> Option<Format> {
    let n = p.to_string_lossy().to_ascii_lowercase();
    if n.ends_with(".zip") {
        Some(Format::Zip)
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some(Format::TarGz)
    } else if n.ends_with(".tar") {
        Some(Format::Tar)
    } else {
        None
    }
}

// The little triangle beside a column name that says which way it is sorted.
// Ascending points up, which is what a file list means by it everywhere: the
// smallest, the earliest, the first alphabetically, at the top.
//
// The one thing to get wrong here is the sign. Screen coordinates grow
// downwards, so the apex of an upward triangle sits at a SMALLER y than its
// base, and writing it the other way round gives a mark that says the opposite
// of what the list is doing without anything else looking amiss.
fn sort_mark(c: egui::Pos2, ascending: bool) -> [egui::Pos2; 3] {
    let (w, h) = (3.8_f32, 2.4_f32);
    if ascending {
        [
            egui::pos2(c.x - w, c.y + h),
            egui::pos2(c.x + w, c.y + h),
            egui::pos2(c.x, c.y - h),
        ]
    } else {
        [
            egui::pos2(c.x - w, c.y - h),
            egui::pos2(c.x + w, c.y - h),
            egui::pos2(c.x, c.y + h),
        ]
    }
}

// How many folders at the front of the path have to go behind the "…" for the
// rest to fit in `room`. Drops from the front, because the folders you are
// nearest are the ones worth seeing, and never drops the last one: the folder
// you are standing in stays whatever its name costs, cut short if it must be.
fn crumbs_hidden(sizes: &[f32], sep: f32, dots: f32, room: f32) -> usize {
    let mut first = 0usize;
    while first + 1 < sizes.len() {
        let shown = sizes.len() - first;
        let mut total: f32 = sizes[first..].iter().sum::<f32>() + sep * (shown - 1) as f32;
        if first > 0 {
            total += dots + sep;
        }
        if total <= room {
            break;
        }
        first += 1;
    }
    first
}

// The row a height falls on, out of the ones the table drew this frame.
//
// The rows do not touch: there is a gap of the item spacing between one and the
// next, and the table paints over it so that the stripes look continuous, but
// the rectangles it hands back stop short. Asking which rectangle *contains* a
// height therefore has no answer whenever the pointer is resting in one of
// those gaps, which is most of the way from one row to the next. So the
// question asked here is which row has started by this height, and the answer
// in a gap is the row above it.
//
// Above the first row it clamps to the first: a drag that has run off that end
// is still asking for everything up to it. Under the last row there are two
// different situations and they cannot share an answer. If there is more list
// below, still to be scrolled into view, the answer is that last row and the
// drag carries on from there. If the list has ended, the height is in the empty
// space under it and the answer is `len`, one past the end, which is not a row:
// pressing down there and moving a little must pick nothing at all rather than
// reach up and grab whatever happens to be last.
fn row_at(rects: &[(usize, egui::Rect)], y: f32, len: usize) -> Option<usize> {
    let (first, top) = *rects.first()?;
    if y <= top.top() {
        return Some(first);
    }
    let (last, bottom) = *rects.last()?;
    if y > bottom.bottom() && last + 1 >= len {
        return Some(len);
    }
    rects
        .iter()
        .rev()
        .find(|(_, r)| y >= r.top())
        .map(|(i, _)| *i)
}

fn saved_of(r: &Row) -> f64 {
    if r.size == 0 {
        0.0
    } else {
        1.0 - r.packed as f64 / r.size as f64
    }
}

// A Unix timestamp as a date somebody can read. Done by hand rather than with
// a date crate: the archive formats store civil time with no zone, so there is
// nothing here worth a dependency that knows about leap seconds and Tokyo.
fn when(mtime: Option<i64>) -> String {
    let Some(t) = mtime.filter(|t| *t > 0) else {
        return String::new();
    };
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    // Days since 1970 to a civil date, by Howard Hinnant's method: shift the
    // epoch to March so the leap day lands at the end of the year and the
    // month lengths follow one formula.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn archive_stem(p: &Path) -> String {
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    for ext in [".tar.gz", ".tgz", ".zip", ".tar"] {
        if lower.ends_with(ext) {
            return name[..name.len() - ext.len()].to_string();
        }
    }
    name
}

fn config_file() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("Arca").join("gui.conf"))
}

struct Settings {
    lang: Option<Lang>,
    theme: ThemePreference,
    columns: Columns,
    // Whether to ask, once at startup, if there is a newer Arca.
    updates: bool,
    // Every file in the archive at once, instead of one folder at a time.
    flat: bool,
    // The folders of the archive down the left hand side.
    tree: bool,
    // Which code page an unflagged zip has its names written in. Only the
    // person looking at the archive can know, so it is remembered: somebody
    // whose archives all come from one machine says it once.
    page: arca_zip::pages::Page,
    // Where the window was left and how big: x, y, width, height. None until it
    // has been opened once.
    window: Option<[f32; 4]>,
    // The archives opened lately, newest first. Paths, so that one that has
    // since been moved can be noticed and dropped rather than opened blind.
    recent: Vec<String>,
    // How wide each column is: the name first, then the ones that can be
    // turned off, in the order of `Columns::ALL`. A column keeps its width
    // while it is off, so turning one back on does not lose how it was set.
    widths: Vec<f32>,
}

impl Settings {
    // What a column starts out at, before anybody has pulled on it.
    fn default_widths() -> Vec<f32> {
        std::iter::once(NAME_WIDE)
            .chain(std::iter::repeat(CELL_WIDE).take(Columns::ALL.len()))
            .collect()
    }

    // The least a column can be pulled down to, by its place in `widths`.
    fn least(slot: usize) -> f32 {
        if slot == 0 {
            NAME_LEAST
        } else {
            CELL_LEAST
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lang: None,
            theme: ThemePreference::System,
            columns: Columns::default(),
            updates: true,
            flat: false,
            tree: false,
            page: arca_zip::pages::Page::default(),
            window: None,
            recent: Vec::new(),
            widths: Settings::default_widths(),
        }
    }
}

impl Settings {
    fn load() -> Self {
        let Some(p) = config_file() else {
            return Settings::default();
        };
        let Ok(text) = fs::read_to_string(p) else {
            return Settings::default();
        };
        Settings::parse(&text)
    }

    /// The settings file read back out of text.
    ///
    /// Apart from `load` so that what `text` writes can be read back and
    /// compared without going near a disk.
    fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match (k.trim(), v.trim()) {
                ("lang", "system") => s.lang = None,
                ("lang", other) => s.lang = Lang::from_code(other),
                ("theme", "light") => s.theme = ThemePreference::Light,
                ("theme", "dark") => s.theme = ThemePreference::Dark,
                ("theme", _) => s.theme = ThemePreference::System,
                ("flat", v) => s.flat = v == "yes",
                ("tree", v) => s.tree = v == "yes",
                ("updates", v) => s.updates = v != "no",
                ("page", v) => {
                    if let Some(p) = arca_zip::pages::Page::from_code(v) {
                        s.page = p;
                    }
                }
                // One line each, because a path can hold anything a filename
                // can and there is no separator left that it could not.
                ("recent", p) if !p.is_empty() => s.recent.push(p.to_string()),
                ("window", v) => {
                    let n: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                    if let [x, y, w, h] = n[..] {
                        // A window smaller than the minimum, or one left on a
                        // screen that is no longer plugged in, is not a window
                        // anybody can use.
                        if w >= 720.0 && h >= 320.0 {
                            s.window = Some([x, y, w, h]);
                        }
                    }
                }
                // Written since the columns could first be turned off and read
                // by nobody, so every window opened with the six of them
                // showing however they had been left.
                ("columns", list) => {
                    let mut c = Columns::default();
                    for (which, _) in Columns::ALL {
                        c.set(which, false);
                    }
                    for name in list.split(',').map(str::trim) {
                        if let Some((which, _)) = Columns::ALL.iter().find(|(_, n)| *n == name) {
                            c.set(*which, true);
                        }
                    }
                    s.columns = c;
                }
                // All of them or none: a line with a column missing from it
                // belongs to a different set of columns than this one has, and
                // guessing which is which would put the widths on the wrong
                // ones. Each is held above its floor in case the file was
                // written by hand.
                ("widths", list) => {
                    let read: Vec<f32> = list
                        .split(',')
                        .filter_map(|n| n.trim().parse::<f32>().ok())
                        .enumerate()
                        .map(|(i, w)| w.max(Settings::least(i)))
                        .collect();
                    if read.len() == s.widths.len() {
                        s.widths = read;
                    }
                }
                _ => {}
            }
        }
        s
    }

    fn save(&self) {
        let Some(p) = config_file() else { return };
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(p, self.text());
    }

    /// The settings file as text.
    ///
    /// Apart from `save` so that what is written can be read back and compared
    /// without going near a disk. That is not tidiness: six settings were being
    /// read at startup and never written, because the line that builds this had
    /// quietly stopped mentioning them -- `flat`, `tree`, `page`, `window` and
    /// `recent` were all built into a string that was then thrown away. A round
    /// trip that never touches a file is the only way that stays fixed.
    fn text(&self) -> String {
        let lang = self.lang.map(|l| l.code()).unwrap_or("system");
        let theme = match self.theme {
            ThemePreference::Light => "light",
            ThemePreference::Dark => "dark",
            ThemePreference::System => "system",
        };
        let yes = |b: bool| if b { "yes" } else { "no" };
        let columns: Vec<&str> = Columns::ALL
            .iter()
            .filter(|(which, _)| self.columns.on(*which))
            .map(|(_, name)| *name)
            .collect();
        let widths: Vec<String> = self.widths.iter().map(|w| format!("{w:.1}")).collect();

        let mut out = String::new();
        out.push_str(&format!("lang = {lang}\n"));
        out.push_str(&format!("theme = {theme}\n"));
        out.push_str(&format!("flat = {}\n", yes(self.flat)));
        out.push_str(&format!("tree = {}\n", yes(self.tree)));
        out.push_str(&format!("updates = {}\n", yes(self.updates)));
        out.push_str(&format!("page = {}\n", self.page.code()));
        out.push_str(&format!("columns = {}\n", columns.join(",")));
        out.push_str(&format!("widths = {}\n", widths.join(",")));
        if let Some([x, y, w, h]) = self.window {
            out.push_str(&format!("window = {x:.0},{y:.0},{w:.0},{h:.0}\n"));
        }
        // One line each: a path can hold anything a filename can and there is
        // no separator left that it could not.
        for path in &self.recent {
            out.push_str(&format!("recent = {path}\n"));
        }
        out
    }

    fn effective_lang(&self) -> Lang {
        self.lang.unwrap_or_else(Lang::from_system)
    }
}

fn open_source(archive: &Path, format: Format) -> std::io::Result<Box<dyn Read>> {
    let f = BufReader::with_capacity(BUF, File::open(archive)?);
    Ok(match format {
        Format::TarGz => Box::new(flate2::read::GzDecoder::new(f)),
        _ => Box::new(f),
    })
}

fn list_entries(archive: &Path) -> arca_core::Result<Vec<Entry>> {
    let Some(format) = detect(archive) else {
        return Err(arca_core::Error::Unsupported(format!(
            "unrecognized extension in '{}'",
            archive.display()
        )));
    };
    match format {
        Format::Zip => Ok(ZipArchive::open(File::open(archive)?)?.entries().to_vec()),
        _ => {
            let mut r = TarReader::new(open_source(archive, format)?);
            let mut v = Vec::new();
            while let Some(e) = r.next_entry()? {
                v.push(e.entry.clone());
                r.skip_data(&e)?;
            }
            Ok(v)
        }
    }
}

// Only the central directory is read, which is a few kilobytes at the tail of
// the file. A .tar has no encryption to look for.
fn is_encrypted(archive: &Path) -> bool {
    if detect(archive) != Some(Format::Zip) {
        return false;
    }
    File::open(archive)
        .ok()
        .and_then(|f| ZipArchive::open(f).ok())
        .map(|a| a.has_encrypted())
        .unwrap_or(false)
}

// The icon the desktop shows for this kind of file, kept as a texture per
// extension. Without the cache a listing of 1513 entries would ask the shell
// 1513 times a frame; with it, once per kind for the life of the window.
//
// A `None` in the map is a remembered failure, so a kind the system has no
// answer for is not asked about again every frame.
fn system_icon(
    ctx: &egui::Context,
    cache: &mut HashMap<String, Option<egui::TextureHandle>>,
    name: &str,
    is_dir: bool,
) -> Option<egui::TextureHandle> {
    let key = arca_icons::cache_key(name, is_dir);
    if let Some(found) = cache.get(&key) {
        return found.clone();
    }
    let made = arca_icons::lookup(name, is_dir).map(|icon| {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [icon.width as usize, icon.height as usize],
            &icon.rgba,
        );
        ctx.load_texture(format!("icon:{key}"), image, egui::TextureOptions::LINEAR)
    });
    cache.insert(key, made.clone());
    made
}

// What the desktop calls this kind of file, cached by extension the way the
// icons are: the answer is the same for every .txt in the archive, and asking
// the shell fifteen hundred times for it would be fifteen hundred round trips
// to another thread while the list is being drawn.
fn system_type(cache: &mut HashMap<String, Option<String>>, name: &str, is_dir: bool) -> String {
    let key = arca_icons::cache_key(name, is_dir);
    if let Some(found) = cache.get(&key) {
        return found.clone().unwrap_or_default();
    }
    let made = arca_icons::type_name(name, is_dir);
    cache.insert(key, made.clone());
    made.unwrap_or_default()
}

// A folder of its own per archive, so two archives holding a file with the same
// name do not overwrite each other's copy. safe_name is what keeps an entry
// called "../../evil" from landing outside it.
/// Where the version before the last change is kept, so it can be put back.
fn undo_path(archive: &Path) -> PathBuf {
    let mut name = archive.as_os_str().to_os_string();
    name.push(".arca-undo");
    PathBuf::from(name)
}

/// Moves the archive out of the way instead of letting the new one overwrite
/// it, so that the change can be taken back.
///
/// A move, not a copy: the file stays on the volume it was already on and
/// nothing is read or written, so keeping the old version costs the time of a
/// directory entry however big the archive is. What it does cost is the space,
/// until the next change replaces it or the window closes.
fn step_aside(archive: &Path) -> std::io::Result<()> {
    let keep = undo_path(archive);
    if keep.exists() {
        fs::remove_file(&keep)?;
    }
    fs::rename(archive, &keep)
}

// One entry straight into memory, for looking at rather than for keeping.
//
// The same walk as `extract_one` without the file at the end of it: a viewer
// that wrote to the temporary folder on the way would have extracted the thing
// it was only supposed to show.
fn read_entry(
    archive: &Path,
    index: usize,
    out: &mut Vec<u8>,
    password: Option<&str>,
) -> arca_core::Result<()> {
    let Some(format) = detect(archive) else {
        return Err(arca_core::Error::Unsupported("unknown format".into()));
    };
    match format {
        Format::Zip => {
            let mut a = ZipArchive::open(File::open(archive)?)?;
            a.extract_to_with(index, out, password)?;
        }
        _ => {
            // A tar has no index, so the only way to one entry is through all
            // the ones before it.
            let mut r = TarReader::new(open_source(archive, format)?);
            let mut at = 0usize;
            while let Some(e) = r.next_entry()? {
                if at == index {
                    r.copy_data(&e, out)?;
                    return Ok(());
                }
                r.skip_data(&e)?;
                at += 1;
            }
            return Err(arca_core::Error::Format(
                "that entry is not in the archive any more".into(),
            ));
        }
    }
    Ok(())
}

fn extract_one(
    archive: &Path,
    entry: &Entry,
    password: Option<&str>,
) -> arca_core::Result<PathBuf> {
    let room = std::env::temp_dir()
        .join("Arca")
        .join(archive_stem(archive));
    let path = room.join(arca_core::safe_name(&entry.name)?);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let Some(format) = detect(archive) else {
        return Err(arca_core::Error::Unsupported("unknown format".into()));
    };
    let mut out = BufWriter::with_capacity(BUF, File::create(&path)?);
    match format {
        Format::Zip => {
            let mut source = BufReader::with_capacity(BUF, File::open(archive)?);
            arca_zip::extract_entry_with(&mut source, entry, &mut out, password)?;
        }
        _ => {
            // A tar has no index, so the only way to one entry is through all
            // the ones before it.
            let mut r = TarReader::new(open_source(archive, format)?);
            let mut found = false;
            while let Some(e) = r.next_entry()? {
                if e.entry.name == entry.name && !e.entry.is_dir {
                    r.copy_data(&e, &mut out)?;
                    found = true;
                    break;
                }
                r.skip_data(&e)?;
            }
            if !found {
                return Err(arca_core::Error::Format(format!(
                    "'{}' is not in the archive any more",
                    entry.name
                )));
            }
        }
    }
    out.flush()?;
    Ok(path)
}

// Whatever the desktop opens this kind of file with. The child is left to run
// on its own; the window does not wait for it and does not care what it was.
#[cfg(windows)]
fn launch_with_system(path: &Path) -> arca_core::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // The empty pair of quotes is the window title `start` insists on eating.
    // Without it a quoted path becomes the title and nothing opens. No /WAIT
    // either: that would leave a cmd sitting around until the viewer is closed.
    std::process::Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .args(["/C", "start", ""])
        .arg(path)
        .spawn()
        .map_err(arca_core::Error::Io)?;
    Ok(())
}

#[cfg(not(windows))]
fn launch_with_system(path: &Path) -> arca_core::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(path)
        .spawn()
        .map_err(arca_core::Error::Io)?;
    Ok(())
}

// A button with a picture on it, and a word next to the picture when the button
// is one of the ones worth naming. Written out rather than built from
// `egui::Button` because that one only takes an image for its icon, and these
// come either from the system icon font or from a painter.
fn tool_button(
    ui: &mut egui::Ui,
    glyph: glyphs::Glyph,
    label: &str,
    enabled: bool,
    tip: &str,
) -> egui::Response {
    let gap = 6.0;
    let pad = ui.spacing().button_padding;
    let galley = (!label.is_empty()).then(|| {
        ui.painter().layout_no_wrap(
            label.to_owned(),
            egui::TextStyle::Button.resolve(ui.style()),
            egui::Color32::PLACEHOLDER,
        )
    });
    let text_w = galley.as_ref().map_or(0.0, |g| g.size().x + gap);
    let size = egui::vec2(
        glyphs::SIZE + text_w + pad.x * 2.0,
        (glyphs::SIZE + pad.y * 2.0).max(ui.spacing().interact_size.y),
    );
    let (rect, response) = ui.allocate_exact_size(
        size,
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );

    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let (fill, stroke, fg) = if enabled {
            (
                visuals.weak_bg_fill,
                visuals.bg_stroke,
                visuals.fg_stroke.color,
            )
        } else {
            let off = ui.visuals().widgets.noninteractive;
            (
                off.weak_bg_fill,
                off.bg_stroke,
                ui.visuals().weak_text_color(),
            )
        };
        ui.painter().rect(rect, visuals.rounding, fill, stroke);
        let icon = egui::Rect::from_min_size(
            egui::pos2(rect.left() + pad.x, rect.center().y - glyphs::SIZE / 2.0),
            egui::Vec2::splat(glyphs::SIZE),
        );
        match glyphs::codepoint(glyph).filter(|_| theme::icons_available()) {
            Some(ch) => {
                ui.painter().text(
                    icon.center(),
                    egui::Align2::CENTER_CENTER,
                    ch,
                    egui::FontId::new(glyphs::SIZE, egui::FontFamily::Name(theme::ICONS.into())),
                    fg,
                );
            }
            None => glyphs::draw(ui.painter(), icon, glyph, fg),
        }
        if let Some(g) = galley {
            let at = egui::pos2(icon.right() + gap, rect.center().y - g.size().y / 2.0);
            ui.painter().galley(at, g, fg);
        }
    }

    if enabled && !tip.is_empty() {
        response.on_hover_text(tip)
    } else {
        response
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    Replace,
    ReplaceAll,
    Skip,
    SkipAll,
    Rename,
    RenameAll,
    Cancel,
}

// The worker asks the window and blocks until it answers. The "all" answers
// stick, so the question is asked once and not per file.
fn conflict_asker<'a>(
    tx: &'a Sender<Message>,
    replies: &'a std::sync::mpsc::Receiver<Answer>,
) -> impl Fn(&Path) -> Answer + 'a {
    let sticky = std::cell::Cell::new(None::<Answer>);
    move |path: &Path| {
        if let Some(a) = sticky.get() {
            return a;
        }
        if tx
            .send(Message::Conflict(path.display().to_string()))
            .is_err()
        {
            return Answer::Cancel;
        }
        let answer = replies.recv().unwrap_or(Answer::Cancel);
        if matches!(
            answer,
            Answer::ReplaceAll | Answer::SkipAll | Answer::RenameAll | Answer::Cancel
        ) {
            sticky.set(Some(answer));
        }
        answer
    }
}

// `claimed` holds the names this run has already handed out. A .zip decides
// every destination before writing anything, so `exists()` alone would give two
// entries with the same name the same free name.
fn free_name(path: &Path, claimed: &HashSet<PathBuf>) -> PathBuf {
    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..10_000u32 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() && !claimed.contains(&candidate) {
            return candidate;
        }
    }
    path.to_path_buf()
}

// Returns None when the entry must be skipped, and Err on cancel.
fn dest_path(
    dest: &Path,
    name: &str,
    is_dir: bool,
    ask: &dyn Fn(&Path) -> Answer,
    claimed: &mut HashSet<PathBuf>,
) -> arca_core::Result<Option<PathBuf>> {
    let path = dest.join(arca_core::safe_name(name)?);
    if is_dir {
        fs::create_dir_all(&path)?;
        return Ok(None);
    }
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    if !path.exists() && !claimed.contains(&path) {
        claimed.insert(path.clone());
        return Ok(Some(path));
    }
    let chosen = match ask(&path) {
        Answer::Replace | Answer::ReplaceAll => path,
        Answer::Skip | Answer::SkipAll => return Ok(None),
        Answer::Rename | Answer::RenameAll => free_name(&path, claimed),
        Answer::Cancel => return Err(arca_core::Error::Format("cancelled".into())),
    };
    claimed.insert(chosen.clone());
    Ok(Some(chosen))
}

// A .zip is random access: the central directory says where every entry starts,
// so one thread per core can each open the file and decompress a different one.
// A .tar is a single stream, and a .tar.gz a single gzip stream on top of it, so
// there is nothing to split there and that branch stays sequential.
//
// The directories and the overwrite questions are settled first, in one thread.
// Asking the window from several threads at once would put the same dialog on
// screen twice, and racing on which name is free gives a different result every
// run.
fn extract(
    archive: &Path,
    dest: &Path,
    wanted: &[bool],
    // Told how far along this is, and answers whether to carry on. False is
    // somebody pressing stop.
    notify: &(dyn Fn(usize, usize, &str) -> bool + Sync),
    ask: &dyn Fn(&Path) -> Answer,
    password: Option<&str>,
) -> arca_core::Result<u64> {
    let Some(format) = detect(archive) else {
        return Err(arca_core::Error::Unsupported("unknown format".into()));
    };
    fs::create_dir_all(dest)?;
    let mut bytes = 0u64;
    let mut claimed: HashSet<PathBuf> = HashSet::new();

    match format {
        Format::Zip => {
            let a = ZipArchive::open(File::open(archive)?)?;
            let mut jobs: Vec<(Entry, PathBuf)> = Vec::new();
            for (i, e) in a.entries().iter().enumerate() {
                if !wanted.is_empty() && !wanted.get(i).copied().unwrap_or(true) {
                    continue;
                }
                if let Some(path) = dest_path(dest, &e.name, e.is_dir, ask, &mut claimed)? {
                    jobs.push((e.clone(), path));
                }
            }
            drop(a);

            let total = jobs.len();
            let done = AtomicUsize::new(0);
            let written: Vec<u64> = jobs
                .par_iter()
                .map(|(e, path)| {
                    let mut source = BufReader::with_capacity(BUF, File::open(archive)?);
                    let mut f = BufWriter::with_capacity(BUF, File::create(path)?);
                    let w = arca_zip::extract_entry_with(&mut source, e, &mut f, password)?;
                    f.flush()?;
                    if !notify(done.fetch_add(1, Ordering::Relaxed) + 1, total, &e.name) {
                        return Err(arca_core::Error::Cancelled);
                    }
                    Ok(w)
                })
                .collect::<arca_core::Result<Vec<u64>>>()?;
            bytes = written.iter().sum();
            let _ = notify(total, total, "");
        }
        _ => {
            let mut r = TarReader::new(open_source(archive, format)?);
            let total = wanted.len();
            let mut i = 0usize;
            while let Some(e) = r.next_entry()? {
                if !notify(i, total, &e.entry.name) {
                    return Err(arca_core::Error::Cancelled);
                }
                if !wanted.is_empty() && !wanted.get(i).copied().unwrap_or(true) {
                    r.skip_data(&e)?;
                    i += 1;
                    continue;
                }
                match dest_path(dest, &e.entry.name, e.entry.is_dir, ask, &mut claimed)? {
                    Some(path) => {
                        let mut w = BufWriter::with_capacity(BUF, File::create(&path)?);
                        bytes += r.copy_data(&e, &mut w)?;
                        w.flush()?;
                    }
                    None => r.skip_data(&e)?,
                }
                i += 1;
            }
            let _ = notify(i, i, "");
        }
    }
    Ok(bytes)
}

fn test_archive(
    archive: &Path,
    only: Option<&HashSet<String>>,
    // Told how far along this is, and answers whether to carry on. False is
    // somebody pressing stop.
    notify: &(dyn Fn(usize, usize, &str) -> bool + Sync),
) -> arca_core::Result<(usize, Vec<String>)> {
    let Some(format) = detect(archive) else {
        return Err(arca_core::Error::Unsupported("unknown format".into()));
    };
    let mut good = 0usize;
    let mut bad = Vec::new();

    match format {
        Format::Zip => {
            let mut a = ZipArchive::open(File::open(archive)?)?;
            let total = a.len();
            for i in 0..total {
                let name = a.entries()[i].name.clone();
                if !notify(i, total, &name) {
                    return Err(arca_core::Error::Cancelled);
                }
                if a.entries()[i].is_dir || only.is_some_and(|set| !set.contains(&name)) {
                    continue;
                }
                match a.extract_to(i, std::io::sink()) {
                    Ok(_) => good += 1,
                    Err(e) => bad.push(format!("{name}: {e}")),
                }
            }
            let _ = notify(total, total, "");
        }
        _ => {
            let mut r = TarReader::new(open_source(archive, format)?);
            let mut i = 0usize;
            while let Some(e) = r.next_entry()? {
                if !notify(i, i + 1, &e.entry.name) {
                    return Err(arca_core::Error::Cancelled);
                }
                if e.entry.is_dir || only.is_some_and(|set| !set.contains(&e.entry.name)) {
                    r.skip_data(&e)?;
                } else {
                    match r.copy_data(&e, &mut std::io::sink()) {
                        Ok(_) => good += 1,
                        Err(err) => bad.push(format!("{}: {err}", e.entry.name)),
                    }
                }
                i += 1;
            }
            let _ = notify(i, i, "");
        }
    }
    Ok((good, bad))
}

fn collect_files(inputs: &[PathBuf]) -> std::io::Result<Vec<(PathBuf, String)>> {
    fn walk(p: &Path, base: &Path, out: &mut Vec<(PathBuf, String)>) -> std::io::Result<()> {
        let meta = fs::symlink_metadata(p)?;
        let rel = p.strip_prefix(base).unwrap_or(p);
        let name = rel.to_string_lossy().replace('\\', "/");
        if meta.is_dir() {
            let mut children: Vec<_> = fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
            children.sort_by_key(|d| d.file_name());
            for c in children {
                walk(&c.path(), base, out)?;
            }
        } else if meta.is_file() {
            out.push((p.to_path_buf(), name));
        }
        Ok(())
    }

    let mut v = Vec::new();
    for e in inputs {
        let base = e.parent().unwrap_or(Path::new(""));
        walk(e, base, &mut v)?;
    }
    Ok(v)
}

fn compress(
    out: &Path,
    inputs: &[PathBuf],
    format: Format,
    codec: Codec,
    level: Level,
    // Told how far along this is, and answers whether to carry on. False is
    // somebody pressing stop.
    notify: &(dyn Fn(usize, usize, &str) -> bool + Sync),
    password: Option<&str>,
) -> arca_core::Result<(u64, u64)> {
    if password.is_some() && format != Format::Zip {
        return Err(arca_core::Error::Unsupported(
            "encryption only exists in .zip".into(),
        ));
    }
    let files = collect_files(inputs)?;
    let total = files.len();
    let mut source_bytes = 0u64;

    match format {
        Format::Zip => {
            let mut w = ZipWriter::new(BufWriter::with_capacity(BUF, File::create(out)?));
            for (i, (path, name)) in files.iter().enumerate() {
                if !notify(i, total, name) {
                    return Err(arca_core::Error::Cancelled);
                }
                let meta = fs::metadata(path)?;
                let f = BufReader::with_capacity(BUF, File::open(path)?);
                w.add_with_password(name, f, codec, level, None, password)?;
                source_bytes += meta.len();
            }
            w.finish()?;
        }
        _ => {
            let raw = BufWriter::with_capacity(BUF, File::create(out)?);
            let sink: Box<dyn Write> = if format == Format::TarGz {
                Box::new(flate2::write::GzEncoder::new(
                    raw,
                    flate2::Compression::new(level.to_flate2()),
                ))
            } else {
                Box::new(raw)
            };
            let mut w = TarWriter::new(sink);
            for (i, (path, name)) in files.iter().enumerate() {
                if !notify(i, total, name) {
                    return Err(arca_core::Error::Cancelled);
                }
                let meta = fs::metadata(path)?;
                let f = BufReader::with_capacity(BUF, File::open(path)?);
                w.add(name, meta.len(), 0, 0o644, f)?;
                source_bytes += meta.len();
            }
            w.finish()?;
        }
    }
    let _ = notify(total, total, "");
    let final_size = fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok((source_bytes, final_size))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Destination {
    Beside,
    Subfolder,
}

enum Job {
    Extract {
        archives: Vec<PathBuf>,
        dest: Destination,
        password: Option<String>,
    },
    Test {
        archive: PathBuf,
        // The names to check, or all of them. A selection is checked by walking
        // the whole archive and skipping what is not in the set: the entries
        // have to be read in the order they are filed anyway.
        only: Option<HashSet<String>>,
    },
    // Rewriting an archive with a different password, or with none.
    Password {
        archive: PathBuf,
        current: Option<String>,
        new: Option<String>,
    },
    // Taking entries out. A zip has no hole to leave behind, so this rebuilds
    // the archive without them.
    Delete {
        archive: PathBuf,
        names: Vec<String>,
        password: Option<String>,
    },
    CopyTo {
        archive: PathBuf,
        dest: PathBuf,
    },
    // Getting the new version. Not a job that ends in a text to show: it ends
    // in an installer to run, and running it closes Arca.
    Update {
        tag: String,
        installer: String,
        sums: String,
    },
    // Dragging entries onto a folder of the same archive.
    Move {
        archive: PathBuf,
        // Pairs of what a name is now and what it becomes, without the slash a
        // folder carries: both spellings are handled where the move is made.
        moves: Vec<(String, String)>,
        password: Option<String>,
    },
    NewFolder {
        archive: PathBuf,
        // The whole path with the slash already on it, worked out where the
        // folder you are looking at is known.
        name: String,
        password: Option<String>,
    },
    Rename {
        archive: PathBuf,
        // Both are full paths inside the archive, not the names on their own:
        // renaming happens in the folder you are looking at and the entries are
        // stored by their whole path.
        from: String,
        to: String,
        // A folder is not one entry but everything filed under it, so the whole
        // branch moves. There is no entry for it to be renamed on its own.
        folder: bool,
        password: Option<String>,
    },
    Compress {
        out: PathBuf,
        inputs: Vec<PathBuf>,
        format: Format,
        codec: Codec,
        level: Level,
        password: Option<String>,
    },
    // Putting files in. Same rebuild as Delete, and for the same reason: the
    // central directory is at the end of the file.
    Add {
        archive: PathBuf,
        inputs: Vec<PathBuf>,
        // Where inside the archive they land, which is the folder the window is
        // showing. Empty means the root.
        dir: String,
        codec: Codec,
        level: Level,
        password: Option<String>,
    },
}

enum Startup {
    Browse(Option<PathBuf>),
    Run(Job),
    Add(Vec<PathBuf>),
}

fn quick_output(inputs: &[PathBuf], format: Format) -> PathBuf {
    let first = &inputs[0];
    let dir = first.parent().map(PathBuf::from).unwrap_or_default();
    let stem = if inputs.len() == 1 {
        let name = first
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "archive".into());
        if first.is_dir() {
            name
        } else {
            Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(name)
        }
    } else {
        dir.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "archive".into())
    };
    dir.join(format!("{stem}.{}", format.extension()))
}

fn parse_args() -> Startup {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return Startup::Browse(None);
    }
    let rest: Vec<PathBuf> = args[1..].iter().map(PathBuf::from).collect();
    match args[0].as_str() {
        "--extract-here" if !rest.is_empty() => Startup::Run(Job::Extract {
            archives: rest,
            dest: Destination::Beside,
            password: None,
        }),
        "--extract-to-folder" if !rest.is_empty() => Startup::Run(Job::Extract {
            archives: rest,
            dest: Destination::Subfolder,
            password: None,
        }),
        "--test" if !rest.is_empty() => Startup::Run(Job::Test {
            archive: rest[0].clone(),
            only: None,
        }),
        "--add" if !rest.is_empty() => Startup::Add(rest),
        "--add-quick" if !rest.is_empty() => Startup::Run(Job::Compress {
            out: quick_output(&rest, Format::Zip),
            inputs: rest,
            format: Format::Zip,
            codec: Codec::Deflate,
            level: Level::Normal,
            password: None,
        }),
        other if !other.starts_with("--") => Startup::Browse(Some(PathBuf::from(other))),
        _ => Startup::Browse(None),
    }
}

fn fill(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut s = template.to_string();
    for (key, value) in pairs {
        s = s.replace(&format!("{{{key}}}"), value);
    }
    s
}

/// Gets the installer and checks it against the sums published beside it.
///
/// That protects against a download cut short or corrupted on the way. It does
/// not protect against a poisoned release, because the sum comes from the same
/// place as the file: for that these binaries would have to be signed, and they
/// are not yet.
fn download_update(
    installer: &str,
    sums: &str,
    s: &'static Strings,
    notify: &(dyn Fn(usize, usize, &str) -> bool + Sync),
) -> std::result::Result<PathBuf, String> {
    use sha2::{Digest, Sha256};

    let agent = format!("Arca/{}", env!("CARGO_PKG_VERSION"));
    let name = installer
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| s.update_failed.to_string())?;

    // The sums first, which are four lines: if those cannot be fetched there is
    // no sense in pulling five megabytes down to not be able to check them.
    let listing = arca_net::get(sums, &agent).ok_or_else(|| s.update_failed.to_string())?;
    let want = sum_for(&listing, name).ok_or_else(|| s.update_failed.to_string())?;

    let body = arca_net::fetch(installer, &agent, INSTALLER_LIMIT, &|so_far, total| {
        notify(so_far, total.unwrap_or(0), name)
    })
    .ok_or_else(|| s.update_failed.to_string())?;

    let got: [u8; 32] = Sha256::digest(&body).into();
    if got != want {
        return Err(s.update_tampered.to_string());
    }

    let path = std::env::temp_dir().join(name);
    fs::write(&path, &body).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Runs the installer and stands aside.
///
/// `/update=1` is ours, not Inno's, and tells the installer two things: not to
/// restart the Explorer to replace the shell menu DLL -- it is left in place
/// for the next boot and the old one still works -- and to open Arca again when
/// it finishes.
///
/// Arca is deliberately not closed here: Inno sees that the program it is about
/// to replace is open and closes it itself.
#[cfg(windows)]
fn install_update(path: &Path) -> std::result::Result<(), String> {
    std::process::Command::new(path)
        .args([
            "/VERYSILENT",
            "/NOCANCEL",
            "/NORESTART",
            // Not the Restart Manager: the installer reopens Arca itself with
            // /update=1, and both doing it would give two windows.
            "/NORESTARTAPPLICATIONS",
            "/update=1",
        ])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn install_update(_path: &Path) -> std::result::Result<(), String> {
    // There is no installer to fetch outside Windows, so this is never reached:
    // `arca_net` does not answer and there is never a new version to offer.
    Err("no installer on this system".into())
}

fn run_job_blocking(
    job: Job,
    s: &'static Strings,
    // Told how far along this is, and answers whether to carry on. False is
    // somebody pressing stop.
    notify: &(dyn Fn(usize, usize, &str) -> bool + Sync),
    ask: &dyn Fn(&Path) -> Answer,
) -> std::result::Result<String, String> {
    match job {
        Job::Extract {
            archives,
            dest,
            password,
        } => {
            if archives.is_empty() {
                return Err(s.nothing_to_do.to_string());
            }
            if archives.iter().any(|a| detect(a).is_none()) {
                return Err(s.unknown_format.to_string());
            }
            let mut total = 0u64;
            let mut last = PathBuf::new();
            for a in &archives {
                let base = a.parent().map(PathBuf::from).unwrap_or_default();
                let target = match dest {
                    Destination::Beside => base,
                    Destination::Subfolder => base.join(archive_stem(a)),
                };
                total += extract(a, &target, &[], notify, ask, password.as_deref())
                    .map_err(|e| e.to_string())?;
                last = target;
            }
            Ok(fill(
                s.extracted_to,
                &[
                    ("size", &human(total)),
                    ("dest", &last.display().to_string()),
                ],
            ))
        }
        Job::Test { archive, only } => {
            if detect(&archive).is_none() {
                return Err(s.unknown_format.to_string());
            }
            let (good, bad) =
                test_archive(&archive, only.as_ref(), notify).map_err(|e| e.to_string())?;
            if bad.is_empty() {
                Ok(fill(s.verified_ok, &[("n", &good.to_string())]))
            } else {
                Err(format!(
                    "{}: {}",
                    fill(
                        s.errors_found,
                        &[("good", &good.to_string()), ("bad", &bad.len().to_string())]
                    ),
                    bad.join("; ")
                ))
            }
        }
        // Built next to the original and read back in full before it replaces
        // it. The archive is the only copy of what is inside it.
        Job::Password {
            archive,
            current,
            new,
        } => {
            let temp = archive.with_file_name(format!(
                "{}.arca-new",
                archive
                    .file_name()
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
            let done = arca_zip::rewrite_password(
                &archive,
                &temp,
                current.as_deref(),
                new.as_deref(),
                notify,
            );
            if let Err(e) = done {
                let _ = fs::remove_file(&temp);
                return Err(e.to_string());
            }
            step_aside(&archive).map_err(|e| e.to_string())?;
            fs::rename(&temp, &archive).map_err(|e| e.to_string())?;
            let name = archive
                .file_name()
                .map(|x| x.to_string_lossy().to_string())
                .unwrap_or_default();
            Ok(fill(
                if new.is_some() {
                    s.password_set
                } else {
                    s.password_removed
                },
                &[("name", &name)],
            ))
        }
        // Same shape as the password rewrite, and the same care: the archive
        // is the only copy of what is inside it, so the new one is built
        // alongside, read back in full, and only then moved over.
        Job::Delete {
            archive,
            names,
            password,
        } => {
            if detect(&archive) != Some(Format::Zip) {
                return Err(s.only_zip_can_change.to_string());
            }
            let doomed: HashSet<String> = names.into_iter().collect();
            let temp = archive.with_file_name(format!(
                "{}.arca-new",
                archive
                    .file_name()
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
            let done = arca_zip::remove_entries(
                &archive,
                &temp,
                password.as_deref(),
                &|e| !doomed.contains(&e.name),
                notify,
            );
            let gone = doomed.len();
            if let Err(e) = done {
                let _ = fs::remove_file(&temp);
                return Err(e.to_string());
            }
            step_aside(&archive).map_err(|e| e.to_string())?;
            fs::rename(&temp, &archive).map_err(|e| e.to_string())?;
            Ok(fill(s.deleted, &[("n", &gone.to_string())]))
        }
        Job::CopyTo { archive, dest } => {
            // Copied by hand rather than with `fs::copy`, which says nothing
            // until it is finished: a three gigabyte archive would be a window
            // that had stopped answering for a minute. This one has a bar and a
            // way out, like everything else that takes a while.
            let total = fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
            let name = dest
                .file_name()
                .map(|x| x.to_string_lossy().to_string())
                .unwrap_or_default();
            let copied = (|| -> std::io::Result<u64> {
                let mut from = BufReader::with_capacity(BUF, File::open(&archive)?);
                let mut to = BufWriter::with_capacity(BUF, File::create(&dest)?);
                let mut buf = vec![0u8; BUF];
                let mut done = 0u64;
                loop {
                    let n = from.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    to.write_all(&buf[..n])?;
                    done += n as u64;
                    // The counters are whole megabytes: a bar that redraws once
                    // per sixty-four kilobytes is a bar drawing itself instead
                    // of the copy getting on with it.
                    if !notify(
                        (done / (1 << 20)) as usize,
                        (total / (1 << 20)).max(1) as usize,
                        &name,
                    ) {
                        return Err(std::io::Error::other("cancelled"));
                    }
                }
                to.flush()?;
                Ok(done)
            })();
            match copied {
                Ok(bytes) => Ok(fill(
                    s.copied_to,
                    &[
                        ("size", &human(bytes)),
                        ("dest", &dest.display().to_string()),
                    ],
                )),
                Err(e) => {
                    // Half a copy is not a copy. Whatever was written goes,
                    // whether the reason was a full disk or somebody pressing
                    // stop.
                    let _ = fs::remove_file(&dest);
                    if e.to_string() == "cancelled" {
                        return Err(arca_core::Error::Cancelled.to_string());
                    }
                    Err(e.to_string())
                }
            }
        }
        Job::Rename {
            archive,
            from,
            to,
            folder,
            password,
        } => {
            if detect(&archive) != Some(Format::Zip) {
                return Err(s.only_zip_can_change.to_string());
            }
            let temp = archive.with_file_name(format!(
                "{}.arca-new",
                archive
                    .file_name()
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
            // A folder answers to two spellings: some tools file an entry for
            // the folder itself with a slash on the end, others only file what
            // is inside it. Both have to move, and neither can be assumed.
            let under = format!("{from}/");
            let moved = format!("{to}/");
            let rename = |name: &str| -> String {
                if !folder {
                    return if name == from {
                        to.clone()
                    } else {
                        name.to_string()
                    };
                }
                if name == from {
                    to.clone()
                } else if name == under {
                    moved.clone()
                } else if let Some(rest) = name.strip_prefix(&under) {
                    format!("{moved}{rest}")
                } else {
                    name.to_string()
                }
            };
            let done = arca_zip::rename_entries(
                &archive,
                &temp,
                password.as_deref(),
                &|e| rename(&e.name),
                notify,
            );
            if let Err(e) = done {
                let _ = fs::remove_file(&temp);
                return Err(e.to_string());
            }
            step_aside(&archive).map_err(|e| e.to_string())?;
            fs::rename(&temp, &archive).map_err(|e| e.to_string())?;
            // Nothing to say: the new name is in the list, which is where the
            // eye already is. An empty word here leaves the summary of the
            // archive standing, which is what the bar is for.
            Ok(String::new())
        }
        Job::Compress {
            out,
            inputs,
            format,
            codec,
            level,
            password,
        } => {
            if inputs.is_empty() {
                return Err(s.nothing_to_do.to_string());
            }
            let (from, to) = compress(
                &out,
                &inputs,
                format,
                codec,
                level,
                notify,
                password.as_deref(),
            )
            .map_err(|e| e.to_string())?;
            let pct = if from == 0 {
                0.0
            } else {
                (1.0 - to as f64 / from as f64) * 100.0
            };
            Ok(fill(
                s.created,
                &[
                    ("name", &out.display().to_string()),
                    ("from", &human(from)),
                    ("to", &human(to)),
                    ("pct", &format!("{pct:.1}%")),
                ],
            ))
        }
        // Same care as Delete and the password rewrite: built alongside, read
        // back in full, and only then moved over the original.
        Job::Add {
            archive,
            inputs,
            dir,
            codec,
            level,
            password,
        } => {
            if detect(&archive) != Some(Format::Zip) {
                return Err(s.only_zip_can_change.to_string());
            }
            if inputs.is_empty() {
                return Err(s.nothing_to_do.to_string());
            }
            let extra: Vec<arca_zip::Addition> = collect_files(&inputs)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|(source, name)| arca_zip::Addition {
                    source: Some(source),
                    name: format!("{dir}{name}"),
                    codec,
                    level,
                })
                .collect();
            if extra.is_empty() {
                return Err(s.nothing_to_do.to_string());
            }
            let temp = archive.with_file_name(format!(
                "{}.arca-new",
                archive
                    .file_name()
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
            let n = extra.len();
            let done = arca_zip::add_entries(&archive, &temp, password.as_deref(), &extra, notify);
            if let Err(e) = done {
                let _ = fs::remove_file(&temp);
                return Err(e.to_string());
            }
            step_aside(&archive).map_err(|e| e.to_string())?;
            fs::rename(&temp, &archive).map_err(|e| e.to_string())?;
            Ok(fill(s.added, &[("n", &n.to_string())]))
        }
        // Not reached: getting the new version is handled earlier, on the
        // thread that starts the job, because it ends in a file to run and not
        // in a text to show.
        Job::Update { .. } => Err(s.update_failed.to_string()),
        Job::Move {
            archive,
            moves,
            password,
        } => {
            if detect(&archive) != Some(Format::Zip) {
                return Err(s.only_zip_can_change.to_string());
            }
            let temp = archive.with_file_name(format!(
                "{}.arca-new",
                archive
                    .file_name()
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
            // All of them in one pass. Moving is renaming with a different
            // folder in front, and renaming is a rewrite of the whole archive:
            // five files moved one at a time would be five rewrites.
            let done = arca_zip::rename_entries(
                &archive,
                &temp,
                password.as_deref(),
                &|e| moved_name(&e.name, &moves),
                notify,
            );
            if let Err(e) = done {
                let _ = fs::remove_file(&temp);
                return Err(e.to_string());
            }
            step_aside(&archive).map_err(|e| e.to_string())?;
            fs::rename(&temp, &archive).map_err(|e| e.to_string())?;
            // The list says where everything is now, which is the whole answer.
            Ok(String::new())
        }
        Job::NewFolder {
            archive,
            name,
            password,
        } => {
            if detect(&archive) != Some(Format::Zip) {
                return Err(s.only_zip_can_change.to_string());
            }
            let temp = archive.with_file_name(format!(
                "{}.arca-new",
                archive
                    .file_name()
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_default()
            ));
            let extra = [arca_zip::Addition {
                // No file behind it: a folder in a zip is a name and nothing
                // else.
                source: None,
                name,
                codec: Codec::Store,
                level: Level::Store,
            }];
            let done = arca_zip::add_entries(&archive, &temp, password.as_deref(), &extra, notify);
            if let Err(e) = done {
                let _ = fs::remove_file(&temp);
                return Err(e.to_string());
            }
            step_aside(&archive).map_err(|e| e.to_string())?;
            fs::rename(&temp, &archive).map_err(|e| e.to_string())?;
            // Nothing to say: the folder is in the list, which is where the eye
            // already is.
            Ok(String::new())
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum SortColumn {
    Name,
    Size,
    Packed,
    Method,
    Saved,
    Modified,
    Crc,
    Type,
    Path,
    Created,
    Accessed,
    Attributes,
}

// Which columns the list shows. Name is not here: a list of nothing but sizes
// would be a strange thing to allow.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Columns {
    size: bool,
    packed: bool,
    method: bool,
    saved: bool,
    modified: bool,
    crc: bool,
    type_: bool,
    path: bool,
    created: bool,
    accessed: bool,
    attributes: bool,
}

impl Default for Columns {
    fn default() -> Self {
        // What was on screen before any of this was a choice, plus the date,
        // which both WinRAR and NanaZip show and which people look for.
        Columns {
            size: true,
            packed: true,
            method: true,
            saved: true,
            modified: true,
            crc: false,
            type_: false,
            path: false,
            created: false,
            accessed: false,
            attributes: false,
        }
    }
}

impl Columns {
    const ALL: [(SortColumn, &'static str); 11] = [
        (SortColumn::Size, "size"),
        (SortColumn::Packed, "packed"),
        (SortColumn::Method, "method"),
        (SortColumn::Saved, "saved"),
        (SortColumn::Modified, "modified"),
        (SortColumn::Crc, "crc"),
        (SortColumn::Type, "type"),
        (SortColumn::Path, "path"),
        (SortColumn::Created, "created"),
        (SortColumn::Accessed, "accessed"),
        (SortColumn::Attributes, "attributes"),
    ];

    fn on(&self, which: SortColumn) -> bool {
        match which {
            SortColumn::Size => self.size,
            SortColumn::Packed => self.packed,
            SortColumn::Method => self.method,
            SortColumn::Saved => self.saved,
            SortColumn::Modified => self.modified,
            SortColumn::Crc => self.crc,
            SortColumn::Type => self.type_,
            SortColumn::Path => self.path,
            SortColumn::Created => self.created,
            SortColumn::Accessed => self.accessed,
            SortColumn::Attributes => self.attributes,
            SortColumn::Name => true,
        }
    }

    fn set(&mut self, which: SortColumn, value: bool) {
        match which {
            SortColumn::Size => self.size = value,
            SortColumn::Packed => self.packed = value,
            SortColumn::Method => self.method = value,
            SortColumn::Saved => self.saved = value,
            SortColumn::Modified => self.modified = value,
            SortColumn::Crc => self.crc = value,
            SortColumn::Type => self.type_ = value,
            SortColumn::Path => self.path = value,
            SortColumn::Created => self.created = value,
            SortColumn::Accessed => self.accessed = value,
            SortColumn::Attributes => self.attributes = value,
            SortColumn::Name => {}
        }
    }

    fn label(which: SortColumn, s: &Strings) -> &'static str {
        match which {
            SortColumn::Size => s.col_size,
            SortColumn::Packed => s.col_packed,
            SortColumn::Method => s.col_method,
            SortColumn::Saved => s.col_saved,
            SortColumn::Modified => s.col_modified,
            SortColumn::Crc => s.col_crc,
            SortColumn::Type => s.col_type,
            SortColumn::Path => s.col_path,
            SortColumn::Created => s.col_created,
            SortColumn::Accessed => s.col_accessed,
            SortColumn::Attributes => s.col_attributes,
            SortColumn::Name => s.col_name,
        }
    }
}

enum Message {
    Listing(PathBuf, Vec<Entry>),
    // The installer is down and checked. It ends in a file to run rather than
    // in a text to read, which is why it is not a `Done`.
    Downloaded(PathBuf),
    Conflict(String),
    Progress(usize, usize, String),
    Done(String),
    Failed(String),
    // A cut reached the clipboard in one piece. Sent before Done, and only
    // then, so a cut that failed halfway never leaves the window waiting to
    // take entries out of an archive on the strength of it.
    CutReady,
}

// What a cut is waiting on. The entries stay in the archive until the paste
// actually happens, and this is what says which ones and how to tell.
struct Cut {
    archive: PathBuf,
    // The extracted copies handed to the shell. A paste with the move effect
    // takes them out of the temporary folder, and their absence is the only
    // sign Windows gives that it happened.
    paths: Vec<PathBuf>,
    names: Vec<String>,
}

// What the password window is standing in front of: a job the context menu
// handed us, or an encrypted archive just opened in the window.
// Which blank the password window is filling in. They are not the same
// question: one needs the password the archive already has, the other the one it
// is about to get.
enum Pending {
    Extract(Box<Job>),
    OpenArchive,
    CurrentPassword(Box<Job>),
    NewPassword(Box<Job>),
}

enum View {
    Browse,
    Add,
    Running,
}

enum AppAction {
    Open(PathBuf),
    Run(Job),
    Extract { only_checked: bool },
    ExtractTo { only_checked: bool, dest: PathBuf },
    PrepareCompress(Vec<PathBuf>),
    SetFilter(String),
    SelectAllVisible,
    InvertVisible,
    Add(Vec<PathBuf>),
    Drop(Vec<PathBuf>),
    Copy { cut: bool },
    Paste,
    OpenFile(usize),
    Navigate(String),
    Back,
    Forward,
    SetChecked { row: Row, value: bool },
    ClearSelection,
    Sort(SortColumn),
    ToggleColumn(SortColumn),
    SetLanguage(Option<Lang>),
    SetTheme(ThemePreference),
    AnswerConflict(Answer),
    CancelPassword,
    SetPasswordInput(String),
    SubmitPassword(String),
    TogglePasswordVisibility,
    BeginPasswordChange,
    RequestDelete,
    ConfirmDelete(bool),
    AnswerDrop(DropChoice),
    CancelJob,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DropChoice {
    Open,
    Add,
    Cancel,
}

// What the overflow button on the toolbar was asked for. A value rather than a
// closure because the menu draws while the toolbar still holds `self`.
#[derive(Clone)]
enum More {
    Test,
    Release,
    Undo,
    NewFolder,
    Page(arca_zip::pages::Page),
    SaveCopy,
    DefaultPassword,
    Flat,
    Tree,
    Open(PathBuf),
    Forget,
    All,
    Invert,
    None_,
    Settings,
    Shortcuts,
}

/// Pressing the wheel drops an anchor and the list then runs towards the
/// pointer, faster the further away it is: the gesture Windows has had since
/// the wheel arrived, and the one every browser copies.
#[derive(Clone, Copy)]
struct Wheel {
    /// Where the wheel went down. The list stands still while the pointer is
    /// near it and runs when it is away.
    anchor: egui::Pos2,
    /// How far down the list is, kept here rather than read back from the
    /// table because it moves by fractions of a pixel per frame and the table
    /// only remembers whole scroll positions.
    at: f32,
    /// Whether the pointer has pulled away from the anchor yet. Letting the
    /// wheel go after it has ends the gesture, letting it go before leaves it
    /// running until the next click; that is what makes press-and-drag and
    /// click-and-go both work off the one button.
    moved: bool,
}

/// The DOS attribute byte as the letters every file manager has shown it with
/// since there were file managers: read only, hidden, system, archive.
///
/// A dash where a bit is off rather than a shorter string, so that the column
/// lines up down the page and the eye can read one position instead of one
/// word. The directory bit is not shown: the list already says which rows are
/// folders, in a way that does not need decoding.
fn attribute_letters(bits: u8) -> String {
    [(0x01, 'R'), (0x02, 'H'), (0x04, 'S'), (0x20, 'A')]
        .iter()
        .map(|(mask, letter)| if bits & mask != 0 { *letter } else { '-' })
        .collect()
}

/// Whether a name claims to be a picture of a kind the window can draw.
fn looks_like_picture(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp")
    )
}

/// One line of a hex dump: where it starts, the bytes, and what they would be
/// if they were letters.
///
/// The three columns are what makes a dump readable: the offset to point at,
/// the bytes to read, and the letters to recognise a string in the middle of
/// something that is not one. A dot stands for everything unprintable, which is
/// the convention every other dump follows.
fn hex_line(at: usize, bytes: &[u8]) -> String {
    let mut out = format!("{at:08X}  ");
    for i in 0..16 {
        match bytes.get(i) {
            Some(b) => out.push_str(&format!("{b:02X} ")),
            None => out.push_str("   "),
        }
        if i == 7 {
            out.push(' ');
        }
    }
    out.push(' ');
    for b in bytes {
        out.push(if (0x20..0x7F).contains(b) {
            *b as char
        } else {
            '.'
        });
    }
    out
}

/// How a file is being looked at in the viewer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Text,
    Hex,
    Picture,
}

/// A file out of the archive, held in memory for looking at.
///
/// The bytes are never written to disk. Viewing something is not the same as
/// extracting it, and a viewer that leaves a copy in the temporary folder has
/// quietly extracted it.
struct Viewed {
    name: String,
    // Shared rather than owned outright: the picture view hands these to egui
    // on every frame, and a file of thirty megabytes copied sixty times a
    // second is two gigabytes a second of nothing.
    bytes: std::sync::Arc<[u8]>,
    look: Look,
    // Split once when the file arrives rather than on every frame: the view is
    // drawn a line at a time and the lines have to exist to be counted.
    lines: Vec<String>,
    // Whether the picture loader made anything of it. Asked once, because a
    // failed decode is as expensive as a successful one.
    picture: bool,
}

/// The most a file can be and still be opened for looking at.
///
/// A viewer holds the whole thing in memory, and the point of it is a glance at
/// a text file or a picture, not reading a database. Past this the answer is to
/// take it out properly, which is what the rest of the window is for.
const VIEW_LIMIT: u64 = 32 * 1024 * 1024;

/// Whether these bytes are meant to be read as words.
///
/// Two questions, in the order that settles it fastest. A zero byte is the one
/// thing text almost never has and binary almost always does, so it is asked
/// first and on its own. Failing that, the balance of what is printable: a
/// stray high byte is a name with an accent in it, a run of them is a program.
///
/// Only the head is read. A file that begins as text and turns into something
/// else halfway down is a file the reader will notice by looking at it.
fn looks_like_text(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8192)];
    if head.is_empty() {
        return true;
    }
    if head.contains(&0) {
        return false;
    }
    let odd = head
        .iter()
        .filter(|b| **b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r'))
        .count();
    odd * 20 < head.len()
}

/// Whether `name` answers to `mask`, where `*` stands for any run of
/// characters and `?` for exactly one.
///
/// The same two wildcards WinRAR and the command line have always used, and
/// nothing else: a mask is something people type in a hurry and a language with
/// character classes in it would turn a typo into a silent mismatch. Case is
/// ignored, because Windows ignores it and the names came off a Windows disk.
///
/// Written as a walk with one point of backtracking rather than as a recursion:
/// `*` is the only thing that can be taken back, so remembering where the last
/// one was and how far it had eaten is the whole of it. That is what keeps a
/// mask of nothing but stars from taking exponential time on a long name.
fn matches_mask(mask: &str, name: &str) -> bool {
    let m: Vec<char> = mask.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    // Where to come back to: the star, and the character after which it had
    // eaten everything up to.
    let mut star: Option<(usize, usize)> = None;

    while j < n.len() {
        match m.get(i) {
            Some('*') => {
                star = Some((i, j));
                i += 1;
            }
            Some('?') => {
                i += 1;
                j += 1;
            }
            Some(c) if *c == n[j] => {
                i += 1;
                j += 1;
            }
            // No match here. If a star is behind us it can swallow one more
            // character and we try again from there; if not, there is nothing
            // left to try.
            _ => match star {
                Some((si, sj)) => {
                    i = si + 1;
                    j = sj + 1;
                    star = Some((si, sj + 1));
                }
                None => return false,
            },
        }
    }
    // Trailing stars match the empty rest of the name; anything else does not.
    m[i..].iter().all(|c| *c == '*')
}

/// One row of the folder tree, ready to be drawn.
struct Twig<'a> {
    name: &'a str,
    path: String,
    depth: usize,
    kids: bool,
    open: bool,
    // The archive itself rather than a folder inside it, which gets the icon
    // the desktop puts on a .zip.
    archive: bool,
}

/// How tall a row of the tree is. Taller than a line of text, because this is a
/// list of places to press rather than a paragraph to read.
const TWIG_HEIGHT: f32 = 26.0;

/// Draws one row of the folder tree and says what was pressed.
///
/// The whole width answers, not the word: a navigation pane where only the
/// letters are a target is a pane you have to aim at, and the highlight that
/// says where you are should reach both edges or it reads as a button that
/// happens to be lit. That is how the Explorer's own pane behaves.
///
/// The chevron on the right belongs to whether the folder is unfolded and
/// nothing else. Pressing the name takes you there whether it is unfolded or
/// not, which is the difference between a tree you can walk and one you have to
/// open first.
fn twig(
    ui: &mut egui::Ui,
    icons: &mut HashMap<String, Option<egui::TextureHandle>>,
    twig: &Twig<'_>,
    here: &str,
) -> egui::Response {
    let full = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(full, TWIG_HEIGHT), egui::Sense::click());
    let on = here == twig.path;
    let fill = if on {
        ui.visuals().selection.bg_fill
    } else if resp.hovered() {
        ui.visuals().widgets.hovered.bg_fill
    } else {
        egui::Color32::TRANSPARENT
    };
    let ink = if on {
        ui.visuals().selection.stroke.color
    } else {
        ui.visuals().widgets.noninteractive.fg_stroke.color
    };
    if fill != egui::Color32::TRANSPARENT {
        ui.painter()
            .rect_filled(rect, egui::Rounding::same(4.0), fill);
    }

    // Each level a thumb further in, and the icon always at the same distance
    // from the name, so a column of names reads as a column.
    let inset = 8.0 + twig.depth as f32 * 14.0;
    let mid = rect.center().y;
    let icon = egui::Rect::from_center_size(
        egui::pos2(rect.left() + inset + 8.0, mid),
        egui::Vec2::splat(16.0),
    );
    let name = if twig.archive {
        "archive.zip"
    } else {
        "folder"
    };
    match system_icon(ui.ctx(), icons, name, !twig.archive) {
        Some(tex) => {
            egui::Image::new(&tex).paint_at(ui, icon);
        }
        None => draw_icon_at(
            ui,
            icon,
            if twig.archive {
                Kind::Archive
            } else {
                Kind::Dir
            },
        ),
    }

    ui.painter().text(
        egui::pos2(icon.right() + 8.0, mid),
        egui::Align2::LEFT_CENTER,
        twig.name,
        egui::TextStyle::Body.resolve(ui.style()),
        ink,
    );

    // A chevron only where there is something folded up behind it, turned down
    // once it is open, at the far edge where every pane on this machine puts
    // the thing that says "there is more".
    if twig.kids {
        let c = egui::pos2(rect.right() - 14.0, mid);
        let (w, h) = (3.5, 5.0);
        let points = if twig.open {
            vec![
                egui::pos2(c.x - h, c.y - w * 0.6),
                egui::pos2(c.x + h, c.y - w * 0.6),
                egui::pos2(c.x, c.y + w),
            ]
        } else {
            vec![
                egui::pos2(c.x - w * 0.6, c.y - h),
                egui::pos2(c.x - w * 0.6, c.y + h),
                egui::pos2(c.x + w, c.y),
            ]
        };
        ui.painter().add(egui::Shape::convex_polygon(
            points,
            ink.gamma_multiply(0.7),
            egui::Stroke::NONE,
        ));
    }
    resp
}

/// One level of the folder tree, and everything under it.
fn branch(
    ui: &mut egui::Ui,
    icons: &mut HashMap<String, Option<egui::TextureHandle>>,
    folder: &tree::Folder,
    prefix: &str,
    depth: usize,
    here: &str,
    go: &mut Option<String>,
) {
    for (name, kid) in &folder.kids {
        let path = format!("{prefix}{name}/");
        let id = ui.make_persistent_id(&path);
        let mut state =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        let open = state.is_open();
        let row = Twig {
            name,
            path: path.clone(),
            depth,
            kids: !kid.is_empty(),
            open,
            archive: false,
        };
        let resp = twig(ui, icons, &row, here);
        if resp.clicked() {
            // The chevron is its own target: the last stretch of the row folds
            // and unfolds, and the rest of it goes there.
            let at_end = ui
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|p| p.x > resp.rect.right() - 28.0);
            if row.kids && at_end {
                state.toggle(ui);
            } else {
                *go = Some(path.clone());
            }
        }
        if open && row.kids {
            branch(ui, icons, kid, &path, depth + 1, here, go);
        }
    }
}

/// Seconds as a clock: `0:07`, `1:38`, `2:05:11`.
///
/// Minutes and seconds until there are hours, and no leading zero on the
/// largest part: a job that says `0:00:07` is a job whose progress window was
/// designed for a job that takes hours.
fn clock(seconds: f64) -> String {
    // A guess of a hundred hours is not a guess; anything past this is capped
    // rather than shown, and NaN falls to nothing rather than to a panic.
    let whole = if seconds.is_finite() {
        seconds.clamp(0.0, 359_999.0) as u64
    } else {
        0
    };
    let (h, m, s) = (whole / 3600, (whole / 60) % 60, whole % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// What a job is being done to: the name that says which of several windows
/// this one is.
fn subject_of(job: &Job) -> String {
    let named = |p: &Path| {
        p.file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    match job {
        Job::Extract { archives, .. } => match archives.split_first() {
            Some((only, [])) => named(only),
            Some((_, rest)) => format!("{} +{}", named(&archives[0]), rest.len()),
            None => String::new(),
        },
        Job::Compress { out, .. } => named(out),
        Job::Test { archive, .. }
        | Job::Password { archive, .. }
        | Job::Delete { archive, .. }
        | Job::CopyTo { archive, .. }
        | Job::Move { archive, .. }
        | Job::NewFolder { archive, .. }
        | Job::Rename { archive, .. }
        | Job::Add { archive, .. } => named(archive),
        // Here the file is the installer, and its name already has the version.
        Job::Update { installer, .. } => {
            installer.rsplit('/').next().unwrap_or_default().to_string()
        }
    }
}

/// Where the announcement is asked for, and where a copy that cannot update
/// itself is sent instead.
const RELEASES_API: &str = "https://api.github.com/repos/THIONG/arca/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/THIONG/arca/releases/latest";

/// As much installer as is ever going to arrive. A reply longer than this is
/// not our release and is not going to be written to disk, let alone run.
const INSTALLER_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct Release {
    tag: String,
    installer: Option<String>,
    sums: Option<String>,
}

/// Reads the announcement.
///
/// Hand written rather than a JSON library, because this asks three questions
/// of one reply and the answers are short strings. A parser for the whole
/// language would be a dependency, and a large one, for that.
///
/// The addresses are picked by what they end in rather than by walking the list
/// of assets: the shape of that list is GitHub's to change, but a file called
/// `SHA256SUMS.txt` is called that because we named it.
fn release_of(reply: &str) -> Option<Release> {
    let tag = tag_of(reply)?;
    let mut installer = None;
    let mut sums = None;
    for piece in reply.split("\"browser_download_url\"").skip(1) {
        let Some(open) = piece.find('"').and_then(|c| piece.get(c + 1..)) else {
            continue;
        };
        let Some(close) = open.find('"') else {
            continue;
        };
        let url = &open[..close];
        // Only ours, and only over the wire we trust. A reply that names some
        // other place is not one to go and fetch an executable from.
        if !url.starts_with("https://github.com/THIONG/arca/releases/download/") {
            continue;
        }
        if url.ends_with("/SHA256SUMS.txt") {
            sums = Some(url.to_string());
        } else if url.ends_with("-x86_64.exe") && url.contains("/arca-setup-") {
            installer = Some(url.to_string());
        }
    }
    Some(Release {
        tag,
        installer,
        sums,
    })
}

/// The line for `name` in a `sha256sum` listing, as raw bytes.
///
/// Two spellings, because that is what the tool writes: two spaces for a file
/// it read as text and a space and a star for one it read as binary. The
/// Windows halves of our own releases come out with the star.
fn sum_for(listing: &str, name: &str) -> Option<[u8; 32]> {
    for line in listing.lines() {
        let (hash, rest) = line.split_once(' ')?;
        let named = rest.trim_start_matches([' ', '*']);
        if named != name || hash.len() != 64 {
            continue;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hash.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        return Some(out);
    }
    None
}

/// Whether this copy of Arca was put here by the installer.
///
/// Inno Setup leaves its uninstaller in the folder it installed to, so that
/// file being next to the program is the program saying how it got there. A
/// copy unpacked from the .zip has no uninstaller and nothing to update: for
/// that one the only honest offer is the page.
fn installed_by_setup() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("unins000.exe")))
        .is_some_and(|u| u.exists())
}

/// Pulls the release's name out of what the announcement page answered.
///
/// What it must not do is find the wrong `tag_name`. There is only one at the
/// top level of that reply, so the first is the right one; anything unexpected
/// gives nothing, and nothing means the window says nothing.
fn tag_of(reply: &str) -> Option<String> {
    let at = reply.find("\"tag_name\"")? + "\"tag_name\"".len();
    let rest = reply.get(at..)?;
    let colon = rest.find(':')?;
    let after = rest.get(colon + 1..)?;
    let open = after.find('"')?;
    let value = after.get(open + 1..)?;
    let close = value.find('"')?;
    let tag = value.get(..close)?.trim();
    // A name of nothing, or one long enough to be somebody being funny, is not
    // a version.
    (!tag.is_empty() && tag.len() <= 32).then(|| tag.to_string())
}

/// Whether `latest` is a later version than `running`.
///
/// Numbers separated by dots, a leading `v` forgiven, and compared a part at a
/// time rather than as text: as text, `0.10.0` comes before `0.9.0` and the
/// window would either nag for ever or never say anything at all.
///
/// A version with something after the numbers -- `0.6.0-rc1` -- counts as
/// earlier than the plain one, which is what those names mean everywhere. And
/// anything that is not a version at all answers no: silence is the right
/// behaviour for an announcement nobody can read.
fn newer(running: &str, latest: &str) -> bool {
    fn parts(v: &str) -> Option<(Vec<u32>, bool)> {
        let v = v.trim().trim_start_matches(['v', 'V']);
        if v.is_empty() {
            return None;
        }
        let (numbers, tail) = match v.find(['-', '+']) {
            Some(cut) => (&v[..cut], true),
            None => (v, false),
        };
        let mut out = Vec::new();
        for piece in numbers.split('.') {
            out.push(piece.parse::<u32>().ok()?);
        }
        (!out.is_empty()).then_some((out, tail))
    }

    let (Some((mine, mine_tail)), Some((theirs, theirs_tail))) = (parts(running), parts(latest))
    else {
        return false;
    };
    // Missing parts count as zero, so 0.6 and 0.6.0 are the same version.
    let deep = mine.len().max(theirs.len());
    for i in 0..deep {
        let a = mine.get(i).copied().unwrap_or(0);
        let b = theirs.get(i).copied().unwrap_or(0);
        if a != b {
            return b > a;
        }
    }
    // The same numbers: the one without a suffix is the finished one.
    mine_tail && !theirs_tail
}

/// A name in the one spelling the window works in.
///
/// A zip written on Windows can hold backslashes, and comparing those against
/// the paths the tree is built from silently matched nothing: a move out of a
/// folder took the whole archive with it, and moving inside a folder did
/// nothing at all in those archives.
fn slashed(name: &str) -> String {
    name.replace('\\', "/")
}

/// What an entry is called after a move.
///
/// A folder is not one entry but everything filed under it, so a move matches
/// the name itself, the name with its slash, and everything beneath it.
fn moved_name(name: &str, moves: &[(String, String)]) -> String {
    // Compared, and written out again, in the spelling the window works in.
    let name = slashed(name);
    for (from, to) in moves {
        if name == *from {
            return to.clone();
        }
        let under = format!("{from}/");
        if name == under {
            return format!("{to}/");
        }
        if let Some(rest) = name.strip_prefix(&under) {
            return format!("{to}/{rest}");
        }
    }
    name
}

/// Whether a name is a folder's, which in a zip is the slash on the end of it
/// and nothing else. Both slashes, because archives from Windows use theirs.
fn is_folder_name(name: &str) -> bool {
    name.ends_with('/') || name.ends_with('\\')
}

/// The folder an entry is filed in, without the name on the end. Empty at the
/// root, which is where the archive itself is.
fn folder_of(path: &str) -> &str {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    }
}

/// The way out of a folder: the row every file list keeps at the top, spelt the
/// way every file list spells it.
///
/// It stands for the folder above and nothing else. There is no entry behind
/// it, so it cannot be picked, weighed, renamed or taken out, and the list
/// leaves it at the top however it is sorted.
fn up_row(dir: &str) -> Row {
    Row {
        label: "..".to_string(),
        path: parent_of(dir),
        kind: Kind::Dir,
        is_dir: true,
        entry: None,
        size: 0,
        packed: 0,
        method: "",
        encrypted: false,
        count: 0,
        mtime: None,
        created: None,
        accessed: None,
        attributes: 0,
        crc32: 0,
        up: true,
    }
}

/// How wide `text` comes out in `style`, laid out on one line.
fn wide_of(ui: &egui::Ui, text: &str, style: egui::TextStyle) -> f32 {
    ui.fonts(|f| {
        f.layout_no_wrap(
            text.to_owned(),
            style.resolve(ui.style()),
            egui::Color32::PLACEHOLDER,
        )
        .size()
        .x
    })
}

/// What a column would have to be to hold everything in it without cutting
/// anything off.
///
/// Measured over the rows on screen -- which is the folder you are looking at,
/// filter and all -- rather than over the whole archive, because that is the
/// list the column is being fitted to. Every cell is measured in the style it
/// is drawn in: the numbers are monospaced and a monospaced digit is wider than
/// a proportional one, so measuring them all as body text would fit a column
/// that then cuts off its own contents.
fn natural_width(ui: &egui::Ui, rows: &[Row], which: SortColumn, s: &Strings) -> f32 {
    use egui::TextStyle::{Body, Monospace};
    let pad = ui.spacing().item_spacing.x * 2.0;
    let widest = |style: egui::TextStyle, of: &dyn Fn(&Row) -> String| -> f32 {
        rows.iter()
            .map(|r| wide_of(ui, &of(r), style.clone()))
            .fold(0.0_f32, f32::max)
    };
    match which {
        // The icon and the gap after it are part of what the name column has to
        // hold, so they are part of what it is fitted to.
        SortColumn::Name => {
            let head = wide_of(ui, s.col_name, Body) + 20.0;
            (widest(Body, &|r| r.label.clone()) + 19.0 + pad).max(head)
        }
        SortColumn::Size => widest(Monospace, &|r| human(r.size)) + pad,
        SortColumn::Packed => widest(Monospace, &|r| human(r.packed)) + pad,
        SortColumn::Method => {
            widest(Body, &|r| {
                if r.is_dir {
                    format!("{} {}", r.count, s.items_word)
                } else if r.encrypted {
                    format!("AES-256 {}", r.method)
                } else {
                    r.method.to_string()
                }
            }) + pad
        }
        SortColumn::Saved => widest(Monospace, &|_| "100%".to_string()) + pad,
        SortColumn::Modified => widest(Monospace, &|r| when(r.mtime)) + pad,
        SortColumn::Crc => widest(Monospace, &|_| "FFFFFFFF".to_string()) + pad,
        // Fitted to the heading alone. The words come from the shell one
        // extension at a time and measuring them here would ask it about every
        // row in the folder before the column could be sized.
        SortColumn::Type => wide_of(ui, s.col_type, Body) + pad,
        SortColumn::Path => widest(Body, &|r| folder_of(&r.path).to_string()) + pad,
        SortColumn::Created => widest(Monospace, &|r| when(r.created)) + pad,
        SortColumn::Accessed => widest(Monospace, &|r| when(r.accessed)) + pad,
        SortColumn::Attributes => wide_of(ui, "RHSA", Monospace) + pad,
    }
}

/// The name of a row, opened for editing where it stands.
///
/// In the row rather than in a dialog, because that is where the name is and
/// where the eye already is: WinRAR and the Explorer both do it here. Enter
/// keeps what was typed, Escape throws it away, and so does clicking somewhere
/// else -- a rename abandoned by looking away has to be abandoned, not left
/// half open on a row nobody is looking at any more.
///
/// `fresh` is true on the first frame only. That is when the box takes the
/// keyboard and picks out the part of the name before the extension, which is
/// the part anybody renaming a file means to change; the extension stays behind
/// the cursor, ready to be kept.
fn name_box(
    ui: &mut egui::Ui,
    typing: &std::cell::RefCell<String>,
    fresh: &std::cell::Cell<bool>,
    finish: &std::cell::Cell<Option<bool>>,
) {
    let id = egui::Id::new("arca-rename");
    let mut text = typing.borrow_mut();
    let field = ui.add(
        egui::TextEdit::singleline(&mut *text)
            .id(id)
            .desired_width(ui.available_width())
            .vertical_align(egui::Align::Center),
    );
    if fresh.get() {
        field.request_focus();
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
            // The stem, or the whole thing when there is no extension to keep
            // out of the way. A leading dot is not an extension, it is how a
            // file asks to be left alone.
            let stem = text.rfind('.').filter(|at| *at > 0).unwrap_or(text.len());
            let upto = text[..stem].chars().count();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(upto),
                )));
            state.store(ui.ctx(), id);
        }
        fresh.set(false);
    }
    if field.lost_focus() {
        // Losing the keyboard to Enter is finishing; losing it any other way --
        // Tab, a click elsewhere -- is walking away.
        let kept = ui.input(|i| i.key_pressed(egui::Key::Enter));
        finish.set(Some(kept));
    } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        finish.set(Some(false));
    }
}

/// How fast the list should run, in pixels a second, for a pointer `away`
/// pixels from the anchor. Negative runs it up.
///
/// Nothing at all inside a dead zone, because the wheel is a button too and a
/// hand that presses one moves a pixel or two doing it. Past that it grows
/// with the square of the distance: gently near the anchor, where the point is
/// to read what goes by, and hard further out, where the point is to get to
/// the end. Capped, because past a certain speed the only difference is how
/// blurred it is.
fn wheel_speed(away: f32) -> f32 {
    const DEAD: f32 = 12.0;
    let past = away.abs() - DEAD;
    if past <= 0.0 {
        return 0.0;
    }
    (past * past / 12.0).min(4000.0) * away.signum()
}

struct AppState {
    view: View,
    settings: Settings,
    archive: Option<PathBuf>,
    entries: Vec<Entry>,
    checked: Vec<bool>,
    filter: String,
    order: (SortColumn, bool),
    channel: Option<Receiver<Message>>,
    notice: String,
    error: bool,
    busy: bool,
    done_count: usize,
    total_count: usize,
    current_file: String,
    started: Option<Instant>,
    format: Format,
    codec: Codec,
    level: Level,
    into_subfolder: bool,
    pending_inputs: Vec<PathBuf>,
    output_name: String,
    close_when_done: bool,
    title: String,
    window_title: String,
    current_dir: String,
    show_settings: bool,
    conflict: Option<String>,
    replies: Option<Sender<Answer>>,
    // A job held back until the password window has an answer. Extraction asks
    // once, before it starts, rather than per entry: every entry in a .zip is
    // encrypted with the same password, and asking again per file is noise.
    waiting_on_password: Option<Pending>,
    password_input: String,
    show_password: bool,
    add_password: String,
    // Held for the archive currently open in the window, so extracting from it
    // does not ask again for every button press.
    archive_password: Option<String>,
    // Where to look again once a password job has rewritten the archive, and the
    // password it now carries.
    after_password: Option<(PathBuf, Option<String>)>,
    // Where the window has been, so the mouse back and forward buttons have
    // somewhere to go. `here` indexes into it; going somewhere new throws away
    // whatever was ahead, the way a browser does.
    history: Vec<String>,
    here: usize,
    // The row the keyboard is on. Everything the arrows, Enter and Space do
    // hangs off this, and there was no such thing before: the table had
    // checkboxes but no cursor. None means nothing is focused yet.
    cursor: Option<usize>,
    // One texture per extension, filled the first time a kind is seen.
    // The desktop's word for each kind of file, by extension. See `system_type`.
    types: HashMap<String, Option<String>>,
    // Where a rubber band started, and what was ticked before it did. The
    // second is what lets the band be recomputed from scratch every frame, so
    // dragging back over a row lets go of it again.
    band_base: Vec<bool>,
    // The row the drag started on, and the scroll position it is dragging the
    // list to when it runs off an edge. Row numbers rather than places on
    // screen, because the list moves while the drag is happening.
    band_anchor: Option<usize>,
    // Set when a press lands on a row that is already picked: the gesture is
    // still ambiguous until it has moved, and this is what it turns into.
    drag_ready: Option<usize>,
    // Set the moment a drag out of the window finishes. `DoDragDrop` runs its
    // own loop and swallows the release that ends it, so the toolkit comes back
    // still believing the button is held: without this the next frame starts a
    // selection band that has already missed its own release and can never be
    // ended, and the list stops answering to anything. It clears when the
    // button really does come up.
    drag_settling: bool,
    band_scroll: Option<f32>,
    // The entry being renamed and what has been typed into it so far. Held by
    // path rather than by row number so that sorting or filtering underneath a
    // half typed name cannot move the box onto somebody else's row.
    renaming: Option<(String, String)>,
    // True for the first frame of a rename, when the box has to be given the
    // keyboard and the part of the name before the extension picked out.
    rename_fresh: bool,
    // One password to try before asking, for a folder of archives all locked
    // with the same word. Never written anywhere: see `default_password_window`.
    default_password: Option<String>,
    asking_default_password: bool,
    // Set while the box that asks for a new folder's name is up, and what has
    // been typed into it.
    asking_folder: bool,
    folder_input: String,
    // The archive that has a previous version kept beside it, and the word for
    // what was done to it. One step back, which is the one anybody wants:
    // deeper than that and the sidecars would pile up.
    undo: Option<(PathBuf, &'static str)>,
    // The selection that is in the air, by path. Where it is going is not
    // decided until the button comes up: inside the window it is a move into
    // another folder of this archive, outside it is a drag into whatever is
    // out there.
    carrying: Option<Vec<String>>,
    // The newer Arca, once the announcement has answered. Its own channel
    // rather than a job, because something nobody asked for must not make the
    // window look busy.
    update: Option<Release>,
    update_rx: Option<std::sync::mpsc::Receiver<Release>>,
    asked_about_updates: bool,
    // Whether the two counts are bytes rather than entries. Only the download
    // measures itself that way.
    in_bytes: bool,
    // What the job is being done to, beside the verb in the title.
    subject: String,
    // Whether the job is a panel over the list it was started from, rather than
    // the whole window. A job that came from the Explorer has no list behind it
    // to go back to.
    overlay: bool,
    // Told to give up, and told to hold. Shared with the thread doing the work,
    // which reads both at the end of every entry -- the one moment it is not in
    // the middle of something.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    hold: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // The folders of the archive, rebuilt when a listing arrives rather than
    // every frame: it is fifteen hundred paths split on every slash and the
    // answer only changes when the archive does.
    folders: tree::Folder,
    // The file being looked at without taking it out of the archive.
    viewing: Option<Viewed>,
    // Set while the box that picks a group by name is up: true to add what
    // matches to the selection, false to take it away.
    picking_group: Option<bool>,
    // The last mask typed, kept so that picking one group and then another
    // does not mean typing it again.
    mask: String,
    // Where the window is and how big, as of this frame. Kept so that `on_exit`
    // has something to write: it is handed no context to ask with.
    geometry: Option<[f32; 4]>,
    // Set while the wheel is being used to walk the list up and down.
    // The last row a left click landed on, and when. What tells a second click
    // on the same row from the first one of a new pair.
    last_click: Option<(usize, f64)>,
    // Names waiting on a yes before they are taken out of the archive. There
    // is no undo, so this one asks.
    confirm_delete: Option<Vec<String>>,
    // An archive dropped onto an open archive, which is two reasonable things
    // at once and so gets asked about rather than guessed at.
    confirm_drop: Option<Vec<PathBuf>>,
    // Set when the cursor moves by keyboard, so the table can scroll it into
    // view on the next frame and then forget about it.
    scroll_to_cursor: bool,
    // The folder the last Ctrl+C or Ctrl+X extracted into. The clipboard is
    // holding paths inside it, so it stays until the next copy replaces it and
    // makes those paths meaningless anyway.
    clip_dir: Option<PathBuf>,
    // What the last Ctrl+X put on the clipboard, so those rows can show it.
    cut_names: HashSet<String>,
    // Made ready before the extraction runs and armed only when it says the
    // clipboard took it, which is what `Message::CutReady` reports.
    cut_armed: Option<Cut>,
    cut_pending: Option<Cut>,
    // Whether the window had the keyboard last frame. Getting it back is when a
    // paste elsewhere has had its chance to happen.
    was_focused: bool,
    show_shortcuts: bool,
    // Set for work that says nothing while it runs. Copying to the clipboard is
    // the only such job: it is over before a bar has finished appearing, and a
    // bar that flashes past says less than nothing.
    quiet: bool,
}

struct AppController {
    state: AppState,
}

struct Arca {
    controller: AppController,
    icons: HashMap<String, Option<egui::TextureHandle>>,
    band: Option<egui::Pos2>,
    wheel: Option<Wheel>,
}

impl AppController {
    fn dispatch(&mut self, action: AppAction) {
        match action {
            AppAction::Open(path) => self.open(path),
            AppAction::Run(job) => self.run_job(job),
            AppAction::Extract { only_checked } => self.ask_extract(only_checked),
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
            AppAction::TogglePasswordVisibility => {
                self.state.show_password = !self.state.show_password
            }
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

    fn begin_password_change(&mut self) {
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
        if self.state.entries.iter().any(|entry| entry.encrypted)
            && self.state.archive_password.is_none()
        {
            self.state.waiting_on_password = Some(Pending::CurrentPassword(Box::new(job)));
        } else {
            self.run_job(job);
        }
    }

    fn sort_by(&mut self, column: SortColumn) {
        if self.state.order.0 == column {
            self.state.order.1 = !self.state.order.1;
        } else {
            self.state.order = (column, true);
        }
    }

    fn start_extract_to(&mut self, only_checked: bool, mut dest: PathBuf) {
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

    fn submit_password(&mut self, password: String) {
        if password.is_empty() {
            return;
        }
        let Some(pending) = self.state.waiting_on_password.take() else {
            return;
        };
        self.state.password_input.clear();
        self.state.show_password = false;
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
            Pending::NewPassword(job) => {
                if let Job::Password {
                    archive, current, ..
                } = *job
                {
                    self.run_job(Job::Password {
                        archive,
                        current,
                        new: Some(password),
                    });
                }
            }
            Pending::CurrentPassword(job) => {
                if let Job::Password { archive, new, .. } = *job {
                    self.state.archive_password = Some(password.clone());
                    self.run_job(Job::Password {
                        archive,
                        current: Some(password),
                        new,
                    });
                }
            }
        }
    }

    fn prepare_compress(&mut self, inputs: Vec<PathBuf>) {
        if inputs.is_empty() {
            return;
        }
        self.state.output_name = quick_output(&inputs, self.state.format)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.state.pending_inputs = inputs;
        self.state.view = View::Add;
    }
    fn select_all_visible(&mut self) {
        for row in self.visible_rows() {
            self.set_checked(&row, true);
        }
    }
    fn invert_visible(&mut self) {
        let rows = self.visible_rows();
        let values: Vec<bool> = rows.iter().map(|r| !self.is_checked(r)).collect();
        for (r, v) in rows.iter().zip(values) {
            self.set_checked(r, v);
        }
    }
    fn request_delete(&mut self) {
        let names = self.selected_names();
        if !names.is_empty() {
            self.state.confirm_delete = Some(names);
        }
    }
    fn confirm_delete(&mut self, yes: bool) {
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
    fn answer_drop(&mut self, choice: DropChoice) {
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

    fn new(settings: Settings) -> Self {
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
                title: String::new(),
                window_title: "Arca".to_string(),
                current_dir: String::new(),
                show_settings: false,
                conflict: None,
                replies: None,
                waiting_on_password: None,
                password_input: String::new(),
                show_password: false,
                add_password: String::new(),
                archive_password: None,
                after_password: None,
                history: vec![String::new()],
                here: 0,
                cursor: None,
                types: HashMap::new(),
                band_base: Vec::new(),
                confirm_delete: None,
                scroll_to_cursor: false,
                clip_dir: None,
                cut_names: HashSet::new(),
                confirm_drop: None,
                band_anchor: None,
                drag_ready: None,
                drag_settling: false,
                band_scroll: None,
                renaming: None,
                rename_fresh: false,
                default_password: None,
                asking_default_password: false,
                asking_folder: false,
                folder_input: String::new(),
                undo: None,
                carrying: None,
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
                mask: String::new(),
                folders: tree::Folder::default(),
                geometry: None,
                last_click: None,
                cut_armed: None,
                cut_pending: None,
                was_focused: true,
                show_shortcuts: false,
                quiet: false,
            },
        }
    }
    fn summary(&self) -> String {
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
    fn go_back(&mut self) {
        if self.can_go_back() {
            self.state.here -= 1;
            self.state.current_dir = self.state.history[self.state.here].clone();
            self.state.filter.clear();
            self.clear_picked();
        }
    }
    fn selected_roots(&self) -> Vec<String> {
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
    fn clear_picked(&mut self) {
        self.state.checked.iter_mut().for_each(|c| *c = false);
        self.state.cursor = None;
        // A row number means something else in the folder now on screen.
        self.state.last_click = None;
    }
    fn go_forward(&mut self) {
        if self.can_go_forward() {
            self.state.here += 1;
            self.state.current_dir = self.state.history[self.state.here].clone();
            self.state.filter.clear();
            self.clear_picked();
        }
    }
    fn s(&self) -> &'static Strings {
        strings(self.state.settings.effective_lang())
    }
    fn selected_names(&self) -> Vec<String> {
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
    fn cancel_password(&mut self) {
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
    fn undo_last(&mut self) {
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

    // Whatever is being kept for an undo is thrown away.
    //
    // Called when the window closes and when the archive is left behind: a file
    // called `something.zip.arca-undo` sitting next to somebody's archive after
    // the program has gone is litter, whatever it was for.
    fn drop_undo(&mut self) {
        if let Some((archive, _)) = self.state.undo.take() {
            let _ = fs::remove_file(undo_path(&archive));
        }
    }

    // Reads the names in the archive again under another code page.
    //
    // Only the person looking at it can know which one an unflagged zip was
    // written in, so it is a choice and not a guess, and the choice is
    // remembered.
    fn reread_names(&mut self, page: arca_zip::pages::Page) {
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
    fn fresh_flags(
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
    fn show_job(&mut self, verb: &str, subject: String, from_here: bool) {
        self.state.title = verb.to_string();
        self.state.subject = subject;
        self.state.overlay = matches!(self.state.view, View::Browse) && from_here;
        if !self.state.overlay {
            self.state.view = View::Running;
            self.state.window_title = if self.state.subject.is_empty() {
                "Arca".to_string()
            } else {
                format!("{} — {}", self.state.title, self.state.subject)
            };
        }
    }

    // Asks, once, whether there is a newer Arca.
    //
    // On a thread and without a word: something nobody asked for must not make
    // the window look busy. If the answer never comes -- no network, no reply,
    // a machine that says no -- nothing happens and nothing is said. There is
    // nothing here worth a complaint.
    fn ask_about_updates(&mut self) {
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
    fn start_update(&mut self) {
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
    fn move_into(&mut self, roots: &[String], target: &str) {
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

    fn extract_here(&mut self) {
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
    fn set_checked(&mut self, row: &Row, value: bool) {
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
    fn dragged_files(&self) -> Vec<(Entry, String)> {
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
    #[cfg(windows)]
    fn go_to(&mut self, path: String) {
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
    fn remember(&mut self, path: &Path) {
        let text = path.to_string_lossy().to_string();
        self.state.settings.recent.retain(|p| *p != text);
        self.state.settings.recent.insert(0, text);
        self.state.settings.recent.truncate(10);
        self.state.settings.save();
    }
    fn spawn<F>(&mut self, total: usize, work: F)
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
    fn can_go_forward(&self) -> bool {
        self.state.here + 1 < self.state.history.len()
    }
    fn codec_name(&self, c: Codec) -> &'static str {
        let s = self.s();
        match c {
            Codec::Store => s.codec_store,
            Codec::Deflate => s.codec_deflate,
            Codec::Zstd => s.codec_zstd,
        }
    }
    fn drag_out(&mut self) {}

    // Ctrl+V: whatever files the shell is holding, into the folder on screen.
    fn ask_extract(&mut self, only_checked: bool) {
        let s: &'static Strings = self.s();
        let Some(archive) = self.state.archive.clone() else {
            return;
        };
        let Some(mut dest) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        if self.state.into_subfolder {
            dest = dest.join(archive_stem(&archive));
        }
        let wanted: Vec<bool> = if only_checked {
            self.state.checked.clone()
        } else {
            vec![true; self.state.entries.len()]
        };
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
            let _ = tx.send(
                match extract(&archive, &dest, &wanted, &notify, &ask, pw.as_deref()) {
                    Ok(bytes) => Message::Done(fill(
                        s.extracted_to,
                        &[
                            ("size", &human(bytes)),
                            ("dest", &dest.display().to_string()),
                        ],
                    )),
                    Err(e) => Message::Failed(e.to_string()),
                },
            );
        });
    }

    // Escape and the Cancel button are the same act, so they go through the
    // same code: two copies of this would drift apart the first time one side
    // grew a step.
    // The names ticked right now, which is what every action that works on a
    // selection needs.
    fn cut_landed(&mut self) {
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
    fn level_name(&self, l: Level) -> &'static str {
        let s = self.s();
        match l {
            Level::Store => s.level_none,
            Level::Fast => s.level_fast,
            Level::Normal => s.level_normal,
            Level::Best => s.level_best,
        }
    }
    fn can_go_back(&self) -> bool {
        self.state.here > 0
    }
    fn add_files(&mut self, inputs: Vec<PathBuf>) {
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
    fn copy_to_clipboard(&mut self, cut: bool) {
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
    fn open_file(&mut self, index: usize) {
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
    fn receive(&mut self) -> bool {
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
                            "{} — Arca",
                            path.file_name()
                                .map(|x| x.to_string_lossy().to_string())
                                .unwrap_or_default()
                        );
                        self.state.archive = Some(path);
                        self.state.history = vec![String::new()];
                        self.state.here = 0;
                        self.state.current_dir = String::new();
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
                        self.state.notice = text;
                        self.state.error = true;
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
            if let Some((path, pw)) = self.state.after_password.take() {
                let notice = std::mem::take(&mut self.state.notice);
                self.open(path);
                self.state.archive_password = pw;
                self.state.notice = notice;
                self.state.view = View::Browse;
            }
        }
        installing
            || (finished_ok && self.state.close_when_done && matches!(self.state.view, View::Running))
    }
    fn paste_from_clipboard(&mut self) {
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
    fn visible_rows(&self) -> Vec<Row> {
        let filter = self.state.filter.trim().to_lowercase();
        // Flat view: every file in the archive at once, wherever it is filed.
        // It is how you find something when you know its name and not its
        // folder, and it is the same list a filter builds, only without one.
        let flat = self.state.settings.flat && filter.is_empty();
        let mut rows = if flat {
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
        } else if filter.is_empty() {
            children_of(&self.state.entries, &self.state.current_dir)
        } else {
            self.state
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.is_dir && e.name.to_lowercase().contains(&filter))
                .map(|(i, e)| Row {
                    label: e.name.replace('\\', "/"),
                    path: e.name.replace('\\', "/"),
                    kind: kind_of(&e.name, false),
                    is_dir: false,
                    entry: Some(i),
                    size: e.size,
                    packed: e.compressed_size,
                    method: e.method.name(),
                    encrypted: e.encrypted,
                    count: 0,
                    mtime: e.mtime,
                    created: e.created,
                    accessed: e.accessed,
                    attributes: e.attributes,
                    crc32: e.crc32,
                    up: false,
                })
                .collect()
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
                SortColumn::Name => x.label.to_lowercase().cmp(&y.label.to_lowercase()),
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
    fn rename_to(&mut self, rows: &[Row], path: &str, name: &str) {
        let Some(row) = rows.iter().find(|r| r.path == path) else {
            return;
        };
        if name == row.label {
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
        let to = match path.rsplit_once('/') {
            Some((parent, _)) => format!("{parent}/{name}"),
            None => name.to_string(),
        };
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
    fn dropped(&mut self, paths: Vec<PathBuf>) {
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
    fn view_entry(&mut self, index: usize) {
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
    fn open(&mut self, path: PathBuf) {
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
    fn run_job(&mut self, job: Job) {
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
        self.state.close_when_done = !matches!(
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
        if let Job::Password { archive, new, .. } = &job {
            self.state.after_password = Some((archive.clone(), new.clone()));
        }
        if let Job::Delete {
            archive, password, ..
        } = &job
        {
            self.state.after_password = Some((archive.clone(), password.clone()));
        }
        if let Job::Add {
            archive, password, ..
        } = &job
        {
            self.state.after_password = Some((archive.clone(), password.clone()));
        }
        if let Job::Rename {
            archive, password, ..
        } = &job
        {
            self.state.after_password = Some((archive.clone(), password.clone()));
        }
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
    fn is_checked(&self, row: &Row) -> bool {
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

impl Arca {
    fn new(controller: AppController) -> Self {
        Arca {
            controller,
            icons: HashMap::new(),
            band: None,
            wheel: None,
        }
    }
    fn tree_panel(&mut self, ctx: &egui::Context) {
        if !self.controller.state.settings.tree || self.controller.state.entries.is_empty() {
            return;
        }
        let mut go: Option<String> = None;
        let folders = self.controller.state.folders.clone();
        let here = self.controller.state.current_dir.clone();
        let s = self.controller.s();
        let mut icons = std::mem::take(&mut self.icons);
        egui::SidePanel::left("tree")
            .resizable(true)
            .default_width(220.0)
            .width_range(140.0..=420.0)
            .show(ctx, |ui| {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // The archive itself, named for what it is rather than
                        // for the file: the path along the top already says
                        // which archive this is, and here it is a place to go
                        // back to.
                        let root = Twig {
                            name: s.archive_root,
                            path: String::new(),
                            depth: 0,
                            kids: false,
                            open: false,
                            archive: true,
                        };
                        if twig(ui, &mut icons, &root, &here).clicked() {
                            go = Some(String::new());
                        }
                        branch(ui, &mut icons, &folders, "", 1, &here, &mut go);
                    });
            });
        self.icons = icons;
        if let Some(path) = go {
            // Going to a folder while the list is showing every file at once is
            // asking for that folder, so the flat view gets out of the way
            // rather than swallowing the click.
            if self.controller.state.settings.flat {
                self.controller.state.settings.flat = false;
                self.controller.state.settings.save();
            }
            self.controller.go_to(path);
        }
    }

    // Reads the names again in a different code page.
    //
    // Nothing is written and the archive is not touched: the bytes of every
    // name were kept as the archive spells them, and this decides again what
    // they mean. Entries the archive marked as UTF-8 are left alone -- there is
    // no question about those and reading them any other way would break the
    // ones that were right.
    //
    // The listing goes back to the root afterwards. The folder you were in was
    // a path made out of those names, and under a different page it is a path
    // that does not exist.
    fn reread_names(&mut self, ctx: &egui::Context, page: arca_zip::pages::Page) {
        self.controller.reread_names(page);
        ctx.request_repaint();
    }

    // A folder made inside the archive, in the one you are looking at.
    //
    // Asked for in a box rather than made as "New folder" and renamed after,
    // because making it is a rewrite of the whole archive and doing that twice
    // for one folder would be silly.
    fn new_folder_window(&mut self, ctx: &egui::Context) {
        if !self.controller.state.asking_folder {
            return;
        }
        let s = self.controller.s();
        let mut go = false;
        let mut cancel = false;
        egui::Window::new(s.new_folder)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(s.folder_name);
                ui.add_space(6.0);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.controller.state.folder_input)
                        .id(egui::Id::new("arca-new-folder"))
                        .desired_width(260.0),
                );
                if !field.has_focus() && !field.lost_focus() {
                    field.request_focus();
                }
                if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    go = true;
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(s.new_folder).clicked() {
                        go = true;
                    }
                    if ui.button(s.cancel).clicked() {
                        cancel = true;
                    }
                });
                ui.add_space(4.0);
            });
        if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.controller.state.asking_folder = false;
            self.controller.state.folder_input.clear();
        }
        if go {
            self.controller.state.asking_folder = false;
            let name = std::mem::take(&mut self.controller.state.folder_input)
                .trim()
                .to_string();
            let Some(archive) = self.controller.state.archive.clone() else {
                return;
            };
            // The same rules a rename lives by: a name is a name and not a
            // path, and nothing here is called that already.
            if name.is_empty() || name.contains('/') || name.contains('\\') {
                self.controller.state.notice = s.bad_name.to_string();
                self.controller.state.error = true;
                return;
            }
            if self
                .controller
                .visible_rows()
                .iter()
                .any(|r| r.label.eq_ignore_ascii_case(&name))
            {
                self.controller.state.notice = fill(s.name_taken, &[("name", &name)]);
                self.controller.state.error = true;
                return;
            }
            self.controller.run_job(Job::NewFolder {
                archive,
                name: format!("{}{name}/", self.controller.state.current_dir),
                password: self.controller.state.archive_password.clone(),
            });
        }
    }

    // A copy of the archive under whatever name is chosen for it.
    //
    // The one thing to do before a change nobody is sure about, and the reason
    // it is here rather than in the file manager is that the archive being
    // looked at is the one that gets copied: no going and finding it again.
    fn save_copy(&mut self, _ctx: &egui::Context) {
        let Some(archive) = self.controller.state.archive.clone() else {
            return;
        };
        let name = archive
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(dest) = rfd::FileDialog::new()
            .set_file_name(&name)
            .set_directory(archive.parent().unwrap_or(Path::new(".")))
            .save_file()
        else {
            return;
        };
        if dest == archive {
            return;
        }
        self.controller.run_job(Job::CopyTo { archive, dest });
    }

    // The password to try on anything that asks for one, so that a folder full
    // of archives locked with the same word is opened once and not fifteen
    // times.
    //
    // In memory and nowhere else. It is never written to the settings file: a
    // password in plain text beside the theme and the column widths is how an
    // encrypted archive stops being encrypted, and a program that offers to
    // remember one for you had better be clear about how long "remember" is.
    fn default_password_window(&mut self, ctx: &egui::Context) {
        if !self.controller.state.asking_default_password {
            return;
        }
        let s = self.controller.s();
        let mut close = false;
        let mut forget = false;
        egui::Window::new(s.default_password)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.controller.state.password_input)
                        .id(egui::Id::new("arca-default-password"))
                        .password(!self.controller.state.show_password)
                        .desired_width(280.0)
                        .hint_text(s.password_hint),
                );
                if !field.has_focus() && !field.lost_focus() {
                    field.request_focus();
                }
                if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    close = true;
                }
                ui.checkbox(&mut self.controller.state.show_password, s.show_password);
                ui.add_space(4.0);
                ui.label(egui::RichText::new(s.password_kept).weak().small());
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(s.start).clicked() {
                        close = true;
                    }
                    if ui
                        .add_enabled(
                            self.controller.state.default_password.is_some(),
                            egui::Button::new(s.remove_password),
                        )
                        .clicked()
                    {
                        forget = true;
                    }
                    if ui.button(s.cancel).clicked() {
                        self.controller.state.asking_default_password = false;
                        self.controller.state.password_input.clear();
                    }
                });
                ui.add_space(4.0);
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.controller.state.asking_default_password = false;
            self.controller.state.password_input.clear();
        }
        if forget {
            self.controller.state.default_password = None;
            self.controller.state.asking_default_password = false;
            self.controller.state.password_input.clear();
            self.controller.state.notice = s.password_forgotten.to_string();
            self.controller.state.error = false;
        }
        if close {
            let given = std::mem::take(&mut self.controller.state.password_input);
            self.controller.state.default_password = (!given.is_empty()).then_some(given);
            self.controller.state.asking_default_password = false;
        }
    }

    // Puts the archive back the way it was before the last change.
    //
    // A swap of two names, because the version before the change was moved
    // aside rather than thrown away. There is one step and no more: taking it
    // back leaves nothing to take back, and the sidecar goes with it.
    fn undo_last(&mut self, _ctx: &egui::Context) {
        self.controller.undo_last();
    }

    // Whatever is being kept for an undo is thrown away.
    //
    // Called when the window closes and when the archive is left behind: a file
    // called `something.zip.arca-undo` sitting next to somebody's archive after
    // the program has gone is litter, whatever it was for.
    fn drop_undo(&mut self) {
        self.controller.drop_undo();
    }

    // Puts an archive at the top of the recent list.
    //
    // Ten of them, which is about as many as anybody scans before giving up and
    // going to the folder instead, and by path rather than by name so that two
    // archives called backup.zip in different places stay two.
    fn settings_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let s = self.controller.s();
        let mut changed = false;

        ui.label(s.language);
        let current = self
            .controller
            .state
            .settings
            .lang
            .map(|l| l.label())
            .unwrap_or(s.theme_system);
        egui::ComboBox::from_id_salt("lang")
            .selected_text(current)
            .width(110.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(
                        self.controller.state.settings.lang.is_none(),
                        s.theme_system,
                    )
                    .clicked()
                {
                    self.controller.state.settings.lang = None;
                    changed = true;
                }
                for l in Lang::ALL {
                    if ui
                        .selectable_label(self.controller.state.settings.lang == Some(l), l.label())
                        .clicked()
                    {
                        self.controller.state.settings.lang = Some(l);
                        changed = true;
                    }
                }
            });

        ui.add_space(10.0);
        ui.label(s.theme);
        let theme_label = match self.controller.state.settings.theme {
            ThemePreference::System => s.theme_system,
            ThemePreference::Light => s.theme_light,
            ThemePreference::Dark => s.theme_dark,
        };
        egui::ComboBox::from_id_salt("theme")
            .selected_text(theme_label)
            .width(100.0)
            .show_ui(ui, |ui| {
                for (t, label) in [
                    (ThemePreference::System, s.theme_system),
                    (ThemePreference::Light, s.theme_light),
                    (ThemePreference::Dark, s.theme_dark),
                ] {
                    if ui
                        .selectable_label(self.controller.state.settings.theme == t, label)
                        .clicked()
                    {
                        self.controller.state.settings.theme = t;
                        ctx.set_theme(t);
                        changed = true;
                    }
                }
            });

        if changed {
            self.controller.state.settings.save();
        }
    }
    fn format_row(&mut self, ui: &mut egui::Ui) {
        let s = self.controller.s();
        let codecs: Vec<(Codec, &'static str)> = [Codec::Store, Codec::Deflate, Codec::Zstd]
            .into_iter()
            .map(|c| (c, self.controller.codec_name(c)))
            .collect();
        let levels: Vec<(Level, &'static str)> =
            [Level::Store, Level::Fast, Level::Normal, Level::Best]
                .into_iter()
                .map(|l| (l, self.controller.level_name(l)))
                .collect();
        let current_codec = self.controller.codec_name(self.controller.state.codec);
        let current_level = self.controller.level_name(self.controller.state.level);
        let is_zip = self.controller.state.format == Format::Zip;

        ui.horizontal(|ui| {
            ui.label(s.format);
            egui::ComboBox::from_id_salt("fmt")
                .selected_text(self.controller.state.format.label())
                .width(90.0)
                .show_ui(ui, |ui| {
                    for f in [Format::Zip, Format::Tar, Format::TarGz] {
                        ui.selectable_value(&mut self.controller.state.format, f, f.label());
                    }
                });
            ui.add_space(8.0);
            ui.label(s.compressor);
            ui.add_enabled_ui(is_zip, |ui| {
                egui::ComboBox::from_id_salt("cdc")
                    .selected_text(current_codec)
                    .width(130.0)
                    .show_ui(ui, |ui| {
                        for (c, label) in &codecs {
                            ui.selectable_value(&mut self.controller.state.codec, *c, *label);
                        }
                    });
            });
            ui.add_space(8.0);
            ui.label(s.level);
            egui::ComboBox::from_id_salt("lvl")
                .selected_text(current_level)
                .width(100.0)
                .show_ui(ui, |ui| {
                    for (l, label) in &levels {
                        ui.selectable_value(&mut self.controller.state.level, *l, *label);
                    }
                });
        });
    }

    // Everything out, beside the archive, without a word. What Alt+W does, and
    // the row menu offers the same thing where the hand already is.
    fn drop_hint(&self, ctx: &egui::Context) {
        if !matches!(self.controller.state.view, View::Browse)
            || self.controller.state.busy
            || ctx.input(|i| i.raw.hovered_files.is_empty())
        {
            return;
        }
        let s = self.controller.s();
        let text = match &self.controller.state.archive {
            Some(a) if detect(a) == Some(Format::Zip) => fill(
                s.drop_to_add,
                &[(
                    "name",
                    &a.file_name()
                        .map(|x| x.to_string_lossy().to_string())
                        .unwrap_or_default(),
                )],
            ),
            _ => s.drop_here.to_string(),
        };
        let screen = ctx.screen_rect();
        let p = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop_hint"),
        ));
        p.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(170));
        p.text(
            screen.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(17.0),
            egui::Color32::WHITE,
        );
    }
    fn confirm_drop_window(&mut self, ctx: &egui::Context) {
        let Some(paths) = self.controller.state.confirm_drop.clone() else {
            return;
        };
        let s = self.controller.s();
        let into = self
            .controller
            .state
            .archive
            .as_ref()
            .and_then(|a| a.file_name())
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut open_it = false;
        let mut add_it = false;
        let mut cancel = false;
        egui::Window::new(s.drop_title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(fill(s.drop_question, &[("name", &into)]));
                ui.add_space(6.0);
                ui.weak(s.dropped_word);
                for p in paths.iter().take(8) {
                    ui.weak(format!(
                        "  {}",
                        p.file_name()
                            .map(|x| x.to_string_lossy().to_string())
                            .unwrap_or_default()
                    ));
                }
                if paths.len() > 8 {
                    ui.weak(format!("  … {}", paths.len() - 8));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    // Opening first: it is what the window used to do, so it is
                    // the answer somebody pressing Enter out of habit expects.
                    if ui.button(s.open_word).clicked() {
                        open_it = true;
                    }
                    if ui.button(s.add_to_archive).clicked() {
                        add_it = true;
                    }
                    if ui.button(s.cancel).clicked() {
                        cancel = true;
                    }
                });
                ui.add_space(4.0);
            });
        if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.controller.state.confirm_drop = None;
        }
        if open_it {
            self.controller.state.confirm_drop = None;
            if let Some(p) = paths.into_iter().next() {
                self.controller.open(p);
            }
        } else if add_it {
            self.controller.state.confirm_drop = None;
            self.controller.add_files(paths);
        }
    }

    // Windows shortcuts that mean something here. The ones that would need the
    // archive to grow a feature it has not got are left out rather than made to
    // look present and do nothing.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if !matches!(self.controller.state.view, View::Browse)
            || self.controller.state.busy
            || self.controller.state.confirm_delete.is_some()
            || self.controller.state.confirm_drop.is_some()
        {
            return;
        }
        let typing = ctx.memory(|m| m.focused().is_some());
        // Cut, copy and paste never arrive as key presses. egui's winit layer
        // recognises those three shortcuts itself and turns them into events of
        // their own, returning before the key is passed on, so watching for
        // Ctrl+X was watching for something that is never sent: those three did
        // nothing from the keyboard and only worked from the row menu.
        //
        // Cut and copy come back as events. Paste does not: that one is only
        // sent when the clipboard has text in it, and a clipboard holding files
        // has none, which is exactly the case here. What does still arrive is
        // the key going back up, because the early return only covers the press
        // -- so that is what a paste is recognised by.
        let (
            ctrl,
            shift,
            o,
            e,
            t,
            n,
            f,
            f5,
            del,
            cut,
            copy,
            paste,
            plus,
            minus,
            alt_w,
            undo,
            ctrl_p,
        ) = ctx.input(|i| {
            (
                i.modifiers.command,
                i.modifiers.shift,
                i.key_pressed(egui::Key::O),
                i.key_pressed(egui::Key::E),
                i.key_pressed(egui::Key::T),
                i.key_pressed(egui::Key::N),
                i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::F5),
                i.key_pressed(egui::Key::Delete),
                i.events.iter().any(|e| matches!(e, egui::Event::Cut)),
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                i.events.iter().any(|e| {
                    matches!(
                        e,
                        egui::Event::Key {
                            key: egui::Key::V,
                            pressed: false,
                            modifiers,
                            ..
                        } if modifiers.command
                    )
                }),
                i.key_pressed(egui::Key::Plus),
                i.key_pressed(egui::Key::Minus),
                i.modifiers.alt && i.key_pressed(egui::Key::W),
                i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                i.modifiers.command && i.key_pressed(egui::Key::P),
            )
        });

        // These three carry their own modifier, so they do not wait behind the
        // Ctrl check below. They do belong to the filter box while it has the
        // keyboard: that is a text field and they mean something there.
        if !typing {
            if copy {
                // Ctrl+Shift+C is the same event with shift held, which is
                // where the names as text went; it is also all a platform
                // without a file clipboard can offer.
                if shift || !clipboard::AVAILABLE {
                    let names = self.controller.selected_names();
                    if !names.is_empty() {
                        ctx.copy_text(names.join("\r\n"));
                    }
                } else {
                    self.controller.copy_to_clipboard(false);
                }
            }
            if cut && clipboard::AVAILABLE {
                self.controller.copy_to_clipboard(true);
            }
            if paste && clipboard::AVAILABLE && self.controller.state.archive.is_some() {
                self.controller.paste_from_clipboard();
            }
        }

        if f5 && !typing {
            if let Some(p) = self.controller.state.archive.clone() {
                let keep = self.controller.state.archive_password.clone();
                self.controller.open(p);
                self.controller.state.archive_password = keep;
            }
        }
        // The keypad's plus and minus, which is where WinRAR has kept picking a
        // group by name since before there were menus to put it in. Its third
        // one, the keypad star for inverting, cannot be told from any other
        // asterisk by the toolkit, so that one stays on Ctrl+I alone.
        if (plus || minus) && !typing && self.controller.state.archive.is_some() {
            self.controller.state.picking_group = Some(plus);
        }
        // Everything out, beside the archive, without asking where. The whole
        // point of it is that it is one keystroke: the folder the archive is in
        // is where an extraction goes nine times out of ten.
        if ctrl_p && !typing {
            self.controller.state.password_input.clear();
            self.controller.state.asking_default_password = true;
        }
        // One step back from the last change to the archive, which is the step
        // anybody wants: the one they just took by mistake.
        if undo && !typing && self.controller.state.undo.is_some() {
            self.undo_last(ctx);
        }
        if alt_w && !typing {
            if let Some(archive) = self.controller.state.archive.clone() {
                self.controller.run_job(Job::Extract {
                    archives: vec![archive],
                    dest: if self.controller.state.into_subfolder {
                        Destination::Subfolder
                    } else {
                        Destination::Beside
                    },
                    password: self.controller.state.archive_password.clone(),
                });
            }
        }
        if del && !typing && self.controller.state.archive.is_some() {
            let names = self.controller.selected_names();
            if !names.is_empty() {
                self.controller.state.confirm_delete = Some(names);
            }
        }
        if !ctrl {
            return;
        }
        if o {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("Archives", &["zip", "tar", "gz", "tgz"])
                .pick_file()
            {
                self.controller.open(p);
            }
        }
        if e && self.controller.state.archive.is_some() {
            self.controller.ask_extract(false);
        }
        // Verifying an archive is a job this program has had all along, reached
        // from the shell menu and from the command line, and from inside the
        // window there was no way to ask for it at all.
        if t {
            if let Some(archive) = self.controller.state.archive.clone() {
                self.controller.run_job(Job::Test {
                    archive,
                    only: None,
                });
            }
        }
        if n {
            if let Some(files) = rfd::FileDialog::new().pick_files() {
                if !files.is_empty() {
                    self.controller.state.output_name =
                        quick_output(&files, self.controller.state.format)
                            .file_name()
                            .map(|x| x.to_string_lossy().to_string())
                            .unwrap_or_default();
                    self.controller.state.pending_inputs = files;
                    self.controller.state.view = View::Add;
                }
            }
        }
        if f {
            // Nothing else here is a text box, so handing the keyboard to the
            // filter is the whole of "find".
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("filter")));
        }
    }

    // Opens an entry for looking at, without taking it out of the archive.
    //
    // Read straight into memory here rather than on a thread. Everything this
    // window does on a thread it does because it might take minutes; this is
    // capped at a size that comes back in the time between two frames, and a
    // progress window that flashes past is worse than a pause nobody notices.
    fn viewer_window(&mut self, ctx: &egui::Context) {
        let Some(view) = &mut self.controller.state.viewing else {
            return;
        };
        let s = i18n::strings(
            self.controller
                .state
                .settings
                .lang
                .unwrap_or_else(Lang::from_system),
        );
        let mut open = true;
        egui::Window::new(&view.name)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([760.0, 520.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut view.look, Look::Text, s.as_text);
                    ui.selectable_value(&mut view.look, Look::Hex, s.as_hex);
                    // Only where there is a picture to show. A tab that says
                    // "picture" over a text file is a tab that lies.
                    if view.picture {
                        ui.selectable_value(&mut view.look, Look::Picture, s.as_picture);
                    }
                    ui.separator();
                    ui.weak(human(view.bytes.len() as u64));
                });
                ui.separator();
                match view.look {
                    Look::Picture => {
                        egui::ScrollArea::both().show(ui, |ui| {
                            ui.add(
                                egui::Image::from_bytes(
                                    format!("bytes://{}", view.name),
                                    egui::load::Bytes::Shared(view.bytes.clone()),
                                )
                                .fit_to_original_size(1.0),
                            );
                        });
                    }
                    Look::Text => {
                        let font = egui::TextStyle::Monospace.resolve(ui.style());
                        let tall = ui.text_style_height(&egui::TextStyle::Monospace);
                        egui::ScrollArea::both().show_rows(
                            ui,
                            tall,
                            view.lines.len(),
                            |ui, range| {
                                for line in &view.lines[range] {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(line).font(font.clone()),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Extend),
                                    );
                                }
                            },
                        );
                    }
                    Look::Hex => {
                        let font = egui::TextStyle::Monospace.resolve(ui.style());
                        let tall = ui.text_style_height(&egui::TextStyle::Monospace);
                        let rows = view.bytes.len().div_ceil(16);
                        egui::ScrollArea::both().show_rows(ui, tall, rows, |ui, range| {
                            for row in range {
                                let at = row * 16;
                                let end = (at + 16).min(view.bytes.len());
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(hex_line(at, &view.bytes[at..end]))
                                            .font(font.clone()),
                                    )
                                    .wrap_mode(egui::TextWrapMode::Extend),
                                );
                            }
                        });
                    }
                }
            });
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.controller.state.viewing = None;
        }
    }

    // Picking a whole group of files by what they are called: `*.txt`, `nota_?`.
    //
    // The two keys WinRAR has always had, on the numeric keypad, and the same
    // box behind both: one adds what matches to what is picked and the other
    // takes it away. It works on what is on screen, so inside a folder it is
    // that folder and in the flat view it is the whole archive, which is what
    // "what is on screen" means either way.
    fn group_window(&mut self, ctx: &egui::Context) {
        let Some(adding) = self.controller.state.picking_group else {
            return;
        };
        let s = self.controller.s();
        let mut go = false;
        let mut cancel = false;
        egui::Window::new(if adding {
            s.select_group
        } else {
            s.deselect_group
        })
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.add_space(6.0);
            ui.label(s.mask_hint);
            ui.add_space(6.0);
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.controller.state.mask)
                    .id(egui::Id::new("arca-mask"))
                    .desired_width(260.0),
            );
            // The box has the keyboard the moment it opens: this is a thing
            // you are typing into, not a thing you are looking at.
            if !field.has_focus() && !field.lost_focus() {
                field.request_focus();
            }
            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .button(if adding {
                        s.select_group
                    } else {
                        s.deselect_group
                    })
                    .clicked()
                {
                    go = true;
                }
                if ui.button(s.cancel).clicked() {
                    cancel = true;
                }
            });
            ui.add_space(4.0);
        });
        if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.controller.state.picking_group = None;
        }
        if go {
            self.controller.state.picking_group = None;
            let mask = self.controller.state.mask.trim().to_string();
            if mask.is_empty() {
                return;
            }
            for row in self.controller.visible_rows() {
                if matches_mask(&mask, &row.label) {
                    self.controller.set_checked(&row, adding);
                }
            }
        }
    }
    fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some(names) = self.controller.state.confirm_delete.clone() else {
            return;
        };
        let s = self.controller.s();
        let mut go = false;
        let mut cancel = false;
        egui::Window::new(s.delete_word)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(fill(s.confirm_delete, &[("n", &names.len().to_string())]));
                ui.add_space(6.0);
                // Enough of them to see what is about to go, not so many that
                // the window becomes the list itself.
                for n in names.iter().take(8) {
                    ui.weak(format!("  {n}"));
                }
                if names.len() > 8 {
                    ui.weak(format!("  … {}", names.len() - 8));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(s.delete_word).clicked() {
                        go = true;
                    }
                    if ui.button(s.cancel).clicked() {
                        cancel = true;
                    }
                });
                ui.add_space(4.0);
            });
        if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.controller.state.confirm_delete = None;
        }
        if go {
            self.controller.state.confirm_delete = None;
            if let Some(archive) = self.controller.state.archive.clone() {
                self.controller.run_job(Job::Delete {
                    archive,
                    names,
                    password: self.controller.state.archive_password.clone(),
                });
            }
        }
    }
    fn password_window(&mut self, ctx: &egui::Context) {
        if self.controller.state.waiting_on_password.is_none() {
            return;
        }
        let s = self.controller.s();
        let setting = matches!(
            self.controller.state.waiting_on_password,
            Some(Pending::NewPassword(_))
        );
        let mut go = false;
        let mut cancel = false;
        egui::Window::new(if setting {
            s.set_password
        } else {
            s.password_needed
        })
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.add_space(6.0);
            ui.label(if setting {
                s.new_password
            } else {
                s.password_hint
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                // Asked before the field is built. The text edit swallows
                // Enter, and asking it afterwards never sees the key, so
                // the window could only be dismissed with the mouse.
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.controller.state.password_input)
                        .password(!self.controller.state.show_password)
                        .desired_width(240.0),
                );
                field.request_focus();
                if enter {
                    go = true;
                }
                ui.checkbox(&mut self.controller.state.show_password, s.show_password);
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(s.start).clicked() {
                    go = true;
                }
                if ui.button(s.cancel).clicked() {
                    cancel = true;
                }
            });
            ui.add_space(4.0);
        });

        if cancel {
            self.controller.cancel_password();
            return;
        }
        if go && !self.controller.state.password_input.is_empty() {
            let given = std::mem::take(&mut self.controller.state.password_input);
            match self.controller.state.waiting_on_password.take() {
                Some(Pending::Extract(job)) => {
                    if let Job::Extract { archives, dest, .. } = *job {
                        self.controller.run_job(Job::Extract {
                            archives,
                            dest,
                            password: Some(given),
                        });
                    }
                }
                Some(Pending::CurrentPassword(job)) => {
                    if let Job::Password { archive, new, .. } = *job {
                        self.controller.state.archive_password = Some(given.clone());
                        self.controller.run_job(Job::Password {
                            archive,
                            current: Some(given),
                            new,
                        });
                    }
                }
                Some(Pending::OpenArchive) => self.controller.state.archive_password = Some(given),
                Some(Pending::NewPassword(job)) => {
                    if let Job::Password {
                        archive, current, ..
                    } = *job
                    {
                        self.controller.run_job(Job::Password {
                            archive,
                            current,
                            new: Some(given),
                        });
                    }
                }
                None => {}
            }
        }
    }
    fn conflict_window(&mut self, ctx: &egui::Context) {
        let Some(path) = self.controller.state.conflict.clone() else {
            return;
        };
        let s = self.controller.s();
        let mut chosen: Option<Answer> = None;
        egui::Window::new(s.conflict_title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(s.already_there);
                ui.add_space(2.0);
                ui.label(egui::RichText::new(&path).monospace().strong());
                ui.add_space(6.0);
                ui.label(s.conflict_text);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(s.yes).clicked() {
                        chosen = Some(Answer::Replace);
                    }
                    if ui.button(s.yes_all).clicked() {
                        chosen = Some(Answer::ReplaceAll);
                    }
                    if ui.button(s.no).clicked() {
                        chosen = Some(Answer::Skip);
                    }
                    if ui.button(s.no_all).clicked() {
                        chosen = Some(Answer::SkipAll);
                    }
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(s.rename).clicked() {
                        chosen = Some(Answer::Rename);
                    }
                    if ui.button(s.rename_all).clicked() {
                        chosen = Some(Answer::RenameAll);
                    }
                    ui.separator();
                    if ui.button(s.cancel).clicked() {
                        chosen = Some(Answer::Cancel);
                    }
                });
                ui.add_space(4.0);
            });
        if let Some(a) = chosen {
            if let Some(tx) = &self.controller.state.replies {
                let _ = tx.send(a);
            }
            self.controller.state.conflict = None;
        }
    }

    // Two rows: what can be done, and where you are. It used to be four, with
    // the archive's name on one of its own and the two ticking buttons on
    // another, which is a lot of furniture above a list. The name of the file
    // moved to the title bar, where the name of the open document goes in every
    // other program, and the ticking buttons in beside the counts they act on.
    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let s = self.controller.s();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            // The password window is not modal on its own, so the toolbar behind
            // it has to be shut off: a click there would run with a password
            // that has not been given yet.
            let idle =
                !self.controller.state.busy && self.controller.state.waiting_on_password.is_none();
            let has = self.controller.state.archive.is_some() && idle;
            ui.add_enabled_ui(idle, |ui| {
                if tool_button(ui, glyphs::Glyph::Open, s.open, true, "Ctrl+O").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Archives", &["zip", "tar", "gz", "tgz"])
                        .pick_file()
                    {
                        self.controller.open(p);
                    }
                }
                if tool_button(ui, glyphs::Glyph::Compress, s.compress, true, "Ctrl+N").clicked() {
                    if let Some(files) = rfd::FileDialog::new().pick_files() {
                        if !files.is_empty() {
                            self.controller.state.output_name =
                                quick_output(&files, self.controller.state.format)
                                    .file_name()
                                    .map(|x| x.to_string_lossy().to_string())
                                    .unwrap_or_default();
                            self.controller.state.pending_inputs = files;
                            self.controller.state.view = View::Add;
                        }
                    }
                }
            });
            ui.separator();
            if tool_button(ui, glyphs::Glyph::ExtractAll, s.extract_all, has, "Ctrl+E").clicked() {
                self.controller.ask_extract(false);
            }
            let n = self.controller.state.checked.iter().filter(|b| **b).count();
            if tool_button(
                ui,
                glyphs::Glyph::ExtractPicked,
                s.extract_selected,
                has && n > 0,
                "",
            )
            .clicked()
            {
                self.controller.ask_extract(true);
            }
            ui.separator();
            // Only a .zip has anywhere to keep a password.
            let zip = has && self.controller.state.format == Format::Zip;
            let encrypted = self.controller.state.entries.iter().any(|e| e.encrypted);
            let glyph = if encrypted {
                glyphs::Glyph::Unlocked
            } else {
                glyphs::Glyph::Locked
            };
            let tip = if encrypted {
                s.remove_password
            } else {
                s.set_password
            };
            if tool_button(ui, glyph, s.password_word, zip, tip).clicked() {
                let archive = self.controller.state.archive.clone().unwrap_or_default();
                if encrypted {
                    let job = Job::Password {
                        archive,
                        current: self.controller.state.archive_password.clone(),
                        new: None,
                    };
                    // Cancelling the question when the archive opened leaves us
                    // without it, so ask again instead of failing halfway
                    // through the rewrite.
                    match self.controller.state.archive_password {
                        Some(_) => self.controller.run_job(job),
                        None => {
                            self.controller.state.password_input.clear();
                            self.controller.state.waiting_on_password =
                                Some(Pending::CurrentPassword(Box::new(job)));
                        }
                    }
                } else {
                    self.controller.state.password_input.clear();
                    self.controller.state.waiting_on_password =
                        Some(Pending::NewPassword(Box::new(Job::Password {
                            archive,
                            current: None,
                            new: None,
                        })));
                }
            }
            // Everything that did not fit, behind one button. That is how a
            // command bar stays coherent: every button on it says what it does,
            // and the ones there is no room to name go here rather than
            // becoming a row of unexplained pictures.
            let mut wants = None;
            let more = tool_button(ui, glyphs::Glyph::More, s.more_word, idle, "");
            // A dot on the button when there is a newer Arca. The word for it
            // is inside the menu, and a notice inside a menu is a notice nobody
            // reads: something has to say from the outside that there is
            // anything in there worth opening. Small and in the corner, because
            // it is news and not a problem.
            if self.controller.state.update.is_some() {
                let at = more.rect.right_top() + egui::vec2(-5.0, 5.0);
                ui.painter()
                    .circle_filled(at, 3.5, theme::cursor(ui.visuals()).color);
            }
            let more_id = egui::Id::new("arca-more-menu");
            if more.clicked() {
                ui.memory_mut(|m| m.toggle_popup(more_id));
            }
            egui::popup_below_widget(
                ui,
                more_id,
                &more,
                egui::PopupCloseBehavior::CloseOnClick,
                |ui| {
                    ui.set_min_width(215.0);
                    // Only when there is one, and at the top, where something
                    // that was not there yesterday belongs.
                    if let Some(release) = self.controller.state.update.clone() {
                        if ui
                            .button(fill(s.update_ready, &[("version", &release.tag)]))
                            .clicked()
                        {
                            wants = Some(More::Release);
                        }
                        ui.separator();
                    }
                    if ui
                        .add_enabled(has, egui::Button::new(format!("{}\tCtrl+T", s.test_word)))
                        .clicked()
                    {
                        wants = Some(More::Test);
                    }
                    if ui
                        .add_enabled(
                            has && self.controller.state.format == Format::Zip,
                            egui::Button::new(s.new_folder),
                        )
                        .clicked()
                    {
                        wants = Some(More::NewFolder);
                    }
                    if ui
                        .add_enabled(has, egui::Button::new(s.save_copy))
                        .clicked()
                    {
                        wants = Some(More::SaveCopy);
                    }
                    if ui
                        .button(format!("{}	Ctrl+P", s.default_password))
                        .clicked()
                    {
                        wants = Some(More::DefaultPassword);
                    }
                    ui.separator();
                    // Named after what it would take back, because "undo" on its
                    // own asks the reader to remember what they did last.
                    let back = self
                        .controller
                        .state
                        .undo
                        .as_ref()
                        .map(|(_, what)| format!("{}: {}	Ctrl+Z", s.undo_word, what))
                        .unwrap_or_else(|| format!("{}	Ctrl+Z", s.undo_word));
                    if ui
                        .add_enabled(
                            self.controller.state.undo.is_some(),
                            egui::Button::new(back),
                        )
                        .clicked()
                    {
                        wants = Some(More::Undo);
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            has,
                            egui::Button::new(s.flat_view)
                                .selected(self.controller.state.settings.flat),
                        )
                        .clicked()
                    {
                        wants = Some(More::Flat);
                    }
                    if ui
                        .add_enabled(
                            has,
                            egui::Button::new(s.folder_tree)
                                .selected(self.controller.state.settings.tree),
                        )
                        .clicked()
                    {
                        wants = Some(More::Tree);
                    }
                    // Only where there is an archive whose names could be read
                    // another way. A tar has none of this argument.
                    ui.add_enabled_ui(has && self.controller.state.format == Format::Zip, |ui| {
                        ui.menu_button(s.name_encoding, |ui| {
                            for (page, _, label) in arca_zip::pages::Page::ALL {
                                let on = self.controller.state.settings.page == page;
                                if ui.selectable_label(on, label).clicked() {
                                    wants = Some(More::Page(page));
                                    ui.close_menu();
                                }
                            }
                        });
                    });
                    // The archives opened lately. By name, with the whole path
                    // on hover: a menu of paths is a menu nobody reads.
                    ui.add_enabled_ui(!self.controller.state.settings.recent.is_empty(), |ui| {
                        ui.menu_button(s.recent_word, |ui| {
                            for path in self.controller.state.settings.recent.clone() {
                                let p = PathBuf::from(&path);
                                let leaf = p
                                    .file_name()
                                    .map(|x| x.to_string_lossy().to_string())
                                    .unwrap_or_else(|| path.clone());
                                if ui.button(leaf).on_hover_text(&path).clicked() {
                                    wants = Some(More::Open(p));
                                    ui.close_menu();
                                }
                            }
                            ui.separator();
                            if ui.button(s.clear_history).clicked() {
                                wants = Some(More::Forget);
                                ui.close_menu();
                            }
                        });
                    });
                    ui.separator();
                    if ui.button(format!("{}\tCtrl+A", s.select_all)).clicked() {
                        wants = Some(More::All);
                    }
                    if ui
                        .button(format!("{}\tCtrl+I", s.invert_selection))
                        .clicked()
                    {
                        wants = Some(More::Invert);
                    }
                    if ui.button(format!("{}\tEsc", s.clear_selection)).clicked() {
                        wants = Some(More::None_);
                    }
                    ui.separator();
                    if ui.button(s.settings).clicked() {
                        wants = Some(More::Settings);
                    }
                    if ui.button(format!("{}\tF1", s.shortcuts_title)).clicked() {
                        wants = Some(More::Shortcuts);
                    }
                },
            );
            match wants {
                Some(More::Test) => {
                    if let Some(archive) = self.controller.state.archive.clone() {
                        self.controller.run_job(Job::Test {
                            archive,
                            only: None,
                        });
                    }
                }
                Some(More::All) => {
                    let rows = self.controller.visible_rows();
                    for r in &rows {
                        self.controller.set_checked(r, true);
                    }
                }
                Some(More::Invert) => {
                    let rows = self.controller.visible_rows();
                    let flipped: Vec<bool> = rows
                        .iter()
                        .map(|r| !self.controller.is_checked(r))
                        .collect();
                    for (r, on) in rows.iter().zip(flipped) {
                        self.controller.set_checked(r, on);
                    }
                }
                Some(More::Flat) => {
                    self.controller.state.settings.flat = !self.controller.state.settings.flat;
                    // A flat list is a list of names with no folder over them,
                    // so the folder each one came from has to go somewhere. It
                    // is left on afterwards: turning the view off and on again
                    // should not keep undoing a column the user has since
                    // arranged.
                    if self.controller.state.settings.flat
                        && !self.controller.state.settings.columns.on(SortColumn::Path)
                    {
                        self.controller
                            .state
                            .settings
                            .columns
                            .set(SortColumn::Path, true);
                    }
                    self.controller.clear_picked();
                    self.controller.state.cursor = None;
                    self.controller.state.settings.save();
                }
                Some(More::Undo) => self.undo_last(ctx),
                Some(More::NewFolder) => {
                    self.controller.state.folder_input.clear();
                    self.controller.state.asking_folder = true;
                }
                Some(More::SaveCopy) => self.save_copy(ctx),
                Some(More::DefaultPassword) => {
                    self.controller.state.password_input.clear();
                    self.controller.state.asking_default_password = true;
                }
                Some(More::Page(p)) => self.reread_names(ctx, p),
                Some(More::Tree) => {
                    self.controller.state.settings.tree = !self.controller.state.settings.tree;
                    self.controller.state.settings.save();
                }
                Some(More::Open(path)) => self.controller.open(path),
                Some(More::Forget) => {
                    self.controller.state.settings.recent.clear();
                    self.controller.state.settings.save();
                }
                Some(More::None_) => self.controller.clear_picked(),
                Some(More::Release) => self.controller.start_update(),
                Some(More::Settings) => self.controller.state.show_settings = true,
                Some(More::Shortcuts) => self.controller.state.show_shortcuts = true,
                None => {}
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // As tall as the buttons beside it, worked out the same way
                // they work theirs out, and as wide as whatever is left between
                // them and the edge. A thin box floating in a gap looked like
                // something that had not finished loading.
                let tall = (glyphs::SIZE + ui.spacing().button_padding.y * 2.0)
                    .max(ui.spacing().interact_size.y);
                let wide = ui.available_width();
                ui.add_sized(
                    egui::vec2(wide, tall),
                    egui::TextEdit::singleline(&mut self.controller.state.filter)
                        .id(egui::Id::new("filter"))
                        .vertical_align(egui::Align::Center)
                        .hint_text(s.filter_hint),
                );
            });
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let at_root = self.controller.state.current_dir.is_empty();
            if tool_button(
                ui,
                glyphs::Glyph::Back,
                "",
                self.controller.can_go_back(),
                s.back,
            )
            .clicked()
            {
                self.controller.go_back();
            }
            if tool_button(
                ui,
                glyphs::Glyph::Forward,
                "",
                self.controller.can_go_forward(),
                s.forward,
            )
            .clicked()
            {
                self.controller.go_forward();
            }
            if tool_button(ui, glyphs::Glyph::Up, "", !at_root, s.up).clicked() {
                let parent = parent_of(&self.controller.state.current_dir);
                self.controller.go_to(parent);
            }
            ui.add_space(4.0);
            // The count is measured and set aside before the path is drawn.
            // Laid out the other way round, a deep path took the whole row and
            // ran out over the top of it.
            let tally = self.controller.state.archive.is_some().then(|| {
                let n = self.controller.state.checked.iter().filter(|b| **b).count();
                // The way out of the folder is not one of the things in it.
                let shown = self
                    .controller
                    .visible_rows()
                    .iter()
                    .filter(|r| !r.up)
                    .count();
                let all = format!(
                    "{shown} {} {} · {n} {}",
                    s.visible_of,
                    self.controller.state.entries.len(),
                    s.checked
                );
                if n == 0 {
                    return all;
                }
                // What is picked, weighed. WinRAR keeps this in the corner of
                // its status bar and it is the answer to the question anybody
                // is asking before they extract something: how much is this.
                let bytes: u64 = self
                    .controller
                    .state
                    .entries
                    .iter()
                    .zip(&self.controller.state.checked)
                    .filter(|(_, &on)| on)
                    .map(|(e, _)| e.size)
                    .sum();
                format!("{all} · {}", human(bytes))
            });
            let keep = tally.as_ref().map_or(0.0, |t| {
                ui.painter()
                    .layout_no_wrap(
                        t.clone(),
                        egui::TextStyle::Body.resolve(ui.style()),
                        egui::Color32::PLACEHOLDER,
                    )
                    .size()
                    .x
                    + 16.0
            });
            let budget = (ui.available_width() - keep).max(140.0);
            self.breadcrumb(ui, budget);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(t) = tally {
                    ui.label(egui::RichText::new(t).weak());
                }
            });
        });
        ui.add_space(6.0);
    }

    // Where you are, as the folders you walked through rather than as one line
    // of text with slashes in it. Each one takes you back to that level, which
    // is three clicks the Up arrow used to be needed for.
    fn breadcrumb(&mut self, ui: &mut egui::Ui, budget: f32) {
        if self.controller.state.archive.is_none() {
            return;
        }
        let root = self
            .controller
            .state
            .archive
            .as_ref()
            .and_then(|a| a.file_name())
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut crumbs: Vec<(String, String)> = vec![(root, String::new())];
        let mut walked = String::new();
        for part in self
            .controller
            .state
            .current_dir
            .split('/')
            .filter(|p| !p.is_empty())
        {
            walked.push_str(part);
            walked.push('/');
            crumbs.push((part.to_string(), walked.clone()));
        }

        // A deep archive has more folders in its path than there is room for,
        // and they used to run out over the count at the other end. So: measure
        // first, keep the ones nearest to where you are, and put the rest
        // behind a "…" that opens them as a list. Which is what the Explorer
        // does with the same problem.
        let font = egui::TextStyle::Body.resolve(ui.style());
        let width = |ui: &egui::Ui, t: &str| {
            ui.painter()
                .layout_no_wrap(t.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
                .size()
                .x
        };
        let gap = 3.0;
        let sep = width(ui, "›") + gap * 2.0;
        let dots = width(ui, "…");
        let sizes: Vec<f32> = crumbs.iter().map(|(n, _)| width(ui, n)).collect();
        let first = crumbs_hidden(&sizes, sep, dots, (budget - 22.0).max(60.0));

        let mut go: Option<String> = None;
        egui::Frame::none()
            .fill(ui.visuals().window_fill)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .rounding(egui::Rounding::same(5.0))
            .inner_margin(egui::Margin::symmetric(9.0, 3.0))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.set_max_width(budget);
                if first > 0 {
                    let more = ui.add(
                        egui::Label::new(egui::RichText::new("…").weak())
                            .selectable(false)
                            .sense(egui::Sense::click()),
                    );
                    let id = egui::Id::new("arca-crumbs");
                    if more.clicked() {
                        ui.memory_mut(|m| m.toggle_popup(id));
                    }
                    egui::popup_below_widget(
                        ui,
                        id,
                        &more,
                        egui::PopupCloseBehavior::CloseOnClick,
                        |ui| {
                            ui.set_min_width(180.0);
                            for (name, path) in &crumbs[..first] {
                                if ui.button(name).clicked() {
                                    go = Some(path.clone());
                                }
                            }
                        },
                    );
                    ui.add(egui::Label::new(egui::RichText::new("›").weak()).selectable(false));
                }
                let last = crumbs.len() - 1;
                for (i, (name, path)) in crumbs.iter().enumerate().skip(first) {
                    if i > first {
                        ui.add(egui::Label::new(egui::RichText::new("›").weak()).selectable(false));
                    }
                    // The one you are on is not a way to anywhere.
                    if i == last {
                        ui.add(
                            egui::Label::new(egui::RichText::new(name).strong())
                                .selectable(false)
                                .truncate(),
                        );
                    } else if ui
                        .add(
                            egui::Label::new(egui::RichText::new(name).weak())
                                .selectable(false)
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        go = Some(path.clone());
                    }
                }
            });
        if let Some(path) = go {
            self.controller.go_to(path);
        }
    }

    // Everything the keyboard does, in one place. There was nowhere to find
    // this out short of reading the source, and a program whose shortcuts are a
    // secret may as well not have them.
    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.controller.state.show_shortcuts {
            return;
        }
        let s = self.controller.s();
        // `open` gives the window its own cross, which is one button fewer at
        // the bottom and one row less of height.
        let mut open = true;
        egui::Window::new(s.shortcuts_title)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                // Two columns side by side. In one column this ran taller than
                // the window it belongs to and lost both ends.
                //
                // The keys are spelled out rather than drawn with the arrows
                // and the page symbols: Consolas has the four arrows and not
                // the page ones, so half of that line came out as hollow boxes.
                let left: [(&str, &str); 19] = [
                    ("Ctrl+O", s.open),
                    ("Ctrl+N", s.compress),
                    ("Ctrl+E", s.extract_all),
                    ("Alt+W", s.extract_here),
                    ("F3   Alt+V", s.view_word),
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
                let right: [(&str, &str); 12] = [
                    ("Ctrl+C", s.copy_word),
                    ("Ctrl+X", s.cut_word),
                    ("Ctrl+V", s.paste_word),
                    ("Ctrl+Shift+C", s.copy_names),
                    ("", ""),
                    ("Enter", s.open_word),
                    ("Backspace", s.up),
                    ("Alt + \u{2191}", s.up),
                    ("Alt + \u{2190} \u{2192}", s.back),
                    ("\u{2191} \u{2193}", s.move_word),
                    ("Home  End", s.move_word),
                    ("A - Z", s.jump_word),
                ];
                let column = |ui: &mut egui::Ui, id: &str, rows: &[(&str, &str)]| {
                    egui::Grid::new(id)
                        .num_columns(2)
                        .spacing(egui::vec2(14.0, 6.0))
                        .show(ui, |ui| {
                            for (key, what) in rows {
                                if key.is_empty() {
                                    ui.end_row();
                                    continue;
                                }
                                ui.label(egui::RichText::new(*key).monospace().strong());
                                ui.label(*what);
                                ui.end_row();
                            }
                        });
                };
                // Space between the columns rather than a separator: a vertical
                // separator inside a horizontal layout grows to the height
                // available to it, and inside a window that is the height of
                // the screen, which stretched this one until both ends of it
                // were off the bottom and the top.
                ui.horizontal_top(|ui| {
                    column(ui, "shortcuts-left", &left);
                    ui.add_space(28.0);
                    column(ui, "shortcuts-right", &right);
                });
                ui.add_space(2.0);
            });
        if !open {
            self.controller.state.show_shortcuts = false;
        }
    }
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.controller.state.show_settings {
            return;
        }
        let s = self.controller.s();
        let mut open = true;
        egui::Window::new(s.settings)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    self.settings_row(ui, ctx);
                });
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.strong(s.defaults_title);
                ui.add_space(6.0);
                self.format_row(ui);
                ui.add_space(6.0);
                ui.checkbox(&mut self.controller.state.into_subfolder, s.into_subfolder);
                ui.add_space(4.0);
                if ui
                    .checkbox(&mut self.controller.state.settings.updates, s.check_updates)
                    .changed()
                {
                    self.controller.state.settings.save();
                }
                ui.add_space(8.0);
            });
        self.controller.state.show_settings = open;
    }
    fn add_view(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) {
        let s = self.controller.s();
        ui.add_space(10.0);
        ui.heading(s.add_to_archive);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(s.output_name);
            ui.add(
                egui::TextEdit::singleline(&mut self.controller.state.output_name)
                    .desired_width(320.0),
            );
        });
        ui.add_space(6.0);
        self.format_row(ui);
        ui.add_space(6.0);
        // Only .zip has anywhere to put encryption, so the field is not offered
        // for the other two rather than accepted and quietly ignored.
        if self.controller.state.format == Format::Zip {
            ui.horizontal(|ui| {
                ui.label(s.password_optional);
                ui.add(
                    egui::TextEdit::singleline(&mut self.controller.state.add_password)
                        .password(!self.controller.state.show_password)
                        .desired_width(220.0),
                );
                ui.checkbox(&mut self.controller.state.show_password, s.show_password);
            });
            ui.add_space(6.0);
        }
        let count = self.controller.state.pending_inputs.len();
        ui.label(format!(
            "{count} {}",
            if count == 1 {
                s.one_entry
            } else {
                s.entries_word
            }
        ));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button(s.start).clicked() {
                let dir = self
                    .controller
                    .state
                    .pending_inputs
                    .first()
                    .and_then(|p| p.parent())
                    .map(PathBuf::from)
                    .unwrap_or_default();
                let mut name = self.controller.state.output_name.trim().to_string();
                if name.is_empty() {
                    name = format!("archive.{}", self.controller.state.format.extension());
                }
                let job = Job::Compress {
                    out: dir.join(name),
                    inputs: self.controller.state.pending_inputs.clone(),
                    format: self.controller.state.format,
                    codec: self.controller.state.codec,
                    level: self.controller.state.level,
                    password: if self.controller.state.format == Format::Zip
                        && !self.controller.state.add_password.is_empty()
                    {
                        Some(self.controller.state.add_password.clone())
                    } else {
                        None
                    },
                };
                self.controller.run_job(job);
            }
            if ui.button(s.cancel).clicked() {
                self.controller.state.view = View::Browse;
            }
        });
    }
    /// The bar, the file of the moment, and the clock under it.
    fn progress_body(&self, ui: &mut egui::Ui) {
        let s = self.controller.s();
        let state = &self.controller.state;
        let fraction = if state.total_count == 0 {
            0.0
        } else {
            state.done_count as f32 / state.total_count as f32
        };
        // What it is being done to. The verb is in the title, so this is only
        // the name, and it is the line that says which of several windows this
        // one is.
        if !state.subject.is_empty() {
            ui.add(egui::Label::new(egui::RichText::new(&state.subject).strong()).truncate());
            ui.add_space(6.0);
        }
        ui.add(
            egui::ProgressBar::new(fraction)
                .text(match (state.total_count, state.in_bytes) {
                    // No end to show: a percentage of nothing.
                    (0, _) => format!("{:.0}%", fraction * 100.0),
                    // A download counts bytes, not files, and nobody reads
                    // "2481152 of 4627170".
                    (total, true) => {
                        format!("{} / {}", human(state.done_count as u64), human(total as u64))
                    }
                    (total, false) => format!("{} / {}", state.done_count, total),
                })
                .desired_width(ui.available_width()),
        );
        ui.add_space(5.0);
        // The file of the moment, and under it the clock. Both small and quiet:
        // they change several times a second and nobody reads them line by
        // line, they are there to say it is still moving.
        ui.add(egui::Label::new(egui::RichText::new(&state.current_file).weak().small()).truncate());
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let held = state.hold.load(std::sync::atomic::Ordering::Relaxed);
            if let Some(t) = state.started {
                let gone = t.elapsed().as_secs_f64();
                ui.label(
                    egui::RichText::new(format!("{} {}", s.elapsed_word, clock(gone)))
                        .weak()
                        .small(),
                );
                // Guessed from how long the part already done took, and only
                // once enough of it is done for the guess to be worth reading:
                // at two per cent it would say an hour and then a minute. A
                // paused job is not going anywhere, so it says nothing.
                if state.busy && !held && fraction > 0.05 {
                    let left = gone / fraction as f64 - gone;
                    ui.label(
                        egui::RichText::new(format!("· {} {}", s.time_left, clock(left)))
                            .weak()
                            .small(),
                    );
                }
            }
            if held {
                ui.label(
                    egui::RichText::new(format!("· {}", s.paused_word))
                        .weak()
                        .small(),
                );
            }
        });
    }

    /// What can be pressed while a job runs, and what is left when it stops.
    /// Answers whether the window should go.
    fn progress_buttons(&self, ui: &mut egui::Ui) -> bool {
        use std::sync::atomic::Ordering;
        let s = self.controller.s();
        let state = &self.controller.state;
        let mut close = false;
        if state.busy {
            let asked = state.stop.load(Ordering::Relaxed);
            let held = state.hold.load(Ordering::Relaxed);
            ui.horizontal(|ui| {
                // Pausing lets go at the end of an entry, not the end of a
                // byte, so a file that has started still has to finish.
                if ui
                    .add_enabled(
                        !asked,
                        egui::Button::new(if held { s.resume_word } else { s.pause_word }),
                    )
                    .clicked()
                {
                    state.hold.store(!held, Ordering::Relaxed);
                }
                // A way out of anything that is going to take a while. The
                // button goes quiet once it is pressed, because the job is over
                // as far as the person pressing it is concerned.
                if ui
                    .add_enabled(!asked, egui::Button::new(s.cancel))
                    .clicked()
                {
                    state.stop.store(true, Ordering::Relaxed);
                    // Let it go first, or the news would sit unread until
                    // somebody pressed Resume.
                    state.hold.store(false, Ordering::Relaxed);
                }
                if asked {
                    ui.label(egui::RichText::new(s.stopping).weak().small());
                }
            });
        } else {
            // Only ever reached when something went wrong or was given up on: a
            // job that finishes takes this window with it.
            let (word, color) = if state.error {
                (s.failed, ui.visuals().error_fg_color)
            } else {
                (s.done, ui.visuals().text_color())
            };
            ui.label(egui::RichText::new(word).color(color).strong());
            if !state.notice.is_empty() {
                ui.add_space(2.0);
                ui.label(&state.notice);
            }
            ui.add_space(10.0);
            if ui.button(s.close).clicked() {
                close = true;
            }
        }
        close
    }

    /// The job as a panel over the list it was started from.
    fn progress_window(&mut self, ctx: &egui::Context) {
        if !self.controller.state.overlay {
            return;
        }
        // The list behind is dimmed rather than left bright: it is not what is
        // being asked about, and anything pressed in it would be a second job
        // on an archive that is being rewritten.
        let screen = ctx.screen_rect();
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::PanelResizeLine,
            egui::Id::new("arca-dim"),
        ))
        .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(120));

        let mut close = false;
        egui::Window::new(&self.controller.state.title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(400.0);
                ui.add_space(2.0);
                self.progress_body(ui);
                ui.add_space(12.0);
                close = self.progress_buttons(ui);
                ui.add_space(2.0);
            });
        if close {
            self.controller.state.overlay = false;
        }
    }

    /// The job as the whole window, which is what a job started from the
    /// Explorer gets: there is no list behind it to go back to.
    fn running_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // No heading here: the title bar of this window already says the verb
        // and the name, and saying it twice in a window this small is most of
        // the window.
        ui.add_space(8.0);
        self.progress_body(ui);
        ui.add_space(14.0);
        if self.progress_buttons(ui) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    // What a finished rename does with what was typed.
    //
    // The checks are the ones the archive cannot make for itself: a name is a
    // name and not a path, nothing else in this folder is already called that,
    // and a rename to the same name is not a rewrite of the whole archive for
    // nothing. Anything else the zip will refuse on its own and say so.
    fn column_edges(
        &mut self,
        ui: &mut egui::Ui,
        heads: &[egui::Rect],
        slots: &[usize],
        cols: &[SortColumn],
        rows: &[Row],
        s: &Strings,
        top: f32,
        foot: f32,
    ) {
        let Some(first) = heads.first() else {
            return;
        };
        let grab = ui.style().interaction.resize_grab_radius_side;
        // The rule sits down the middle of the gap between two cells.
        let half = ui.spacing().item_spacing.x * 0.5;
        let quiet = ui.visuals().widgets.noninteractive.bg_stroke;
        // Every edge is read off the left of the cell that follows it, never
        // off the right of the cell before. A header cell reports a rectangle
        // that has been stretched to hold what was drawn in it, so its right
        // hand edge wanders past the column and the rules came out scattered
        // across the words; its left is where the table put it.
        //
        // Counted from one, so the edge in hand is the one this cell begins
        // with and the column it resizes is the one before. The last column
        // has no edge of its own: it ends where the table does.
        for (i, cell) in heads.iter().enumerate().skip(1) {
            let x = cell.left() - half;
            let rect = egui::Rect::from_x_y_ranges((x - grab)..=(x + grab), first.y_range());
            // Clicks as well as drags, so that catching the edge and letting go
            // again does not fall through to the heading and sort the list.
            let resp = ui.interact(
                rect,
                egui::Id::new(("arca-column-edge", i)),
                egui::Sense::click_and_drag(),
            );
            if resp.hovered() || resp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeColumn);
            }
            if resp.dragged() {
                if let Some(slot) = slots.get(i - 1).copied() {
                    if let Some(width) = self.controller.state.settings.widths.get_mut(slot) {
                        *width = (*width + resp.drag_delta().x).max(Settings::least(slot));
                    }
                }
            }
            // Double clicking an edge fits the column to what is in it, which
            // is what the same gesture does in WinRAR and in the Explorer.
            // Capped, because one absurd name in a folder of sensible ones
            // should not push every other column off the window.
            if resp.double_clicked() {
                if let (Some(slot), Some(which)) =
                    (slots.get(i - 1).copied(), cols.get(i - 1).copied())
                {
                    if let Some(width) = self.controller.state.settings.widths.get_mut(slot) {
                        *width =
                            natural_width(ui, rows, which, s).clamp(Settings::least(slot), 640.0);
                    }
                }
                self.controller.state.settings.save();
            }
            // Written when the hand lets go rather than on the way, so that
            // pulling an edge across the window is one visit to the disk and
            // not one per frame.
            if resp.drag_stopped() {
                self.controller.state.settings.save();
            }
            let stroke = if resp.dragged() {
                ui.visuals().widgets.active.bg_stroke
            } else if resp.hovered() {
                ui.visuals().widgets.hovered.bg_stroke
            } else {
                quiet
            };
            ui.painter()
                .line_segment([egui::pos2(x, top), egui::pos2(x, foot)], stroke);
        }
    }

    // The wheel used as a button: press it and the list follows the pointer
    // until something puts it away.
    // What is being carried, until the button comes up and says where.
    //
    // Leaving the window hands it to the system's own drag; letting go over a
    // folder of this archive moves it there. The native drag is only started
    // once the pointer is gone, because the instant it begins the system takes
    // the pointer and there is no way back into the list.
    fn carry(
        &mut self,
        ui: &mut egui::Ui,
        visible: &[Row],
        row_rects: &[(usize, egui::Rect)],
        viewport: egui::Rect,
    ) {
        if self.controller.state.carrying.is_none() {
            return;
        }
        let (down, at) = ui.input(|i| (i.pointer.primary_down(), i.pointer.latest_pos()));

        // Gone from the window. Whatever happens now happens out there.
        let Some(at) = at else {
            self.controller.state.carrying = None;
            if down {
                self.controller.drag_out();
            }
            return;
        };

        // The folder under the pointer, if it is one and it is not one of the
        // things being carried: dropping a folder into itself is not a move.
        let carried = self.controller.state.carrying.clone().unwrap_or_default();
        let over = row_at(row_rects, at.y, visible.len())
            .and_then(|i| visible.get(i))
            .filter(|r| r.is_dir && viewport.contains(at))
            .filter(|r| {
                r.up || !carried
                    .iter()
                    .any(|c| c.trim_end_matches('/') == r.path.trim_end_matches('/'))
            });

        if !down {
            self.controller.state.carrying = None;
            if let Some(row) = over {
                let target = row.path.clone();
                self.controller.move_into(&carried, &target);
            }
            return;
        }

        // While it is in the air: the folder it would go into is outlined, and
        // the pointer says what would happen.
        ui.ctx().set_cursor_icon(if over.is_some() {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::NoDrop
        });
        if let Some(row) = over {
            if let Some((_, rect)) = row_rects
                .iter()
                .find(|(i, _)| visible.get(*i).is_some_and(|r| r.path == row.path))
            {
                let accent = theme::cursor(ui.visuals());
                ui.painter().rect_stroke(rect.shrink(1.0), 3.0, accent);
            }
        }
    }

    fn wheel_scroll(&mut self, ui: &mut egui::Ui, viewport: egui::Rect, offset: f32, reach: f32) {
        let (pressed, released, here, elsewhere, escaped, spun, dt) = ui.input(|i| {
            (
                i.pointer.button_pressed(egui::PointerButton::Middle),
                i.pointer.button_released(egui::PointerButton::Middle),
                i.pointer.latest_pos(),
                i.pointer.button_pressed(egui::PointerButton::Primary)
                    || i.pointer.button_pressed(egui::PointerButton::Secondary),
                i.key_down(egui::Key::Escape),
                i.raw_scroll_delta.y != 0.0,
                // A frame that took a long time -- the window came back from
                // being hidden, say -- would otherwise jump the list a page.
                i.stable_dt.min(0.1),
            )
        });

        if pressed {
            // Pressing again puts it away, the way it does in a browser.
            self.wheel = match self.wheel {
                Some(_) => None,
                None => here.filter(|p| viewport.contains(*p)).map(|p| Wheel {
                    anchor: p,
                    at: offset,
                    moved: false,
                }),
            };
        }
        let Some(mut wheel) = self.wheel else {
            return;
        };
        // Any other button, the wheel itself turning, or Escape: all of them
        // are somebody asking for something else.
        if elsewhere || escaped || spun {
            self.wheel = None;
            return;
        }
        let Some(at) = here else {
            return;
        };

        let speed = wheel_speed(at.y - wheel.anchor.y);
        wheel.moved |= speed != 0.0;
        if released && wheel.moved {
            // Held down and pulled: the gesture ends where the hand lets go.
            // Let go without having pulled and it stays on, waiting.
            self.wheel = None;
            return;
        }
        wheel.at = (wheel.at + speed * dt).clamp(0.0, reach);
        self.wheel = Some(wheel);
        if speed != 0.0 {
            // Nothing else on screen is moving, so without this the list would
            // take one step per stray mouse event instead of running.
            ui.ctx().request_repaint();
        }

        // Windows says which way the list is going with the pointer itself: an
        // arrow up while it runs up, down while it runs down, and both ways
        // while it stands still. There is no asking the system for those --
        // they live in its own resources, and the toolkit offers one
        // double-headed arrow and no way to tell it apart from a resize -- so
        // the real pointer is put away here and this one drawn in its place.
        //
        // On its own layer rather than on the table's, because the hand is free
        // to wander off the list while the gesture runs and a pointer that
        // vanished at the edge of it would be worse than no pointer at all.
        ui.ctx().set_cursor_icon(egui::CursorIcon::None);
        let paint = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("arca-wheel"),
        ));

        // The anchor, left where the wheel went down: a ring with an arrow out
        // of the top and one out of the bottom, which is the mark Windows
        // leaves, so it reads as the same gesture rather than as one of ours.
        let ink = ui.visuals().weak_text_color();
        paint.circle(
            wheel.anchor,
            10.0,
            ui.visuals().panel_fill,
            egui::Stroke::new(1.0_f32, ink),
        );
        paint.circle_filled(wheel.anchor, 1.5, ink);
        for up in [1.0_f32, -1.0] {
            let tip = wheel.anchor.y - up * 6.5;
            paint.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(wheel.anchor.x - 3.0, tip + up * 3.0),
                    egui::pos2(wheel.anchor.x + 3.0, tip + up * 3.0),
                    egui::pos2(wheel.anchor.x, tip),
                ],
                ink,
                egui::Stroke::NONE,
            ));
        }

        // Pale with a dark edge, the way every pointer is drawn: it has to be
        // seen over a picked row as easily as over an empty list, and neither
        // theme gets a say in what a pointer looks like.
        let face = egui::Color32::WHITE;
        let edge = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(30));
        let arrow = |down: f32, base: f32, tip: f32| {
            egui::Shape::convex_polygon(
                vec![
                    egui::pos2(at.x - 5.0, at.y + down * base),
                    egui::pos2(at.x + 5.0, at.y + down * base),
                    egui::pos2(at.x, at.y + down * tip),
                ],
                face,
                edge,
            )
        };
        if speed < 0.0 {
            paint.add(arrow(-1.0, 1.0, 11.0));
        } else if speed > 0.0 {
            paint.add(arrow(1.0, 1.0, 11.0));
        } else {
            // Still: both ways at once, held apart to leave the hot spot clear.
            paint.add(arrow(-1.0, 3.0, 12.0));
            paint.add(arrow(1.0, 3.0, 12.0));
        }
    }

    // Press on the list and drag: a rectangle follows the pointer and every row
    // it touches gets ticked, the way it works in any file list.
    //
    // It is worked out again from `band_base` on every frame instead of being
    // added to as the pointer moves. That is what lets dragging back over a row
    // let go of it: growing a selection is easy, shrinking one is what needs
    // the starting point remembered.
    fn rubber_band(
        &mut self,
        ui: &mut egui::Ui,
        visible: &[Row],
        row_rects: &[(usize, egui::Rect)],
        viewport: egui::Rect,
        offset: f32,
        reach: f32,
    ) {
        let (down, origin, now, ctrl) = ui.input(|i| {
            (
                i.pointer.primary_down(),
                i.pointer.press_origin(),
                i.pointer.interact_pos(),
                i.modifiers.command,
            )
        });

        // Something is already in the air. The press that is on the books
        // belongs to that gesture, and a band drawn from it would follow the
        // pointer around underneath what is being carried.
        if self.controller.state.carrying.is_some() {
            return;
        }

        // Just back from a drag out of the window, with the button still down
        // as far as the toolkit knows. Nothing happens until it comes up for
        // real; the press that is still on the books belongs to a gesture that
        // is over.
        if self.controller.state.drag_settling {
            self.band = None;
            self.controller.state.band_anchor = None;
            self.controller.state.band_scroll = None;
            // Cleared by the button coming up, and also by a fresh press, in
            // case the release happened over somebody else's window and this
            // one never hears about it. Either way the gesture that
            // `DoDragDrop` swallowed is over.
            if !down || ui.input(|i| i.pointer.any_pressed()) {
                self.controller.state.drag_settling = false;
            }
            return;
        }

        // Something else has the pointer: the handle that resizes a column, or
        // the scroll bar. Both are drawn over the list rather than beside it, so
        // a press on either lands inside the rows and used to start a band as
        // well as doing its own job. A row only senses clicks and can never be
        // the thing being dragged, so this cannot turn off what it is for.
        if !down || ui.ctx().dragged_id().is_some() {
            self.band = None;
            self.controller.state.band_anchor = None;
            self.controller.state.band_scroll = None;
            return;
        }
        // `viewport` is the scrolling part alone, so the header is already out
        // of it: dragging a column edge cannot start a selection. The scroll
        // bar is a different matter, because it is drawn over the right hand
        // edge of that same area rather than beside it, so a press on it lands
        // inside the viewport and used to start a band. Dragging the bar is
        // scrolling, not picking.
        if self.band.is_none() {
            let mut room = viewport;
            if reach > 0.0 {
                let bar = ui.spacing().scroll;
                let wide = if bar.floating {
                    bar.bar_width
                } else {
                    bar.bar_width + bar.bar_inner_margin
                };
                room.set_right(viewport.right() - wide - bar.bar_outer_margin);
            }
            let Some(p) = origin.filter(|p| room.contains(*p)) else {
                return;
            };
            let Some(anchor) = row_at(row_rects, p.y, visible.len()) else {
                return;
            };
            // Pressing on a row that is already picked and pulling is how you
            // take the selection somewhere else; pressing anywhere else and
            // pulling draws a new one. That is the rule in the Explorer, and it
            // is the only one that lets both gestures share a button. Ctrl and
            // Shift are for adding to a selection, never for carrying it.
            let shift = ui.input(|i| i.modifiers.shift);
            let picked = anchor < visible.len() && self.controller.is_checked(&visible[anchor]);
            self.controller.state.drag_ready = (picked && !ctrl && !shift).then_some(anchor);
            self.band = origin;
            self.controller.state.band_anchor = Some(anchor);
            self.controller.state.band_base = if ctrl {
                self.controller.state.checked.clone()
            } else {
                vec![false; self.controller.state.checked.len()]
            };
        }

        let (Some(start), Some(here), Some(anchor)) =
            (self.band, now, self.controller.state.band_anchor)
        else {
            return;
        };
        // A click is a drag of no distance. Under this it is left alone, so
        // clicking a row still means clicking a row.
        //
        // Further than egui waits before calling a drag a drag, on purpose: a
        // band that appeared first would flash over the rows for the pixel or
        // two between the two thresholds every time a column was resized.
        if (here - start).length() < 10.0 {
            return;
        }

        // Far enough to be a gesture, and it began on something picked: the
        // selection is being carried somewhere, not redrawn.
        //
        // Where it is going is not decided yet. Inside the window it is a move
        // into another folder of this archive; outside it is a drag into
        // whatever is out there, and that one cannot be started early because
        // the moment it is, the system takes the pointer and there is no way
        // back into the list.
        if self.controller.state.drag_ready.take().is_some() {
            self.band = None;
            self.controller.state.band_anchor = None;
            self.controller.state.band_scroll = None;
            self.controller.state.carrying = Some(self.controller.selected_roots());
            return;
        }

        // Past either edge the list follows the pointer, the way the Explorer
        // does it. Without this a selection could never be longer than the
        // window, because dragging no longer scrolls.
        let over = if here.y < viewport.top() {
            here.y - viewport.top()
        } else if here.y > viewport.bottom() {
            here.y - viewport.bottom()
        } else {
            0.0
        };
        if over == 0.0 {
            self.controller.state.band_scroll = None;
        } else {
            self.controller.state.band_scroll =
                Some((offset + over.clamp(-24.0, 24.0)).clamp(0.0, reach));
            // Nothing else is moving, so without this the list would take one
            // step per stray mouse event instead of running.
            ui.ctx().request_repaint();
        }

        // Two row numbers, not a rectangle in window coordinates. The list
        // moves underneath while the drag is happening, and a rectangle frozen
        // where the button went down stops meaning anything the moment it does;
        // it also loses every row that scrolls out of sight, because those are
        // the only ones the table still knows the position of.
        let head = row_at(row_rects, here.y, visible.len()).unwrap_or(anchor);
        let (lo, hi) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        self.controller
            .state
            .checked
            .clone_from(&self.controller.state.band_base);
        // Both ends past the last row means the band is entirely in the empty
        // space under the list, and has reached nothing.
        if lo < visible.len() {
            for row in &visible[lo..=hi.min(visible.len() - 1)] {
                self.controller.set_checked(row, true);
            }
        }

        // Drawn from the row the drag began on rather than from the point the
        // button went down, so that it stays put against the rows when the list
        // scrolls under it.
        let anchor_rect = if anchor < visible.len() {
            row_rects.iter().find(|(i, _)| *i == anchor)
        } else {
            None
        };
        let edge = match anchor_rect {
            // Its far side, so that the row the drag began on falls inside the
            // band whichever way the drag then went.
            Some((_, r)) => {
                if here.y < r.top() {
                    r.bottom()
                } else {
                    r.top()
                }
            }
            // Begun in the empty space under the list, where there is no row to
            // hang the band on, so it hangs from where the button went down.
            None if anchor >= visible.len() => start.y,
            // Scrolled out of sight, which happens as soon as a drag has run
            // far enough for the list to follow it. Which edge to start from is
            // decided by where that row went, and that is its number against
            // the ones still on screen. Asking where the pointer is instead
            // said nothing about the anchor: after dragging to the bottom and
            // turning back up, the band was drawn below the pointer, growing
            // away from everything it had picked.
            None => {
                let first = row_rects.first().map(|(i, _)| *i).unwrap_or(anchor);
                if anchor < first {
                    viewport.top()
                } else {
                    viewport.bottom()
                }
            }
        };
        let band = egui::Rect::from_two_pos(egui::pos2(start.x, edge), here);
        let fill = ui.visuals().selection.bg_fill.linear_multiply(0.25);
        ui.painter().rect(
            band.intersect(viewport),
            0.0,
            fill,
            theme::cursor(ui.visuals()),
        );
    }
    fn keyboard(&mut self, ctx: &egui::Context, rows: &[Row]) {
        // The box that picks a group by name has just opened and does not have
        // the keyboard yet. The key that opened it is still in this frame, and
        // a minus is a character the list would otherwise jump to.
        if self.controller.state.picking_group.is_some() {
            return;
        }
        if rows.is_empty() {
            self.controller.state.cursor = None;
            return;
        }
        // A text box has the keyboard: the filter field, or a dialog. Arrows
        // and Space belong to it, not to the list.
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }

        let last = rows.len() - 1;
        let page = 12usize;
        let mut moved = None;
        let mut enter = false;
        let mut space = false;
        let mut up_level = false;
        let mut shift = false;
        let mut check_all = false;
        let mut invert = false;
        let mut typed = String::new();
        let mut rename = false;
        let mut look = false;

        ctx.input(|i| {
            let at = self.controller.state.cursor.unwrap_or(0);
            shift = i.modifiers.shift;
            check_all = i.modifiers.command && i.key_pressed(egui::Key::A);
            invert = i.modifiers.command && i.key_pressed(egui::Key::I);
            // Alt belongs to the shortcuts that walk the folders, not to the
            // cursor: Alt and up is one level out, not one row up.
            if !i.modifiers.alt {
                for (key, to) in [
                    (egui::Key::ArrowDown, (at + 1).min(last)),
                    (egui::Key::ArrowUp, at.saturating_sub(1)),
                    (egui::Key::PageDown, (at + page).min(last)),
                    (egui::Key::PageUp, at.saturating_sub(page)),
                    (egui::Key::Home, 0),
                    (egui::Key::End, last),
                ] {
                    if i.key_pressed(key) {
                        // The first press only lands the cursor somewhere
                        // visible instead of jumping a row from nowhere.
                        moved = Some(if self.controller.state.cursor.is_none() {
                            0
                        } else {
                            to
                        });
                    }
                }
            }
            rename = i.key_pressed(egui::Key::F2);
            // Alt+V is what WinRAR uses; F3 is what every file manager since
            // Norton has used for the same thing, and it is one key.
            look = (i.modifiers.alt && i.key_pressed(egui::Key::V)) || i.key_pressed(egui::Key::F3);
            enter = i.key_pressed(egui::Key::Enter);
            space = i.key_pressed(egui::Key::Space);
            up_level = i.key_pressed(egui::Key::Backspace)
                || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowUp));
            // Plain letters, the way the Explorer jumps to a name. Ctrl and Alt
            // are somebody else's shortcut.
            if !i.modifiers.command && !i.modifiers.alt {
                for e in &i.events {
                    if let egui::Event::Text(t) = e {
                        typed.push_str(t);
                    }
                }
            }
        });

        // What every file list calls inverting a selection: keep what was not
        // picked and let go of what was, which is how you pick everything but
        // the handful you can see.
        if invert {
            let flipped: Vec<bool> = rows
                .iter()
                .map(|r| !self.controller.is_checked(r))
                .collect();
            for (r, on) in rows.iter().zip(flipped) {
                self.controller.set_checked(r, on);
            }
            return;
        }

        if check_all {
            let value = rows.iter().any(|r| !self.controller.is_checked(r));
            for r in rows {
                self.controller.set_checked(r, value);
            }
            return;
        }

        // Typing jumps to the next row starting with what was typed, wrapping
        // round, so pressing the same letter walks through the matches.
        if !typed.is_empty() && typed != " " {
            let needle = typed.to_lowercase();
            let from = self.controller.state.cursor.map_or(0, |c| c + 1);
            let hit = (0..rows.len())
                .map(|n| (from + n) % rows.len())
                .find(|&n| rows[n].label.to_lowercase().starts_with(&needle));
            if let Some(n) = hit {
                self.controller.state.cursor = Some(n);
                self.controller.state.scroll_to_cursor = true;
                return;
            }
        }

        if let Some(to) = moved {
            // Shift drags the ticks along with the cursor, so a run of files
            // can be picked without reaching for the mouse.
            if shift {
                let from = self.controller.state.cursor.unwrap_or(to);
                let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
                for r in &rows[lo..=hi] {
                    self.controller.set_checked(r, true);
                }
            }
            self.controller.state.cursor = Some(to);
            self.controller.state.scroll_to_cursor = true;
        }

        if up_level && !self.controller.state.current_dir.is_empty() {
            let parent = parent_of(&self.controller.state.current_dir);
            self.controller.go_to(parent);
            return;
        }

        let Some(at) = self.controller.state.cursor else {
            return;
        };
        let Some(row) = rows.get(at) else { return };

        // F2 opens the name for editing where it stands, which is what it does
        // in WinRAR and in the Explorer. Only a zip can be written to, so
        // anywhere else it does nothing rather than opening a box that would
        // have to say no afterwards.
        // Looking at what is under the cursor without taking it out, which is
        // what Alt+V has always done in WinRAR.
        if look {
            if let Some(i) = row.entry {
                self.controller.view_entry(i);
            }
            return;
        }
        if rename && self.controller.state.format == Format::Zip && !row.up {
            self.controller.state.renaming = Some((row.path.clone(), row.label.clone()));
            self.controller.state.rename_fresh = true;
            return;
        }
        if space {
            let value = !self.controller.is_checked(row);
            self.controller.set_checked(row, value);
        }
        if enter {
            if row.is_dir {
                let path = row.path.clone();
                self.controller.go_to(path);
            } else if let Some(i) = row.entry {
                self.controller.open_file(i);
            }
        }
    }

    // Going somewhere new drops whatever was ahead in the history, the way a
    // browser does. Re-entering the folder already showing is not a move.
    fn table(&mut self, ui: &mut egui::Ui) {
        let s = self.controller.s();
        let visible = self.controller.visible_rows();
        // Before the table is drawn, so a move this frame is painted this
        // frame rather than one behind.
        self.keyboard(ui.ctx(), &visible);
        if self
            .controller
            .state
            .cursor
            .is_some_and(|c| c >= visible.len())
        {
            self.controller.state.cursor = if visible.is_empty() {
                None
            } else {
                Some(visible.len() - 1)
            };
        }

        let mut requested: Option<SortColumn> = None;
        let mut opened: Option<usize> = None;
        let mut clicked: Option<usize> = None;
        // Only the left button, and only from a row. The row menu also fills in
        // `clicked` so that right clicking something picks it, and a right
        // click is not half of a double click.
        let mut left_click: Option<usize> = None;
        let columns = self.controller.state.settings.columns;
        // The ones on, in the order they are drawn. The header, the cells and
        // the column widths all walk this same list, so they cannot drift.
        let shown: Vec<SortColumn> = Columns::ALL
            .iter()
            .map(|(which, _)| *which)
            .filter(|w| columns.on(*w))
            .collect();
        // Which width belongs to each column on screen, left to right. The
        // name is always the first, and the rest keep their own width whether
        // they are showing or not, so turning one off and on again does not
        // lose how wide it was pulled.
        let slots: Vec<usize> = std::iter::once(0)
            .chain(
                shown
                    .iter()
                    .map(|w| Columns::ALL.iter().position(|(c, _)| c == w).unwrap_or(0) + 1),
            )
            .collect();
        // The whole of each header cell, edge to edge: the rectangle of the
        // cell's response, not the one `col` hands back, which is only as wide
        // as the word inside it. This is what says where the column edges are,
        // and the edges are where the handles go.
        let mut heads: Vec<egui::Rect> = Vec::new();
        let toggle_column: std::cell::Cell<Option<SortColumn>> = std::cell::Cell::new(None);
        // What the row menu asked for. Cells again, and acted on after the
        // table: doing any of it inside the closure would be borrowing self
        // while the table still holds it.
        let wants_extract = std::cell::Cell::new(false);
        let wants_delete = std::cell::Cell::new(false);
        let wants_here = std::cell::Cell::new(false);
        let wants_view: std::cell::Cell<Option<usize>> = std::cell::Cell::new(None);
        let wants_test = std::cell::Cell::new(false);
        // The rename in progress, unpacked into pieces the row closure can hold
        // while the table still has `self`. `finish` is how the box says it is
        // done: yes to keep what was typed, no to throw it away.
        let editing: Option<String> = self
            .controller
            .state
            .renaming
            .as_ref()
            .map(|(p, _)| p.clone());
        let typing = std::cell::RefCell::new(
            self.controller
                .state
                .renaming
                .as_ref()
                .map_or(String::new(), |(_, t)| t.clone()),
        );
        let fresh = std::cell::Cell::new(self.controller.state.rename_fresh);
        let finish: std::cell::Cell<Option<bool>> = std::cell::Cell::new(None);
        let wants_rename = std::cell::Cell::new(false);
        let wants_copy_names = std::cell::Cell::new(false);
        let wants_clip: std::cell::Cell<Option<bool>> = std::cell::Cell::new(None);
        let wants_paste = std::cell::Cell::new(false);
        let wants_select_all = std::cell::Cell::new(false);
        let picked = std::cell::Cell::new(false);
        // The row the cursor is on, painted after the table: the highlight now
        // belongs to the ticks, so the cursor needs a mark of its own.
        let cursor_rect: std::cell::Cell<Option<egui::Rect>> = std::cell::Cell::new(None);
        // Paired with the index they came from. `body.rows` only builds the
        // ones on screen, so after any scrolling these do not start at nought.
        let mut row_rects: Vec<(usize, egui::Rect)> = Vec::with_capacity(visible.len());
        let pressing = ui.input(|i| i.pointer.any_down());
        let mut icons = std::mem::take(&mut self.icons);
        // A RefCell rather than a plain take: the cell closures are handed out one
        // per column and two of them would otherwise want the same &mut.
        let types = std::cell::RefCell::new(std::mem::take(&mut self.controller.state.types));
        let order = self.controller.state.order;
        let hint = s.sort_hint;
        // The whole header cell answers, not the four letters of the name:
        // aiming at the text to sort by a column is a nuisance, and the cell is
        // what looks like the button.
        //
        // It cannot use the response the table hands back for the cell. The
        // header row is built with row index 0, the same number the first row
        // of the body carries, and the cell id is made out of that number and
        // the column, so the two rows end up sharing ids and each one is handed
        // the other's clicks: pressing a cell of the first row sorted by that
        // column, and pressing a column name picked the first row. So the
        // header asks for an interaction of its own, under an id of its own.
        // The rules between the columns run the whole height of the list, which
        // is how WinRAR and every list of files with columns has always drawn
        // them: they are what tells you which number belongs under which
        // heading when the eye is halfway down the page. They were taken out
        // here for a while on the grounds that they looked like a spreadsheet,
        // which was a change nobody asked for and the wrong call. The table
        // used to draw them along with its own resize handles; they are drawn
        // in `column_edges` now, with the handles.
        let accent = theme::mark(ui.visuals());
        let head = |ui: &mut egui::Ui, text: &str, col: SortColumn| -> egui::Response {
            let cell = ui.max_rect();
            // Asked for before the word is drawn, so the ground can be laid
            // under it: a heading lights up when the pointer is on it, which is
            // how WinRAR says that a column name is a thing you press and not
            // just a label, and the column the list is sorted by keeps a ground
            // of its own. Spread half the gap either side, the same as the fill
            // on a row, so that a lit heading reaches its neighbours.
            let resp = ui.interact(
                cell,
                egui::Id::new(("arca-head", text)),
                egui::Sense::click(),
            );
            let fill = if resp.hovered() {
                Some(ui.visuals().widgets.hovered.bg_fill)
            } else if order.0 == col {
                Some(theme::header_sorted(ui.visuals()))
            } else {
                None
            };
            if let Some(fill) = fill {
                let half = ui.spacing().item_spacing.x * 0.5;
                ui.painter()
                    .rect_filled(cell.expand2(egui::vec2(half, 0.0)), 0.0, fill);
            }
            // The table puts its cells in truncating mode, and a truncating
            // label takes the whole width it is offered. That is right for a
            // file name and wrong for a column heading: it left no room beside
            // the word, so the mark that says which way the sort runs was
            // allocated past the edge of the cell and clipped away.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            ui.add(egui::Label::new(egui::RichText::new(text).strong()).selectable(false));
            if order.0 == col {
                // A small triangle beside the name rather than a caret typed
                // into it: "Name ^" put a character in the middle of a word
                // that was never part of the word.
                let (mark, _) =
                    ui.allocate_exact_size(egui::vec2(11.0, 11.0), egui::Sense::hover());
                ui.painter().add(egui::Shape::convex_polygon(
                    sort_mark(mark.center(), order.1).to_vec(),
                    accent,
                    egui::Stroke::NONE,
                ));
            }
            resp.on_hover_text(hint)
        };

        // Where the table begins, taken before it is built and kept for the
        // band the headings stand on and for the top of the column rules: the
        // cells themselves start a few pixels below this, and rules that began
        // there left a gap of bare window above them.
        let table_top = ui.cursor().top();
        // A place in the paint list held open for that band, so it can be
        // filled in once the header has said how tall it is and still come out
        // underneath the words rather than over them.
        let band = ui.painter().add(egui::Shape::Noop);
        let mut builder = TableBuilder::new(ui)
            // Stripes, ticks and a highlight were three ways of saying the same
            // thing. What is picked is painted; the rest is left quiet.
            .striped(false)
            // The table's own handles run the whole height of the list; ours
            // are in the header. See `widths`.
            .resizable(false)
            // Without this the cells only sense hovering, and row.response()
            // would never report a double click.
            .sense(egui::Sense::click())
            // Dragging in a file list draws a selection; it does not push the
            // list about. With this on, dragging did both at once, and the two
            // pull opposite ways: dragging down moves the content down, which
            // is the list scrolling up, so a downward selection ran upwards.
            .drag_to_scroll(false)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
        // Name first and wide, with the icon inside it. That is where the
        // Explorer and every archiver put it, and a separate icon column only
        // pushed the one thing you read away from its picture.
        //
        // Each column is given its exact width, so that what the header hands
        // back is what was asked for: the last one takes whatever is left, as
        // the date does in every file list.
        for (n, slot) in slots.iter().enumerate() {
            builder = if n + 1 == slots.len() {
                builder.column(Column::remainder().at_least(CELL_LEAST))
            } else {
                builder.column(Column::exact(self.controller.state.settings.widths[*slot]))
            };
        }
        // Set only while a selection drag has run off the end of the list, so
        // the rest of the time the table keeps its own scroll position.
        if let Some(y) = self
            .controller
            .state
            .band_scroll
            .or(self.wheel.map(|w| w.at))
        {
            builder = builder.vertical_scroll_offset(y);
        }

        let out = builder
            .header(32.0, |mut h| {
                // Right clicking anywhere along the header offers the list of
                // columns, which is where both WinRAR and NanaZip keep it.
                // A Cell because every header cell hands the same menu to
                // egui, and several closures cannot hold one &mut between them.
                let menu = |ui: &mut egui::Ui| {
                    ui.label(s.columns_word);
                    ui.separator();
                    for (which, _) in Columns::ALL {
                        let mut on = columns.on(which);
                        if ui.checkbox(&mut on, Columns::label(which, s)).clicked() {
                            toggle_column.set(Some(which));
                            ui.close_menu();
                        }
                    }
                };
                let mut resp = None;
                let (_, cell) = h.col(|ui| resp = Some(head(ui, s.col_name, SortColumn::Name)));
                heads.push(cell.rect);
                if let Some(r) = resp {
                    if r.clicked() {
                        requested = Some(SortColumn::Name);
                    }
                    r.context_menu(|ui| menu(ui));
                }
                for which in &shown {
                    let mut resp = None;
                    let (_, cell) =
                        h.col(|ui| resp = Some(head(ui, Columns::label(*which, s), *which)));
                    heads.push(cell.rect);
                    if let Some(r) = resp {
                        if r.clicked() {
                            requested = Some(*which);
                        }
                        r.context_menu(|ui| menu(ui));
                    }
                }
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, visible.len(), |mut row| {
                    let idx = row.index();
                    let r = &visible[idx];
                    // Ticked is selected, and selected is what gets painted, the
                    // way WinRAR and the Explorer do it. Highlighting only the
                    // row the keyboard was on said nothing about what the
                    // buttons were going to act on.
                    row.set_selected(self.controller.is_checked(r));
                    // The table works out which row the pointer is over, keeps
                    // it, and tints it on the next frame. One frame late is
                    // fine while the pointer is only passing over rows, and is
                    // a flicker trailing behind it once a button is held down
                    // and the pointer is moving with intent: rows light up as
                    // if picked, a step behind, and go out again. Nothing in
                    // the Explorer lights up under a held button either.
                    if pressing {
                        row.set_hovered(false);
                    }
                    let cut = self.controller.state.cut_names.contains(&r.path)
                        || r.entry.is_some_and(|i| {
                            self.controller
                                .state
                                .cut_names
                                .contains(&self.controller.state.entries[i].name)
                        });
                    row.col(|ui| {
                        // The system icon when the desktop has one, and the
                        // drawn one when it does not, which is every platform
                        // that is not Windows so far.
                        match system_icon(ui.ctx(), &mut icons, &r.label, r.is_dir) {
                            Some(tex) => {
                                ui.add(
                                    egui::Image::new(&tex)
                                        .fit_to_exact_size(egui::vec2(15.0, 15.0)),
                                );
                            }
                            None => draw_icon(ui, r.kind),
                        }
                        ui.add_space(4.0);
                        if editing.as_deref() == Some(r.path.as_str()) {
                            name_box(ui, &typing, &fresh, &finish);
                            return;
                        }
                        let text = if r.is_dir {
                            egui::RichText::new(&r.label).strong()
                        } else {
                            egui::RichText::new(&r.label)
                        };
                        // Faded while it is on the clipboard as a cut, which is
                        // the only sign the Explorer gives either.
                        let text = if cut { text.weak() } else { text };
                        let room = ui.available_width();
                        let name = ui.add(egui::Label::new(text).selectable(false).truncate());
                        // The whole name on hover, but only when the column is
                        // too narrow to hold it. A tip that repeats what is
                        // already legible is a tip that teaches you to ignore
                        // tips.
                        if wide_of(ui, &r.label, egui::TextStyle::Body) > room {
                            name.on_hover_text(&r.label);
                        }
                    });
                    for which in &shown {
                        row.col(|ui| {
                            // The way out of the folder has no size, no date and
                            // no kind: it is a door, not a thing in the room.
                            if r.up {
                                return;
                            }
                            match which {
                                SortColumn::Size => {
                                    ui.monospace(human(r.size));
                                }
                                SortColumn::Packed => {
                                    ui.monospace(human(r.packed));
                                }
                                SortColumn::Method => {
                                    if r.is_dir {
                                        ui.weak(format!("{} {}", r.count, s.items_word));
                                    } else if r.encrypted {
                                        ui.label(format!("AES-256 {}", r.method));
                                    } else {
                                        ui.label(r.method);
                                    }
                                }
                                SortColumn::Saved => {
                                    let pct = saved_of(r) * 100.0;
                                    let value = if pct.abs() < 0.5 { 0.0 } else { pct };
                                    ui.monospace(format!("{value:.0}%"));
                                }
                                SortColumn::Modified => {
                                    ui.monospace(when(r.mtime));
                                }
                                SortColumn::Crc => {
                                    // A folder has no contents of its own to sum.
                                    if r.is_dir {
                                        ui.weak("");
                                    } else {
                                        ui.monospace(format!("{:08X}", r.crc32));
                                    }
                                }
                                SortColumn::Type => {
                                    ui.add(
                                        egui::Label::new(system_type(
                                            &mut types.borrow_mut(),
                                            &r.label,
                                            r.is_dir,
                                        ))
                                        .selectable(false)
                                        .truncate(),
                                    );
                                }
                                SortColumn::Created => {
                                    ui.monospace(when(r.created));
                                }
                                SortColumn::Accessed => {
                                    ui.monospace(when(r.accessed));
                                }
                                SortColumn::Attributes => {
                                    ui.monospace(attribute_letters(r.attributes));
                                }
                                SortColumn::Path => {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(folder_of(&r.path)).weak(),
                                        )
                                        .selectable(false)
                                        .truncate(),
                                    );
                                }
                                SortColumn::Name => {}
                            }
                        });
                    }
                    // The whole row answers, not just the name: aiming at the
                    // text to open something is a nuisance nobody expects.
                    let resp = row.response();
                    // Only what there is something behind. A menu offering
                    // things this window cannot do would be worse than none.
                    // The way out of the folder has nothing behind it, so there is
                    // nothing to offer for it: no menu rather than a menu of
                    // things that would all do nothing.
                    let menu_for = (!r.up).then_some(&resp);
                    if let Some(resp) = menu_for {
                        resp.context_menu(|ui| {
                            // Right clicking something that is not picked picks it,
                            // which is what every file list does.
                            if !picked.get() {
                                clicked = Some(idx);
                                picked.set(true);
                            }
                            // Written out, not the return glyph: the fonts egui ships do
                            // not have it and it came out as an empty box.
                            if ui.button(format!("{}	Enter", s.open_word)).clicked() {
                                opened = Some(idx);
                                ui.close_menu();
                            }
                            if ui
                                .button(format!("{}	Ctrl+E", s.extract_selected))
                                .clicked()
                            {
                                wants_extract.set(true);
                                ui.close_menu();
                            }
                            // Only a file has anything to look at. A folder is a
                            // prefix on some names, not a thing with bytes.
                            if let Some(at) = r.entry {
                                if ui.button(format!("{}	F3", s.view_word)).clicked() {
                                    wants_view.set(Some(at));
                                    ui.close_menu();
                                }
                            }
                            if ui.button(format!("{}	Alt+W", s.extract_here)).clicked() {
                                wants_here.set(true);
                                ui.close_menu();
                            }
                            if ui.button(s.test_selection).clicked() {
                                wants_test.set(true);
                                ui.close_menu();
                            }
                            ui.separator();
                            // The same list the header offers by being clicked, for
                            // the times the pointer is already down here. WinRAR
                            // keeps one in its row menu too.
                            ui.menu_button(s.sort_by, |ui| {
                                for which in std::iter::once(SortColumn::Name).chain(shown.clone())
                                {
                                    let on = self.controller.state.order.0 == which;
                                    let arrow = if !on {
                                        ""
                                    } else if self.controller.state.order.1 {
                                        " \u{25B2}"
                                    } else {
                                        " \u{25BC}"
                                    };
                                    let label = format!("{}{arrow}", Columns::label(which, s));
                                    if ui.selectable_label(on, label).clicked() {
                                        requested = Some(which);
                                        ui.close_menu();
                                    }
                                }
                            });
                            ui.separator();
                            // Only a zip can be written to, so anywhere else this
                            // is left out rather than offered and refused.
                            if self.controller.state.format == Format::Zip
                                && ui.button(format!("{}	F2", s.rename_word)).clicked()
                            {
                                clicked = Some(idx);
                                wants_rename.set(true);
                                ui.close_menu();
                            }
                            if ui.button(format!("{}	Supr", s.delete_word)).clicked() {
                                wants_delete.set(true);
                                ui.close_menu();
                            }
                            ui.separator();
                            // Only where the shell has somewhere to paste them.
                            // Offering a copy that no other window can take would
                            // be worse than not offering one.
                            if clipboard::AVAILABLE {
                                if ui.button(format!("{}	Ctrl+C", s.copy_word)).clicked() {
                                    wants_clip.set(Some(false));
                                    ui.close_menu();
                                }
                                if ui.button(format!("{}	Ctrl+X", s.cut_word)).clicked() {
                                    wants_clip.set(Some(true));
                                    ui.close_menu();
                                }
                                // Always offered rather than greyed out by looking:
                                // the clipboard is one global lock, and opening it
                                // on every frame the menu is up to find out what is
                                // in it would be taking it from whoever else wants
                                // it. An empty one says so in the status bar.
                                if ui.button(format!("{}	Ctrl+V", s.paste_word)).clicked() {
                                    wants_paste.set(true);
                                    ui.close_menu();
                                }
                                ui.separator();
                            }
                            if ui
                                .button(format!("{}	Ctrl+Shift+C", s.copy_names))
                                .clicked()
                            {
                                wants_copy_names.set(true);
                                ui.close_menu();
                            }
                            if ui.button(format!("{}	Ctrl+A", s.select_all)).clicked() {
                                wants_select_all.set(true);
                                ui.close_menu();
                            }
                        });
                    }
                    if resp.clicked() {
                        clicked = Some(idx);
                        left_click = Some(idx);
                    }
                    row_rects.push((idx, resp.rect));
                    if self.controller.state.cursor == Some(idx) {
                        cursor_rect.set(Some(resp.rect));
                    }
                    // Only when the keyboard moved it: doing this every frame
                    // would fight the scroll wheel.
                    if self.controller.state.scroll_to_cursor
                        && self.controller.state.cursor == Some(idx)
                    {
                        resp.scroll_to_me(Some(egui::Align::Center));
                    }
                });
            });

        self.icons = icons;
        self.controller.state.types = types.into_inner();
        self.controller.state.scroll_to_cursor = false;

        // Everything the two menus asked for, now that the table has let go of
        // self. Turning a column off is not allowed to leave the list with
        // nothing but names to look at, so the last one stays.
        if let Some(which) = toggle_column.get() {
            let mut c = self.controller.state.settings.columns;
            let turning_off = c.on(which);
            if !turning_off || shown.len() > 1 {
                c.set(which, !turning_off);
                self.controller.state.settings.columns = c;
                self.controller.state.settings.save();
            }
        }

        // Ctrl adds or removes one, Shift takes everything between here and
        // where the cursor was, and a plain click starts again with just this
        // one. That is what every file list does, and the ticks are the
        // selection here, so this is what they act on.
        if let Some(index) = clicked {
            let mods = ui.input(|i| i.modifiers);
            let target = visible[index].clone();
            if mods.command {
                let value = !self.controller.is_checked(&target);
                self.controller.set_checked(&target, value);
            } else if mods.shift {
                let from = self.controller.state.cursor.unwrap_or(index);
                let (lo, hi) = if from <= index {
                    (from, index)
                } else {
                    (index, from)
                };
                for r in &visible[lo..=hi] {
                    self.controller.set_checked(r, true);
                }
            } else {
                self.controller
                    .state
                    .checked
                    .iter_mut()
                    .for_each(|c| *c = false);
                self.controller.set_checked(&target, true);
            }
            self.controller.state.cursor = Some(index);
        }

        // Opening keeps its own count of clicks rather than asking egui whether
        // this was a double one. egui counts by time alone, and never counts
        // back down to two: after a double click the next one is a triple, and
        // so is the one after that, so opening a second folder straight after
        // the first did nothing until you had waited long enough for the run to
        // lapse. This count starts again every time something opens, so one
        // folder after another works at whatever speed they are clicked.
        if let Some(index) = left_click {
            let (now, plain) = ui.input(|i| (i.time, !i.modifiers.command && !i.modifiers.shift));
            let again = plain
                && self
                    .controller
                    .state
                    .last_click
                    .is_some_and(|(i, t)| i == index && now - t < DOUBLE_CLICK);
            self.controller.state.last_click = if again { None } else { Some((index, now)) };
            if again {
                opened = Some(index);
            }
        }

        if wants_select_all.get() {
            for r in &visible {
                self.controller.set_checked(r, true);
            }
        }
        if wants_copy_names.get() {
            let names = self.controller.selected_names();
            if !names.is_empty() {
                ui.ctx().copy_text(names.join(
                    "
",
                ));
            }
        }
        if wants_delete.get() {
            let names = self.controller.selected_names();
            if !names.is_empty() {
                self.controller.state.confirm_delete = Some(names);
            }
        }
        // The menu asked for a rename of the row it was opened on.
        if wants_rename.get() {
            if let Some(row) = clicked.and_then(|i| visible.get(i)) {
                self.controller.state.renaming = Some((row.path.clone(), row.label.clone()));
                self.controller.state.rename_fresh = true;
            }
        }
        // What the box in the row typed, and whether it was finished or walked
        // away from. Done here rather than inside the table because starting a
        // job needs `self` and the table still had it.
        self.controller.state.rename_fresh = fresh.get();
        if let Some((path, _)) = self.controller.state.renaming.clone() {
            let text = typing.borrow().clone();
            self.controller.state.renaming = Some((path.clone(), text.clone()));
            match finish.get() {
                None => {}
                Some(false) => self.controller.state.renaming = None,
                Some(true) => {
                    self.controller.state.renaming = None;
                    let _ctx = ui.ctx().clone();
                    self.controller.rename_to(&visible, &path, text.trim());
                }
            }
        }
        if wants_extract.get() {
            let _ctx = ui.ctx().clone();
            self.controller.ask_extract(true);
        }
        if let Some(at) = wants_view.get() {
            self.controller.view_entry(at);
        }
        if wants_here.get() {
            let _ctx = ui.ctx().clone();
            self.controller.extract_here();
        }
        if wants_test.get() {
            let names = self.controller.selected_names();
            if let Some(archive) = self.controller.state.archive.clone() {
                let _ctx = ui.ctx().clone();
                self.controller.run_job(Job::Test {
                    archive,
                    only: (!names.is_empty()).then(|| names.into_iter().collect()),
                });
            }
        }
        if let Some(cut) = wants_clip.get() {
            let _ctx = ui.ctx().clone();
            self.controller.copy_to_clipboard(cut);
        }
        if wants_paste.get() {
            let _ctx = ui.ctx().clone();
            self.controller.paste_from_clipboard();
        }

        // A thin outline where the keyboard is, over the fill that says what is
        // ticked. Two different things, so they cannot share the one colour.
        //
        // Only the top and bottom of the row are taken from the row itself. Its
        // rectangle is the union of its cells', and a cell reports itself as
        // wide as whatever was drawn inside it, so the sides came out wherever
        // the longest name happened to end: the outline bit into the icon on
        // the left and hung past the blue on the right. The blue is painted
        // cell by cell, each spread half the gap between columns wider than its
        // column so that the row comes out unbroken, and that is the shape the
        // outline has to follow.
        if let Some(rect) = cursor_rect.get() {
            let half = ui.spacing().item_spacing * 0.5;
            let here = egui::Rect::from_x_y_ranges(
                out.inner_rect.expand(half.x).x_range(),
                rect.expand(half.y).y_range(),
            );
            ui.painter()
                .rect_stroke(here.shrink(0.5), 0.0, theme::cursor(ui.visuals()));
        }

        // The scrollable part on its own, without the header the outer rect
        // takes in, and how far down the list it currently sits: both are what
        // a drag needs to know when it reaches an edge.
        let reach = (out.content_size.y - out.inner_rect.height()).max(0.0);
        if let Some(first) = heads.first() {
            let half = ui.spacing().item_spacing * 0.5;
            ui.painter().set(
                band,
                egui::Shape::rect_filled(
                    egui::Rect::from_x_y_ranges(
                        out.inner_rect.expand(half.x).x_range(),
                        table_top..=first.bottom() + half.y,
                    ),
                    0.0,
                    theme::header(ui.visuals()),
                ),
            );
        }
        let cols: Vec<SortColumn> = std::iter::once(SortColumn::Name)
            .chain(shown.iter().copied())
            .collect();
        self.column_edges(
            ui,
            &heads,
            &slots,
            &cols,
            &visible,
            s,
            table_top,
            // The foot of the panel, not the foot of the rows: the rules run the
            // whole height of the list and the list now ends where the window
            // does.
            ui.max_rect().bottom(),
        );
        self.rubber_band(
            ui,
            &visible,
            &row_rects,
            out.inner_rect,
            out.state.offset.y,
            reach,
        );
        self.carry(ui, &visible, &row_rects, out.inner_rect);
        self.wheel_scroll(ui, out.inner_rect, out.state.offset.y, reach);
        if let Some(index) = opened {
            let target = &visible[index];
            if target.is_dir {
                let path = target.path.clone();
                self.controller.go_to(path);
            } else if let Some(i) = target.entry {
                self.controller.open_file(i);
            }
        }
        if let Some(c) = requested {
            if self.controller.state.order.0 == c {
                self.controller.state.order.1 = !self.controller.state.order.1;
            } else {
                self.controller.state.order = (c, true);
            }
        }
    }
}

impl eframe::App for Arca {
    // Written when the window closes rather than every time it is dragged: the
    // size and place change with every pixel of a resize and the settings file
    // is not a thing to write sixty times a second.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.controller.state.geometry.is_some() {
            self.controller.state.settings.window = self.controller.state.geometry;
            self.controller.state.settings.save();
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Kept fresh every frame because `on_exit` is handed no context to ask.
        // Only a window somebody is browsing in: the small one a job runs in
        // would otherwise be what came back next time.
        if matches!(self.controller.state.view, View::Browse) {
            if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                self.controller.state.geometry =
                    Some([rect.min.x, rect.min.y, rect.width(), rect.height()]);
            }
        }
        self.controller.ask_about_updates();
        if self.controller.receive() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(
            self.controller.state.window_title.clone(),
        ));
        // Getting the keyboard back is when whatever was done elsewhere has
        // been done. Only on the change, not every frame it is focused.
        let focused = ctx.input(|i| i.focused);
        if focused
            && !self.controller.state.was_focused
            && matches!(self.controller.state.view, View::Browse)
        {
            self.controller.cut_landed();
        }
        self.controller.state.was_focused = focused;
        self.shortcuts(ctx);

        // Escape backs out of whatever is on top, innermost first, the way it
        // does everywhere else. The password prompt goes through the same path
        // as its Cancel button so a cancelled job is cancelled once.
        // One place, and a switch rather than an opening: handled where the
        // window is drawn as well, the same press would open it and close it
        // again inside the one frame.
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            self.controller.state.show_shortcuts = !self.controller.state.show_shortcuts;
        }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.controller.state.waiting_on_password.is_some() {
                self.controller.cancel_password();
            } else if self.controller.state.show_shortcuts {
                self.controller.state.show_shortcuts = false;
            } else if self.controller.state.show_settings {
                self.controller.state.show_settings = false;
            // The viewer and the group box close themselves on Escape, and
            // closing one of them is all that press was for: it must not also
            // let go of everything that was picked underneath.
            } else if self.controller.state.viewing.is_some()
                || self.controller.state.picking_group.is_some()
            {
            } else if matches!(self.controller.state.view, View::Browse)
                && !self.controller.state.busy
            {
                // Nothing on top of the list any more, so it backs out of the
                // last thing there is to back out of: what is picked.
                self.controller.clear_picked();
            }
        }

        // The side buttons on a mouse, which winit reports as Back and Forward
        // and egui hands over as Extra1 and Extra2. Alt+Left and Alt+Right do
        // the same, for anyone without them.
        if matches!(self.controller.state.view, View::Browse) && !self.controller.state.busy {
            let (back, forward) = ctx.input(|i| {
                (
                    i.pointer.button_pressed(egui::PointerButton::Extra1)
                        || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)),
                    i.pointer.button_pressed(egui::PointerButton::Extra2)
                        || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight)),
                )
            });
            if back {
                self.controller.go_back();
            }
            if forward {
                self.controller.go_forward();
            }
        }

        // Not while something is running or a window is waiting on an answer:
        // a drop that lands then would be acting on a state that is about to
        // change under it.
        if matches!(self.controller.state.view, View::Browse)
            && !self.controller.state.busy
            && self.controller.state.confirm_delete.is_none()
            && self.controller.state.confirm_drop.is_none()
            && self.controller.state.waiting_on_password.is_none()
        {
            let dropped: Vec<PathBuf> = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect()
            });
            self.controller.dropped(dropped);
        }

        if self.controller.state.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let ctx2 = ctx.clone();
        match self.controller.state.view {
            View::Running => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.running_view(ui, &ctx2);
                });
                self.conflict_window(&ctx2);
                self.password_window(&ctx2);
            }
            View::Add => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.add_view(ui, &ctx2);
                });
            }
            View::Browse => {
                egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
                    self.toolbar(ui, &ctx2);
                });
                self.settings_window(&ctx2);
                self.shortcuts_window(&ctx2);
                self.group_window(&ctx2);
                self.viewer_window(&ctx2);
                self.default_password_window(&ctx2);
                self.new_folder_window(&ctx2);
                self.conflict_window(&ctx2);
                self.password_window(&ctx2);
                self.confirm_delete_window(&ctx2);
                self.confirm_drop_window(&ctx2);
                // An exact height with the content centred inside it. Padding
                // above and below looked symmetrical in the source and was not
                // on screen: the text sat high in the bar.
                egui::TopBottomPanel::bottom("status")
                    .exact_height(30.0)
                    .show(ctx, |ui| {
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            if self.controller.state.busy && !self.controller.state.quiet {
                                let f = if self.controller.state.total_count == 0 {
                                    0.0
                                } else {
                                    self.controller.state.done_count as f32
                                        / self.controller.state.total_count as f32
                                };
                                ui.add(
                                    egui::ProgressBar::new(f)
                                        .text(format!(
                                            "{} / {}",
                                            self.controller.state.done_count,
                                            self.controller.state.total_count
                                        ))
                                        .desired_width(ui.available_width()),
                                );
                            } else {
                                let color = if self.controller.state.error {
                                    egui::Color32::from_rgb(220, 90, 90)
                                } else {
                                    ui.visuals().weak_text_color()
                                };
                                ui.colored_label(color, &self.controller.state.notice);
                            }
                        });
                    });
                // No gap under the list: it ends against the status bar, the
                // way a file list ends against the bottom of its window
                // everywhere else. The margin left there was the reason the
                // rules between the columns stopped short of the foot.
                self.tree_panel(ctx);
                let mut frame = egui::Frame::central_panel(&ctx.style());
                frame.inner_margin.bottom = 0.0;
                egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
                    if self.controller.state.entries.is_empty() {
                        let text = self.controller.s().drop_here;
                        ui.centered_and_justified(|ui| {
                            ui.label(egui::RichText::new(text).size(16.0).weak());
                        });
                        return;
                    }
                    // The list sits on the window, not in a card on it. It was
                    // given a fill, a border and rounded corners back when the
                    // column rules were gone and the rows had no edge to end
                    // against; the rules are back and they do that job, so all
                    // the card left was a box drawn inside a box.
                    self.table(ui);
                });
                // Over the list, and last, so it is drawn on top of everything
                // it is dimming.
                self.progress_window(&ctx2);
                self.drop_hint(&ctx2);
            }
        }
    }
}

#[cfg(not(feature = "gpui"))]
fn main() -> eframe::Result<()> {
    let startup = parse_args();
    let settings = Settings::load();
    let compact = !matches!(startup, Startup::Browse(_));

    // The size and place the window was left at, when there is one and this is
    // a window somebody is going to browse in. The little window a job runs in
    // is a different shape and a different job, and giving it the browsing
    // window's size would open a progress bar the size of a desk.
    let remembered = (!compact).then_some(settings.window).flatten();
    let size = match remembered {
        Some([_, _, w, h]) => [w, h],
        None if compact => [560.0, 300.0],
        None => [1000.0, 660.0],
    };
    // The icon compiled into the executable covers the Explorer and the
    // shortcut, but winit does not read it for the window itself, so the title
    // bar and the taskbar keep the generic one unless it is set here too.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        // Wide enough for the row of commands, now that every one of them says
        // its name. Below this the filter box at the end of it has no width
        // left to be given and the bar starts running off its own edge.
        .with_min_inner_size([720.0, 320.0])
        .with_title("Arca");
    if let Some([x, y, _, _]) = remembered {
        viewport = viewport.with_position([x, y]);
    }
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Arca",
        options,
        Box::new(move |cc| {
            // What lets the viewer draw a picture straight from the bytes it has
            // in memory, with no file on disk for it to point at.
            egui_extras::install_image_loaders(&cc.egui_ctx);
            cc.egui_ctx.set_fonts(theme::fonts());
            // Both, not just the one in use: the setting can be changed while
            // the window is open, and egui keeps a style per theme.
            cc.egui_ctx.set_visuals_of(egui::Theme::Dark, theme::dark());
            cc.egui_ctx
                .set_visuals_of(egui::Theme::Light, theme::light());
            cc.egui_ctx.all_styles_mut(theme::style);
            cc.egui_ctx.set_theme(settings.theme);

            let mut app = Arca::new(AppController::new(settings));
            match startup {
                Startup::Browse(Some(p)) => app.controller.open(p),
                Startup::Browse(None) => {}
                Startup::Run(job) => app.controller.run_job(job),
                Startup::Add(files) => {
                    app.controller.state.output_name =
                        quick_output(&files, app.controller.state.format)
                            .file_name()
                            .map(|x| x.to_string_lossy().to_string())
                            .unwrap_or_default();
                    app.controller.state.pending_inputs = files;
                    app.controller.state.view = View::Add;
                }
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(feature = "gpui")]
fn main() {
    gpui_shell::run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_attribute_letters_hold_their_places() {
        assert_eq!(attribute_letters(0), "----");
        assert_eq!(attribute_letters(0x01), "R---");
        assert_eq!(attribute_letters(0x20), "---A");
        assert_eq!(attribute_letters(0x01 | 0x02 | 0x04 | 0x20), "RHSA");
        // The directory bit is the list's job, not this column's.
        assert_eq!(attribute_letters(0x10), "----");
    }

    #[test]
    fn text_is_told_from_the_rest_by_what_it_does_not_have() {
        assert!(looks_like_text(b""), "an empty file opens as an empty page");
        assert!(looks_like_text(b"hola\r\nque tal\ttabulado\n"));
        assert!(
            looks_like_text("acentos y enes: aeiou \u{f1}\u{e1}".as_bytes()),
            "high bytes are a name with an accent, not a program"
        );
        assert!(
            !looks_like_text(b"MZ\x90\x00\x03\x00\x00\x00"),
            "a zero settles it"
        );
        // No zeros, but nothing readable either.
        let noise: Vec<u8> = (1..=200u8).map(|b| b % 0x1F + 1).collect();
        assert!(!looks_like_text(&noise));
    }

    #[test]
    fn a_later_version_is_the_one_with_the_larger_numbers() {
        assert!(newer("0.5.1", "0.6.0"));
        assert!(newer("0.9.0", "0.10.0"), "ten comes after nine");
        assert!(newer("0.5.1", "v0.5.2"), "a leading v is forgiven");
        assert!(!newer("0.6.0", "0.5.9"), "older is not newer");
        assert!(!newer("0.6.0", "0.6.0"), "the same is not newer");
        assert!(!newer("0.10.0", "0.9.0"), "and the other way round too");
        // Missing parts are zero, so these are the same version.
        assert!(!newer("0.6", "0.6.0"));
        assert!(!newer("0.6.0", "0.6"));
        // A release candidate is earlier than the release it is a candidate for.
        assert!(newer("0.6.0-rc1", "0.6.0"));
        assert!(!newer("0.6.0", "0.6.0-rc1"));
    }

    // Anything unreadable has to answer no. A window that cannot tell what the
    // announcement said should say nothing, not guess.
    #[test]
    fn nonsense_never_announces_an_update() {
        assert!(!newer("0.5.1", ""));
        assert!(!newer("0.5.1", "manana"));
        assert!(!newer("0.5.1", "0.5.uno"));
        assert!(!newer("", "9.9.9"));
        assert!(
            !newer("0.5.1", "99999999999999999999"),
            "past what a number holds"
        );
    }

    #[test]
    fn the_release_name_comes_out_of_the_reply() {
        let reply = r#"{"url":"https://x/1","tag_name":"v0.6.0","name":"Arca 0.6.0"}"#;
        assert_eq!(tag_of(reply).as_deref(), Some("v0.6.0"));
        // Spacing is the writer's business, not ours.
        assert_eq!(
            tag_of(r#"{ "tag_name" : "0.7.0" }"#).as_deref(),
            Some("0.7.0")
        );
        // And everything that is not an answer is not an answer.
        assert_eq!(tag_of("{}"), None);
        assert_eq!(tag_of(""), None);
        assert_eq!(tag_of(r#"{"tag_name":""}"#), None, "a name of nothing");
        assert_eq!(tag_of(r#"{"tag_name":"x"}"#).as_deref(), Some("x"));
        let long = format!(r#"{{"tag_name":"{}"}}"#, "v".repeat(64));
        assert_eq!(tag_of(&long), None, "somebody being funny");
    }

    #[test]
    fn la_respuesta_de_la_release_da_version_instalador_y_sumas() {
        let reply = r#"{"url":"https://api.github.com/x","tag_name":"v0.6.2","assets":[
            {"name":"SHA256SUMS.txt","browser_download_url":"https://github.com/THIONG/arca/releases/download/v0.6.2/SHA256SUMS.txt"},
            {"name":"arca-setup-0.6.2-x86_64.exe","browser_download_url":"https://github.com/THIONG/arca/releases/download/v0.6.2/arca-setup-0.6.2-x86_64.exe"},
            {"name":"arca-v0.6.2-linux-x86_64.tar.gz","browser_download_url":"https://github.com/THIONG/arca/releases/download/v0.6.2/arca-v0.6.2-linux-x86_64.tar.gz"}]}"#;
        let r = release_of(reply).expect("una release");
        assert_eq!(r.tag, "v0.6.2");
        assert_eq!(
            r.installer.as_deref(),
            Some("https://github.com/THIONG/arca/releases/download/v0.6.2/arca-setup-0.6.2-x86_64.exe")
        );
        assert!(r.sums.is_some());
    }

    #[test]
    fn una_direccion_que_no_sea_la_nuestra_no_se_acepta() {
        let reply = r#"{"tag_name":"v9.9.9","assets":[
            {"browser_download_url":"https://evil.example/arca-setup-9.9.9-x86_64.exe"},
            {"browser_download_url":"http://github.com/THIONG/arca/releases/download/v9/arca-setup-9-x86_64.exe"},
            {"browser_download_url":"https://github.com/otro/arca/releases/download/v9/arca-setup-9-x86_64.exe"},
            {"browser_download_url":"https://github.com/THIONG/arca/releases/download/v9/SHA256SUMS.txt"}]}"#;
        let r = release_of(reply).expect("una release");
        assert_eq!(r.tag, "v9.9.9");
        assert!(
            r.installer.is_none(),
            "ni otro dominio, ni sin cifrar, ni otro repositorio"
        );
        assert!(r.sums.is_some(), "la nuestra si");
    }

    // `sha256sum` escribe dos espacios para lo que leyo como texto y espacio y
    // asterisco para lo que leyo como binario. Las mitades de Windows de
    // nuestras propias releases salen con el asterisco.
    #[test]
    fn la_suma_se_encuentra_con_las_dos_escrituras() {
        let listing = "\
8c0a3844b53278b6fb1557c2cdc28f5c6b0a2eaf29e6214a734798d81473b5f3  arca-v0.6.1-linux-x86_64.tar.gz
c76ecf12e8e05b8f4730fb9933450ea121fe1ce3699ab9b7d20b058f872815f4 *arca-setup-0.6.1-x86_64.exe
";
        let binario = sum_for(listing, "arca-setup-0.6.1-x86_64.exe").expect("la del exe");
        assert_eq!(binario[0], 0xc7);
        assert_eq!(binario[31], 0xf4);
        let texto = sum_for(listing, "arca-v0.6.1-linux-x86_64.tar.gz").expect("la del tar");
        assert_eq!(texto[0], 0x8c);
        assert!(sum_for(listing, "arca-setup-0.6.2-x86_64.exe").is_none());
    }

    // Five settings were read at startup and never written, because the line
    // that built the file had quietly stopped mentioning them. A round trip
    // that never touches a disk is the only way that stays fixed.
    #[test]
    fn every_setting_survives_being_written_and_read_again() {
        let mut before = Settings {
            lang: Some(Lang::Es),
            theme: ThemePreference::Light,
            flat: true,
            tree: true,
            updates: false,
            page: arca_zip::pages::Page::Cp1252,
            ..Default::default()
        };
        before.columns.set(SortColumn::Crc, true);
        before.columns.set(SortColumn::Size, false);
        before.widths[0] = 271.0;
        before.widths[3] = 88.0;
        before.window = Some([12.0, 34.0, 1000.0, 700.0]);
        before.recent = vec!["C:\\uno.zip".into(), "D:\\dos, con coma.zip".into()];

        let after = Settings::parse(&before.text());

        assert_eq!(after.lang, before.lang);
        assert_eq!(after.theme, before.theme);
        assert_eq!(after.flat, before.flat);
        assert_eq!(after.tree, before.tree);
        assert_eq!(after.updates, before.updates);
        assert_eq!(after.page, before.page);
        assert_eq!(after.widths, before.widths);
        assert_eq!(after.window, before.window);
        assert_eq!(
            after.recent, before.recent,
            "y una coma en un nombre no parte nada"
        );
        for (which, name) in Columns::ALL {
            assert_eq!(
                after.columns.on(which),
                before.columns.on(which),
                "la columna {name}"
            );
        }
    }

    #[test]
    fn moving_reads_an_archive_written_with_backslashes() {
        // Windows's own Compress-Archive writes these, and the window shows and
        // compares forward slashes. Before this they matched nothing and a
        // move inside a folder did nothing without saying so.
        let moves = vec![("carpeta/f1.txt".to_string(), "f1.txt".to_string())];
        assert_eq!(moved_name(r"carpeta\f1.txt", &moves), "f1.txt");
        assert_eq!(moved_name(r"carpeta\f2.txt", &moves), "carpeta/f2.txt");
    }

    #[test]
    fn moving_carries_a_whole_branch_and_leaves_everything_else_alone() {
        let moves = vec![
            ("docs/notas".to_string(), "notas".to_string()),
            ("leeme.txt".to_string(), "docs/leeme.txt".to_string()),
        ];
        let of = |n: &str| moved_name(n, &moves);

        // The folder, both ways it can be written, and what is under it.
        assert_eq!(of("docs/notas"), "notas");
        assert_eq!(of("docs/notas/"), "notas/");
        assert_eq!(of("docs/notas/uno.md"), "notas/uno.md");
        assert_eq!(of("docs/notas/dos/tres.md"), "notas/dos/tres.md");
        // A file on its own.
        assert_eq!(of("leeme.txt"), "docs/leeme.txt");
        // Everything else, including names that begin the same way and are not
        // the same folder at all.
        assert_eq!(of("docs/notas2/otro.md"), "docs/notas2/otro.md");
        assert_eq!(of("docs/uno.txt"), "docs/uno.txt");
        assert_eq!(of("leeme.txt.bak"), "leeme.txt.bak");
        assert_eq!(of("otra/cosa.bin"), "otra/cosa.bin");
    }

    #[test]
    fn a_mask_picks_the_names_it_describes() {
        assert!(matches_mask("*.txt", "notes.txt"));
        assert!(
            matches_mask("*.TXT", "notes.txt"),
            "case is not the question"
        );
        assert!(!matches_mask("*.txt", "notes.txt.bak"));
        assert!(matches_mask("nota_?.md", "nota_3.md"));
        assert!(!matches_mask("nota_?.md", "nota_33.md"));
        assert!(matches_mask("*", "anything at all"));
        assert!(matches_mask("a*b*c", "axxbyyc"));
        assert!(!matches_mask("a*b*c", "axxbyy"));
        // A mask with nothing special in it is just a name.
        assert!(matches_mask("leeme.txt", "leeme.txt"));
        assert!(!matches_mask("leeme.txt", "leeme.txt.old"));
    }

    // A row of stars against a long name is the case that turns a naive
    // recursive matcher into a hang. It has to come back in no time at all.
    #[test]
    fn a_mask_of_nothing_but_stars_does_not_take_all_afternoon() {
        let name = "a".repeat(64);
        let mask = format!("{}b", "*a".repeat(20));
        let began = std::time::Instant::now();
        assert!(!matches_mask(&mask, &name));
        assert!(began.elapsed().as_millis() < 50, "backtracking ran away");
    }

    // The wheel is a button as well as a wheel, and pressing one moves the
    // hand: a dead zone is the difference between a list that waits and a list
    // that creeps for as long as the anchor is down.
    #[test]
    fn a_hand_resting_on_the_wheel_leaves_the_list_where_it_is() {
        assert_eq!(wheel_speed(0.0), 0.0);
        assert_eq!(wheel_speed(-8.0), 0.0);
        assert_eq!(wheel_speed(12.0), 0.0);
    }

    #[test]
    fn the_list_runs_the_way_the_pointer_went_and_harder_the_further_it_is() {
        assert!(wheel_speed(40.0) > 0.0);
        assert!(wheel_speed(-40.0) < 0.0);
        assert!(wheel_speed(90.0) > wheel_speed(40.0));
        // Up and down are the same gesture mirrored, and a list that ran
        // faster one way than the other would be maddening rather than wrong.
        assert_eq!(wheel_speed(40.0), -wheel_speed(-40.0));
        // Far enough out and it stops getting faster: everything past here is
        // a blur either way.
        assert_eq!(wheel_speed(900.0), wheel_speed(2000.0));
    }

    // The only arithmetic in this file that can be wrong without anyone
    // noticing: a date is either right or plausible, and plausible is worse.
    #[test]
    fn timestamps_become_the_dates_they_are() {
        for (secs, text) in [
            (0_i64, ""), // no date recorded
            (-1, ""),    // before the epoch: tar can hold these
            (1, "1970-01-01 00:00"),
            (951_827_696, "2000-02-29 12:34"), // leap day of a leap century
            (1_078_012_800, "2004-02-29 00:00"), // ordinary leap year
            (1_709_164_800, "2024-02-29 00:00"),
            (1_709_251_199, "2024-02-29 23:59"), // last minute of that day
            (1_735_689_600, "2025-01-01 00:00"), // year boundary
            (1_767_225_599, "2025-12-31 23:59"),
            (2_208_988_800, "2040-01-01 00:00"), // past a 32-bit second count
        ] {
            assert_eq!(when(Some(secs)), text, "{secs}");
        }
        assert_eq!(when(None), "");
    }

    // A folder keeps its whole name and a file loses its extension, and the
    // shell extension has a copy of this that has to agree.
    // Which way the sort mark points is a sign, and a sign is the one thing you
    // cannot check by looking at a screenshot of a list with one row in it.
    #[test]
    fn the_clock_reads_as_a_clock() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(7.4), "0:07");
        assert_eq!(clock(98.0), "1:38");
        assert_eq!(clock(3600.0), "1:00:00");
        assert_eq!(clock(7511.0), "2:05:11");
        // A guess made from almost nothing, and one made from nonsense.
        assert_eq!(clock(-5.0), "0:00");
        assert_eq!(clock(f64::NAN), "0:00");
        assert_eq!(clock(f64::INFINITY), "0:00");
    }

    #[test]
    fn the_sort_mark_points_up_when_the_sort_goes_up() {
        let c = egui::pos2(50.0, 50.0);

        let up = sort_mark(c, true);
        // Two along the bottom and the point above them. Larger y is lower down.
        assert_eq!(up[0].y, up[1].y, "the base is level");
        assert!(up[2].y < up[0].y, "the point is above the base");
        assert!(
            up[0].x < c.x && up[1].x > c.x,
            "the base straddles the centre"
        );
        assert_eq!(up[2].x, c.x, "the point is centred");

        let down = sort_mark(c, false);
        assert_eq!(down[0].y, down[1].y);
        assert!(down[2].y > down[0].y, "the point is below the base");

        // One is the other turned over, and neither leaves the little square it
        // is drawn in.
        assert_eq!(up[2].y - c.y, -(down[2].y - c.y));
        for p in up.iter().chain(down.iter()) {
            assert!((p.x - c.x).abs() <= 5.5 && (p.y - c.y).abs() <= 5.5);
        }
    }

    // The path row is the one place the window can run out of width, and it did:
    // a deep folder pushed the trail out over the count at the other end.
    #[test]
    fn a_path_too_long_gives_up_its_oldest_folders_first() {
        let sep = 10.0;
        let dots = 8.0;
        // Five folders of 100 each: 500 of names plus 40 of separators.
        let five = [100.0_f32; 5];

        // Room to spare: nothing hidden.
        assert_eq!(crumbs_hidden(&five, sep, dots, 600.0), 0);
        // Exactly enough: still nothing.
        assert_eq!(crumbs_hidden(&five, sep, dots, 540.0), 0);
        // One short. Dropping the first leaves 400 + 30 + 18 for the mark = 448.
        assert_eq!(crumbs_hidden(&five, sep, dots, 539.0), 1);
        assert_eq!(crumbs_hidden(&five, sep, dots, 448.0), 1);
        assert_eq!(crumbs_hidden(&five, sep, dots, 447.0), 2);
        // Absurdly narrow: everything goes but the folder you are in, which is
        // left to be cut short rather than dropped.
        assert_eq!(crumbs_hidden(&five, sep, dots, 10.0), 4);
        // A single folder is that folder, whatever the width.
        assert_eq!(crumbs_hidden(&[100.0], sep, dots, 1.0), 0);
        // One long name in the middle is not a reason to drop the ones after it.
        let uneven = [40.0_f32, 900.0, 40.0, 40.0];
        assert_eq!(crumbs_hidden(&uneven, sep, dots, 200.0), 2);
    }

    #[test]
    fn archive_stem_strips_what_it_should() {
        for (name, stem) in [
            ("game.zip", "game"),
            ("backup.tar.gz", "backup"),
            ("backup.tgz", "backup"),
            ("plain.tar", "plain"),
            ("UPPER.ZIP", "UPPER"),
            ("dots.in.name.zip", "dots.in.name"),
            ("no-extension", "no-extension"),
        ] {
            assert_eq!(archive_stem(Path::new(name)), stem, "{name}");
        }
    }

    #[test]
    fn turning_columns_on_and_off_survives_a_round_trip() {
        let mut c = Columns::default();
        c.set(SortColumn::Crc, true);
        c.set(SortColumn::Packed, false);
        assert!(c.on(SortColumn::Crc));
        assert!(!c.on(SortColumn::Packed));
        // Name is not a column anyone may turn off.
        c.set(SortColumn::Name, false);
        assert!(c.on(SortColumn::Name));
    }
}
