#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod clipboard;
mod gpui_shell;
mod gpui_theme;
mod i18n;
mod tree;

use arca_core::{Codec, Entry, Level};
use arca_tar::{TarReader, TarWriter};
use arca_zip::{ZipArchive, ZipWriter};
use i18n::{strings, Lang, Strings};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;
use tree::{children_of, entries_under, kind_of, parent_of, Kind, Row};

const BUF: usize = 256 * 1024;
// How wide a column starts out and the least it can be pulled down to. The
// name gets the room because it is the thing being read; the rest hold a
// number or a word and are sized for it.
const NAME_WIDE: f32 = 320.0;
const NAME_LEAST: f32 = 140.0;
const CELL_WIDE: f32 = 95.0;
const CELL_LEAST: f32 = 60.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemePreference {
    System,
    Light,
    Dark,
}

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
// How many folders at the front of the path have to go behind the "…" for the
// rest to fit in `room`. Drops from the front, because the folders you are
// nearest are the ones worth seeing, and never drops the last one: the folder
// you are standing in stays whatever its name costs, cut short if it must be.
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
// What the desktop calls this kind of file, cached by extension the way the
// icons are: the answer is the same for every .txt in the archive, and asking
// the shell fifteen hundred times for it would be fifteen hundred round trips
// to another thread while the list is being drawn.
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
// is one of the ones worth naming. GPUI Kit supplies the button surface;
// icons and labels remain separate so each can be styled independently.
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
}

enum View {
    Browse,
    Add,
    Running,
}

enum AppAction {
    Open(PathBuf),
    Run(Job),
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
    // Shared rather than owned outright: the picture view hands these to GPUI
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
    // The entry being renamed and what has been typed into it so far. Held by
    // path rather than by row number so that sorting or filtering underneath a
    // half typed name cannot move the box onto somebody else's row.
    renaming: Option<(String, String)>,
    // One password to try before asking, for a folder of archives all locked
    // with the same word. Never written anywhere: see `default_password_window`.
    default_password: Option<String>,
    asking_default_password: bool,
    // Set while the box that asks for a new folder's name is up.
    asking_folder: bool,
    // The archive that has a previous version kept beside it, and the word for
    // what was done to it. One step back, which is the one anybody wants:
    // deeper than that and the sidecars would pile up.
    undo: Option<(PathBuf, &'static str)>,
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
    show_shortcuts: bool,
    // Set for work that says nothing while it runs. Copying to the clipboard is
    // the only such job: it is over before a bar has finished appearing, and a
    // bar that flashes past says less than nothing.
    quiet: bool,
}

struct AppController {
    state: AppState,
}

impl AppController {
    fn dispatch(&mut self, action: AppAction) {
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
                confirm_delete: None,
                clip_dir: None,
                cut_names: HashSet::new(),
                confirm_drop: None,
                renaming: None,
                default_password: None,
                asking_default_password: false,
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
    fn drag_out(&mut self) {
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
    fn drag_out(&mut self) {}

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
            if let Some((path, pw)) = self.state.after_password.take() {
                let notice = std::mem::take(&mut self.state.notice);
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

    // The path row is the one place the window can run out of width, and it did:
    // a deep folder pushed the trail out over the count at the other end.
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
