#![forbid(unsafe_code)]

pub mod aes;
pub mod pages;

use arca_core::{limits, Codec, Cursor, Entry, Error, Level, Method, Result};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use std::io::{self, Read, Seek, SeekFrom, Write};

const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_EOCD64: u32 = 0x0606_4b50;
const SIG_LOC64: u32 = 0x0706_4b50;
const SIG_CD: u32 = 0x0201_4b50;
const SIG_LFH: u32 = 0x0403_4b50;

const EOCD_MIN: usize = 22;
const CD_FIXED: usize = 46;
const LFH_FIXED: usize = 30;
const EXTRA_Z64: usize = 20;
const MAX_COMMENT: usize = 65_535;
const STREAM_BUF: usize = 256 * 1024;

struct CrcWriter<W: Write> {
    inner: W,
    hasher: crc32fast::Hasher,
    written: u64,
}

impl<W: Write> CrcWriter<W> {
    fn new(inner: W) -> Self {
        CrcWriter {
            inner,
            hasher: crc32fast::Hasher::new(),
            written: 0,
        }
    }
    fn finalize(self) -> (W, u32, u64) {
        (self.inner, self.hasher.finalize(), self.written)
    }
}

impl<W: Write> Write for CrcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub struct ZipArchive<R: Read + Seek> {
    source: R,
    entries_list: Vec<Entry>,
}

impl<R: Read + Seek> ZipArchive<R> {
    pub fn open(mut source: R) -> Result<Self> {
        let total = source.seek(SeekFrom::End(0))?;
        if total < EOCD_MIN as u64 {
            return Err(Error::Format("too short to be a ZIP".into()));
        }

        let tail_len = (EOCD_MIN + MAX_COMMENT).min(total as usize);
        let tail_start = total - tail_len as u64;
        source.seek(SeekFrom::Start(tail_start))?;
        let mut tail = vec![0u8; tail_len];
        source.read_exact(&mut tail)?;

        let pos_eocd = find_backwards(&tail, SIG_EOCD).ok_or_else(|| {
            Error::Format("end of central directory not found (is this a ZIP?)".into())
        })?;

        let mut c = Cursor::at(&tail, pos_eocd)?;
        c.skip(4, "EOCD signature")?;
        c.skip(4, "disk numbers")?;
        let _ent_disco = c.u16le("entries on this disk")?;
        let mut n_entries = c.u16le("total entries")? as u64;
        let mut cd_size = c.u32le("central directory size")? as u64;
        let mut cd_offset = c.u32le("central directory offset")? as u64;

        if n_entries == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
            if let Some(p) = find_backwards(&tail[..pos_eocd], SIG_LOC64) {
                let mut l = Cursor::at(&tail, p)?;
                l.skip(4, "Zip64 locator signature")?;
                l.skip(4, "EOCD64 disk")?;
                let off64 = l.u64le("EOCD64 offset")?;
                if off64 >= total {
                    return Err(Error::Format(
                        "the Zip64 locator points past the end of the archive".into(),
                    ));
                }
                source.seek(SeekFrom::Start(off64))?;
                let mut r64 = [0u8; 56];
                source.read_exact(&mut r64)?;
                let mut z = Cursor::new(&r64);
                if z.u32le("EOCD64 signature")? != SIG_EOCD64 {
                    return Err(Error::Format("invalid Zip64 signature".into()));
                }
                z.skip(20, "EOCD64 header")?;
                z.skip(8, "entries on this disk")?;
                n_entries = z.u64le("total entries (Zip64)")?;
                cd_size = z.u64le("central directory size (Zip64)")?;
                cd_offset = z.u64le("central directory offset (Zip64)")?;
            }
        }

        if n_entries > limits::MAX_ENTRIES {
            return Err(Error::Limit(format!("{n_entries} entries declared")));
        }
        if cd_offset > total || cd_size > total || cd_offset + cd_size > total {
            return Err(Error::Format(
                "the central directory falls outside the archive".into(),
            ));
        }

        source.seek(SeekFrom::Start(cd_offset))?;
        let mut cd = vec![0u8; cd_size as usize];
        source.read_exact(&mut cd)?;

        let mut entries_list = Vec::with_capacity(n_entries.min(4096) as usize);
        let mut c = Cursor::new(&cd);
        for i in 0..n_entries {
            if c.remaining() < CD_FIXED {
                break;
            }
            match read_central_header(&mut c) {
                Ok(e) => entries_list.push(e),
                Err(e) => {
                    return Err(Error::Format(format!(
                        "entry {i} of the central directory: {e}"
                    )))
                }
            }
        }

        Ok(ZipArchive {
            source,
            entries_list,
        })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries_list
    }

    pub fn len(&self) -> usize {
        self.entries_list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries_list.is_empty()
    }

    pub fn extract_to<W: Write>(&mut self, idx: usize, dest: W) -> Result<u64> {
        self.extract_to_with(idx, dest, None)
    }

    pub fn extract_to_with<W: Write>(
        &mut self,
        idx: usize,
        dest: W,
        password: Option<&str>,
    ) -> Result<u64> {
        let e = self
            .entries_list
            .get(idx)
            .ok_or_else(|| Error::Format(format!("no such entry: {idx}")))?
            .clone();
        extract_entry_with(&mut self.source, &e, dest, password)
    }

    pub fn has_encrypted(&self) -> bool {
        self.entries_list.iter().any(|e| e.encrypted)
    }
}

// Takes its own reader instead of borrowing the archive, so several threads
// can each open the file and pull a different entry at the same time. A ZIP is
// random access through its central directory, which is what makes that
// possible at all.
pub fn extract_entry<R: Read + Seek, W: Write>(source: &mut R, e: &Entry, dest: W) -> Result<u64> {
    extract_entry_with(source, e, dest, None)
}

pub fn extract_entry_with<R: Read + Seek, W: Write>(
    source: &mut R,
    e: &Entry,
    dest: W,
    password: Option<&str>,
) -> Result<u64> {
    let (crc_val, written) = extract_inner(source, e, dest, password)?;

    // AE-2 stores a zero CRC on purpose, and the HMAC has already spoken for
    // the contents. AE-1 keeps the real one, so a non-zero value still gets
    // checked either way.
    if !(e.encrypted && e.crc32 == 0) && crc_val != e.crc32 {
        return Err(Error::Integrity {
            name: e.name.clone(),
            expected: e.crc32,
            found: crc_val,
        });
    }
    if written != e.size {
        return Err(Error::Format(format!(
            "'{}': expected {} bytes but produced {written}",
            e.name, e.size
        )));
    }
    Ok(written)
}

// An AE-2 entry carries no CRC of its own, so rewriting it as a plain entry
// means working the checksum out first, and the only way to do that is to
// decompress the whole thing. Nothing is kept: it goes straight to a sink.
pub fn checksum_entry<R: Read + Seek>(
    source: &mut R,
    e: &Entry,
    password: Option<&str>,
) -> Result<u32> {
    Ok(extract_inner(source, e, io::sink(), password)?.0)
}

// Decrypts without decompressing, which is what changing a password comes down
// to. WinZip AES encrypts the already compressed bytes, so taking the
// encryption off gives back the exact deflate stream that was there before:
// there is nothing to compress again.
pub fn copy_compressed<R: Read + Seek, W: Write + ?Sized>(
    source: &mut R,
    e: &Entry,
    dest: &mut W,
    password: Option<&str>,
) -> Result<u64> {
    let mut bounded = open_data(source, e)?;
    if !e.encrypted {
        return Ok(io::copy(&mut bounded, dest)?);
    }
    let (keys, cipher_len) = unlock(&mut bounded, e, password)?;
    let mut reader = aes::AesReader::new(&mut bounded, &keys, cipher_len)?;
    let n = io::copy(&mut reader, dest)?;
    let computed = reader.finish()?;
    let mut auth = [0u8; aes::AUTH_CODE];
    bounded.read_exact(&mut auth)?;
    if !aes::auth_matches(&computed, &auth) {
        return Err(Error::Tampered {
            name: e.name.clone(),
        });
    }
    Ok(n)
}

// Reads the local header, checks it agrees with the central directory, and
// hands back a reader stopped at the end of this entry's data.
fn open_data<'a, R: Read + Seek>(source: &'a mut R, e: &Entry) -> Result<io::Take<&'a mut R>> {
    source.seek(SeekFrom::Start(e.offset))?;
    let mut lfh = [0u8; LFH_FIXED];
    source.read_exact(&mut lfh)?;
    let mut c = Cursor::new(&lfh);
    if c.u32le("local header signature")? != SIG_LFH {
        return Err(Error::Format(format!(
            "'{}': no local header at offset {}",
            e.name, e.offset
        )));
    }
    c.skip(2, "version")?;
    let flags = c.u16le("flags")?;
    if flags & 1 != 0 && !e.encrypted {
        return Err(Error::Unsupported(format!(
            "'{}' uses ZipCrypto, the old password scheme (only AES-256 is supported)",
            e.name
        )));
    }
    c.skip(18, "rest of the local header")?;
    let n_len = c.u16le("name length")? as u64;
    let x_len = c.u16le("extra field length")? as u64;

    let data_start = e.offset + LFH_FIXED as u64 + n_len + x_len;
    source.seek(SeekFrom::Start(data_start))?;
    Ok(source.take(e.compressed_size))
}

