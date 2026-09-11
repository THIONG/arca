//! Pure application model, settings, and projections shared by the controller and GPUI.

use crate::i18n::Strings;
use crate::tree::Row;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemePreference {
    System,
    Light,
    Dark,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum Format {
    Zip,
    Tar,
    TarGz,
}

impl Format {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Format::Zip => "ZIP",
            Format::Tar => "TAR",
            Format::TarGz => "TAR.GZ",
        }
    }
}

pub(crate) fn detect(p: &Path) -> Option<Format> {
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
pub(crate) fn saved_of(r: &Row) -> f64 {
    if r.size == 0 {
        0.0
    } else {
        1.0 - r.packed as f64 / r.size as f64
    }
}

// A Unix timestamp as a date somebody can read. Done by hand rather than with
// a date crate: the archive formats store civil time with no zone, so there is
// nothing here worth a dependency that knows about leap seconds and Tokyo.
pub(crate) fn when(mtime: Option<i64>) -> String {
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

pub(crate) fn human(n: u64) -> String {
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

pub(crate) fn archive_stem(p: &Path) -> String {
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

#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum SortColumn {
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
pub(crate) struct Columns {
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
    pub(crate) const ALL: [(SortColumn, &'static str); 11] = [
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

    pub(crate) fn on(&self, which: SortColumn) -> bool {
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

    pub(crate) fn set(&mut self, which: SortColumn, value: bool) {
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

    pub(crate) fn label(which: SortColumn, s: &Strings) -> &'static str {
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
