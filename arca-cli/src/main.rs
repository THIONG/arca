use arca_core::{Codec, Error, Level, Result};
use arca_tar::{TarReader, TarWriter};
use arca_zip::ZipArchive;
use clap::{Parser, Subcommand, ValueEnum};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const BUF: usize = 256 * 1024;

#[derive(Parser)]
#[command(
    name = "arca",
    version,
    about = "Fast, safe archiver",
    long_about = "Arca compresses and extracts archives. The container parsers are written in\nsafe Rust: a malformed file produces an error, never memory corruption."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    #[command(visible_alias = "c", about = "Create an archive")]
    Create {
        #[arg(help = "Output archive (.zip, .tar, .tar.gz)")]
        out: PathBuf,
        #[arg(required = true, help = "Files or directories to include")]
        inputs: Vec<PathBuf>,
        #[arg(short, long, value_enum, default_value_t = LevelArg::Normal,
              help = "Compression level")]
        level: LevelArg,
        #[arg(short, long, value_enum, default_value_t = CodecArg::Auto,
              help = "Compressor. 'auto' uses deflate in .zip for compatibility")]
        codec: CodecArg,
        #[arg(
            short = 'j',
            long,
            default_value_t = 0,
            help = "Threads to use. 0 means every core"
        )]
        threads: usize,
        #[arg(
            short = 'p',
            long,
            help = "Encrypt with AES-256. Other tools will ask for it to open the archive"
        )]
        password: Option<String>,
    },
    #[command(
        visible_alias = "l",
        about = "List the contents without extracting them"
    )]
    List {
        #[arg(help = "Archive to read")]
        archive: PathBuf,
        #[arg(short, long, help = "Report how long it took")]
        time: bool,
    },
    #[command(visible_alias = "x", about = "Extract the contents")]
    Extract {
        #[arg(help = "Archive to extract")]
        archive: PathBuf,
        #[arg(short = 'o', long, default_value = ".", help = "Destination directory")]
        dest: PathBuf,
        #[arg(long, value_enum, default_value_t = OnConflict::Overwrite,
              help = "What to do when the file is already in the destination")]
        on_conflict: OnConflict,
        #[arg(
            short = 'j',
            long,
            default_value_t = 0,
            help = "Threads to use. 0 means every core. Only .zip can go parallel"
        )]
        threads: usize,
        #[arg(short = 'p', long, help = "Password of an AES-256 encrypted archive")]
        password: Option<String>,
    },
    #[command(visible_alias = "t", about = "Check integrity without writing to disk")]
    Test {
        #[arg(help = "Archive to check")]
        archive: PathBuf,
        #[arg(short = 'p', long, help = "Password of an AES-256 encrypted archive")]
        password: Option<String>,
    },
    #[command(
        about = "Add, change or remove the password of an existing archive",
        long_about = "Rewrites the archive with a different password, or with none.\n\nThe entries are not compressed again: WinZip AES encrypts the already\ncompressed bytes, so taking the encryption off gives back the same stream\nthat was there before."
    )]
    Password {
        #[arg(help = "Archive to rewrite (.zip)")]
        archive: PathBuf,
        #[arg(short = 'p', long, help = "Current password, if the archive has one")]
        password: Option<String>,
        #[arg(long, help = "New password. Leave it out to remove the encryption")]
        new: Option<String>,
        #[arg(
            short = 'o',
            long,
            help = "Write here instead of replacing the archive in place"
        )]
        out: Option<PathBuf>,
    },
    #[command(about = "Measure the R1 and R2 performance requirements")]
    Bench {
        #[arg(help = "Archive to measure against")]
        archive: PathBuf,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum CodecArg {
    #[value(help = "Deflate in .zip so anything can read it, zstd where possible")]
    Auto,
    Store,
    Deflate,
    Zstd,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum OnConflict {
    #[value(help = "Replace the file that is already there")]
    Overwrite,
    #[value(help = "Leave the file that is already there and move on")]
    Skip,
    #[value(help = "Write it next to the other one as name (1).ext")]
    Rename,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum LevelArg {
    Store,
    Fast,
    Normal,
    Best,
}

impl From<LevelArg> for Level {
    fn from(n: LevelArg) -> Level {
        match n {
            LevelArg::Store => Level::Store,
            LevelArg::Fast => Level::Fast,
            LevelArg::Normal => Level::Normal,
            LevelArg::Best => Level::Best,
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Format {
    Zip,
    Tar,
    TarGz,
}

fn detect(p: &Path) -> Result<Format> {
    let n = p.to_string_lossy().to_ascii_lowercase();
    if n.ends_with(".zip") {
        Ok(Format::Zip)
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Ok(Format::TarGz)
    } else if n.ends_with(".tar") {
        Ok(Format::Tar)
    } else {
        Err(Error::Unsupported(format!(
            "unrecognized extension in '{}' (.zip, .tar and .tar.gz are supported)",
            p.display()
        )))
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("arca: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Cmd::Create {
            out,
            inputs,
            level,
            codec,
            threads,
            password,
        } => create(
            &out,
            &inputs,
            level.into(),
            codec,
            threads,
            password.as_deref(),
        ),
        Cmd::List { archive, time } => list(&archive, time),
        Cmd::Extract {
            archive,
            dest,
            on_conflict,
            threads,
            password,
        } => extract(&archive, &dest, on_conflict, threads, password.as_deref()),
        Cmd::Test { archive, password } => test_archive(&archive, password.as_deref()),
        Cmd::Password {
            archive,
            password,
            new,
            out,
        } => change_password(
            &archive,
            out.as_deref(),
            password.as_deref(),
            new.as_deref(),
        ),
        Cmd::Bench { archive } => bench(&archive),
    }
}

fn collect_files(inputs: &[PathBuf]) -> Result<Vec<(PathBuf, String)>> {
    let mut v = Vec::new();
    for e in inputs {
        let base = e.parent().unwrap_or(Path::new(""));
        walk(e, base, &mut v)?;
    }
    Ok(v)
}

fn walk(p: &Path, base: &Path, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
    let meta = fs::symlink_metadata(p)?;
    let rel = p.strip_prefix(base).unwrap_or(p);
    let name = rel.to_string_lossy().replace('\\', "/");
    if meta.is_dir() {
        let mut children: Vec<_> = fs::read_dir(p)?.collect::<io::Result<Vec<_>>>()?;
        children.sort_by_key(|d| d.file_name());
        for h in children {
            walk(&h.path(), base, out)?;
        }
    } else if meta.is_file() {
        out.push((p.to_path_buf(), name));
    }
    Ok(())
}

fn mtime_of(m: &fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn resolve_threads(requested: usize) -> usize {
    if requested > 0 {
        return requested;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn resolve_codec(c: CodecArg, format_kind: Format) -> Codec {
    match c {
        CodecArg::Store => Codec::Store,
        CodecArg::Deflate => Codec::Deflate,
        CodecArg::Zstd => Codec::Zstd,
        CodecArg::Auto => match format_kind {
            Format::Zip => Codec::Deflate,
            _ => Codec::Deflate,
        },
    }
}

fn create(
    out: &Path,
    inputs: &[PathBuf],
    level: Level,
    codec_arg: CodecArg,
    requested_threads: usize,
    password: Option<&str>,
) -> Result<()> {
    let format_kind = detect(out)?;
    if password.is_some() && format_kind != Format::Zip {
        return Err(Error::Unsupported(
            "encryption only exists in .zip; .tar and .tar.gz have no place to put it".into(),
        ));
    }
    let codec = resolve_codec(codec_arg, format_kind);
    let threads = resolve_threads(requested_threads);
    let raw_list = collect_files(inputs)?;
    if raw_list.is_empty() {
        return Err(Error::Format("there is nothing to add".into()));
    }

    let mut files: Vec<arca_zip::Source> = Vec::with_capacity(raw_list.len());
    let mut total = 0u64;
    for (path, name) in raw_list {
        let m = fs::metadata(&path)?;
        total += m.len();
        files.push(arca_zip::Source {
            path,
            name,
            size: m.len(),
            mtime: mtime_of(&m),
            codec,
            level,
        });
    }

    let t0 = Instant::now();
    match format_kind {
        // Nothing to report while it runs -- the summary is printed at the end
        // -- so the answer to "carry on?" is always yes.
        Format::Zip => arca_zip::create_zip(out, &files, threads, password, &|_, _, _| true)?,
        Format::Tar | Format::TarGz => {
            let f = BufWriter::with_capacity(BUF, File::create(out)?);
            let dest: Box<dyn Write> = if format_kind == Format::TarGz {
                Box::new(flate2::write::GzEncoder::new(
                    f,
                    flate2::Compression::new(level.to_flate2()),
                ))
            } else {
                Box::new(f)
            };
            let mut w = TarWriter::new(dest);
            for s in &files {
                let entrada = BufReader::with_capacity(BUF, File::open(&s.path)?);
                w.add(&s.name, s.size, s.mtime, 0o644, entrada)?;
            }
            let mut d = w.finish()?;
            d.flush()?;
        }
    }

    let dt = t0.elapsed();
    let final_size = fs::metadata(out)?.len();
    let ratio = if total > 0 {
        100.0 * (1.0 - final_size as f64 / total as f64)
    } else {
        0.0
    };
    let mbs = if dt.as_secs_f64() > 0.0 {
        total as f64 / 1_048_576.0 / dt.as_secs_f64()
    } else {
        0.0
    };
    println!(
        "{}: {} files, {} -> {} ({:.1}% smaller) in {:.3} s · {:.0} MB/s · {} threads",
        out.display(),
        files.len(),
        human(total),
        human(final_size),
        ratio,
        dt.as_secs_f64(),
        mbs,
        threads
    );
    Ok(())
}

fn list(archive: &Path, time: bool) -> Result<()> {
    let t0 = Instant::now();
    let format_kind = detect(archive)?;
    let mut n = 0u64;
    let mut bytes = 0u64;

    let mut out = BufWriter::new(io::stdout().lock());
    match format_kind {
        Format::Zip => {
            let a = ZipArchive::open(File::open(archive)?)?;
            for e in a.entries() {
                writeln!(
                    out,
                    "{:>12}  {:>7}  {:>5.1}%  {}",
                    e.size,
                    e.method.name(),
                    e.ratio() * 100.0,
                    e.name
                )?;
                n += 1;
                bytes += e.size;
            }
        }
        Format::Tar | Format::TarGz => {
            let f = BufReader::with_capacity(BUF, File::open(archive)?);
            let source: Box<dyn Read> = if format_kind == Format::TarGz {
                Box::new(flate2::read::GzDecoder::new(f))
            } else {
                Box::new(f)
            };
            let mut r = TarReader::new(source);
            while let Some(e) = r.next_entry()? {
                writeln!(
                    out,
                    "{:>12}  {:>7}  {:>5}   {}",
                    e.entry.size, "store", "", e.entry.name
                )?;
                n += 1;
                bytes += e.entry.size;
                r.skip_data(&e)?;
            }
        }
    }
    out.flush()?;

    if time {
        eprintln!(
            "{n} entries, {} uncompressed, listed in {:.1} ms",
            human(bytes),
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

// `claimed` holds the names this run has already handed out. Extraction decides
// every destination before it writes anything, so `exists()` alone would give
// two entries with the same name the same free name.
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

fn resolve_conflict(
    path: PathBuf,
    policy: OnConflict,
    claimed: &mut HashSet<PathBuf>,
) -> Option<PathBuf> {
    let taken = path.exists() || claimed.contains(&path);
    let chosen = if !taken {
        path
    } else {
        match policy {
            OnConflict::Overwrite => path,
            OnConflict::Skip => return None,
            OnConflict::Rename => free_name(&path, claimed),
        }
    };
    claimed.insert(chosen.clone());
    Some(chosen)
}

// A .zip is random access: the central directory says where every entry starts,
// so one thread per core can each open the file and decompress a different one.
// A .tar is a single stream, and a .tar.gz a single gzip stream on top of it, so
// there is nothing to split there and that branch stays sequential.
fn extract(
    archive: &Path,
    dest: &Path,
    policy: OnConflict,
    requested_threads: usize,
    password: Option<&str>,
) -> Result<()> {
    let format_kind = detect(archive)?;
    fs::create_dir_all(dest)?;
    let t0 = Instant::now();
    let mut n = 0u64;
    let mut bytes = 0u64;
    let threads = resolve_threads(requested_threads);

    match format_kind {
        Format::Zip => {
            let a = ZipArchive::open(File::open(archive)?)?;
            // Directories and conflicts are settled here, single threaded: two
            // threads racing on create_dir_all or on picking a free name would
            // give a result that depends on who won.
            let mut claimed: HashSet<PathBuf> = HashSet::new();
            let mut jobs: Vec<(arca_core::Entry, PathBuf)> = Vec::new();
            for e in a.entries() {
                let path = dest.join(arca_core::safe_name(&e.name)?);
                if e.is_dir {
                    fs::create_dir_all(&path)?;
                    continue;
                }
                if let Some(p) = path.parent() {
                    fs::create_dir_all(p)?;
                }
                let Some(path) = resolve_conflict(path, policy, &mut claimed) else {
                    continue;
                };
                jobs.push((e.clone(), path));
            }
            drop(a);

            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .map_err(|e| Error::Format(format!("cannot start the thread pool: {e}")))?;
            let written: Vec<u64> = pool.install(|| {
                jobs.par_iter()
                    .map(|(e, path)| {
                        let mut source = BufReader::with_capacity(BUF, File::open(archive)?);
                        let mut f = BufWriter::with_capacity(BUF, File::create(path)?);
                        let w = arca_zip::extract_entry_with(&mut source, e, &mut f, password)?;
                        f.flush()?;
                        Ok(w)
                    })
                    .collect::<Result<Vec<u64>>>()
            })?;
            n = written.len() as u64;
            bytes = written.iter().sum();
        }
        Format::Tar | Format::TarGz => {
            let f = BufReader::with_capacity(BUF, File::open(archive)?);
            let source: Box<dyn Read> = if format_kind == Format::TarGz {
                Box::new(flate2::read::GzDecoder::new(f))
            } else {
                Box::new(f)
            };
            let mut r = TarReader::new(source);
            let mut claimed: HashSet<PathBuf> = HashSet::new();
            while let Some(e) = r.next_entry()? {
                let path = dest.join(arca_core::safe_name(&e.entry.name)?);
                if e.entry.is_dir {
                    fs::create_dir_all(&path)?;
                    r.skip_data(&e)?;
                    continue;
                }
                if let Some(p) = path.parent() {
                    fs::create_dir_all(p)?;
                }
                let Some(path) = resolve_conflict(path, policy, &mut claimed) else {
                    r.skip_data(&e)?;
                    continue;
                };
                let mut w = BufWriter::with_capacity(BUF, File::create(&path)?);
                bytes += r.copy_data(&e, &mut w)?;
                w.flush()?;
                n += 1;
            }
        }
    }

    println!(
        "{n} files, {} written in {:.3} s",
        human(bytes),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

// Rewrites the archive with a different password, or with none. The entries go
// through decrypted but still compressed, so nothing is compressed again and
// the sizes come out identical to the ones that went in.
//
// Replacing in place is the destructive case, and there is only one copy of the
// data. So the new archive is built next to the old one, verified end to end,
// and only then moved over it. Any failure leaves the original untouched.
fn change_password(
    archive: &Path,
    out: Option<&Path>,
    current: Option<&str>,
    new: Option<&str>,
) -> Result<()> {
    if detect(archive)? != Format::Zip {
        return Err(Error::Unsupported(
            "only .zip carries encryption; .tar and .tar.gz have nowhere to put it".into(),
        ));
    }
    let entries = ZipArchive::open(File::open(archive)?)?.entries().to_vec();
    let was_encrypted = entries.iter().any(|e| e.encrypted);
    if !was_encrypted && new.is_none() {
        return Err(Error::Format(format!(
            "'{}' has no password to remove",
            archive.display()
        )));
    }
    if !was_encrypted && current.is_some() {
        return Err(Error::Format(format!(
            "'{}' is not encrypted, so there is no current password",
            archive.display()
        )));
    }

    let target = out.unwrap_or(archive);
    let temp = target.with_file_name(format!(
        "{}.arca-new",
        target
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    let t0 = Instant::now();

    let bytes = match arca_zip::rewrite_password(archive, &temp, current, new, &|_, _, _| true) {
        Ok(b) => b,
        Err(e) => {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
    };

    fs::rename(&temp, target)?;
    println!(
        "{}: {} entries, {} {} in {:.3} s",
        target.display(),
        entries.len(),
        human(bytes),
        if new.is_some() {
            "now encrypted with AES-256"
        } else {
            "no longer encrypted"
        },
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

fn test_archive(archive: &Path, password: Option<&str>) -> Result<()> {
    let format_kind = detect(archive)?;
    let t0 = Instant::now();
    let mut n = 0u64;
    let mut failures = 0u64;

    match format_kind {
        Format::Zip => {
            let mut a = ZipArchive::open(File::open(archive)?)?;
            for i in 0..a.len() {
                if a.entries()[i].is_dir {
                    continue;
                }
                let name = a.entries()[i].name.clone();
                match a.extract_to_with(i, io::sink(), password) {
                    Ok(_) => n += 1,
                    Err(e) => {
                        eprintln!("  FALLO  {name}: {e}");
                        failures += 1;
                    }
                }
            }
        }
        Format::Tar | Format::TarGz => {
            let f = BufReader::with_capacity(BUF, File::open(archive)?);
            let source: Box<dyn Read> = if format_kind == Format::TarGz {
                Box::new(flate2::read::GzDecoder::new(f))
            } else {
                Box::new(f)
            };
            let mut r = TarReader::new(source);
            while let Some(e) = r.next_entry()? {
                r.skip_data(&e)?;
                n += 1;
            }
        }
    }

    if failures > 0 {
        return Err(Error::Format(format!(
            "{failures} corrupt entries out of {}",
            n + failures
        )));
    }
    println!(
        "{n} entries verified, no errors ({:.3} s)",
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

fn bench(archive: &Path) -> Result<()> {
    println!("Performance requirements (design document, section 05)\n");

    let mut best_of = f64::MAX;
    let mut inputs = 0usize;
    for _ in 0..5 {
        let t = Instant::now();
        let a = ZipArchive::open(File::open(archive)?)?;
        inputs = a.len();
        let d = t.elapsed().as_secs_f64() * 1000.0;
        if d < best_of {
            best_of = d;
        }
    }
    let size = fs::metadata(archive)?.len();
    let r2 = best_of < 200.0;
    println!("  R2  list without extracting");
    println!("      {} entries in a {} archive", inputs, human(size));
    println!(
        "      {:.1} ms   target < 200 ms   {}",
        best_of,
        pass_fail(r2)
    );
    println!();
    println!("  R1  cold start: measured from outside, with hyperfine");
    println!("      hyperfine --warmup 20 'arca --version'");
    Ok(())
}

fn pass_fail(ok: bool) -> &'static str {
    if ok {
        "PASS"
    } else {
        "FAIL"
    }
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