// Eats the salt and the verifier off the front of the entry and returns the
// keys, plus how many bytes of ciphertext follow before the authentication
// code. A wrong password stops here, before anything is decrypted.
fn unlock<R: Read>(bounded: &mut R, e: &Entry, password: Option<&str>) -> Result<(aes::Keys, u64)> {
    let pw = password.ok_or_else(|| {
        Error::Unsupported(format!("'{}' is encrypted and needs a password", e.name))
    })?;
    let cipher_len = e
        .compressed_size
        .checked_sub(aes::OVERHEAD as u64)
        .ok_or_else(|| {
            Error::Format(format!(
                "'{}': {} bytes, shorter than its own AES header",
                e.name, e.compressed_size
            ))
        })?;
    let mut salt = [0u8; aes::SALT_256];
    bounded.read_exact(&mut salt)?;
    let mut given = [0u8; aes::VERIFIER];
    bounded.read_exact(&mut given)?;

    let keys = aes::derive(pw, &salt);
    if !aes::verifier_matches(&keys, &given) {
        return Err(Error::Format(format!("'{}': wrong password", e.name)));
    }
    Ok((keys, cipher_len))
}

fn extract_inner<R: Read + Seek, W: Write>(
    source: &mut R,
    e: &Entry,
    dest: W,
    password: Option<&str>,
) -> Result<(u32, u64)> {
    let mut bounded = open_data(source, e)?;
    let mut cw = CrcWriter::new(dest);

    if e.encrypted {
        let (keys, cipher_len) = unlock(&mut bounded, e, password)?;
        let reader = aes::AesReader::new(&mut bounded, &keys, cipher_len)?;
        let reader = decompress_into(reader, e.method, &mut cw)?;
        let computed = reader.finish()?;

        // The authentication code sits after the ciphertext, so it can only be
        // checked once everything has already been written out. Whoever called
        // this must throw away what it wrote if this fails: the bytes came from
        // a key that verified, but nothing so far proves they were not altered.
        let mut auth = [0u8; aes::AUTH_CODE];
        bounded.read_exact(&mut auth)?;
        if !aes::auth_matches(&computed, &auth) {
            return Err(Error::Tampered {
                name: e.name.clone(),
            });
        }
    } else {
        decompress_into(bounded, e.method, &mut cw)?;
    }

    let (_, crc_val, written) = cw.finalize();
    Ok((crc_val, written))
}

// Hands the reader back afterwards, which is what lets the encrypted path get
// its AES reader out to ask it for the authentication code. A decompressor may
// stop before the end of its input, so the reader is not necessarily drained.
fn decompress_into<Rd: Read, W: Write>(
    src: Rd,
    method_code: Method,
    cw: &mut CrcWriter<W>,
) -> Result<Rd> {
    match method_code {
        Method::Store => {
            let mut a = src;
            io::copy(&mut a, cw)?;
            Ok(a)
        }
        Method::Deflate => {
            let mut dec = flate2::read::DeflateDecoder::new(src);
            io::copy(&mut dec, cw)?;
            Ok(dec.into_inner())
        }
        Method::Zstd => {
            #[cfg(feature = "codecs-native")]
            {
                let mut dec = zstd::stream::read::Decoder::new(src).map_err(Error::Io)?;
                io::copy(&mut dec, cw)?;
                Ok(dec.finish().into_inner())
            }
            #[cfg(not(feature = "codecs-native"))]
            {
                let _ = src;
                Err(Error::Unsupported(
                    "the entry uses Zstandard and this binary was built without it".into(),
                ))
            }
        }
    }
}

fn find_backwards(buf: &[u8], signature: u32) -> Option<usize> {
    let f = signature.to_le_bytes();
    if buf.len() < 4 {
        return None;
    }
    (0..=buf.len() - 4).rev().find(|&i| buf[i..i + 4] == f)
}

#[rustfmt::skip]
const CP437_HIGH: [char; 128] = [
    '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00E4}', '\u{00E0}', '\u{00E5}', '\u{00E7}',
    '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{00EC}', '\u{00C4}', '\u{00C5}',
    '\u{00C9}', '\u{00E6}', '\u{00C6}', '\u{00F4}', '\u{00F6}', '\u{00F2}', '\u{00FB}', '\u{00F9}',
    '\u{00FF}', '\u{00D6}', '\u{00DC}', '\u{00A2}', '\u{00A3}', '\u{00A5}', '\u{20A7}', '\u{0192}',
    '\u{00E1}', '\u{00ED}', '\u{00F3}', '\u{00FA}', '\u{00F1}', '\u{00D1}', '\u{00AA}', '\u{00BA}',
    '\u{00BF}', '\u{2310}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00A1}', '\u{00AB}', '\u{00BB}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}',
    '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}',
    '\u{03B1}', '\u{00DF}', '\u{0393}', '\u{03C0}', '\u{03A3}', '\u{03C3}', '\u{00B5}', '\u{03C4}',
    '\u{03A6}', '\u{0398}', '\u{03A9}', '\u{03B4}', '\u{221E}', '\u{03C6}', '\u{03B5}', '\u{2229}',
    '\u{2261}', '\u{00B1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00F7}', '\u{2248}',
    '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{207F}', '\u{00B2}', '\u{25A0}', '\u{00A0}',
];

fn from_cp437(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii() {
                b as char
            } else {
                CP437_HIGH[usize::from(b) - 0x80]
            }
        })
        .collect()
}

/// The two times a zip does not have room for in its header.
#[derive(Default, PartialEq, Eq, Debug)]
struct Times {
    created: Option<i64>,
    accessed: Option<i64>,
}

/// A Windows FILETIME as seconds since 1970.
///
/// It counts hundreds of nanoseconds from the first day of 1601, which is a
/// unit and an epoch nothing else uses. Zero means "not recorded" rather than
/// the year 1601, which is what every tool that writes one of these means by
/// leaving it empty.
fn filetime_to_unix(ticks: u64) -> Option<i64> {
    if ticks == 0 {
        return None;
    }
    Some((ticks / 10_000_000) as i64 - 11_644_473_600)
}

