//! Blocking archive jobs and their command-line entry points.

mod io;
pub(crate) use io::*;

use crate::i18n::Strings;
use crate::{archive_stem, detect, human, moved_name, sum_for, Format, INSTALLER_LIMIT};
use arca_core::{Codec, Level};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) enum Job {
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

pub(crate) enum Startup {
    Browse(Option<PathBuf>),
    Run(Job),
    Add(Vec<PathBuf>),
}

pub(crate) fn quick_output(inputs: &[PathBuf], format: Format) -> PathBuf {
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

pub(crate) fn parse_args() -> Startup {
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

pub(crate) fn fill(template: &str, pairs: &[(&str, &str)]) -> String {
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
pub(crate) fn download_update(
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
pub(crate) fn install_update(path: &Path) -> std::result::Result<(), String> {
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
pub(crate) fn install_update(_path: &Path) -> std::result::Result<(), String> {
    // There is no installer to fetch outside Windows, so this is never reached:
    // `arca_net` does not answer and there is never a new version to offer.
    Err("no installer on this system".into())
}

pub(crate) fn run_job_blocking(
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
