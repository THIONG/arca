#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clipboard;
mod i18n;
mod theme;
mod tree;

use arca_core::{Codec, Entry, Level};
use arca_tar::{TarReader, TarWriter};
use arca_zip::{ZipArchive, ZipWriter};
use eframe::egui;
use rayon::prelude::*;
use egui::ThemePreference;
use egui_extras::{Column, TableBuilder};
use i18n::{strings, Lang, Strings};
use tree::{children_of, draw_icon, entries_under, kind_of, parent_of, Row};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;

const BUF: usize = 256 * 1024;
const ROW_HEIGHT: f32 = 23.0;
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
// Below the last row that gives the last row, and above the first it clamps to
// the first: a drag that has run off one end of the list is still asking for
// everything up to that end.
fn row_at(rects: &[(usize, egui::Rect)], y: f32) -> Option<usize> {
    let (first, top) = *rects.first()?;
    if y <= top.top() {
        return Some(first);
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lang: None,
            theme: ThemePreference::System,
            columns: Columns::default(),
        }
    }
}

impl Settings {
    fn load() -> Self {
        let mut s = Settings::default();
        let Some(p) = config_file() else { return s };
        let Ok(text) = fs::read_to_string(p) else {
            return s;
        };
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
        let lang = self.lang.map(|l| l.code()).unwrap_or("system");
        let theme = match self.theme {
            ThemePreference::Light => "light",
            ThemePreference::Dark => "dark",
            ThemePreference::System => "system",
        };
        let columns: Vec<&str> = Columns::ALL
            .iter()
            .filter(|(which, _)| self.columns.on(*which))
            .map(|(_, name)| *name)
            .collect();
        let _ = fs::write(
            p,
            format!(
                "lang = {lang}\ntheme = {theme}\ncolumns = {}\n",
                columns.join(",")
            ),
        );
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

// A folder of its own per archive, so two archives holding a file with the same
// name do not overwrite each other's copy. safe_name is what keeps an entry
// called "../../evil" from landing outside it.
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
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(path)
        .spawn()
        .map_err(arca_core::Error::Io)?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arrow {
    Left,
    Right,
    Up,
}

// Painted rather than written. The arrows that would say this in text are not
// in the fonts egui ships by default, and a button showing a hollow box is
// worse than no button at all.
fn arrow_button(ui: &mut egui::Ui, dir: Arrow, enabled: bool, tip: &str) -> egui::Response {
    let size = egui::vec2(28.0, 22.0);
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
        let fill = if enabled {
            visuals.weak_bg_fill
        } else {
            ui.visuals().widgets.noninteractive.weak_bg_fill
        };
        let stroke = if enabled {
            visuals.fg_stroke.color
        } else {
            ui.visuals().widgets.noninteractive.fg_stroke.color
        };
        ui.painter().rect(rect, 3.0, fill, visuals.bg_stroke);

        let c = rect.center();
        let w = 4.5;
        let h = 5.5;
        let p = match dir {
            Arrow::Left => [
                egui::pos2(c.x + w * 0.6, c.y - h),
                egui::pos2(c.x + w * 0.6, c.y + h),
                egui::pos2(c.x - w, c.y),
            ],
            Arrow::Right => [
                egui::pos2(c.x - w * 0.6, c.y - h),
                egui::pos2(c.x - w * 0.6, c.y + h),
                egui::pos2(c.x + w, c.y),
            ],
            Arrow::Up => [
                egui::pos2(c.x - h, c.y + w * 0.6),
                egui::pos2(c.x + h, c.y + w * 0.6),
                egui::pos2(c.x, c.y - w),
            ],
        };
        ui.painter()
            .add(egui::Shape::convex_polygon(p.to_vec(), stroke, egui::Stroke::NONE));
    }

    if enabled {
        response.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
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
    ctx: &'a egui::Context,
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
        ctx.request_repaint();
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
    notify: &(dyn Fn(usize, usize, &str) + Sync),
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
                    notify(done.fetch_add(1, Ordering::Relaxed) + 1, total, &e.name);
                    Ok(w)
                })
                .collect::<arca_core::Result<Vec<u64>>>()?;
            bytes = written.iter().sum();
            notify(total, total, "");
        }
        _ => {
            let mut r = TarReader::new(open_source(archive, format)?);
            let total = wanted.len();
            let mut i = 0usize;
            while let Some(e) = r.next_entry()? {
                notify(i, total, &e.entry.name);
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
            notify(i, i, "");
        }
    }
    Ok(bytes)
}

fn test_archive(
    archive: &Path,
    notify: &(dyn Fn(usize, usize, &str) + Sync),
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
                notify(i, total, &name);
                if a.entries()[i].is_dir {
                    continue;
                }
                match a.extract_to(i, std::io::sink()) {
                    Ok(_) => good += 1,
                    Err(e) => bad.push(format!("{name}: {e}")),
                }
            }
            notify(total, total, "");
        }
        _ => {
            let mut r = TarReader::new(open_source(archive, format)?);
            let mut i = 0usize;
            while let Some(e) = r.next_entry()? {
                notify(i, i + 1, &e.entry.name);
                if e.entry.is_dir {
                    r.skip_data(&e)?;
                } else {
                    match r.copy_data(&e, &mut std::io::sink()) {
                        Ok(_) => good += 1,
                        Err(err) => bad.push(format!("{}: {err}", e.entry.name)),
                    }
                }
                i += 1;
            }
            notify(i, i, "");
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
    notify: &(dyn Fn(usize, usize, &str) + Sync),
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
                notify(i, total, name);
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
                notify(i, total, name);
                let meta = fs::metadata(path)?;
                let f = BufReader::with_capacity(BUF, File::open(path)?);
                w.add(name, meta.len(), 0, 0o644, f)?;
                source_bytes += meta.len();
            }
            w.finish()?;
        }
    }
    notify(total, total, "");
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
    Test(PathBuf),
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
        "--test" if !rest.is_empty() => Startup::Run(Job::Test(rest[0].clone())),
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

