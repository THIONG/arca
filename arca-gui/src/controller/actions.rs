//! Controller protocol and pure runtime projections.

use crate::archive_ops::*;
use crate::i18n::Lang;
use crate::model::*;
use crate::tree::{parent_of, Kind, Row};
use arca_core::Entry;
use std::path::{Path, PathBuf};

pub(crate) enum Message {
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
pub(crate) struct Cut {
    pub(super) archive: PathBuf,
    // The extracted copies handed to the shell. A paste with the move effect
    // takes them out of the temporary folder, and their absence is the only
    // sign Windows gives that it happened.
    pub(super) paths: Vec<PathBuf>,
    pub(super) names: Vec<String>,
}

// What the password window is standing in front of: a job the context menu
// handed us, or an encrypted archive just opened in the window.
// Which blank the password window is filling in. They are not the same
// question: one needs the password the archive already has, the other the one it
// is about to get.
pub(crate) enum Pending {
    Extract(Box<Job>),
    OpenArchive,
    CurrentPassword(Box<Job>),
}

pub(crate) enum View {
    Browse,
    Add,
    Running,
}

pub(crate) enum AppAction {
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
pub(crate) fn attribute_letters(bits: u8) -> String {
    [(0x01, 'R'), (0x02, 'H'), (0x04, 'S'), (0x20, 'A')]
        .iter()
        .map(|(mask, letter)| if bits & mask != 0 { *letter } else { '-' })
        .collect()
}

/// Whether a name claims to be a picture of a kind the window can draw.
pub(crate) fn looks_like_picture(name: &str) -> bool {
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
pub(crate) fn hex_line(at: usize, bytes: &[u8]) -> String {
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
pub(crate) enum Look {
    Text,
    Hex,
    Picture,
}

/// A file out of the archive, held in memory for looking at.
///
/// The bytes are never written to disk. Viewing something is not the same as
/// extracting it, and a viewer that leaves a copy in the temporary folder has
/// quietly extracted it.
pub(crate) struct Viewed {
    pub(crate) name: String,
    // Shared rather than owned outright: the picture view hands these to GPUI
    // on every frame, and a file of thirty megabytes copied sixty times a
    // second is two gigabytes a second of nothing.
    pub(crate) bytes: std::sync::Arc<[u8]>,
    pub(crate) look: Look,
    // Split once when the file arrives rather than on every frame: the view is
    // drawn a line at a time and the lines have to exist to be counted.
    pub(crate) lines: Vec<String>,
    // Whether the picture loader made anything of it. Asked once, because a
    // failed decode is as expensive as a successful one.
    pub(crate) picture: bool,
}

/// The most a file can be and still be opened for looking at.
///
/// A viewer holds the whole thing in memory, and the point of it is a glance at
/// a text file or a picture, not reading a database. Past this the answer is to
/// take it out properly, which is what the rest of the window is for.
pub(crate) const VIEW_LIMIT: u64 = 32 * 1024 * 1024;

/// Whether these bytes are meant to be read as words.
///
/// Two questions, in the order that settles it fastest. A zero byte is the one
/// thing text almost never has and binary almost always does, so it is asked
/// first and on its own. Failing that, the balance of what is printable: a
/// stray high byte is a name with an accent in it, a run of them is a program.
///
/// Only the head is read. A file that begins as text and turns into something
/// else halfway down is a file the reader will notice by looking at it.
pub(crate) fn looks_like_text(bytes: &[u8]) -> bool {
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
pub(crate) fn matches_mask(mask: &str, name: &str) -> bool {
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
pub(crate) fn clock(seconds: f64) -> String {
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
pub(crate) fn subject_of(job: &Job) -> String {
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
pub(crate) const RELEASES_API: &str = "https://api.github.com/repos/THIONG/arca/releases/latest";
pub(crate) const RELEASES_PAGE: &str = "https://github.com/THIONG/arca/releases/latest";

/// As much installer as is ever going to arrive. A reply longer than this is
/// not our release and is not going to be written to disk, let alone run.
pub(crate) const INSTALLER_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Release {
    pub(crate) tag: String,
    pub(crate) installer: Option<String>,
    pub(crate) sums: Option<String>,
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
pub(crate) fn release_of(reply: &str) -> Option<Release> {
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
pub(crate) fn sum_for(listing: &str, name: &str) -> Option<[u8; 32]> {
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
pub(crate) fn installed_by_setup() -> bool {
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
pub(crate) fn tag_of(reply: &str) -> Option<String> {
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
pub(crate) fn newer(running: &str, latest: &str) -> bool {
    pub(crate) fn parts(v: &str) -> Option<(Vec<u32>, bool)> {
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
pub(crate) fn slashed(name: &str) -> String {
    name.replace('\\', "/")
}

/// What an entry is called after a move.
///
/// A folder is not one entry but everything filed under it, so a move matches
/// the name itself, the name with its slash, and everything beneath it.
pub(crate) fn moved_name(name: &str, moves: &[(String, String)]) -> String {
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
pub(crate) fn folder_of(path: &str) -> &str {
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
pub(crate) fn up_row(dir: &str) -> Row {
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
pub(crate) fn wheel_speed(away: f32) -> f32 {
    const DEAD: f32 = 12.0;
    let past = away.abs() - DEAD;
    if past <= 0.0 {
        return 0.0;
    }
    (past * past / 12.0).min(4000.0) * away.signum()
}
