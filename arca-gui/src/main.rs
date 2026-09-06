#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod i18n;

use arca_core::{Codec, Entry, Level};
use arca_tar::{TarReader, TarWriter};
use arca_zip::{ZipArchive, ZipWriter};
use eframe::egui;
use egui::ThemePreference;
use egui_extras::{Column, TableBuilder};
use i18n::{strings, Lang, Strings};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Instant;

const BUF: usize = 256 * 1024;
const ROW_HEIGHT: f32 = 20.0;

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

fn dest_path(dest: &Path, name: &str, is_dir: bool) -> arca_core::Result<Option<PathBuf>> {
    let path = dest.join(arca_core::safe_name(name)?);
    if is_dir {
        fs::create_dir_all(&path)?;
        return Ok(None);
    }
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    Ok(Some(path))
}

fn extract(
    archive: &Path,
    dest: &Path,
    wanted: &[bool],
    notify: &dyn Fn(usize, usize, &str),
) -> arca_core::Result<u64> {
    let Some(format) = detect(archive) else {
        return Err(arca_core::Error::Unsupported("unknown format".into()));
    };
    fs::create_dir_all(dest)?;
    let mut bytes = 0u64;

    match format {
        Format::Zip => {
            let mut a = ZipArchive::open(File::open(archive)?)?;
            let total = a.len();
            for i in 0..total {
                let e = a.entries()[i].clone();
                notify(i, total, &e.name);
                if !wanted.is_empty() && !wanted.get(i).copied().unwrap_or(true) {
                    continue;
                }
                if let Some(path) = dest_path(dest, &e.name, e.is_dir)? {
                    let f = BufWriter::with_capacity(BUF, File::create(&path)?);
                    bytes += a.extract_to(i, f)?;
                }
            }
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
                match dest_path(dest, &e.entry.name, e.entry.is_dir)? {
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
    notify: &dyn Fn(usize, usize, &str),
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
    notify: &dyn Fn(usize, usize, &str),
) -> arca_core::Result<(u64, u64)> {
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
                w.add(name, f, codec, level, None)?;
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
    },
    Test(PathBuf),
    Compress {
        out: PathBuf,
        inputs: Vec<PathBuf>,
        format: Format,
        codec: Codec,
        level: Level,
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
        }),
        "--extract-to-folder" if !rest.is_empty() => Startup::Run(Job::Extract {
            archives: rest,
            dest: Destination::Subfolder,
        }),
        "--test" if !rest.is_empty() => Startup::Run(Job::Test(rest[0].clone())),
        "--add" if !rest.is_empty() => Startup::Add(rest),
        "--add-quick" if !rest.is_empty() => Startup::Run(Job::Compress {
            out: quick_output(&rest, Format::Zip),
            inputs: rest,
            format: Format::Zip,
            codec: Codec::Deflate,
            level: Level::Normal,
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
    notify: &dyn Fn(usize, usize, &str),
) -> std::result::Result<String, String> {
    match job {
        Job::Extract { archives, dest } => {
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
                total += extract(a, &target, &[], notify).map_err(|e| e.to_string())?;
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
        Job::Compress {
            out,
            inputs,
            format,
            codec,
            level,
        } => {
            if inputs.is_empty() {
                return Err(s.nothing_to_do.to_string());
            }
            let (from, to) =
                compress(&out, &inputs, format, codec, level, notify).map_err(|e| e.to_string())?;
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
    Progress(usize, usize, String),
    Done(String),
    Failed(String),
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

    fn visible_rows(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        let mut v: Vec<usize> = (0..self.entries.len())
            .filter(|&i| f.is_empty() || self.entries[i].name.to_lowercase().contains(&f))
            .collect();
        let (col, asc) = self.order;
        v.sort_by(|&a, &b| {
            let x = &self.entries[a];
            let y = &self.entries[b];
            let o = match col {
                SortColumn::Name => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                SortColumn::Size => x.size.cmp(&y.size),
                SortColumn::Packed => x.compressed_size.cmp(&y.compressed_size),
                SortColumn::Method => x.method.name().cmp(y.method.name()),
                SortColumn::Saved => x
                    .ratio()
                    .partial_cmp(&y.ratio())
                    .unwrap_or(std::cmp::Ordering::Equal),
            };
            if asc {
                o
            } else {
                o.reverse()
            }
        });
        v
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

    fn run_job(&mut self, ctx: &egui::Context, job: Job) {
        let s: &'static Strings = self.s();
        self.view = View::Running;
        self.title = match &job {
            Job::Extract { .. } => s.extracting.to_string(),
            Job::Test(_) => s.testing.to_string(),
            Job::Compress { .. } => s.compressing.to_string(),
        };
        self.close_when_done = !matches!(job, Job::Test(_));

        let ctx2 = ctx.clone();
        self.spawn(ctx, 0, move |tx| {
            let notify = |i: usize, n: usize, name: &str| {
                let _ = tx.send(Message::Progress(i, n, name.to_string()));
                ctx2.request_repaint();
            };
            let outcome = run_job_blocking(job, s, &notify);
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
                        self.checked = vec![true; v.len()];
                        self.entries = v;
                        if let Some(f) = detect(&path) {
                            self.format = f;
                        }
                        self.archive = Some(path);
                        self.busy = false;
                        close = true;
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
        self.close_when_done = false;
        let ctx2 = ctx.clone();
        self.spawn(ctx, total, move |tx| {
            let notify = |i: usize, n: usize, name: &str| {
                let _ = tx.send(Message::Progress(i, n, name.to_string()));
                ctx2.request_repaint();
            };
            let _ = tx.send(match extract(&archive, &dest, &wanted, &notify) {
                Ok(bytes) => Message::Done(fill(
                    s.extracted_to,
                    &[("size", &human(bytes)), ("dest", &dest.display().to_string())],
                )),
                Err(e) => Message::Failed(e.to_string()),
            });
        });
    }

    fn start_test(&mut self, ctx: &egui::Context) {
        let Some(a) = self.archive.clone() else { return };
        let s: &'static Strings = self.s();
        self.title = s.testing.to_string();
        self.close_when_done = false;
        let total = self.entries.len();
        let ctx2 = ctx.clone();
        self.spawn(ctx, total, move |tx| {
            let notify = |i: usize, n: usize, name: &str| {
                let _ = tx.send(Message::Progress(i, n, name.to_string()));
                ctx2.request_repaint();
            };
            let _ = tx.send(match test_archive(&a, &notify) {
                Ok((good, bad)) if bad.is_empty() => {
                    Message::Done(fill(s.verified_ok, &[("n", &good.to_string())]))
                }
                Ok((good, bad)) => Message::Failed(format!(
                    "{}: {}",
                    fill(
                        s.errors_found,
                        &[("good", &good.to_string()), ("bad", &bad.len().to_string())]
                    ),
                    bad.join("; ")
                )),
                Err(e) => Message::Failed(e.to_string()),
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let s = self.s();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.busy, |ui| {
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
                if ui.add_enabled(has, egui::Button::new(s.test)).clicked() {
                    self.start_test(ctx);
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
            ui.add_enabled_ui(!self.busy, |ui| {
                ui.checkbox(&mut self.into_subfolder, s.into_subfolder);
                ui.add_space(12.0);
                self.settings_row(ui, ctx);
            });
        });
        ui.add_space(6.0);
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

    fn table(&mut self, ui: &mut egui::Ui) {
        let s = self.s();
        let visible = self.visible_rows();

        ui.horizontal(|ui| {
            if ui.small_button(s.check_all).clicked() {
                for i in &visible {
                    self.checked[*i] = true;
                }
            }
            if ui.small_button(s.uncheck_all).clicked() {
                for i in &visible {
                    self.checked[*i] = false;
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
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(26.0))
            .column(Column::initial(90.0).at_least(70.0))
            .column(Column::initial(90.0).at_least(70.0))
            .column(Column::initial(80.0).at_least(60.0))
            .column(Column::initial(60.0).at_least(50.0))
            .column(Column::remainder().at_least(120.0))
            .header(22.0, |mut h| {
                h.col(|_| {});
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
                h.col(|ui| {
                    if head(ui, s.col_name, SortColumn::Name) {
                        requested = Some(SortColumn::Name);
                    }
                });
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, visible.len(), |mut row| {
                    let i = visible[row.index()];
                    let e = self.entries[i].clone();
                    row.col(|ui| {
                        ui.checkbox(&mut self.checked[i], "");
                    });
                    row.col(|ui| {
                        ui.monospace(human(e.size));
                    });
                    row.col(|ui| {
                        ui.monospace(human(e.compressed_size));
                    });
                    row.col(|ui| {
                        ui.label(e.method.name());
                    });
                    row.col(|ui| {
                        let pct = e.ratio() * 100.0;
                        let shown = if pct.abs() < 0.5 { 0.0 } else { pct };
                        ui.monospace(format!("{shown:.0}%"));
                    });
                    row.col(|ui| {
                        if e.is_dir {
                            ui.weak(&e.name);
                        } else {
                            ui.label(&e.name);
                        }
                    });
                });
            });

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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size([460.0, 240.0])
            .with_title("Arca"),
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