fn run_job_blocking(
    job: Job,
    s: &'static Strings,
    notify: &(dyn Fn(usize, usize, &str) + Sync),
    ask: &dyn Fn(&Path) -> Answer,
) -> std::result::Result<String, String> {
    match job {
        Job::Extract { archives, dest, password } => {
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
        Job::Test(a) => {
            if detect(&a).is_none() {
                return Err(s.unknown_format.to_string());
            }
            let (good, bad) = test_archive(&a, notify).map_err(|e| e.to_string())?;
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
            fs::rename(&temp, &archive).map_err(|e| e.to_string())?;
            Ok(fill(s.deleted, &[("n", &gone.to_string())]))
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
                &out, &inputs, format, codec, level, notify, password.as_deref(),
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
                    source,
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
            fs::rename(&temp, &archive).map_err(|e| e.to_string())?;
            Ok(fill(s.added, &[("n", &n.to_string())]))
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
}

impl Default for Columns {
    fn default() -> Self {
        // What was on screen before any of this was a choice, plus the date,
        // which both WinRAR and NanaZip show and which people look for.
        Columns { size: true, packed: true, method: true, saved: true, modified: true, crc: false }
    }
}

impl Columns {
    const ALL: [(SortColumn, &'static str); 6] = [
        (SortColumn::Size, "size"),
        (SortColumn::Packed, "packed"),
        (SortColumn::Method, "method"),
        (SortColumn::Saved, "saved"),
        (SortColumn::Modified, "modified"),
        (SortColumn::Crc, "crc"),
    ];

    fn on(&self, which: SortColumn) -> bool {
        match which {
            SortColumn::Size => self.size,
            SortColumn::Packed => self.packed,
            SortColumn::Method => self.method,
            SortColumn::Saved => self.saved,
            SortColumn::Modified => self.modified,
            SortColumn::Crc => self.crc,
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
            SortColumn::Name => s.col_name,
        }
    }
}

enum Message {
    Listing(PathBuf, Vec<Entry>),
    Conflict(String),
    Progress(usize, usize, String),
    Done(String),
    Failed(String),
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

struct Arca {
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
    icons: HashMap<String, Option<egui::TextureHandle>>,
    // Where a rubber band started, and what was ticked before it did. The
    // second is what lets the band be recomputed from scratch every frame, so
    // dragging back over a row lets go of it again.
    band: Option<egui::Pos2>,
    band_base: Vec<bool>,
    // The row the drag started on, and the scroll position it is dragging the
    // list to when it runs off an edge. Row numbers rather than places on
    // screen, because the list moves while the drag is happening.
    band_anchor: Option<usize>,
    band_scroll: Option<f32>,
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
    // What the last Ctrl+X put on the clipboard, so those rows can show it. The
    // archive is not touched: see `copy_to_clipboard`.
    cut_names: HashSet<String>,
}

impl Arca {
    fn new(settings: Settings) -> Self {
        Arca {
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
            icons: HashMap::new(),
            band: None,
            band_base: Vec::new(),
            confirm_delete: None,
            scroll_to_cursor: false,
            clip_dir: None,
            cut_names: HashSet::new(),
            confirm_drop: None,
            band_anchor: None,
            band_scroll: None,
        }
    }

    fn s(&self) -> &'static Strings {
        strings(self.settings.effective_lang())
    }

    fn level_name(&self, l: Level) -> &'static str {
        let s = self.s();
        match l {
            Level::Store => s.level_none,
            Level::Fast => s.level_fast,
            Level::Normal => s.level_normal,
            Level::Best => s.level_best,
        }
    }

    fn codec_name(&self, c: Codec) -> &'static str {
        let s = self.s();
        match c {
            Codec::Store => s.codec_store,
            Codec::Deflate => s.codec_deflate,
            Codec::Zstd => s.codec_zstd,
        }
    }

    fn summary(&self) -> String {
        let s = self.s();
        let n = self.entries.iter().filter(|e| !e.is_dir).count();
        let raw: u64 = self.entries.iter().map(|e| e.size).sum();
        let packed: u64 = self.entries.iter().map(|e| e.compressed_size).sum();
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

    fn visible_rows(&self) -> Vec<Row> {
        let filter = self.filter.trim().to_lowercase();
        let mut rows = if filter.is_empty() {
            children_of(&self.entries, &self.current_dir)
        } else {
            self.entries
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
                    crc32: e.crc32,
                })
                .collect()
        };

        let (col, asc) = self.order;
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
            };
            if asc {
                o
            } else {
                o.reverse()
            }
        });
        rows
    }

    fn spawn<F>(&mut self, ctx: &egui::Context, total: usize, work: F)
    where
        F: FnOnce(&Sender<Message>) + Send + 'static,
    {
        let (tx, rx) = channel();
        self.channel = Some(rx);
        self.busy = true;
        self.error = false;
        self.done_count = 0;
        self.total_count = total;
        self.current_file.clear();
        self.started = Some(Instant::now());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            work(&tx);
            ctx.request_repaint();
        });
    }

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.archive_password = None;
        // Whatever was cut belonged to the listing being replaced, and so did
        // whatever the status bar was saying: the summary of the archive being
        // closed sat there over the one that had just opened.
        self.cut_names.clear();
        self.notice.clear();
        self.error = false;
        let ctx2 = ctx.clone();
        self.spawn(ctx, 0, move |tx| {
            let m = match list_entries(&path) {
                Ok(v) => Message::Listing(path, v),
                Err(e) => Message::Failed(e.to_string()),
            };
            let _ = tx.send(m);
            ctx2.request_repaint();
        });
    }

    // Reading the central directory is enough to know whether the archive is
    // encrypted, and costs nothing next to extracting it. Asking here, before
    // any work starts, keeps the question on the window's own thread.
    fn run_job(&mut self, ctx: &egui::Context, job: Job) {
        if let Job::Extract { archives, password: None, .. } = &job {
            if archives.iter().any(|a| is_encrypted(a)) {
                self.password_input.clear();
                self.waiting_on_password = Some(Pending::Extract(Box::new(job)));
                self.view = View::Running;
                self.title = self.s().extracting.to_string();
                ctx.request_repaint();
                return;
            }
        }
        let s: &'static Strings = self.s();
        self.view = View::Running;
        self.title = match &job {
            Job::Extract { .. } => s.extracting.to_string(),
            Job::Test(_) => s.testing.to_string(),
            Job::Password { .. } => s.changing_password.to_string(),
            Job::Delete { .. } => s.deleting.to_string(),
            Job::Compress { .. } => s.compressing.to_string(),
            Job::Add { .. } => s.adding.to_string(),
        };
        self.close_when_done = !matches!(
            job,
            Job::Test(_) | Job::Password { .. } | Job::Delete { .. } | Job::Add { .. }
        );
        // The file on disk is about to change, so the listing has to be redone.
        if let Job::Password { archive, new, .. } = &job {
            self.after_password = Some((archive.clone(), new.clone()));
        }
        if let Job::Delete { archive, password, .. } = &job {
            self.after_password = Some((archive.clone(), password.clone()));
        }
        if let Job::Add { archive, password, .. } = &job {
            self.after_password = Some((archive.clone(), password.clone()));
        }

        let (reply_tx, reply_rx) = channel::<Answer>();
        self.replies = Some(reply_tx);
        let ctx2 = ctx.clone();
        self.spawn(ctx, 0, move |tx| {
            let notify = |i: usize, n: usize, name: &str| {
                let _ = tx.send(Message::Progress(i, n, name.to_string()));
                ctx2.request_repaint();
            };
            let ask = conflict_asker(tx, &ctx2, &reply_rx);
            let outcome = run_job_blocking(job, s, &notify, &ask);
            let _ = tx.send(match outcome {
                Ok(text) => Message::Done(text),
                Err(text) => Message::Failed(text),
            });
            ctx2.request_repaint();
        });
    }

    fn receive(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut finished_ok = false;
        if let Some(rx) = &self.channel {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Message::Listing(path, v) => {
                        if v.iter().any(|e| e.encrypted) && self.archive_password.is_none() {
                            self.password_input.clear();
                            self.archive_password = None;
                            self.waiting_on_password = Some(Pending::OpenArchive);
                        }
                        // Nothing picked to begin with. It used to be
                        // everything, which was invisible while the ticks were
                        // the only sign of it; now that a picked row is painted
                        // it would open as a wall of blue, and "everything is
                        // selected" is not what a list means when you open it.
                        // The buttons that work on the whole archive never
                        // looked at the ticks anyway.
                        self.checked = vec![false; v.len()];
                        self.entries = v;
                        if let Some(f) = detect(&path) {
                            self.format = f;
                        }
                        self.archive = Some(path);
                        self.history = vec![String::new()];
                        self.here = 0;
                        self.current_dir = String::new();
                        self.busy = false;
                        close = true;
                    }
                    Message::Conflict(path) => {
                        self.conflict = Some(path);
                    }
                    Message::Progress(done, total, name) => {
                        self.done_count = done;
                        self.total_count = total;
                        self.current_file = name;
                    }
                    Message::Done(text) => {
                        self.notice = text;
                        self.busy = false;
                        close = true;
                        finished_ok = true;
                    }
                    Message::Failed(text) => {
                        self.notice = text;
                        self.error = true;
                        self.busy = false;
                        close = true;
                    }
                }
            }
        }
        if close {
            self.channel = None;
            if !self.entries.is_empty() && self.notice.is_empty() {
                self.notice = self.summary();
            }
        }
        // The archive on disk is not the one that was listed any more. Reopen it
        // with the password it now carries, so the browse view shows the new
        // state and does not ask for a password it was just handed.
        if finished_ok {
            if let Some((path, pw)) = self.after_password.take() {
                let notice = std::mem::take(&mut self.notice);
                self.open(ctx, path);
                self.archive_password = pw;
                self.notice = notice;
                self.view = View::Browse;
            }
        }
        if finished_ok && self.close_when_done && matches!(self.view, View::Running) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn settings_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let s = self.s();
        let mut changed = false;

        ui.label(s.language);
        let current = self
            .settings
            .lang
            .map(|l| l.label())
            .unwrap_or(s.theme_system);
        egui::ComboBox::from_id_salt("lang")
            .selected_text(current)
            .width(110.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.settings.lang.is_none(), s.theme_system)
                    .clicked()
                {
                    self.settings.lang = None;
                    changed = true;
                }
                for l in Lang::ALL {
                    if ui
                        .selectable_label(self.settings.lang == Some(l), l.label())
                        .clicked()
                    {
                        self.settings.lang = Some(l);
                        changed = true;
                    }
                }
            });

        ui.add_space(10.0);
        ui.label(s.theme);
        let theme_label = match self.settings.theme {
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
                        .selectable_label(self.settings.theme == t, label)
                        .clicked()
                    {
                        self.settings.theme = t;
                        ctx.set_theme(t);
                        changed = true;
                    }
                }
            });

        if changed {
            self.settings.save();
        }
    }

    fn format_row(&mut self, ui: &mut egui::Ui) {
        let s = self.s();
        let codecs: Vec<(Codec, &'static str)> = [Codec::Store, Codec::Deflate, Codec::Zstd]
            .into_iter()
            .map(|c| (c, self.codec_name(c)))
            .collect();
        let levels: Vec<(Level, &'static str)> =
            [Level::Store, Level::Fast, Level::Normal, Level::Best]
                .into_iter()
                .map(|l| (l, self.level_name(l)))
                .collect();
        let current_codec = self.codec_name(self.codec);
        let current_level = self.level_name(self.level);
        let is_zip = self.format == Format::Zip;

        ui.horizontal(|ui| {
            ui.label(s.format);
            egui::ComboBox::from_id_salt("fmt")
                .selected_text(self.format.label())
                .width(90.0)
                .show_ui(ui, |ui| {
                    for f in [Format::Zip, Format::Tar, Format::TarGz] {
                        ui.selectable_value(&mut self.format, f, f.label());
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
                            ui.selectable_value(&mut self.codec, *c, *label);
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
                        ui.selectable_value(&mut self.level, *l, *label);
                    }
                });
        });
    }

    fn ask_extract(&mut self, ctx: &egui::Context, only_checked: bool) {
        let s: &'static Strings = self.s();
        let Some(archive) = self.archive.clone() else {
            return;
        };
        let Some(mut dest) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        if self.into_subfolder {
            dest = dest.join(archive_stem(&archive));
        }
        let wanted: Vec<bool> = if only_checked {
            self.checked.clone()
        } else {
            vec![true; self.entries.len()]
        };
        let total = wanted.iter().filter(|b| **b).count();
        let pw = self.archive_password.clone();
        self.close_when_done = false;
        let (reply_tx, reply_rx) = channel::<Answer>();
        self.replies = Some(reply_tx);
        let ctx2 = ctx.clone();
        self.spawn(ctx, total, move |tx| {
            let notify = |i: usize, n: usize, name: &str| {
                let _ = tx.send(Message::Progress(i, n, name.to_string()));
                ctx2.request_repaint();
            };
            let ask = conflict_asker(tx, &ctx2, &reply_rx);
            let _ = tx.send(match extract(&archive, &dest, &wanted, &notify, &ask, pw.as_deref()) {
                Ok(bytes) => Message::Done(fill(
                    s.extracted_to,
                    &[("size", &human(bytes)), ("dest", &dest.display().to_string())],
                )),
                Err(e) => Message::Failed(e.to_string()),
            });
        });
    }

    // Escape and the Cancel button are the same act, so they go through the
    // same code: two copies of this would drift apart the first time one side
    // grew a step.
    // The names ticked right now, which is what every action that works on a
    // selection needs.
    fn selected_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .zip(&self.checked)
            .filter(|(_, &on)| on)
            .map(|(e, _)| e.name.clone())
            .collect()
    }

    // The top of what is ticked. A folder with every one of its entries ticked
    // stands for all of them, so a copy hands the clipboard one folder instead
    // of the fifteen hundred files inside it, and the Explorer pastes a folder
    // rather than a heap of loose files.
    fn selected_roots(&self) -> Vec<String> {
        let names: Vec<String> = self
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
            if !self.checked.get(i).copied().unwrap_or(false) {
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
                        .all(|(j, _)| self.checked.get(j).copied().unwrap_or(false))
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
    fn copy_to_clipboard(&mut self, ctx: &egui::Context, cut: bool) {
        let s: &'static Strings = self.s();
        let Some(archive) = self.archive.clone() else {
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
        let previous = self.clip_dir.replace(dir.clone());
        self.cut_names = if cut {
            self.selected_names().into_iter().collect()
        } else {
            HashSet::new()
        };

        let wanted = self.checked.clone();
        let total = wanted.iter().filter(|b| **b).count();
        let pw = self.archive_password.clone();
        self.close_when_done = false;
        let ctx2 = ctx.clone();
        self.spawn(ctx, total, move |tx| {
            let notify = |i: usize, n: usize, name: &str| {
                let _ = tx.send(Message::Progress(i, n, name.to_string()));
                ctx2.request_repaint();
            };
            // A folder nobody has seen yet has nothing in it to overwrite, so
            // there is no question to put on screen.
            let ask = |_: &Path| Answer::Replace;
            let outcome = extract(&archive, &dir, &wanted, &notify, &ask, pw.as_deref())
                .map_err(|e| e.to_string())
                .and_then(|_| {
                    let paths: Vec<PathBuf> = roots
                        .iter()
                        .filter_map(|r| arca_core::safe_name(r).ok())
                        .map(|r| dir.join(r))
                        .collect();
                    clipboard::set_files(&paths, cut).map(|()| paths.len())
                });
            // Only once the new list is on the clipboard: until that moment the
            // old paths are still what a paste would reach for.
            if let Some(old) = previous {
                let _ = fs::remove_dir_all(old);
            }
            let _ = tx.send(match outcome {
                Ok(n) => Message::Done(fill(
                    if cut {
                        s.cut_to_clipboard
                    } else {
                        s.copied_to_clipboard
                    },
                    &[("n", &n.to_string())],
                )),
                Err(why) => Message::Failed(fill(s.clipboard_failed, &[("why", &why)])),
            });
            ctx2.request_repaint();
        });
    }

    // Ctrl+V: whatever files the shell is holding, into the folder on screen.
    fn paste_from_clipboard(&mut self, ctx: &egui::Context) {
        let Some(archive) = self.archive.clone() else {
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
            self.notice = self.s().clipboard_empty.to_string();
            self.error = true;
            return;
        }
        self.add_files(ctx, inputs);
    }

    // Files from anywhere outside into the folder the window is showing. Both
    // the paste and the drop end here so they cannot answer the same question
    // two different ways.
    fn add_files(&mut self, ctx: &egui::Context, inputs: Vec<PathBuf>) {
        let Some(archive) = self.archive.clone() else {
            return;
        };
        self.run_job(
            ctx,
            Job::Add {
                archive,
                inputs,
                dir: self.current_dir.clone(),
                codec: self.codec,
                level: self.level,
                password: self.archive_password.clone(),
            },
        );
    }

    // What a drop means depends on what the window is already showing. With
    // nothing open there is only one thing it can be, and that is what it has
    // always done: open it. With an archive open, dropping a file on it means
    // putting the file inside, which is what every other archiver does and what
    // opening a second archive over the first never was.
    //
    // The exception is dropping an archive onto an archive, which is honestly
    // both, so it asks instead of picking one and being wrong half the time.
    fn dropped(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.notice.clear();
        self.error = false;
        let open_first = |me: &mut Self, paths: Vec<PathBuf>| {
            if let Some(p) = paths.into_iter().next() {
                me.open(ctx, p);
            }
        };
        let Some(archive) = self.archive.clone() else {
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
                self.notice = self.s().only_zip_can_change.to_string();
                self.error = true;
            }
            return;
        }
        if all_archives {
            self.confirm_drop = Some(paths);
            return;
        }
        self.add_files(ctx, paths);
    }

    // A drop used to mean one thing and now means another, so while something
    // is held over the window it says which. Guessing in silence is what made
    // the old behaviour surprising in the first place.
    fn drop_hint(&self, ctx: &egui::Context) {
        if !matches!(self.view, View::Browse)
            || self.busy
            || ctx.input(|i| i.raw.hovered_files.is_empty())
        {
            return;
        }
        let s = self.s();
        let text = match &self.archive {
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
        let Some(paths) = self.confirm_drop.clone() else {
            return;
        };
        let s = self.s();
        let into = self
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
            self.confirm_drop = None;
        }
        if open_it {
            self.confirm_drop = None;
            if let Some(p) = paths.into_iter().next() {
                self.open(ctx, p);
            }
        } else if add_it {
            self.confirm_drop = None;
            self.add_files(ctx, paths);
        }
    }

    // Windows shortcuts that mean something here. The ones that would need the
    // archive to grow a feature it has not got are left out rather than made to
    // look present and do nothing.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if !matches!(self.view, View::Browse)
            || self.busy
            || self.confirm_delete.is_some()
            || self.confirm_drop.is_some()
        {
            return;
        }
        let typing = ctx.memory(|m| m.focused().is_some());
        let (ctrl, shift, o, e, c, x, v, n, f, f5, del) = ctx.input(|i| {
            (
                i.modifiers.command,
                i.modifiers.shift,
                i.key_pressed(egui::Key::O),
                i.key_pressed(egui::Key::E),
                i.key_pressed(egui::Key::C),
                i.key_pressed(egui::Key::X),
                i.key_pressed(egui::Key::V),
                i.key_pressed(egui::Key::N),
                i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::F5),
                i.key_pressed(egui::Key::Delete),
            )
        });

        if f5 && !typing {
            if let Some(p) = self.archive.clone() {
                let keep = self.archive_password.clone();
                self.open(ctx, p);
                self.archive_password = keep;
            }
        }
        if del && !typing && self.archive.is_some() {
            let names = self.selected_names();
            if !names.is_empty() {
                self.confirm_delete = Some(names);
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
                self.open(ctx, p);
            }
        }
        if e && self.archive.is_some() {
            self.ask_extract(ctx, false);
        }
        if n {
            if let Some(files) = rfd::FileDialog::new().pick_files() {
                if !files.is_empty() {
                    self.output_name = quick_output(&files, self.format)
                        .file_name()
                        .map(|x| x.to_string_lossy().to_string())
                        .unwrap_or_default();
                    self.pending_inputs = files;
                    self.view = View::Add;
                }
            }
        }
        if f {
            // Nothing else here is a text box, so handing the keyboard to the
            // filter is the whole of "find".
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("filter")));
        }
        // Cut, copy and paste belong to the filter box while it has the
        // keyboard: that is a text field, and taking its Ctrl+V away to add
        // files to the archive would be the last thing anybody expected.
        if typing {
            return;
        }
        if c {
            // Plain Ctrl+C is what it is everywhere else: the files, ready to
            // paste into a folder. The names as text move to Ctrl+Shift+C,
            // which is also all a platform without a file clipboard can offer.
            if shift || !clipboard::AVAILABLE {
                let names = self.selected_names();
                if !names.is_empty() {
                    ctx.copy_text(names.join("\r\n"));
                }
            } else {
                self.copy_to_clipboard(ctx, false);
            }
        }
        if x && clipboard::AVAILABLE {
            self.copy_to_clipboard(ctx, true);
        }
        if v && clipboard::AVAILABLE && self.archive.is_some() {
            self.paste_from_clipboard(ctx);
        }
    }

    fn confirm_delete_window(&mut self, ctx: &egui::Context) {
        let Some(names) = self.confirm_delete.clone() else {
            return;
        };
        let s = self.s();
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
            self.confirm_delete = None;
        }
        if go {
            self.confirm_delete = None;
            if let Some(archive) = self.archive.clone() {
                self.run_job(
                    ctx,
                    Job::Delete {
                        archive,
                        names,
                        password: self.archive_password.clone(),
                    },
                );
            }
        }
    }

    fn cancel_password(&mut self) {
        let was_job = matches!(self.waiting_on_password, Some(Pending::Extract(_)));
        self.waiting_on_password = None;
        self.password_input.clear();
        // Only a job left the window on the running view with nothing running.
        if was_job {
            self.view = View::Browse;
        }
    }

    fn password_window(&mut self, ctx: &egui::Context) {
        if self.waiting_on_password.is_none() {
            return;
        }
        let s = self.s();
        let setting = matches!(self.waiting_on_password, Some(Pending::NewPassword(_)));
        let mut go = false;
        let mut cancel = false;
        egui::Window::new(if setting { s.set_password } else { s.password_needed })
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(if setting { s.new_password } else { s.password_hint });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    // Asked before the field is built. The text edit swallows
                    // Enter, and asking it afterwards never sees the key, so
                    // the window could only be dismissed with the mouse.
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.password_input)
                            .password(!self.show_password)
                            .desired_width(240.0),
                    );
                    field.request_focus();
                    if enter {
                        go = true;
                    }
                    ui.checkbox(&mut self.show_password, s.show_password);
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
            self.cancel_password();
            return;
        }
        if go && !self.password_input.is_empty() {
            let given = std::mem::take(&mut self.password_input);
            match self.waiting_on_password.take() {
                Some(Pending::Extract(job)) => {
                    if let Job::Extract { archives, dest, .. } = *job {
                        self.run_job(ctx, Job::Extract { archives, dest, password: Some(given) });
                    }
                }
                Some(Pending::CurrentPassword(job)) => {
                    if let Job::Password { archive, new, .. } = *job {
                        self.archive_password = Some(given.clone());
                        self.run_job(ctx, Job::Password { archive, current: Some(given), new });
                    }
                }
                Some(Pending::OpenArchive) => self.archive_password = Some(given),
                Some(Pending::NewPassword(job)) => {
                    if let Job::Password { archive, current, .. } = *job {
                        self.run_job(ctx, Job::Password { archive, current, new: Some(given) });
                    }
                }
                None => {}
            }
        }
    }

    fn conflict_window(&mut self, ctx: &egui::Context) {
        let Some(path) = self.conflict.clone() else {
            return;
        };
        let s = self.s();
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
            if let Some(tx) = &self.replies {
                let _ = tx.send(a);
            }
            self.conflict = None;
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let s = self.s();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            // The password window is not modal on its own, so the toolbar behind it
            // has to be shut off: a click there would run with a password that
            // has not been given yet.
            let idle = !self.busy && self.waiting_on_password.is_none();
            ui.add_enabled_ui(idle, |ui| {
                if ui.button(s.open).clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Archives", &["zip", "tar", "gz", "tgz"])
                        .pick_file()
                    {
                        self.open(ctx, p);
                    }
                }
                if ui.button(s.compress).clicked() {
                    if let Some(files) = rfd::FileDialog::new().pick_files() {
                        if !files.is_empty() {
                            self.output_name = quick_output(&files, self.format)
                                .file_name()
                                .map(|x| x.to_string_lossy().to_string())
                                .unwrap_or_default();
                            self.pending_inputs = files;
                            self.view = View::Add;
                        }
                    }
                }
                ui.separator();
                let has = self.archive.is_some();
                if ui
                    .add_enabled(has, egui::Button::new(s.extract_all))
                    .clicked()
                {
                    self.ask_extract(ctx, false);
                }
                let n = self.checked.iter().filter(|b| **b).count();
                if ui
                    .add_enabled(has && n > 0, egui::Button::new(s.extract_selected))
                    .clicked()
                {
                    self.ask_extract(ctx, true);
                }
                // Only .zip has anywhere to keep a password.
                if has && self.format == Format::Zip {
                    ui.separator();
                    let encrypted = self.entries.iter().any(|e| e.encrypted);
                    let archive = self.archive.clone().unwrap_or_default();
                    if encrypted {
                        if ui.button(s.remove_password).clicked() {
                            let job = Job::Password {
                                archive,
                                current: self.archive_password.clone(),
                                new: None,
                            };
                            // Cancelling the question when the archive opened
                            // leaves us without it, so ask again instead of
                            // failing halfway through the rewrite.
                            match self.archive_password {
                                Some(_) => self.run_job(ctx, job),
                                None => {
                                    self.password_input.clear();
                                    self.waiting_on_password =
                                        Some(Pending::CurrentPassword(Box::new(job)));
                                }
                            }
                        }
                    } else if ui.button(s.set_password).clicked() {
                        self.password_input.clear();
                        self.waiting_on_password =
                            Some(Pending::NewPassword(Box::new(Job::Password {
                                archive,
                                current: None,
                                new: None,
                            })));
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .id(egui::Id::new("filter"))
                        .hint_text(s.filter_hint)
                        .desired_width(170.0),
                );
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let at_root = self.current_dir.is_empty();
            if arrow_button(ui, Arrow::Left, self.can_go_back(), s.back).clicked() {
                self.go_back();
            }
            if arrow_button(ui, Arrow::Right, self.can_go_forward(), s.forward).clicked() {
                self.go_forward();
            }
            if arrow_button(ui, Arrow::Up, !at_root, s.up).clicked() {
                let parent = parent_of(&self.current_dir);
                self.go_to(parent);
            }
            ui.separator();
            let here = if self.current_dir.is_empty() {
                "/".to_string()
            } else {
                format!("/{}", self.current_dir)
            };
            ui.label(egui::RichText::new(here).monospace());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(s.settings).clicked() {
                    self.show_settings = true;
                }
            });
        });
        ui.add_space(6.0);
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let s = self.s();
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
                ui.checkbox(&mut self.into_subfolder, s.into_subfolder);
                ui.add_space(8.0);
            });
        self.show_settings = open;
    }

    fn add_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let s = self.s();
        ui.add_space(10.0);
        ui.heading(s.add_to_archive);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(s.output_name);
            ui.add(egui::TextEdit::singleline(&mut self.output_name).desired_width(320.0));
        });
        ui.add_space(6.0);
        self.format_row(ui);
        ui.add_space(6.0);
        // Only .zip has anywhere to put encryption, so the field is not offered
        // for the other two rather than accepted and quietly ignored.
        if self.format == Format::Zip {
            ui.horizontal(|ui| {
                ui.label(s.password_optional);
                ui.add(
                    egui::TextEdit::singleline(&mut self.add_password)
                        .password(!self.show_password)
                        .desired_width(220.0),
                );
                ui.checkbox(&mut self.show_password, s.show_password);
            });
            ui.add_space(6.0);
        }
        let count = self.pending_inputs.len();
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
                    .pending_inputs
                    .first()
                    .and_then(|p| p.parent())
                    .map(PathBuf::from)
                    .unwrap_or_default();
                let mut name = self.output_name.trim().to_string();
                if name.is_empty() {
                    name = format!("archive.{}", self.format.extension());
                }
                let job = Job::Compress {
                    out: dir.join(name),
                    inputs: self.pending_inputs.clone(),
                    format: self.format,
                    codec: self.codec,
                    level: self.level,
                    password: if self.format == Format::Zip && !self.add_password.is_empty() {
                        Some(self.add_password.clone())
                    } else {
                        None
                    },
                };
                self.run_job(ctx, job);
            }
            if ui.button(s.cancel).clicked() {
                self.view = View::Browse;
            }
        });
    }

    fn running_view(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let s = self.s();
        ui.add_space(12.0);
        ui.heading(&self.title);
        ui.add_space(10.0);

        let fraction = if self.total_count == 0 {
            0.0
        } else {
            self.done_count as f32 / self.total_count as f32
        };
        ui.add(
            egui::ProgressBar::new(fraction)
                .text(format!("{} / {}", self.done_count, self.total_count))
                .desired_width(ui.available_width()),
        );
        ui.add_space(6.0);
        ui.label(egui::RichText::new(&self.current_file).weak());

        if let Some(t) = self.started {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("{:.1} s", t.elapsed().as_secs_f64()))
                    .weak()
                    .small(),
            );
        }

        if !self.busy {
            ui.add_space(12.0);
            let color = if self.error {
                egui::Color32::from_rgb(220, 90, 90)
            } else {
                ui.visuals().text_color()
            };
            ui.colored_label(color, if self.error { s.failed } else { s.done });
            ui.add_space(4.0);
            ui.label(&self.notice);
            ui.add_space(12.0);
            if ui.button(s.close).clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
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

        if !down {
            self.band = None;
            self.band_anchor = None;
            self.band_scroll = None;
            return;
        }
        // `viewport` is the scrolling part alone, so the header is already out
        // of it: dragging a column edge cannot start a selection.
        if self.band.is_none() {
            let Some(p) = origin.filter(|p| viewport.contains(*p)) else {
                return;
            };
            let Some(anchor) = row_at(row_rects, p.y) else {
                return;
            };
            self.band = origin;
            self.band_anchor = Some(anchor);
            self.band_base = if ctrl {
                self.checked.clone()
            } else {
                vec![false; self.checked.len()]
            };
        }

        let (Some(start), Some(here), Some(anchor)) = (self.band, now, self.band_anchor) else {
            return;
        };
        // A click is a drag of no distance. Under this it is left alone, so
        // clicking a row still means clicking a row.
        if (here - start).length() < 4.0 {
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
            self.band_scroll = None;
        } else {
            self.band_scroll = Some((offset + over.clamp(-24.0, 24.0)).clamp(0.0, reach));
            // Nothing else is moving, so without this the list would take one
            // step per stray mouse event instead of running.
            ui.ctx().request_repaint();
        }

        // Two row numbers, not a rectangle in window coordinates. The list
        // moves underneath while the drag is happening, and a rectangle frozen
        // where the button went down stops meaning anything the moment it does;
        // it also loses every row that scrolls out of sight, because those are
        // the only ones the table still knows the position of.
        let head = row_at(row_rects, here.y).unwrap_or(anchor);
        let (lo, hi) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        self.checked.clone_from(&self.band_base);
        for row in visible.get(lo..=hi).unwrap_or_default() {
            self.set_checked(row, true);
        }

        // Drawn from the row the drag began on rather than from the point the
        // button went down, so that it stays put against the rows when the list
        // scrolls under it.
        let edge = match row_rects.iter().find(|(i, _)| *i == anchor) {
            // Its far side, so that the row the drag began on falls inside the
            // band whichever way the drag then went.
            Some((_, r)) => {
                if here.y < r.top() {
                    r.bottom()
                } else {
                    r.top()
                }
            }
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

    fn set_checked(&mut self, row: &Row, value: bool) {
        match row.entry {
            Some(i) => self.checked[i] = value,
            None => {
                for i in entries_under(&self.entries, &row.path) {
                    self.checked[i] = value;
                }
            }
        }
    }

    // Double clicking a file pulls that one entry out to a temporary folder and
    // hands it to whatever the system opens it with. It runs on its own thread
    // because the entry can be large, and reports through the same progress
    // window as everything else.
    fn open_file(&mut self, ctx: &egui::Context, index: usize) {
        let Some(archive) = self.archive.clone() else {
            return;
        };
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        if entry.is_dir {
            return;
        }
        let password = self.archive_password.clone();
        let s = self.s();
        let ctx2 = ctx.clone();
        self.close_when_done = false;
        self.title = s.opening.to_string();
        self.view = View::Running;
        self.spawn(ctx, 1, move |tx| {
            let _ = tx.send(Message::Progress(0, 1, entry.name.clone()));
            ctx2.request_repaint();
            let outcome = extract_one(&archive, &entry, password.as_deref())
                .and_then(|path| launch_with_system(&path).map(|()| path));
            let _ = tx.send(match outcome {
                Ok(path) => Message::Done(fill(
                    s.opened_with_system,
                    &[("name", &path.display().to_string())],
                )),
                Err(e) => Message::Failed(e.to_string()),
            });
            ctx2.request_repaint();
        });
    }

    // Everything the keyboard does to the list, in one place. `rows` is what is
    // on screen right now, which is what the arrows should walk: filtering or
    // changing folder changes the list under the cursor, so it is clamped here
    // rather than tracked separately.
    fn keyboard(&mut self, ctx: &egui::Context, rows: &[Row]) {
        if rows.is_empty() {
            self.cursor = None;
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
        let mut typed = String::new();

        ctx.input(|i| {
            let at = self.cursor.unwrap_or(0);
            shift = i.modifiers.shift;
            check_all = i.modifiers.command && i.key_pressed(egui::Key::A);
            for (key, to) in [
                (egui::Key::ArrowDown, (at + 1).min(last)),
                (egui::Key::ArrowUp, at.saturating_sub(1)),
                (egui::Key::PageDown, (at + page).min(last)),
                (egui::Key::PageUp, at.saturating_sub(page)),
                (egui::Key::Home, 0),
                (egui::Key::End, last),
            ] {
                if i.key_pressed(key) {
                    // The first press only lands the cursor somewhere visible
                    // instead of jumping a row from nowhere.
                    moved = Some(if self.cursor.is_none() { 0 } else { to });
                }
            }
            enter = i.key_pressed(egui::Key::Enter);
            space = i.key_pressed(egui::Key::Space);
            up_level = i.key_pressed(egui::Key::Backspace);
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

        if check_all {
            let value = rows.iter().any(|r| !self.is_checked(r));
            for r in rows {
                self.set_checked(r, value);
            }
            return;
        }

        // Typing jumps to the next row starting with what was typed, wrapping
        // round, so pressing the same letter walks through the matches.
        if !typed.is_empty() && typed != " " {
            let needle = typed.to_lowercase();
            let from = self.cursor.map_or(0, |c| c + 1);
            let hit = (0..rows.len())
                .map(|n| (from + n) % rows.len())
                .find(|&n| rows[n].label.to_lowercase().starts_with(&needle));
            if let Some(n) = hit {
                self.cursor = Some(n);
                self.scroll_to_cursor = true;
                return;
            }
        }

        if let Some(to) = moved {
            // Shift drags the ticks along with the cursor, so a run of files
            // can be picked without reaching for the mouse.
            if shift {
                let from = self.cursor.unwrap_or(to);
                let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
                for r in &rows[lo..=hi] {
                    self.set_checked(r, true);
                }
            }
            self.cursor = Some(to);
            self.scroll_to_cursor = true;
        }

        if up_level && !self.current_dir.is_empty() {
            let parent = parent_of(&self.current_dir);
            self.go_to(parent);
            return;
        }

        let Some(at) = self.cursor else { return };
        let Some(row) = rows.get(at) else { return };

        if space {
            let value = !self.is_checked(row);
            self.set_checked(row, value);
        }
        if enter {
            if row.is_dir {
                let path = row.path.clone();
                self.go_to(path);
            } else if let Some(i) = row.entry {
                self.open_file(ctx, i);
            }
        }
    }

    // Going somewhere new drops whatever was ahead in the history, the way a
    // browser does. Re-entering the folder already showing is not a move.
    fn go_to(&mut self, path: String) {
        if self.history.get(self.here) == Some(&path) {
            return;
        }
        self.history.truncate(self.here + 1);
        self.history.push(path.clone());
        self.here = self.history.len() - 1;
        self.current_dir = path;
        self.filter.clear();
    }

    fn can_go_back(&self) -> bool {
        self.here > 0
    }

    fn can_go_forward(&self) -> bool {
        self.here + 1 < self.history.len()
    }

    fn go_back(&mut self) {
        if self.can_go_back() {
            self.here -= 1;
            self.current_dir = self.history[self.here].clone();
            self.filter.clear();
        }
    }

    fn go_forward(&mut self) {
        if self.can_go_forward() {
            self.here += 1;
            self.current_dir = self.history[self.here].clone();
            self.filter.clear();
        }
    }

    fn is_checked(&self, row: &Row) -> bool {
        match row.entry {
            Some(i) => self.checked[i],
            None => {
                let under = entries_under(&self.entries, &row.path);
                !under.is_empty() && under.iter().all(|&i| self.checked[i])
            }
        }
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let s = self.s();
        let visible = self.visible_rows();
        // Before the table is drawn, so a move this frame is painted this
        // frame rather than one behind.
        self.keyboard(ui.ctx(), &visible);
        if self.cursor.is_some_and(|c| c >= visible.len()) {
            self.cursor = if visible.is_empty() { None } else { Some(visible.len() - 1) };
        }

        ui.horizontal(|ui| {
            if ui.small_button(s.check_all).clicked() {
                for r in &visible {
                    self.set_checked(r, true);
                }
            }
            if ui.small_button(s.uncheck_all).clicked() {
                for r in &visible {
                    self.set_checked(r, false);
                }
            }
            ui.separator();
            let n = self.checked.iter().filter(|b| **b).count();
            ui.label(format!(
                "{} {} {} · {n} {}",
                visible.len(),
                s.visible_of,
                self.entries.len(),
                s.checked
            ));
        });
        ui.add_space(4.0);

        let mut requested: Option<SortColumn> = None;
        let mut toggle: Option<(usize, bool)> = None;
        let mut opened: Option<usize> = None;
        let mut clicked: Option<usize> = None;
        let columns = self.settings.columns;
        // The ones on, in the order they are drawn. The header, the cells and
        // the column widths all walk this same list, so they cannot drift.
        let shown: Vec<SortColumn> = Columns::ALL
            .iter()
            .map(|(which, _)| *which)
            .filter(|w| columns.on(*w))
            .collect();
        let toggle_column: std::cell::Cell<Option<SortColumn>> = std::cell::Cell::new(None);
        // What the row menu asked for. Cells again, and acted on after the
        // table: doing any of it inside the closure would be borrowing self
        // while the table still holds it.
        let wants_extract = std::cell::Cell::new(false);
        let wants_delete = std::cell::Cell::new(false);
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
        let order = self.order;
        let hint = s.sort_hint;
        let head = |ui: &mut egui::Ui, text: &str, col: SortColumn| -> bool {
            let arrow = if order.0 != col {
                ""
            } else if order.1 {
                " ^"
            } else {
                " v"
            };
            ui.add(
                egui::Label::new(egui::RichText::new(format!("{text}{arrow}")).strong())
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(hint)
            .clicked()
        };

        let mut builder = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            // Without this the cells only sense hovering, and row.response()
            // would never report a double click.
            .sense(egui::Sense::click())
            // Dragging in a file list draws a selection; it does not push the
            // list about. With this on, dragging did both at once, and the two
            // pull opposite ways: dragging down moves the content down, which
            // is the list scrolling up, so a downward selection ran upwards.
            .drag_to_scroll(false)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(26.0))
            // Name first and wide, with the icon inside it. That is where the
            // Explorer and every archiver put it, and a separate icon column
            // only pushed the one thing you read away from its picture.
            .column(Column::initial(300.0).at_least(140.0))
            .columns(Column::initial(95.0).at_least(60.0), shown.len().saturating_sub(1))
            .column(Column::remainder().at_least(60.0));
        // Set only while a selection drag has run off the end of the list, so
        // the rest of the time the table keeps its own scroll position.
        if let Some(y) = self.band_scroll {
            builder = builder.vertical_scroll_offset(y);
        }

        let out = builder
            .header(26.0, |mut h| {
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
                h.col(|_| {}).1.context_menu(|ui| menu(ui));
                h.col(|ui| {
                    if head(ui, s.col_name, SortColumn::Name) {
                        requested = Some(SortColumn::Name);
                    }
                })
                .1
                .context_menu(|ui| menu(ui));
                for which in &shown {
                    h.col(|ui| {
                        if head(ui, Columns::label(*which, s), *which) {
                            requested = Some(*which);
                        }
                    })
                    .1
                    .context_menu(|ui| menu(ui));
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
                    row.set_selected(self.is_checked(r));
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
                    let cut = self.cut_names.contains(&r.path) || r.entry.is_some_and(|i| self.cut_names.contains(&self.entries[i].name));
                    let mut flag = self.is_checked(r);
                    row.col(|ui| {
                        if ui.checkbox(&mut flag, "").changed() {
                            toggle = Some((idx, flag));
                        }
                    });
                    row.col(|ui| {
                        // The system icon when the desktop has one, and the
                        // drawn one when it does not, which is every platform
                        // that is not Windows so far.
                        match system_icon(ui.ctx(), &mut icons, &r.label, r.is_dir) {
                            Some(tex) => {
                                ui.add(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(15.0, 15.0)));
                            }
                            None => draw_icon(ui, r.kind),
                        }
                        ui.add_space(4.0);
                        let text = if r.is_dir {
                            egui::RichText::new(&r.label).strong()
                        } else {
                            egui::RichText::new(&r.label)
                        };
                        // Faded while it is on the clipboard as a cut, which is
                        // the only sign the Explorer gives either.
                        let text = if cut { text.weak() } else { text };
                        ui.add(egui::Label::new(text).selectable(false).truncate());
                    });
                    for which in &shown {
                        row.col(|ui| match which {
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
                            SortColumn::Name => {}
                        });
                    }
                    // The whole row answers, not just the name: aiming at the
                    // text to open something is a nuisance nobody expects.
                    let resp = row.response();
                    // Only what there is something behind. A menu offering
                    // things this window cannot do would be worse than none.
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
                        if ui.button(format!("{}	Ctrl+E", s.extract_selected)).clicked() {
                            wants_extract.set(true);
                            ui.close_menu();
                        }
                        ui.separator();
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
                    if resp.hovered() {
                        resp.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        clicked = Some(idx);
                    }
                    row_rects.push((idx, resp.rect));
                    if resp.double_clicked() {
                        opened = Some(idx);
                    }
                    if self.cursor == Some(idx) {
                        cursor_rect.set(Some(resp.rect));
                    }
                    // Only when the keyboard moved it: doing this every frame
                    // would fight the scroll wheel.
                    if self.scroll_to_cursor && self.cursor == Some(idx) {
                        resp.scroll_to_me(Some(egui::Align::Center));
                    }
                });
            });

        if let Some((index, value)) = toggle {
            let (path, entry) = (visible[index].path.clone(), visible[index].entry);
            match entry {
                Some(i) => self.checked[i] = value,
                None => {
                    for i in entries_under(&self.entries, &path) {
                        self.checked[i] = value;
                    }
                }
            }
        }
        self.icons = icons;
        self.scroll_to_cursor = false;

        // Everything the two menus asked for, now that the table has let go of
        // self. Turning a column off is not allowed to leave the list with
        // nothing but names to look at, so the last one stays.
        if let Some(which) = toggle_column.get() {
            let mut c = self.settings.columns;
            let turning_off = c.on(which);
            if !turning_off || shown.len() > 1 {
                c.set(which, !turning_off);
                self.settings.columns = c;
                self.settings.save();
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
                let value = !self.is_checked(&target);
                self.set_checked(&target, value);
            } else if mods.shift {
                let from = self.cursor.unwrap_or(index);
                let (lo, hi) = if from <= index { (from, index) } else { (index, from) };
                for r in &visible[lo..=hi] {
                    self.set_checked(r, true);
                }
            } else {
                self.checked.iter_mut().for_each(|c| *c = false);
                self.set_checked(&target, true);
            }
            self.cursor = Some(index);
        }

        if wants_select_all.get() {
            for r in &visible {
                self.set_checked(r, true);
            }
        }
        if wants_copy_names.get() {
            let names = self.selected_names();
            if !names.is_empty() {
                ui.ctx().copy_text(names.join("
"));
            }
        }
        if wants_delete.get() {
            let names = self.selected_names();
            if !names.is_empty() {
                self.confirm_delete = Some(names);
            }
        }
        if wants_extract.get() {
            let ctx = ui.ctx().clone();
            self.ask_extract(&ctx, true);
        }
        if let Some(cut) = wants_clip.get() {
            let ctx = ui.ctx().clone();
            self.copy_to_clipboard(&ctx, cut);
        }
        if wants_paste.get() {
            let ctx = ui.ctx().clone();
            self.paste_from_clipboard(&ctx);
        }

        // A thin outline where the keyboard is, over the fill that says what is
        // ticked. Two different things, so they cannot share the one colour.
        if let Some(rect) = cursor_rect.get() {
            ui.painter()
                .rect_stroke(rect.shrink(0.5), 0.0, theme::cursor(ui.visuals()));
        }

        // The scrollable part on its own, without the header the outer rect
        // takes in, and how far down the list it currently sits: both are what
        // a drag needs to know when it reaches an edge.
        let reach = (out.content_size.y - out.inner_rect.height()).max(0.0);
        self.rubber_band(ui, &visible, &row_rects, out.inner_rect, out.state.offset.y, reach);
        if let Some(index) = opened {
            let target = &visible[index];
            if target.is_dir {
                let path = target.path.clone();
                self.go_to(path);
            } else if let Some(i) = target.entry {
                self.open_file(ui.ctx(), i);
            }
        }
        if let Some(c) = requested {
            if self.order.0 == c {
                self.order.1 = !self.order.1;
            } else {
                self.order = (c, true);
            }
        }
    }
}

impl eframe::App for Arca {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive(ctx);
        self.shortcuts(ctx);

        // Escape backs out of whatever is on top, innermost first, the way it
        // does everywhere else. The password prompt goes through the same path
        // as its Cancel button so a cancelled job is cancelled once.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.waiting_on_password.is_some() {
                self.cancel_password();
            } else if self.show_settings {
                self.show_settings = false;
            }
        }

        // The side buttons on a mouse, which winit reports as Back and Forward
        // and egui hands over as Extra1 and Extra2. Alt+Left and Alt+Right do
        // the same, for anyone without them.
        if matches!(self.view, View::Browse) && !self.busy {
            let (back, forward) = ctx.input(|i| {
                (
                    i.pointer.button_pressed(egui::PointerButton::Extra1)
                        || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)),
                    i.pointer.button_pressed(egui::PointerButton::Extra2)
                        || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight)),
                )
            });
            if back {
                self.go_back();
            }
            if forward {
                self.go_forward();
            }
        }

        // Not while something is running or a window is waiting on an answer:
        // a drop that lands then would be acting on a state that is about to
        // change under it.
        if matches!(self.view, View::Browse)
            && !self.busy
            && self.confirm_delete.is_none()
            && self.confirm_drop.is_none()
            && self.waiting_on_password.is_none()
        {
            let dropped: Vec<PathBuf> = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect()
            });
            self.dropped(ctx, dropped);
        }

        if self.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let ctx2 = ctx.clone();
        match self.view {
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
                    if self.busy {
                        let f = if self.total_count == 0 {
                            0.0
                        } else {
                            self.done_count as f32 / self.total_count as f32
                        };
                        ui.add(
                            egui::ProgressBar::new(f)
                                .text(format!("{} / {}", self.done_count, self.total_count))
                                .desired_width(ui.available_width()),
                        );
                    } else {
                        let color = if self.error {
                            egui::Color32::from_rgb(220, 90, 90)
                        } else {
                            ui.visuals().weak_text_color()
                        };
                        ui.colored_label(color, &self.notice);
                    }
                    });
                });
                egui::CentralPanel::default().show(ctx, |ui| {
                    if self.entries.is_empty() {
                        let text = self.s().drop_here;
                        ui.centered_and_justified(|ui| {
                            ui.label(egui::RichText::new(text).size(16.0).weak());
                        });
                        return;
                    }
                    if let Some(a) = self.archive.clone() {
                        ui.horizontal(|ui| {
                            ui.strong(
                                a.file_name()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default(),
                            );
                            ui.weak(
                                a.parent()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_default(),
                            );
                        });
                        ui.add_space(2.0);
                    }
                    self.table(ui);
                });
                self.drop_hint(&ctx2);
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let startup = parse_args();
    let settings = Settings::load();
    let compact = !matches!(startup, Startup::Browse(_));

    let size = if compact {
        [560.0, 300.0]
    } else {
        [1000.0, 660.0]
    };
    // The icon compiled into the executable covers the Explorer and the
    // shortcut, but winit does not read it for the window itself, so the title
    // bar and the taskbar keep the generic one unless it is set here too.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size([460.0, 240.0])
        .with_title("Arca");
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
            cc.egui_ctx.set_fonts(theme::fonts());
            // Both, not just the one in use: the setting can be changed while
            // the window is open, and egui keeps a style per theme.
            cc.egui_ctx.set_visuals_of(egui::Theme::Dark, theme::dark());
            cc.egui_ctx.set_visuals_of(egui::Theme::Light, theme::light());
            cc.egui_ctx.all_styles_mut(theme::style);
            cc.egui_ctx.set_theme(settings.theme);

            let mut app = Arca::new(settings);
            match startup {
                Startup::Browse(Some(p)) => app.open(&cc.egui_ctx, p),
                Startup::Browse(None) => {}
                Startup::Run(job) => app.run_job(&cc.egui_ctx, job),
                Startup::Add(files) => {
                    app.output_name = quick_output(&files, app.format)
                        .file_name()
                        .map(|x| x.to_string_lossy().to_string())
                        .unwrap_or_default();
                    app.pending_inputs = files;
                    app.view = View::Add;
                }
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The only arithmetic in this file that can be wrong without anyone
    // noticing: a date is either right or plausible, and plausible is worse.
    #[test]
    fn timestamps_become_the_dates_they_are() {
        for (secs, text) in [
            (0_i64, ""),                              // no date recorded
            (-1, ""),                                 // before the epoch: tar can hold these
            (1, "1970-01-01 00:00"),
            (951_827_696, "2000-02-29 12:34"),        // leap day of a leap century
            (1_078_012_800, "2004-02-29 00:00"),      // ordinary leap year
            (1_709_164_800, "2024-02-29 00:00"),
            (1_709_251_199, "2024-02-29 23:59"),      // last minute of that day
            (1_735_689_600, "2025-01-01 00:00"),      // year boundary
            (1_767_225_599, "2025-12-31 23:59"),
            (2_208_988_800, "2040-01-01 00:00"),      // past a 32-bit second count
        ] {
            assert_eq!(when(Some(secs)), text, "{secs}");
        }
        assert_eq!(when(None), "");
    }

    // A folder keeps its whole name and a file loses its extension, and the
    // shell extension has a copy of this that has to agree.
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