/// Reads the creation and access times out of an entry's extra fields.
///
/// Two spellings, because the two halves of the world that write zips do not
/// agree. Windows writes 0x000A, a block of three Windows FILETIMEs; the unix
/// tools write 0x5455, a flag byte saying which of the three are there followed
/// by that many 32-bit unix times. Neither is required and most archives carry
/// neither, so the answer is usually nothing at all.
///
/// Anything malformed is skipped rather than refused. These fields are a
/// courtesy from whoever wrote the archive and a wrong one is not a reason to
/// stop reading it.
fn read_times(extra: &[u8]) -> Times {
    let mut out = Times::default();
    let mut x = Cursor::new(extra);
    while x.remaining() >= 4 {
        let Ok(id) = x.u16le("extra field id") else {
            break;
        };
        let Ok(size_val) = x.u16le("extra field size") else {
            break;
        };
        let size_val = size_val as usize;
        if size_val > x.remaining() {
            break;
        }
        let Ok(body) = x.bytes(size_val, "extra field body") else {
            break;
        };
        match id {
            // NTFS: four reserved bytes, then tagged blocks. Tag 1 holds the
            // three times in the order the file system keeps them.
            0x000A => {
                let mut n = Cursor::new(body);
                if n.skip(4, "reserved").is_err() {
                    continue;
                }
                while n.remaining() >= 4 {
                    let (Ok(tag), Ok(len)) = (n.u16le("tag"), n.u16le("tag size")) else {
                        break;
                    };
                    let len = len as usize;
                    if len > n.remaining() {
                        break;
                    }
                    if tag == 0x0001 && len >= 24 {
                        let Ok(block) = n.bytes(len, "times") else {
                            break;
                        };
                        let mut t = Cursor::new(block);
                        let _mtime = t.u64le("mtime");
                        if let Ok(v) = t.u64le("atime") {
                            out.accessed = filetime_to_unix(v);
                        }
                        if let Ok(v) = t.u64le("ctime") {
                            out.created = filetime_to_unix(v);
                        }
                        break;
                    }
                    if n.skip(len, "tag body").is_err() {
                        break;
                    }
                }
            }
            // Extended timestamp: one flag byte, then the times that are
            // present, always in the order modified, accessed, created.
            0x5455 => {
                let mut e = Cursor::new(body);
                let Ok(first) = e.bytes(1, "flags") else {
                    continue;
                };
                let flags = first[0];
                if flags & 1 != 0 && e.remaining() >= 4 {
                    let _ = e.u32le("mtime");
                }
                if flags & 2 != 0 && e.remaining() >= 4 {
                    if let Ok(v) = e.u32le("atime") {
                        out.accessed = Some(v as i32 as i64);
                    }
                }
                if flags & 4 != 0 && e.remaining() >= 4 {
                    if let Ok(v) = e.u32le("ctime") {
                        out.created = Some(v as i32 as i64);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn read_central_header(c: &mut Cursor<'_>) -> Result<Entry> {
    if c.u32le("central directory signature")? != SIG_CD {
        return Err(Error::Format("invalid entry signature".into()));
    }
    c.skip(4, "versions")?;
    let flags = c.u16le("flags")?;
    let method_code = c.u16le("method")?;
    let time_val = c.u16le("time")?;
    let date_val = c.u16le("date")?;
    let crc_val = c.u32le("crc32")?;
    let mut comp_size = c.u32le("compressed size")? as u64;
    let mut uncompressed = c.u32le("uncompressed size")? as u64;
    let n_len = c.u16le("name length")? as usize;
    let x_len = c.u16le("extra field length")? as usize;
    let k_len = c.u16le("comment length")? as usize;
    c.skip(4, "disk and internal attributes")?;
    let ext_attr = c.u32le("external attributes")?;
    let mut offset = c.u32le("local offset")? as u64;

    if n_len > limits::MAX_NAME {
        return Err(Error::Limit(format!("name of {n_len} bytes")));
    }
    if x_len > limits::MAX_EXTRA {
        return Err(Error::Limit(format!("extra field of {x_len} bytes")));
    }

    let name_bytes = c.bytes(n_len, "name")?;
    let extra = c.bytes(x_len, "extra field")?;
    c.skip(k_len, "comment")?;

    if uncompressed == 0xFFFF_FFFF || comp_size == 0xFFFF_FFFF || offset == 0xFFFF_FFFF {
        let mut x = Cursor::new(extra);
        while x.remaining() >= 4 {
            let id = x.u16le("extra field id")?;
            let size_val = x.u16le("extra field size")? as usize;
            if size_val > x.remaining() {
                break;
            }
            if id == 0x0001 {
                let mut z = Cursor::new(x.bytes(size_val, "Zip64 data")?);
                if uncompressed == 0xFFFF_FFFF && z.remaining() >= 8 {
                    uncompressed = z.u64le("uncompressed size (Zip64)")?;
                }
                if comp_size == 0xFFFF_FFFF && z.remaining() >= 8 {
                    comp_size = z.u64le("compressed size (Zip64)")?;
                }
                if offset == 0xFFFF_FFFF && z.remaining() >= 8 {
                    offset = z.u64le("offset (Zip64)")?;
                }
                break;
            }
            x.skip(size_val, "extra field")?;
        }
    }

    // Method 99 means WinZip AES, and the field no longer says how the entry
    // was compressed: the real method lives in the 0x9901 extra field.
    let encrypted = method_code == aes::METHOD_AE;
    let real_code = if encrypted {
        let info = aes::parse_extra(extra).ok_or_else(|| {
            Error::Format("entry says AES but carries no 0x9901 extra field".into())
        })?;
        if info.strength != aes::STRENGTH_256 {
            return Err(Error::Unsupported(format!(
                "AES-{} (only AES-256 is supported)",
                match info.strength {
                    1 => "128",
                    2 => "192",
                    _ => "?",
                }
            )));
        }
        info.real_method
    } else {
        if flags & 1 != 0 {
            return Err(Error::Unsupported(
                "ZipCrypto, the old password scheme (only AES-256 is supported)".into(),
            ));
        }
        method_code
    };

    let method = match real_code {
        0 => Method::Store,
        8 => Method::Deflate,
        93 => Method::Zstd,
        other => {
            return Err(Error::Unsupported(format!(
                "compression method {other} (store, deflate and zstd are supported)"
            )))
        }
    };

    let utf8 = flags & 0x800 != 0;
    let name = if utf8 {
        String::from_utf8_lossy(name_bytes).into_owned()
    } else {
        from_cp437(name_bytes)
    };
    let is_dir = name.ends_with('/') || name.ends_with('\\');
    let times = read_times(extra);

    Ok(Entry {
        name,
        raw_name: name_bytes.to_vec(),
        utf8,
        size: uncompressed,
        compressed_size: comp_size,
        method,
        crc32: crc_val,
        is_dir,
        mtime: arca_core::dos_to_unix(date_val, time_val),
        created: times.created,
        accessed: times.accessed,
        // The low byte of the external attributes is the DOS byte, whatever
        // system made the archive claims to be.
        attributes: (ext_attr & 0xFF) as u8,
        offset,
        encrypted,
    })
}

struct Record {
    name_str: Vec<u8>,
    crc_val: u32,
    comp_size: u64,
    uncompressed: u64,
    offset: u64,
    method_code: u16,
    date_val: u16,
    time_val: u16,
    is_directory: bool,
    encrypted: bool,
    real_method: u16,
}

pub struct ZipWriter<W: Write + Seek> {
    out: W,
    registros: Vec<Record>,
    pos: u64,
}

impl<W: Write + Seek> ZipWriter<W> {
    pub fn new(out: W) -> Self {
        ZipWriter {
            out,
            registros: Vec::new(),
            pos: 0,
        }
    }

    pub fn add<R: Read>(
        &mut self,
        name_str: &str,
        data: R,
        codec: Codec,
        level: Level,
        mtime: Option<i64>,
    ) -> Result<()> {
        self.add_with_password(name_str, data, codec, level, mtime, None)
    }

    // With a password the entry is compressed first and encrypted after, which
    // is the order WinZip AES specifies: encrypting first would leave the
    // compressor nothing to find. The method field then says 99 and the real
    // compressor moves into the 0x9901 extra field.
    pub fn add_with_password<R: Read>(
        &mut self,
        name_str: &str,
        mut data: R,
        codec: Codec,
        level: Level,
        mtime: Option<i64>,
        password: Option<&str>,
    ) -> Result<()> {
        let (date_val, time_val) = arca_core::unix_to_dos(mtime.unwrap_or(0));
        let name_bytes = check_name(name_str)?;
        let method_code = method_of(codec);
        let offset = self.pos;
        let aes_extra = password.map(|_| aes::extra_field(method_code.code()));
        let stored_code = if aes_extra.is_some() {
            aes::METHOD_AE
        } else {
            method_code.code()
        };
        self.write_lfh(
            &name_bytes,
            stored_code,
            date_val,
            time_val,
            aes_extra.as_deref(),
        )?;
        let extra_offset = offset + LFH_FIXED as u64 + name_bytes.len() as u64;
        let extra_len = EXTRA_Z64 + aes_extra.as_ref().map_or(0, |x| x.len());

        let mut hasher = crc32fast::Hasher::new();

        let (comp_size, uncompressed, crc_val) = match password {
            None => {
                let (_, comp, un) =
                    compress_stream(&mut data, &mut self.out, method_code, level, &mut hasher)?;
                (comp, un, hasher.finalize())
            }
            Some(pw) => {
                let salt = aes::random_salt()?;
                let keys = aes::derive(pw, &salt);
                self.out.write_all(&salt)?;
                self.out.write_all(&aes::verifier_of(&keys))?;
                let sink = aes::AesWriter::new(&mut self.out, &keys)?;
                let (sink, cipher_len, un) =
                    compress_stream(&mut data, sink, method_code, level, &mut hasher)?;
                let (_, auth, _) = sink.finish();
                self.out.write_all(&auth)?;
                // AE-2 leaves the CRC field at zero on purpose: it would leak a
                // checksum of the plaintext, and the HMAC already does the job.
                (aes::OVERHEAD as u64 + cipher_len, un, 0)
            }
        };

        self.close_entry(
            name_bytes,
            offset,
            extra_offset,
            extra_len,
            crc_val,
            comp_size,
            uncompressed,
            method_code,
            date_val,
            time_val,
            name_str.ends_with('/'),
            aes_extra.is_some(),
        )
    }

    pub fn add_compressed(
        &mut self,
        name_str: &str,
        compressed: &[u8],
        crc_val: u32,
        uncompressed: u64,
        method_code: Method,
        mtime: Option<i64>,
    ) -> Result<()> {
        self.put_block(
            name_str,
            compressed,
            crc_val,
            uncompressed,
            method_code,
            mtime,
            false,
        )
    }

    // Takes a block already through `seal_block`, so the key derivation, which
    // is a thousand rounds of PBKDF2 per entry, happens wherever the caller
    // compressed it. Doing it here would put every one of them on one thread.
    pub fn add_sealed(
        &mut self,
        name_str: &str,
        sealed: &[u8],
        uncompressed: u64,
        method_code: Method,
        mtime: Option<i64>,
    ) -> Result<()> {
        self.put_block(name_str, sealed, 0, uncompressed, method_code, mtime, true)
    }

    // Writes an entry whose compressed bytes come from somewhere else, straight
    // through, with whatever encryption is asked for now rather than whatever it
    // had before. `body` is handed the sink and streams into it; nothing is held
    // in memory, so a password can be taken off a large archive without it.
    #[allow(clippy::too_many_arguments)]
    pub fn copy_entry<F>(
        &mut self,
        name_str: &str,
        crc_val: u32,
        uncompressed: u64,
        method_code: Method,
        mtime: Option<i64>,
        password: Option<&str>,
        body: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut dyn Write) -> Result<u64>,
    {
        let (date_val, time_val) = arca_core::unix_to_dos(mtime.unwrap_or(0));
        let name_bytes = check_name(name_str)?;
        let offset = self.pos;
        let aes_extra = password.map(|_| aes::extra_field(method_code.code()));
        let stored_code = if aes_extra.is_some() {
            aes::METHOD_AE
        } else {
            method_code.code()
        };
        self.write_lfh(
            &name_bytes,
            stored_code,
            date_val,
            time_val,
            aes_extra.as_deref(),
        )?;
        let extra_offset = offset + LFH_FIXED as u64 + name_bytes.len() as u64;
        let extra_len = EXTRA_Z64 + aes_extra.as_ref().map_or(0, |x| x.len());

        let (comp_size, stored_crc) = match password {
            None => (body(&mut self.out)?, crc_val),
            Some(pw) => {
                let salt = aes::random_salt()?;
                let keys = aes::derive(pw, &salt);
                self.out.write_all(&salt)?;
                self.out.write_all(&aes::verifier_of(&keys))?;
                let mut sink = aes::AesWriter::new(&mut self.out, &keys)?;
                body(&mut sink)?;
                let (_, auth, written) = sink.finish();
                self.out.write_all(&auth)?;
                (aes::OVERHEAD as u64 + written, 0)
            }
        };

        self.close_entry(
            name_bytes,
            offset,
            extra_offset,
            extra_len,
            stored_crc,
            comp_size,
            uncompressed,
            method_code,
            date_val,
            time_val,
            name_str.ends_with('/'),
            password.is_some(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn put_block(
        &mut self,
        name_str: &str,
        body: &[u8],
        crc_val: u32,
        uncompressed: u64,
        method_code: Method,
        mtime: Option<i64>,
        encrypted: bool,
    ) -> Result<()> {
        let (date_val, time_val) = arca_core::unix_to_dos(mtime.unwrap_or(0));
        let name_bytes = check_name(name_str)?;
        let offset = self.pos;
        let aes_extra = encrypted.then(|| aes::extra_field(method_code.code()));
        let stored_code = if encrypted {
            aes::METHOD_AE
        } else {
            method_code.code()
        };
        self.write_lfh(
            &name_bytes,
            stored_code,
            date_val,
            time_val,
            aes_extra.as_deref(),
        )?;
        let extra_offset = offset + LFH_FIXED as u64 + name_bytes.len() as u64;
        let extra_len = EXTRA_Z64 + aes_extra.as_ref().map_or(0, |x| x.len());
        self.out.write_all(body)?;
        self.close_entry(
            name_bytes,
            offset,
            extra_offset,
            extra_len,
            crc_val,
            body.len() as u64,
            uncompressed,
            method_code,
            date_val,
            time_val,
            name_str.ends_with('/'),
            encrypted,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn close_entry(
        &mut self,
        name_bytes: Vec<u8>,
        offset: u64,
        extra_offset: u64,
        extra_len: usize,
        crc_val: u32,
        comp_size: u64,
        uncompressed: u64,
        method_code: Method,
        date_val: u16,
        time_val: u16,
        is_directory: bool,
        encrypted: bool,
    ) -> Result<()> {
        let fin = extra_offset + extra_len as u64 + comp_size;
        let sat_u = uncompressed >= 0xFFFF_FFFF;
        let sat_c = comp_size >= 0xFFFF_FFFF;
        self.out.seek(SeekFrom::Start(offset + 14))?;
        self.out.write_all(&crc_val.to_le_bytes())?;
        self.out.write_all(
            &(if sat_c {
                0xFFFF_FFFFu32
            } else {
                comp_size as u32
            })
            .to_le_bytes(),
        )?;
        self.out.write_all(
            &(if sat_u {
                0xFFFF_FFFFu32
            } else {
                uncompressed as u32
            })
            .to_le_bytes(),
        )?;
        self.out.seek(SeekFrom::Start(extra_offset + 4))?;
        self.out.write_all(&uncompressed.to_le_bytes())?;
        self.out.write_all(&comp_size.to_le_bytes())?;
        self.out.seek(SeekFrom::Start(fin))?;
        self.pos = fin;
        self.registros.push(Record {
            name_str: name_bytes,
            crc_val,
            comp_size,
            uncompressed,
            offset,
            method_code: if encrypted {
                aes::METHOD_AE
            } else {
                method_code.code()
            },
            date_val,
            time_val,
            is_directory,
            encrypted,
            real_method: method_code.code(),
        });
        Ok(())
    }

    // The Zip64 field goes first and keeps its fixed size, because close_entry
    // seeks straight back to it to patch the sizes once they are known. The AES
    // field, when there is one, goes after it so that offset stays put.
    fn write_lfh(
        &mut self,
        name_str: &[u8],
        method_code: u16,
        date_val: u16,
        time_val: u16,
        aes_extra: Option<&[u8]>,
    ) -> Result<()> {
        let extra_len = EXTRA_Z64 + aes_extra.map_or(0, |x| x.len());
        let flags: u16 = if aes_extra.is_some() { 0x0801 } else { 0x0800 };
        let mut h = Vec::with_capacity(LFH_FIXED + name_str.len() + extra_len);
        h.extend_from_slice(&SIG_LFH.to_le_bytes());
        h.extend_from_slice(&45u16.to_le_bytes());
        h.extend_from_slice(&flags.to_le_bytes());
        h.extend_from_slice(&method_code.to_le_bytes());
        h.extend_from_slice(&time_val.to_le_bytes());
        h.extend_from_slice(&date_val.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&(name_str.len() as u16).to_le_bytes());
        h.extend_from_slice(&(extra_len as u16).to_le_bytes());
        h.extend_from_slice(name_str);
        h.extend_from_slice(&0x0001u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        if let Some(x) = aes_extra {
            h.extend_from_slice(x);
        }
        self.out.write_all(&h)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        let cd_offset = self.pos;
        let mut cd_size = 0u64;

        for r in &self.registros {
            let sat_u = r.uncompressed >= 0xFFFF_FFFF;
            let sat_c = r.comp_size >= 0xFFFF_FFFF;
            let sat_o = r.offset >= 0xFFFF_FFFF;
            let extra: Vec<u8> = if sat_u || sat_c || sat_o {
                let cuerpo = 8 * (sat_u as usize + sat_c as usize + sat_o as usize);
                let mut e = Vec::with_capacity(4 + cuerpo);
                e.extend_from_slice(&0x0001u16.to_le_bytes());
                e.extend_from_slice(&(cuerpo as u16).to_le_bytes());
                if sat_u {
                    e.extend_from_slice(&r.uncompressed.to_le_bytes());
                }
                if sat_c {
                    e.extend_from_slice(&r.comp_size.to_le_bytes());
                }
                if sat_o {
                    e.extend_from_slice(&r.offset.to_le_bytes());
                }
                e
            } else {
                Vec::new()
            };
            // The reader looks the AES field up in the central directory, so it
            // has to be here too and not only in the local header.
            let mut extra = extra;
            if r.encrypted {
                extra.extend_from_slice(&aes::extra_field(r.real_method));
            }
            let flags: u16 = if r.encrypted { 0x0801 } else { 0x0800 };

            let mut h = Vec::with_capacity(CD_FIXED + r.name_str.len() + extra.len());
            h.extend_from_slice(&SIG_CD.to_le_bytes());
            h.extend_from_slice(&(0x031Eu16).to_le_bytes());
            h.extend_from_slice(&45u16.to_le_bytes());
            h.extend_from_slice(&flags.to_le_bytes());
            h.extend_from_slice(&r.method_code.to_le_bytes());
            h.extend_from_slice(&r.time_val.to_le_bytes());
            h.extend_from_slice(&r.date_val.to_le_bytes());
            h.extend_from_slice(&r.crc_val.to_le_bytes());
            h.extend_from_slice(&(r.comp_size.min(0xFFFF_FFFF) as u32).to_le_bytes());
            h.extend_from_slice(&(r.uncompressed.min(0xFFFF_FFFF) as u32).to_le_bytes());
            h.extend_from_slice(&(r.name_str.len() as u16).to_le_bytes());
            h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            let mode: u32 = if r.is_directory { 0o040755 } else { 0o100644 };
            h.extend_from_slice(
                &((mode << 16) | if r.is_directory { 0x10 } else { 0 }).to_le_bytes(),
            );
            h.extend_from_slice(&(r.offset.min(0xFFFF_FFFF) as u32).to_le_bytes());
            h.extend_from_slice(&r.name_str);
            h.extend_from_slice(&extra);

            self.out.write_all(&h)?;
            cd_size += h.len() as u64;
        }

        let n = self.registros.len() as u64;
        let necesita_z64 =
            n > u16::MAX as u64 || cd_offset >= 0xFFFF_FFFF || cd_size >= 0xFFFF_FFFF;

        if necesita_z64 {
            let z_off = cd_offset + cd_size;
            let mut z = Vec::with_capacity(76);
            z.extend_from_slice(&SIG_EOCD64.to_le_bytes());
            z.extend_from_slice(&44u64.to_le_bytes());
            z.extend_from_slice(&(0x031Eu16).to_le_bytes());
            z.extend_from_slice(&45u16.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&n.to_le_bytes());
            z.extend_from_slice(&n.to_le_bytes());
            z.extend_from_slice(&cd_size.to_le_bytes());
            z.extend_from_slice(&cd_offset.to_le_bytes());
            z.extend_from_slice(&SIG_LOC64.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&z_off.to_le_bytes());
            z.extend_from_slice(&1u32.to_le_bytes());
            self.out.write_all(&z)?;
        }

        let mut e = Vec::with_capacity(EOCD_MIN);
        e.extend_from_slice(&SIG_EOCD.to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        e.extend_from_slice(&(n.min(0xFFFF) as u16).to_le_bytes());
        e.extend_from_slice(&(n.min(0xFFFF) as u16).to_le_bytes());
        e.extend_from_slice(&(cd_size.min(0xFFFF_FFFF) as u32).to_le_bytes());
        e.extend_from_slice(&(cd_offset.min(0xFFFF_FFFF) as u32).to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        self.out.write_all(&e)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

struct Counter<W: Write> {
    inner: W,
    written: u64,
}

impl<W: Write> Counter<W> {
    fn new(inner: W) -> Self {
        Counter { inner, written: 0 }
    }
}

impl<W: Write> Write for Counter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// Pulls everything out of `data`, compresses it into `sink` and returns the
// sink back along with how many bytes came out and how many went in. It is
// generic over the sink so the same three arms serve a plain entry, where the
// sink is the file, and an encrypted one, where it is the AES writer.
fn compress_stream<R: Read, S: Write>(
    data: &mut R,
    sink: S,
    method_code: Method,
    level: Level,
    hasher: &mut crc32fast::Hasher,
) -> Result<(S, u64, u64)> {
    let mut counter = Counter::new(sink);
    let mut uncompressed = 0u64;
    let mut buf = vec![0u8; STREAM_BUF];

    match method_code {
        Method::Store => loop {
            let n = data.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            counter.write_all(&buf[..n])?;
            uncompressed += n as u64;
        },
        Method::Deflate => {
            let mut enc = DeflateEncoder::new(counter, Compression::new(level.to_flate2()));
            loop {
                let n = data.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                enc.write_all(&buf[..n])?;
                uncompressed += n as u64;
            }
            counter = enc.finish()?;
        }
        Method::Zstd => {
            #[cfg(feature = "codecs-native")]
            {
                let mut enc = zstd::stream::write::Encoder::new(counter, level.to_zstd())
                    .map_err(Error::Io)?;
                let _ = enc.multithread(available_threads());
                loop {
                    let n = data.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                    enc.write_all(&buf[..n])?;
                    uncompressed += n as u64;
                }
                counter = enc.finish().map_err(Error::Io)?;
            }
            #[cfg(not(feature = "codecs-native"))]
            {
                let _ = counter;
                return Err(Error::Unsupported(
                    "this binary was built without Zstandard".into(),
                ));
            }
        }
    }

    let comp_size = counter.written;
    Ok((counter.inner, comp_size, uncompressed))
}

fn check_name(name_str: &str) -> Result<Vec<u8>> {
    let b = name_str.as_bytes().to_vec();
    if b.len() > u16::MAX as usize {
        return Err(Error::Limit("name too long for ZIP".into()));
    }
    Ok(b)
}

fn method_of(c: Codec) -> Method {
    match c {
        Codec::Store => Method::Store,
        Codec::Deflate => Method::Deflate,
        Codec::Zstd => Method::Zstd,
    }
}

#[cfg(feature = "codecs-native")]
fn available_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

// Writes a copy of `archive` at `out` carrying a different password, or none.
//
// Nothing is compressed again. WinZip AES encrypts the already compressed
// bytes, so the deflate stream comes back out of the decryption exactly as it
// went in and is written straight through. What that costs instead is the
// checksum: an AE-2 entry stores a zero CRC, so the entry has to be decompressed
// once to work out the real one before it can be written as a plain entry.
//
// The copy is read back in full before this returns. Replacing an archive in
// place destroys the only copy of the data, and the caller can only make that
// safe if it knows the new file is sound first.
pub fn rewrite_password(
    archive: &std::path::Path,
    out: &std::path::Path,
    current: Option<&str>,
    new: Option<&str>,
    // Told how far along this is, and answers whether to carry on: false is
    // somebody pressing stop, and the rewrite gives up where it stands rather
    // than finishing a copy nobody is waiting for.
    notify: &dyn Fn(usize, usize, &str) -> bool,
) -> Result<u64> {
    rewrite(
        archive,
        out,
        current,
        new,
        &|_| true,
        &keep_name,
        &[],
        notify,
    )
}

/// A file on disk waiting to go into an archive: where to read it from, the
/// name it takes inside, and how it should be compressed. The codec travels
/// with the file rather than sitting in the call so a paste can use whatever
/// the window is set to without the rewrite having to know about it.
pub struct Addition {
    // The file to read the bytes from, or nothing at all: a folder in a zip is
    // an entry with no contents and a slash on the end of its name, and there
    // is no file on disk behind one that somebody has just asked for.
    pub source: Option<std::path::PathBuf>,
    pub name: String,
    pub codec: Codec,
    pub level: Level,
}

/// Writes a copy of `archive` at `out` with `extra` added to it.
///
/// Built again rather than appended to for the same reason as
/// [`remove_entries`]: the central directory sits at the end and records where
/// every entry starts, so growing an archive in place still means writing that
/// directory out afresh. What is already in there is copied through without
/// going near a compressor; only the new files are compressed. A new file whose
/// name is already taken replaces the entry that had it, which is what dropping
/// something into a folder that already holds it means everywhere else.
pub fn add_entries(
    archive: &std::path::Path,
    out: &std::path::Path,
    password: Option<&str>,
    extra: &[Addition],
    // Told how far along this is, and answers whether to carry on: false is
    // somebody pressing stop, and the rewrite gives up where it stands rather
    // than finishing a copy nobody is waiting for.
    notify: &dyn Fn(usize, usize, &str) -> bool,
) -> Result<u64> {
    let taken: std::collections::HashSet<&str> = extra.iter().map(|a| a.name.as_str()).collect();
    rewrite(
        archive,
        out,
        password,
        password,
        &|e| !taken.contains(e.name.as_str()),
        &keep_name,
        extra,
        notify,
    )
}

/// Writes a copy of `archive` at `out` without the entries `keep` turns down.
///
/// A zip cannot have entries taken out of it in place: the central directory
/// records where every one of them starts, so removing bytes from the middle
/// moves everything after it. The archive is built again instead, and since
/// nothing is compressed a second time this costs a copy of the bytes that
/// survive and nothing else.
pub fn remove_entries(
    archive: &std::path::Path,
    out: &std::path::Path,
    password: Option<&str>,
    keep: &dyn Fn(&Entry) -> bool,
    // Told how far along this is, and answers whether to carry on: false is
    // somebody pressing stop, and the rewrite gives up where it stands rather
    // than finishing a copy nobody is waiting for.
    notify: &dyn Fn(usize, usize, &str) -> bool,
) -> Result<u64> {
    rewrite(
        archive,
        out,
        password,
        password,
        keep,
        &keep_name,
        &[],
        notify,
    )
}

/// The name an entry keeps when nothing is being renamed.
fn keep_name(e: &Entry) -> String {
    e.name.clone()
}

/// Writes a copy of `archive` at `out` with every entry under whatever name
/// `name` gives it.
///
/// The same rebuild as [`remove_entries`], and for the same reason: an entry's
/// name is written twice, once beside the data and once in the central
/// directory at the end, and the second copy records how far into the file the
/// first one sits. Change the length of a name in place and every offset after
/// it is a lie. Nothing is compressed again; the bytes are copied through.
///
/// Renaming a folder is renaming everything under it, which is the caller's
/// business: `name` is asked about every entry, so a caller that wants a whole
/// branch moved answers for the branch.
pub fn rename_entries(
    archive: &std::path::Path,
    out: &std::path::Path,
    password: Option<&str>,
    name: &dyn Fn(&Entry) -> String,
    // Told how far along this is, and answers whether to carry on: false is
    // somebody pressing stop, and the rewrite gives up where it stands rather
    // than finishing a copy nobody is waiting for.
    notify: &dyn Fn(usize, usize, &str) -> bool,
) -> Result<u64> {
    rewrite(
        archive,
        out,
        password,
        password,
        &|_| true,
        name,
        &[],
        notify,
    )
}

#[allow(clippy::too_many_arguments)]
fn rewrite(
    archive: &std::path::Path,
    out: &std::path::Path,
    current: Option<&str>,
    new: Option<&str>,
    keep: &dyn Fn(&Entry) -> bool,
    name: &dyn Fn(&Entry) -> String,
    extra: &[Addition],
    // Told how far along this is, and answers whether to carry on: false is
    // somebody pressing stop, and the rewrite gives up where it stands rather
    // than finishing a copy nobody is waiting for.
    notify: &dyn Fn(usize, usize, &str) -> bool,
) -> Result<u64> {
    use std::fs::File;
    use std::io::{BufReader, BufWriter};

    let entries: Vec<Entry> = ZipArchive::open(File::open(archive)?)?
        .entries()
        .iter()
        .filter(|e| keep(e))
        .cloned()
        .collect();
    let total = entries.len() + extra.len();
    let mut w = ZipWriter::new(BufWriter::with_capacity(STREAM_BUF, File::create(out)?));
    let mut bytes = 0u64;

    for (i, e) in entries.iter().enumerate() {
        if !notify(i, total, &e.name) {
            return Err(Error::Cancelled);
        }
        // Directories hold nothing, and no other tool encrypts them either.
        let pw = if e.is_dir { None } else { new };
        let crc = if e.encrypted && e.crc32 == 0 {
            let mut src = BufReader::with_capacity(STREAM_BUF, File::open(archive)?);
            checksum_entry(&mut src, e, current)?
        } else {
            e.crc32
        };
        let mut src = BufReader::with_capacity(STREAM_BUF, File::open(archive)?);
        w.copy_entry(&name(e), crc, e.size, e.method, e.mtime, pw, |sink| {
            copy_compressed(&mut src, e, sink, current)
        })?;
        bytes += e.size;
    }

    // The new ones last, so copying what was already there stays one straight
    // pass over the source file instead of one interleaved with compression.
    for (i, a) in extra.iter().enumerate() {
        if !notify(entries.len() + i, total, &a.name) {
            return Err(Error::Cancelled);
        }
        let Some(from) = &a.source else {
            // A folder: nothing to read, nothing to compress, and the slash on
            // the end of the name is what makes it one.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64);
            w.add_with_password(&a.name, io::empty(), Codec::Store, Level::Store, now, None)?;
            continue;
        };
        let meta = std::fs::metadata(from)?;
        let mtime = meta.modified().ok().and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        });
        let f = BufReader::with_capacity(STREAM_BUF, File::open(from)?);
        w.add_with_password(&a.name, f, a.codec, a.level, mtime, new)?;
        bytes += meta.len();
    }
    w.finish()?.flush()?;

    let mut check = ZipArchive::open(File::open(out)?)?;
    for i in 0..check.len() {
        if check.entries()[i].is_dir {
            continue;
        }
        check.extract_to_with(i, io::sink(), new)?;
    }
    let _ = notify(total, total, "");
    Ok(bytes)
}

// Wraps an already compressed block the way an encrypted entry sits on disk:
// salt, password verifier, ciphertext and authentication code. Kept apart from
// the writer so it can run on whatever thread compressed the block.
pub fn seal_block(compressed: &[u8], password: &str) -> Result<Vec<u8>> {
    let mut body = compressed.to_vec();
    let (salt, verifier, auth) = aes::encrypt(password, &mut body)?;
    let mut out = Vec::with_capacity(body.len() + aes::OVERHEAD);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&verifier);
    out.extend_from_slice(&body);
    out.extend_from_slice(&auth);
    Ok(out)
}

pub fn compress_block(data: &[u8], codec: Codec, level: Level) -> Result<(Vec<u8>, Method, u32)> {
    let crc_val = arca_core::crc32(data);
    match codec {
        Codec::Store => Ok((data.to_vec(), Method::Store, crc_val)),
        Codec::Deflate => {
            let mut e = DeflateEncoder::new(Vec::new(), Compression::new(level.to_flate2()));
            e.write_all(data)?;
            Ok((e.finish()?, Method::Deflate, crc_val))
        }
        Codec::Zstd => {
            #[cfg(feature = "codecs-native")]
            {
                let c = zstd::bulk::compress(data, level.to_zstd()).map_err(Error::Io)?;
                Ok((c, Method::Zstd, crc_val))
            }
            #[cfg(not(feature = "codecs-native"))]
            {
                Err(Error::Unsupported(
                    "this binary was built without Zstandard".into(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;

    fn round_trip(codec: Codec, level: Level) {
        let content = b"Arca. ".repeat(5000);
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        w.add(
            "dir/file.txt",
            &content[..],
            codec,
            level,
            Some(1_700_000_000),
        )
        .unwrap();
        let buf = w.finish().unwrap().into_inner();

        let mut a = ZipArchive::open(IoCursor::new(buf)).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a.entries()[0].name, "dir/file.txt");
        assert_eq!(a.entries()[0].size, content.len() as u64);
        let mut out = Vec::new();
        a.extract_to(0, &mut out).unwrap();
        assert_eq!(out, content);
    }

    #[test]
    fn round_trip_deflate() {
        round_trip(Codec::Deflate, Level::Normal);
    }

    #[cfg(feature = "codecs-native")]
    #[test]
    fn round_trip_zstd() {
        round_trip(Codec::Zstd, Level::Normal);
    }

    #[test]
    fn round_trip_store() {
        round_trip(Codec::Store, Level::Store);
    }

    #[test]
    fn corrupt_crc_is_detected() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        w.add(
            "a.txt",
            &b"content original"[..],
            Codec::Store,
            Level::Store,
            None,
        )
        .unwrap();
        let mut buf = w.finish().unwrap().into_inner();
        let p = LFH_FIXED + "a.txt".len() + EXTRA_Z64 + 3;
        buf[p] ^= 0xFF;
        let mut a = ZipArchive::open(IoCursor::new(buf)).unwrap();
        let r = a.extract_to(0, &mut Vec::new());
        assert!(matches!(r, Err(Error::Integrity { .. })), "{r:?}");
    }

    // extract_entry takes the entry and the reader separately so several
    // threads can pull from the same archive at once. That also means it can be
    // handed an entry that does not describe what is at that offset.
    #[test]
    fn extract_entry_with_an_entry_that_lies_does_not_panic() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        w.add("a.txt", &b"content"[..], Codec::Store, Level::Store, None)
            .unwrap();
        let buf = w.finish().unwrap().into_inner();
        let real = ZipArchive::open(IoCursor::new(buf.clone()))
            .unwrap()
            .entries()[0]
            .clone();

        for seed in 0u64..500 {
            let mut e = real.clone();
            e.offset = seed.wrapping_mul(2_654_435_761) % (buf.len() as u64 + 64);
            e.compressed_size = seed.wrapping_mul(97) % 4096;
            e.size = seed.wrapping_mul(31) % 4096;
            e.crc32 = seed as u32;
            let mut source = IoCursor::new(buf.clone());
            let r = extract_entry(&mut source, &e, &mut Vec::new());
            assert!(r.is_err() || e.offset == real.offset, "{r:?}");
        }
    }

    #[test]
    fn extract_entry_pulls_the_same_bytes_as_the_archive() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for i in 0..8 {
            let body = format!("entry number {i} ").repeat(40);
            w.add(
                &format!("f{i}.txt"),
                body.as_bytes(),
                Codec::Deflate,
                Level::Normal,
                None,
            )
            .unwrap();
        }
        let buf = w.finish().unwrap().into_inner();
        let mut a = ZipArchive::open(IoCursor::new(buf.clone())).unwrap();
        let entries = a.entries().to_vec();
        for (i, e) in entries.iter().enumerate() {
            let mut through_archive = Vec::new();
            a.extract_to(i, &mut through_archive).unwrap();
            let mut alone = Vec::new();
            let mut source = IoCursor::new(buf.clone());
            extract_entry(&mut source, e, &mut alone).unwrap();
            assert_eq!(through_archive, alone, "entry {i}");
        }
    }

    fn encrypted_archive(password: &str, codec: Codec, body: &[u8]) -> Vec<u8> {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        w.add_with_password(
            "secret.txt",
            body,
            codec,
            Level::Normal,
            None,
            Some(password),
        )
        .unwrap();
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn encrypted_round_trip_for_every_codec() {
        let body = b"the quick brown fox ".repeat(500);
        let mut codecs = vec![Codec::Store, Codec::Deflate];
        if cfg!(feature = "codecs-native") {
            codecs.push(Codec::Zstd);
        }
        for codec in codecs {
            let buf = encrypted_archive("hunter2", codec, &body);
            let mut a = ZipArchive::open(IoCursor::new(buf)).unwrap();
            assert!(a.has_encrypted());
            assert_eq!(a.entries()[0].size, body.len() as u64);
            let mut out = Vec::new();
            a.extract_to_with(0, &mut out, Some("hunter2")).unwrap();
            assert_eq!(out, body, "{codec:?}");
        }
    }

    // The password goes into PBKDF2 as UTF-8, which is what every other tool
    // does. interop.sh cannot check this because Git Bash mangles a non-ASCII
    // argument before it reaches a native .exe.
    #[test]
    fn a_password_that_is_not_ascii_round_trips() {
        let body = b"contenido".repeat(20);
        for pw in [
            "contraseña",
            "пароль",
            "密码",
            "clave con espacios y ñ",
            "🔑",
        ] {
            let buf = encrypted_archive(pw, Codec::Deflate, &body);
            let mut a = ZipArchive::open(IoCursor::new(buf)).unwrap();
            let mut out = Vec::new();
            a.extract_to_with(0, &mut out, Some(pw)).unwrap();
            assert_eq!(out, body, "{pw}");
        }
    }

    // The same three steps the CLI's `password` command takes, in memory: work
    // out the checksum, stream the compressed bytes through, write them back
    // under whatever encryption is wanted now.
    fn rewrite(buf: &[u8], current: Option<&str>, new: Option<&str>) -> Vec<u8> {
        let entries = ZipArchive::open(IoCursor::new(buf.to_vec()))
            .unwrap()
            .entries()
            .to_vec();
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for e in &entries {
            let crc = if e.encrypted && e.crc32 == 0 {
                let mut src = IoCursor::new(buf.to_vec());
                checksum_entry(&mut src, e, current).unwrap()
            } else {
                e.crc32
            };
            let mut src = IoCursor::new(buf.to_vec());
            w.copy_entry(&e.name, crc, e.size, e.method, e.mtime, new, |sink| {
                copy_compressed(&mut src, e, sink, current)
            })
            .unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn only_entry(buf: &[u8], password: Option<&str>) -> (Vec<u8>, bool, u64) {
        let mut a = ZipArchive::open(IoCursor::new(buf.to_vec())).unwrap();
        let encrypted = a.entries()[0].encrypted;
        let packed = a.entries()[0].compressed_size;
        let mut out = Vec::new();
        a.extract_to_with(0, &mut out, password).unwrap();
        (out, encrypted, packed)
    }

    // What remove_entries does, without going through a file: filter, then let
    // the survivors through untouched.
    fn keeping(buf: &[u8], keep: &dyn Fn(&Entry) -> bool) -> Vec<u8> {
        let entries: Vec<Entry> = ZipArchive::open(IoCursor::new(buf.to_vec()))
            .unwrap()
            .entries()
            .iter()
            .filter(|e| keep(e))
            .cloned()
            .collect();
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for e in &entries {
            let mut src = IoCursor::new(buf.to_vec());
            w.copy_entry(&e.name, e.crc32, e.size, e.method, e.mtime, None, |sink| {
                copy_compressed(&mut src, e, sink, None)
            })
            .unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    // Taking entries out is the one operation with no undo, so whatever
    // survives has to survive byte for byte.
    #[test]
    fn removing_entries_leaves_the_rest_untouched() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        let bodies: Vec<Vec<u8>> = (0..6)
            .map(|i| format!("contents of number {i} ").repeat(30).into_bytes())
            .collect();
        for (i, body) in bodies.iter().enumerate() {
            w.add(
                &format!("f{i}.txt"),
                &body[..],
                Codec::Deflate,
                Level::Normal,
                None,
            )
            .unwrap();
        }
        let buf = w.finish().unwrap().into_inner();

        let doomed = ["f1.txt", "f4.txt"];
        let kept = keeping(&buf, &|e| !doomed.contains(&e.name.as_str()));

        let mut a = ZipArchive::open(IoCursor::new(kept)).unwrap();
        let names: Vec<String> = a.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["f0.txt", "f2.txt", "f3.txt", "f5.txt"]);
        for (i, name) in names.iter().enumerate() {
            let mut out = Vec::new();
            a.extract_to(i, &mut out).unwrap();
            let which: usize = name[1..2].parse().unwrap();
            assert_eq!(out, bodies[which], "{name}");
        }
    }

    #[test]
    fn removing_nothing_and_removing_everything_both_work() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        w.add(
            "only.txt",
            &b"payload"[..],
            Codec::Deflate,
            Level::Normal,
            None,
        )
        .unwrap();
        let buf = w.finish().unwrap().into_inner();

        let same = keeping(&buf, &|_| true);
        let mut a = ZipArchive::open(IoCursor::new(same)).unwrap();
        assert_eq!(a.len(), 1);
        let mut out = Vec::new();
        a.extract_to(0, &mut out).unwrap();
        assert_eq!(out, b"payload");

        // An archive with nothing left in it is still a readable archive.
        let empty = keeping(&buf, &|_| false);
        let a = ZipArchive::open(IoCursor::new(empty)).unwrap();
        assert!(a.is_empty());
    }

    #[test]
    fn taking_the_password_off_keeps_the_contents() {
        let body = b"something worth keeping ".repeat(400);
        let encrypted = encrypted_archive("hunter2", Codec::Deflate, &body);
        let plain = rewrite(&encrypted, Some("hunter2"), None);

        let (out, still_encrypted, packed) = only_entry(&plain, None);
        assert_eq!(out, body);
        assert!(!still_encrypted, "it should not be encrypted any more");
        // The compressed stream came through untouched: only the AES wrapper
        // went away, and that is exactly its own overhead.
        let before = ZipArchive::open(IoCursor::new(encrypted))
            .unwrap()
            .entries()[0]
            .compressed_size;
        assert_eq!(packed, before - aes::OVERHEAD as u64);
    }

    #[test]
    fn putting_a_password_on_a_plain_archive() {
        let body = b"plain to begin with ".repeat(300);
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        w.add("secret.txt", &body[..], Codec::Deflate, Level::Normal, None)
            .unwrap();
        let plain = w.finish().unwrap().into_inner();

        let encrypted = rewrite(&plain, None, Some("nueva"));
        let (out, is_encrypted, _) = only_entry(&encrypted, Some("nueva"));
        assert_eq!(out, body);
        assert!(is_encrypted);

        let mut a = ZipArchive::open(IoCursor::new(encrypted)).unwrap();
        assert!(
            a.extract_to(0, &mut Vec::new()).is_err(),
            "it must not open without the password"
        );
    }

    #[test]
    fn changing_the_password_locks_out_the_old_one() {
        let body = b"contents".repeat(100);
        let first = encrypted_archive("old", Codec::Deflate, &body);
        let second = rewrite(&first, Some("old"), Some("new"));

        let (out, _, _) = only_entry(&second, Some("new"));
        assert_eq!(out, body);
        let mut a = ZipArchive::open(IoCursor::new(second)).unwrap();
        assert!(a.extract_to_with(0, &mut Vec::new(), Some("old")).is_err());
    }

    #[test]
    fn a_wrong_password_stops_the_rewrite_before_it_writes() {
        let buf = encrypted_archive("right", Codec::Deflate, b"payload");
        let entries = ZipArchive::open(IoCursor::new(buf.clone()))
            .unwrap()
            .entries()
            .to_vec();
        let mut src = IoCursor::new(buf);
        let mut sink = Vec::new();
        let r = copy_compressed(&mut src, &entries[0], &mut sink, Some("wrong"));
        assert!(r.is_err(), "{r:?}");
        assert!(sink.is_empty(), "nothing may come out for a wrong password");
    }

    #[test]
    fn the_bytes_on_disk_are_not_the_plaintext() {
        let body = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_vec();
        let buf = encrypted_archive("k", Codec::Store, &body);
        assert!(
            !buf.windows(body.len()).any(|w| w == &body[..]),
            "the plaintext is still sitting in the archive"
        );
    }

    #[test]
    fn a_wrong_password_is_refused_before_anything_is_written() {
        let buf = encrypted_archive("right", Codec::Deflate, b"payload");
        let mut a = ZipArchive::open(IoCursor::new(buf)).unwrap();
        let mut out = Vec::new();
        let r = a.extract_to_with(0, &mut out, Some("wrong"));
        assert!(r.is_err(), "{r:?}");
        assert!(
            out.is_empty(),
            "nothing may be written for a wrong password"
        );
    }

    #[test]
    fn an_encrypted_entry_without_a_password_says_so() {
        let buf = encrypted_archive("k", Codec::Deflate, b"payload");
        let mut a = ZipArchive::open(IoCursor::new(buf)).unwrap();
        let r = a.extract_to(0, &mut Vec::new());
        assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
    }

    // Every byte of the ciphertext, flipped one at a time, has to be caught by
    // the authentication code. The password is right in all of these, so the
    // verifier waves them through and only the HMAC stands in the way.
    #[test]
    fn a_flipped_bit_anywhere_in_the_ciphertext_is_caught() {
        let buf = encrypted_archive("k", Codec::Store, b"0123456789abcdefghij");
        let first = LFH_FIXED + "secret.txt".len() + EXTRA_Z64 + 11;
        for i in 0..20usize {
            let mut bad = buf.clone();
            bad[first + aes::SALT_256 + aes::VERIFIER + i] ^= 0x01;
            let mut a = ZipArchive::open(IoCursor::new(bad)).unwrap();
            let r = a.extract_to_with(0, &mut Vec::new(), Some("k"));
            assert!(
                matches!(r, Err(Error::Tampered { .. })),
                "byte {i} went through unnoticed: {r:?}"
            );
        }
    }

    #[test]
    fn two_entries_with_the_same_password_do_not_share_a_keystream() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for n in ["a.txt", "b.txt"] {
            w.add_with_password(
                n,
                &b"identical contents"[..],
                Codec::Store,
                Level::Store,
                None,
                Some("k"),
            )
            .unwrap();
        }
        let buf = w.finish().unwrap().into_inner();
        let a = ZipArchive::open(IoCursor::new(buf.clone())).unwrap();
        let one = &buf[a.entries()[0].offset as usize..][..80];
        let two = &buf[a.entries()[1].offset as usize..][..80];
        assert_ne!(one, two, "a repeated salt would leak that the two match");
    }

    #[test]
    fn garbage_in_an_encrypted_entry_does_not_panic() {
        let buf = encrypted_archive("k", Codec::Deflate, &b"content".repeat(100));
        for seed in 0u64..300 {
            let mut bad = buf.clone();
            let i = (seed.wrapping_mul(2_654_435_761) as usize) % bad.len();
            bad[i] ^= ((seed % 255) + 1) as u8;
            if let Ok(mut a) = ZipArchive::open(IoCursor::new(bad)) {
                let _ = a.extract_to_with(0, &mut Vec::new(), Some("k"));
                let _ = a.extract_to_with(0, &mut Vec::new(), None);
            }
        }
    }

    #[test]
    fn garbage_does_not_panic() {
        for seed in 0u32..2000 {
            let n = (seed as usize % 300) + 1;
            let data: Vec<u8> = (0..n)
                .map(|i| ((seed.wrapping_mul(2_654_435_761) >> (i % 24)) & 0xFF) as u8)
                .collect();
            let _ = ZipArchive::open(IoCursor::new(data));
        }
    }

    #[test]
    fn eocd_claiming_giant_directory() {
        let mut b = Vec::new();
        b.extend_from_slice(&SIG_EOCD.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        let r = ZipArchive::open(IoCursor::new(b));
        assert!(
            r.is_err(),
            "an impossible central directory must be rejected"
        );
    }

    #[test]
    fn many_entries() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for i in 0..500 {
            w.add(
                &format!("f{i:04}.txt"),
                &b"x"[..],
                Codec::Deflate,
                Level::Normal,
                None,
            )
            .unwrap();
        }
        let buf = w.finish().unwrap().into_inner();
        let a = ZipArchive::open(IoCursor::new(buf)).unwrap();
        assert_eq!(a.len(), 500);
        assert_eq!(a.entries()[499].name, "f0499.txt");
    }

    fn central_header(flags: u16, name_str: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&SIG_CD.to_le_bytes());
        b.extend_from_slice(&0x031Eu16.to_le_bytes());
        b.extend_from_slice(&45u16.to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&(name_str.len() as u16).to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(name_str);
        b
    }

    #[test]
    fn name_without_utf8_bit_reads_as_cp437() {
        let crudo = b"caf\x82.txt";

        let b = central_header(0, crudo);
        let e = read_central_header(&mut Cursor::new(&b)).unwrap();
        assert_eq!(e.name, "caf\u{00E9}.txt");

        let b = central_header(0x800, crudo);
        let e = read_central_header(&mut Cursor::new(&b)).unwrap();
        assert_eq!(e.name, "caf\u{FFFD}.txt");
    }

    #[test]
    fn cp437_covers_all_256_bytes() {
        let todos: Vec<u8> = (0..=255u8).collect();
        let s = from_cp437(&todos);
        assert_eq!(
            s.chars().count(),
            256,
            "every byte must map to one character"
        );
        assert!(
            !s.contains('\u{FFFD}'),
            "CP437 has no gaps: no replacement character should appear"
        );
    }

    // A renamed entry has to come out the other side byte for byte. The name is
    // written twice in a zip and the second copy says how far into the file the
    // first one is, so a rename that only changed one of them would leave an
    // archive that still opens and gives back rubbish.
    #[test]
    fn renaming_an_entry_leaves_its_bytes_alone_and_carries_a_whole_folder() {
        let room = std::env::temp_dir().join(format!("arca-rename-{}", std::process::id()));
        std::fs::create_dir_all(&room).unwrap();

        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for name in ["notes.txt", "old/one.bin", "old/deep/two.bin"] {
            let body = format!("body of {name} ").repeat(40).into_bytes();
            w.add(name, &body[..], Codec::Deflate, Level::Normal, None)
                .unwrap();
        }
        let archive = room.join("a.zip");
        std::fs::write(&archive, w.finish().unwrap().into_inner()).unwrap();

        let out = room.join("b.zip");
        rename_entries(
            &archive,
            &out,
            None,
            &|e| {
                if e.name == "notes.txt" {
                    // A name that is longer than the one it replaces, which is
                    // what moves every offset after it.
                    "a much longer name.txt".to_string()
                } else if let Some(rest) = e.name.strip_prefix("old/") {
                    format!("new/{rest}")
                } else {
                    e.name.clone()
                }
            },
            &|_, _, _| true,
        )
        .unwrap();

        let mut a = ZipArchive::open(std::fs::File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = a.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(
            names,
            ["a much longer name.txt", "new/one.bin", "new/deep/two.bin"]
        );
        for (i, was) in ["notes.txt", "old/one.bin", "old/deep/two.bin"]
            .iter()
            .enumerate()
        {
            let mut got = Vec::new();
            a.extract_to(i, &mut got).unwrap();
            assert_eq!(got, format!("body of {was} ").repeat(40).into_bytes());
        }

        let _ = std::fs::remove_dir_all(&room);
    }

    // The two extra fields that carry a creation and an access time, in the two
    // spellings that exist. Both are optional and most archives have neither,
    // so the thing worth checking is that a field that is there is read right
    // and a field that is broken is stepped over rather than believed.
    #[test]
    fn the_times_a_zip_hides_in_its_extra_fields_come_back_out() {
        // NTFS: id 0x000A, four reserved bytes, tag 1 of 24 bytes holding
        // modified, accessed and created as Windows FILETIMEs.
        let mut ntfs = Vec::new();
        ntfs.extend_from_slice(&0x000Au16.to_le_bytes());
        ntfs.extend_from_slice(&32u16.to_le_bytes());
        ntfs.extend_from_slice(&0u32.to_le_bytes());
        ntfs.extend_from_slice(&1u16.to_le_bytes());
        ntfs.extend_from_slice(&24u16.to_le_bytes());
        // 2026-09-08 00:00:00 UTC, and two round hours either side of it.
        let base = 1_788_912_000i64;
        for t in [base, base + 3600, base - 3600] {
            let ticks = (t + 11_644_473_600) as u64 * 10_000_000;
            ntfs.extend_from_slice(&ticks.to_le_bytes());
        }
        let got = read_times(&ntfs);
        assert_eq!(got.accessed, Some(base + 3600));
        assert_eq!(got.created, Some(base - 3600));

        // Extended timestamp: id 0x5455, a flag byte saying which of the three
        // follow, then that many 32-bit unix times in a fixed order.
        let mut ext = Vec::new();
        ext.extend_from_slice(&0x5455u16.to_le_bytes());
        // One flag byte and three four byte times.
        ext.extend_from_slice(&13u16.to_le_bytes());
        ext.push(0b0000_0111);
        for t in [base, base + 60, base - 60] {
            ext.extend_from_slice(&(t as i32).to_le_bytes());
        }
        let got = read_times(&ext);
        assert_eq!(got.accessed, Some(base + 60));
        assert_eq!(got.created, Some(base - 60));

        // A zero FILETIME is how a tool says it did not record one, not the
        // year 1601.
        let mut empty = ntfs.clone();
        for b in empty.iter_mut().skip(12).take(24) {
            *b = 0;
        }
        assert_eq!(read_times(&empty), Times::default());

        // Nothing at all, and rubbish, both come back with nothing rather than
        // with a wrong answer or a panic.
        assert_eq!(read_times(&[]), Times::default());
        assert_eq!(
            read_times(&[0x0A, 0x00, 0xFF, 0xFF, 1, 2, 3]),
            Times::default()
        );
    }

    // A folder is an entry with nothing in it and a slash on the end, which is
    // the only way a zip has of recording one. Worth its own test because there
    // is no file on disk behind it: everything else in the writer starts by
    // opening something.
    #[test]
    fn a_folder_goes_in_as_an_empty_entry_with_a_slash() {
        let room = std::env::temp_dir().join(format!("arca-mkdir-{}", std::process::id()));
        std::fs::create_dir_all(&room).unwrap();

        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        w.add(
            "keep.txt",
            &b"hola"[..],
            Codec::Deflate,
            Level::Normal,
            None,
        )
        .unwrap();
        let archive = room.join("a.zip");
        std::fs::write(&archive, w.finish().unwrap().into_inner()).unwrap();

        let out = room.join("b.zip");
        let extra = [Addition {
            source: None,
            name: "nueva carpeta/".into(),
            codec: Codec::Store,
            level: Level::Store,
        }];
        add_entries(&archive, &out, None, &extra, &|_, _, _| true).unwrap();

        let a = ZipArchive::open(std::fs::File::open(&out).unwrap()).unwrap();
        let made = a
            .entries()
            .iter()
            .find(|e| e.name == "nueva carpeta/")
            .expect("the folder is in the archive");
        assert!(made.is_dir, "and the archive knows it is one");
        assert_eq!(made.size, 0);

        let _ = std::fs::remove_dir_all(&room);
    }

    // Stopping halfway has to leave the archive exactly as it was. Every one of
    // these builds a new file beside the old one and swaps at the end, so what
    // this really checks is that the giving up happens before the swap and that
    // it says so instead of returning a half written archive as a success.
    #[test]
    fn giving_up_halfway_leaves_the_original_where_it_was() {
        let room = std::env::temp_dir().join(format!("arca-stop-{}", std::process::id()));
        std::fs::create_dir_all(&room).unwrap();

        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for i in 0..6 {
            let body = format!("entry {i} ").repeat(40).into_bytes();
            w.add(
                &format!("f{i}.txt"),
                &body[..],
                Codec::Deflate,
                Level::Normal,
                None,
            )
            .unwrap();
        }
        let archive = room.join("a.zip");
        let before = w.finish().unwrap().into_inner();
        std::fs::write(&archive, &before).unwrap();

        let out = room.join("b.zip");
        let stopped = rename_entries(
            &archive,
            &out,
            None,
            &|e| format!("new-{}", e.name),
            // Three through, then stop.
            &|i, _, _| i < 3,
        );
        assert!(
            matches!(stopped, Err(Error::Cancelled)),
            "stopping is its own answer, not a success and not a failure"
        );
        assert_eq!(
            std::fs::read(&archive).unwrap(),
            before,
            "the archive being rewritten is never the one being written to"
        );

        let _ = std::fs::remove_dir_all(&room);
    }

    // Pasting into an archive rewrites it, so what was already inside has to
    // come back out unharmed, and a name that was already taken has to end up
    // pointing at the new bytes rather than appearing twice.
    #[test]
    fn adding_entries_keeps_the_old_ones_and_replaces_by_name() {
        let room = std::env::temp_dir().join(format!("arca-add-{}", std::process::id()));
        std::fs::create_dir_all(&room).unwrap();

        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for name in ["keep.txt", "replace.txt"] {
            let body = format!("original {name} ").repeat(40).into_bytes();
            w.add(name, &body[..], Codec::Deflate, Level::Normal, None)
                .unwrap();
        }
        let archive = room.join("a.zip");
        std::fs::write(&archive, w.finish().unwrap().into_inner()).unwrap();

        let fresh = room.join("fresh.bin");
        std::fs::write(&fresh, b"brand new bytes".repeat(50)).unwrap();
        let extra = [
            Addition {
                source: Some(fresh.clone()),
                name: "replace.txt".into(),
                codec: Codec::Deflate,
                level: Level::Normal,
            },
            Addition {
                source: Some(fresh.clone()),
                name: "sub/added.bin".into(),
                codec: Codec::Store,
                level: Level::Store,
            },
        ];
        let out = room.join("b.zip");
        add_entries(&archive, &out, None, &extra, &|_, _, _| true).unwrap();

        let mut a = ZipArchive::open(std::fs::File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = a.entries().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["keep.txt", "replace.txt", "sub/added.bin"]);

        let mut got = Vec::new();
        a.extract_to(0, &mut got).unwrap();
        assert_eq!(got, "original keep.txt ".repeat(40).into_bytes());
        for i in [1, 2] {
            let mut got = Vec::new();
            a.extract_to(i, &mut got).unwrap();
            assert_eq!(got, b"brand new bytes".repeat(50), "entry {i}");
        }

        std::fs::remove_dir_all(&room).unwrap();
    }

    #[test]
    fn central_header_with_garbage_name_does_not_panic() {
        for seed in 0u32..200 {
            let n = (seed as usize % 40) + 1;
            let name_str: Vec<u8> = (0..n)
                .map(|i| ((seed.wrapping_mul(2_654_435_761) >> (i % 24)) & 0xFF) as u8)
                .collect();
            for flags in [0u16, 0x800] {
                let b = central_header(flags, &name_str);
                let _ = read_central_header(&mut Cursor::new(&b));
                for cut_at in 0..b.len() {
                    let _ = read_central_header(&mut Cursor::new(&b[..cut_at]));
                }
            }
        }
    }
}
