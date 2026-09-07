#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod i18n;
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
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;

const BUF: usize = 256 * 1024;
const ROW_HEIGHT: f32 = 20.0;
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

fn saved_of(r: &Row) -> f64 {
    if r.size == 0 {
        0.0
    } else {
        1.0 - r.packed as f64 / r.size as f64
    }
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lang: None,
            theme: ThemePreference::System,
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
        let _ = fs::write(p, format!("lang = {lang}\ntheme = {theme}\n"));
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
    Compress {
        out: PathBuf,
        inputs: Vec<PathBuf>,
        format: Format,
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
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum SortColumn {
    Name,
    Size,
    Packed,
    Method,
    Saved,
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
    // Set when the cursor moves by keyboard, so the table can scroll it into
    // view on the next frame and then forget about it.
    scroll_to_cursor: bool,
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
            scroll_to_cursor: false,
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
            Job::Compress { .. } => s.compressing.to_string(),
        };
        self.close_when_done = !matches!(job, Job::Test(_) | Job::Password { .. });
        // The file on disk is about to change, so the listing has to be redone.
        if let Job::Password { archive, new, .. } = &job {
            self.after_password = Some((archive.clone(), new.clone()));
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
                        self.checked = vec![true; v.len()];
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
        let mut moved_cursor: Option<usize> = None;
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

        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            // Without this the cells only sense hovering, and row.response()
            // would never report a double click.
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(26.0))
            // Name first and wide, with the icon inside it. That is where the
            // Explorer and every archiver put it, and a separate icon column
            // only pushed the one thing you read away from its picture.
            .column(Column::initial(300.0).at_least(140.0))
            .column(Column::initial(90.0).at_least(70.0))
            .column(Column::initial(90.0).at_least(70.0))
            .column(Column::initial(80.0).at_least(60.0))
            .column(Column::remainder().at_least(50.0))
            .header(22.0, |mut h| {
                h.col(|_| {});
                h.col(|ui| {
                    if head(ui, s.col_name, SortColumn::Name) {
                        requested = Some(SortColumn::Name);
                    }
                });
                h.col(|ui| {
                    if head(ui, s.col_size, SortColumn::Size) {
                        requested = Some(SortColumn::Size);
                    }
                });
                h.col(|ui| {
                    if head(ui, s.col_packed, SortColumn::Packed) {
                        requested = Some(SortColumn::Packed);
                    }
                });
                h.col(|ui| {
                    if head(ui, s.col_method, SortColumn::Method) {
                        requested = Some(SortColumn::Method);
                    }
                });
                h.col(|ui| {
                    if head(ui, s.col_saved, SortColumn::Saved) {
                        requested = Some(SortColumn::Saved);
                    }
                });
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, visible.len(), |mut row| {
                    let idx = row.index();
                    let r = &visible[idx];
                    row.set_selected(self.cursor == Some(idx));
                    let mut flag = self.is_checked(r);
                    row.col(|ui| {
                        if ui.checkbox(&mut flag, "").changed() {
                            toggle = Some((idx, flag));
                        }
                    });
                    row.col(|ui| {
                        draw_icon(ui, r.kind);
                        ui.add_space(4.0);
                        let text = if r.is_dir {
                            egui::RichText::new(&r.label).strong()
                        } else {
                            egui::RichText::new(&r.label)
                        };
                        ui.add(egui::Label::new(text).selectable(false).truncate());
                    });
                    row.col(|ui| {
                        ui.monospace(human(r.size));
                    });
                    row.col(|ui| {
                        ui.monospace(human(r.packed));
                    });
                    row.col(|ui| {
                        if r.is_dir {
                            ui.weak(format!("{} {}", r.count, s.items_word));
                        } else if r.encrypted {
                            ui.label(format!("AES-256 {}", r.method));
                        } else {
                            ui.label(r.method);
                        }
                    });
                    row.col(|ui| {
                        let pct = saved_of(r) * 100.0;
                        let shown = if pct.abs() < 0.5 { 0.0 } else { pct };
                        ui.monospace(format!("{shown:.0}%"));
                    });
                    // The whole row answers, not just the name: aiming at the
                    // text to open something is a nuisance nobody expects.
                    let resp = row.response();
                    if resp.hovered() {
                        resp.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        moved_cursor = Some(idx);
                    }
                    if resp.double_clicked() {
                        opened = Some(idx);
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
        self.scroll_to_cursor = false;
        if let Some(index) = moved_cursor {
            self.cursor = Some(index);
        }
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
            let (open, extract) = ctx.input(|i| {
                (
                    i.modifiers.command && i.key_pressed(egui::Key::O),
                    i.modifiers.command && i.key_pressed(egui::Key::E),
                )
            });
            if open {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Archives", &["zip", "tar", "gz", "tgz"])
                    .pick_file()
                {
                    self.open(ctx, p);
                }
            }
            if extract && self.archive.is_some() {
                self.ask_extract(ctx, false);
            }

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

        if matches!(self.view, View::Browse) {
            let dropped: Vec<PathBuf> = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect()
            });
            if let Some(p) = dropped.into_iter().next() {
                if !self.busy {
                    self.notice.clear();
                    self.open(ctx, p);
                }
            }
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
                egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
                    ui.add_space(5.0);
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
                    ui.add_space(5.0);
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
            let mut style = (*cc.egui_ctx.style()).clone();
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(8.0, 4.0);
            cc.egui_ctx.set_style(style);
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
