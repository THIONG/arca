use std::fmt;
use std::io;

pub mod limits {
    pub const MAX_NAME: usize = 4096;
    pub const MAX_ENTRIES: u64 = 10_000_000;
    pub const MAX_EXTRA: usize = 65_535;
    pub const MAX_RATIO: u64 = 1000;
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Format(String),
    Limit(String),
    Integrity {
        name: String,
        expected: u32,
        found: u32,
    },
    // The AES authentication code did not match. Separate from Integrity
    // because there is no CRC to report here: the HMAC is what failed, and
    // the difference matters, since it also catches deliberate tampering.
    Tampered {
        name: String,
    },
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Format(m) => write!(f, "malformed archive: {m}"),
            Error::Limit(m) => write!(f, "limit exceeded: {m}"),
            Error::Integrity {
                name,
                expected,
                found,
            } => write!(
                f,
                "integrity failure in '{name}': expected CRC {expected:08x}, found {found:08x}"
            ),
            Error::Tampered { name } => write!(
                f,
                "'{name}' did not pass its authentication code: the archive was altered after it was encrypted"
            ),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Store,
    Deflate,
    Zstd,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Store => "store",
            Method::Deflate => "deflate",
            Method::Zstd => "zstd",
        }
    }

    pub fn code(self) -> u16 {
        match self {
            Method::Store => 0,
            Method::Deflate => 8,
            Method::Zstd => 93,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Store,
    Fast,
    Normal,
    Best,
}

impl Level {
    pub fn to_flate2(self) -> u32 {
        match self {
            Level::Store => 0,
            Level::Fast => 1,
            Level::Normal => 6,
            Level::Best => 9,
        }
    }

    pub fn to_zstd(self) -> i32 {
        match self {
            Level::Store => 1,
            Level::Fast => 1,
            Level::Normal => 3,
            Level::Best => 12,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Store,
    Deflate,
    Zstd,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub compressed_size: u64,
    pub method: Method,
    pub crc32: u32,
    pub is_dir: bool,
    pub mtime: Option<i64>,
    pub offset: u64,
    // WinZip AES: the entry is encrypted and `method` holds the real compressor,
    // read out of the 0x9901 extra field rather than the method field, which
    // says 99 for every encrypted entry.
    pub encrypted: bool,
}

impl Entry {
    pub fn ratio(&self) -> f64 {
        if self.size == 0 {
            return 0.0;
        }
        1.0 - (self.compressed_size as f64 / self.size as f64)
    }
}

pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    pub fn at(data: &'a [u8], pos: usize) -> Result<Self> {
        if pos > data.len() {
            return Err(Error::Format(format!(
                "offset {pos} past end of archive ({} bytes)",
                data.len()
            )));
        }
        Ok(Cursor { data, pos })
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| Error::Format(format!("overflow while reading {what}")))?;
        if end > self.data.len() {
            return Err(Error::Format(format!(
                "archive truncated while reading {what}: {} bytes missing",
                end - self.data.len()
            )));
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u16le(&mut self, what: &str) -> Result<u16> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32le(&mut self, what: &str) -> Result<u32> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64le(&mut self, what: &str) -> Result<u64> {
        let b = self.take(8, what)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn bytes(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        self.take(n, what)
    }

    pub fn skip(&mut self, n: usize, what: &str) -> Result<()> {
        self.take(n, what).map(|_| ())
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

pub fn dos_to_unix(date: u16, time: u16) -> Option<i64> {
    let year = 1980i64 + ((date >> 9) & 0x7f) as i64;
    let month = ((date >> 5) & 0x0f) as i64;
    let day = (date & 0x1f) as i64;
    let h = ((time >> 11) & 0x1f) as i64;
    let m = ((time >> 5) & 0x3f) as i64;
    let s = ((time & 0x1f) * 2) as i64;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 59 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + m * 60 + s)
}

pub fn unix_to_dos(ts: i64) -> (u16, u16) {
    let days = ts.div_euclid(86_400);
    let rest = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let dos_year = (year - 1980).clamp(0, 127);
    let date = ((dos_year as u16) << 9) | ((month as u16) << 5) | day as u16;
    let time = (((rest / 3600) as u16) << 11)
        | ((((rest % 3600) / 60) as u16) << 5)
        | (((rest % 60) / 2) as u16);
    (date, time)
}

pub fn safe_name(name: &str) -> Result<std::path::PathBuf> {
    if name.len() > limits::MAX_NAME {
        return Err(Error::Limit(format!("name of {} bytes", name.len())));
    }
    if name.contains('\0') {
        return Err(Error::Format("name contains a null byte".into()));
    }
    let normalized = name.replace('\\', "/");
    let mut out = std::path::PathBuf::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                return Err(Error::Format(format!(
                    "entry '{name}' tries to escape the destination directory"
                )))
            }
            p => {
                if p.len() >= 2 && p.as_bytes()[1] == b':' {
                    return Err(Error::Format(format!(
                        "entry '{name}' carries a drive letter"
                    )));
                }
                out.push(p);
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(Error::Format(format!("empty name: '{name}'")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_truncates_without_panic() {
        let d = [1u8, 2, 3];
        let mut c = Cursor::new(&d);
        assert!(c.u16le("x").is_ok());
        assert!(matches!(c.u32le("y"), Err(Error::Format(_))));
    }

    #[test]
    fn cursor_rejects_offset_past_end() {
        let d = [0u8; 4];
        assert!(Cursor::at(&d, 99).is_err());
    }

    #[test]
    fn zip_slip_rejected() {
        assert!(safe_name("../../etc/passwd").is_err());
        assert!(safe_name("..\\..\\windows\\system32").is_err());
        assert!(safe_name("C:/tmp/x").is_err());
        assert!(safe_name("a/b/c.txt").is_ok());
        assert_eq!(
            safe_name("./a//b.txt").unwrap(),
            std::path::PathBuf::from("a/b.txt")
        );
    }

    #[test]
    fn dos_dates_round_trip() {
        let ts = 1_710_498_600i64;
        let (f, h) = unix_to_dos(ts);
        let back = dos_to_unix(f, h).unwrap();
        assert!((back - ts).abs() <= 2, "{back} vs {ts}");
    }
}
