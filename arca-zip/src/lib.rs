#![forbid(unsafe_code)]

pub mod aes;

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
        CrcWriter { inner, hasher: crc32fast::Hasher::new(), written: 0 }
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
                    return Err(Error::Format("the Zip64 locator points past the end of the archive".into()));
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

        Ok(ZipArchive { source, entries_list })
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
        let e = self
            .entries_list
            .get(idx)
            .ok_or_else(|| Error::Format(format!("no such entry: {idx}")))?
            .clone();

        self.source.seek(SeekFrom::Start(e.offset))?;
        let mut lfh = [0u8; LFH_FIXED];
        self.source.read_exact(&mut lfh)?;
        let mut c = Cursor::new(&lfh);
        if c.u32le("local header signature")? != SIG_LFH {
            return Err(Error::Format(format!(
                "'{}': no local header at offset {}",
                e.name, e.offset
            )));
        }
        c.skip(2, "version")?;
        let flags = c.u16le("flags")?;
        if flags & 1 != 0 {
            return Err(Error::Unsupported(format!("'{}' is encrypted", e.name)));
        }
        c.skip(18, "rest of the local header")?;
        let n_len = c.u16le("name length")? as u64;
        let x_len = c.u16le("extra field length")? as u64;

        let inicio_datos = e.offset + LFH_FIXED as u64 + n_len + x_len;
        self.source.seek(SeekFrom::Start(inicio_datos))?;
        let acotado = (&mut self.source).take(e.compressed_size);

        let mut cw = CrcWriter::new(dest);
        match e.method {
            Method::Store => {
                let mut a = acotado;
                io::copy(&mut a, &mut cw)?;
            }
            Method::Deflate => {
                let mut dec = flate2::read::DeflateDecoder::new(acotado);
                io::copy(&mut dec, &mut cw)?;
            }
            Method::Zstd => {
                #[cfg(feature = "codecs-native")]
                {
                    let mut dec = zstd::stream::read::Decoder::new(acotado)
                        .map_err(Error::Io)?;
                    io::copy(&mut dec, &mut cw)?;
                }
                #[cfg(not(feature = "codecs-native"))]
                {
                    let _ = acotado;
                    return Err(Error::Unsupported(
                        "'{}' uses Zstandard and this binary was built without it".into(),
                    ));
                }
            }
        }
        let (_, crc_val, written) = cw.finalize();

        if crc_val != e.crc32 {
            return Err(Error::Integrity { name: e.name.clone(), expected: e.crc32, found: crc_val });
        }
        if written != e.size {
            return Err(Error::Format(format!(
                "'{}': expected {} bytes but produced {written}",
                e.name, e.size
            )));
        }
        Ok(written)
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
    let _ext_attr = c.u32le("external attributes")?;
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

    let method = match method_code {
        0 => Method::Store,
        8 => Method::Deflate,
        93 => Method::Zstd,
        other => {
            return Err(Error::Unsupported(format!(
                "compression method {other} (store, deflate and zstd are supported)"
            )))
        }
    };

    let name = if flags & 0x800 != 0 {
        String::from_utf8_lossy(name_bytes).into_owned()
    } else {
        from_cp437(name_bytes)
    };
    let is_dir = name.ends_with('/') || name.ends_with('\\');

    Ok(Entry {
        name,
        size: uncompressed,
        compressed_size: comp_size,
        method,
        crc32: crc_val,
        is_dir,
        mtime: arca_core::dos_to_unix(date_val, time_val),
        offset,
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
}

pub struct ZipWriter<W: Write + Seek> {
    out: W,
    registros: Vec<Record>,
    pos: u64,
}

impl<W: Write + Seek> ZipWriter<W> {
    pub fn new(out: W) -> Self {
        ZipWriter { out, registros: Vec::new(), pos: 0 }
    }

    pub fn add<R: Read>(
        &mut self,
        name_str: &str,
        mut data: R,
        codec: Codec,
        level: Level,
        mtime: Option<i64>,
    ) -> Result<()> {
        let (date_val, time_val) = arca_core::unix_to_dos(mtime.unwrap_or(0));
        let name_bytes = check_name(name_str)?;
        let method_code = method_of(codec);
        let offset = self.pos;
        self.write_lfh(&name_bytes, method_code.code(), date_val, time_val)?;
        let extra_offset = offset + LFH_FIXED as u64 + name_bytes.len() as u64;

        let mut hasher = crc32fast::Hasher::new();
        let mut uncompressed: u64 = 0;
        let comp_size: u64;
        let mut buf = vec![0u8; STREAM_BUF];

        match method_code {
            Method::Store => {
                loop {
                    let n = data.read(&mut buf)?;
                    if n == 0 { break; }
                    hasher.update(&buf[..n]);
                    self.out.write_all(&buf[..n])?;
                    uncompressed += n as u64;
                }
                comp_size = uncompressed;
            }
            Method::Deflate => {
                let mut enc = DeflateEncoder::new(
                    Counter::new(&mut self.out),
                    Compression::new(level.to_flate2()),
                );
                loop {
                    let n = data.read(&mut buf)?;
                    if n == 0 { break; }
                    hasher.update(&buf[..n]);
                    enc.write_all(&buf[..n])?;
                    uncompressed += n as u64;
                }
                comp_size = enc.finish()?.written;
            }
            Method::Zstd => {
                #[cfg(feature = "codecs-native")]
                {
                    let mut enc = zstd::stream::write::Encoder::new(
                        Counter::new(&mut self.out),
                        level.to_zstd(),
                    )
                    .map_err(Error::Io)?;
                    let _ = enc.multithread(available_threads());
                    loop {
                        let n = data.read(&mut buf)?;
                        if n == 0 { break; }
                        hasher.update(&buf[..n]);
                        enc.write_all(&buf[..n])?;
                        uncompressed += n as u64;
                    }
                    comp_size = enc.finish().map_err(Error::Io)?.written;
                }
                #[cfg(not(feature = "codecs-native"))]
                {
                    return Err(Error::Unsupported(
                        "this binary was built without Zstandard".into(),
                    ));
                }
            }
        }

        let crc_val = hasher.finalize();
        self.close_entry(
            name_bytes, offset, extra_offset, crc_val, comp_size, uncompressed, method_code, date_val, time_val,
            name_str.ends_with('/'),
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
        let (date_val, time_val) = arca_core::unix_to_dos(mtime.unwrap_or(0));
        let name_bytes = check_name(name_str)?;
        let offset = self.pos;
        self.write_lfh(&name_bytes, method_code.code(), date_val, time_val)?;
        let extra_offset = offset + LFH_FIXED as u64 + name_bytes.len() as u64;
        self.out.write_all(compressed)?;
        self.close_entry(
            name_bytes, offset, extra_offset, crc_val, compressed.len() as u64, uncompressed, method_code,
            date_val, time_val, name_str.ends_with('/'),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn close_entry(
        &mut self,
        name_bytes: Vec<u8>,
        offset: u64,
        extra_offset: u64,
        crc_val: u32,
        comp_size: u64,
        uncompressed: u64,
        method_code: Method,
        date_val: u16,
        time_val: u16,
        is_directory: bool,
    ) -> Result<()> {
        let fin = extra_offset + EXTRA_Z64 as u64 + comp_size;
        let sat_u = uncompressed >= 0xFFFF_FFFF;
        let sat_c = comp_size >= 0xFFFF_FFFF;
        self.out.seek(SeekFrom::Start(offset + 14))?;
        self.out.write_all(&crc_val.to_le_bytes())?;
        self.out
            .write_all(&(if sat_c { 0xFFFF_FFFFu32 } else { comp_size as u32 }).to_le_bytes())?;
        self.out
            .write_all(&(if sat_u { 0xFFFF_FFFFu32 } else { uncompressed as u32 }).to_le_bytes())?;
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
            method_code: method_code.code(),
            date_val,
            time_val,
            is_directory,
        });
        Ok(())
    }

    fn write_lfh(&mut self, name_str: &[u8], method_code: u16, date_val: u16, time_val: u16) -> Result<()> {
        let mut h = Vec::with_capacity(LFH_FIXED + name_str.len() + EXTRA_Z64);
        h.extend_from_slice(&SIG_LFH.to_le_bytes());
        h.extend_from_slice(&45u16.to_le_bytes());
        h.extend_from_slice(&(0x0800u16).to_le_bytes());
        h.extend_from_slice(&method_code.to_le_bytes());
        h.extend_from_slice(&time_val.to_le_bytes());
        h.extend_from_slice(&date_val.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&(name_str.len() as u16).to_le_bytes());
        h.extend_from_slice(&(EXTRA_Z64 as u16).to_le_bytes());
        h.extend_from_slice(name_str);
        h.extend_from_slice(&0x0001u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
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

            let mut h = Vec::with_capacity(CD_FIXED + r.name_str.len() + extra.len());
            h.extend_from_slice(&SIG_CD.to_le_bytes());
            h.extend_from_slice(&(0x031Eu16).to_le_bytes());
            h.extend_from_slice(&45u16.to_le_bytes());
            h.extend_from_slice(&(0x0800u16).to_le_bytes());
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
            h.extend_from_slice(&((mode << 16) | if r.is_directory { 0x10 } else { 0 }).to_le_bytes());
            h.extend_from_slice(&(r.offset.min(0xFFFF_FFFF) as u32).to_le_bytes());
            h.extend_from_slice(&r.name_str);
            h.extend_from_slice(&extra);

            self.out.write_all(&h)?;
            cd_size += h.len() as u64;
        }

        let n = self.registros.len() as u64;
        let necesita_z64 = n > u16::MAX as u64 || cd_offset >= 0xFFFF_FFFF || cd_size >= 0xFFFF_FFFF;

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
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1)
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
                Err(Error::Unsupported("this binary was built without Zstandard".into()))
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
        w.add("dir/file.txt", &content[..], codec, level, Some(1_700_000_000)).unwrap();
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
        w.add("a.txt", &b"content original"[..], Codec::Store, Level::Store, None).unwrap();
        let mut buf = w.finish().unwrap().into_inner();
        let p = LFH_FIXED + "a.txt".len() + EXTRA_Z64 + 3;
        buf[p] ^= 0xFF;
        let mut a = ZipArchive::open(IoCursor::new(buf)).unwrap();
        let r = a.extract_to(0, &mut Vec::new());
        assert!(matches!(r, Err(Error::Integrity { .. })), "{r:?}");
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
        assert!(r.is_err(), "an impossible central directory must be rejected");
    }

    #[test]
    fn many_entries() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for i in 0..500 {
            w.add(&format!("f{i:04}.txt"), &b"x"[..], Codec::Deflate, Level::Normal, None).unwrap();
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
        assert_eq!(s.chars().count(), 256, "every byte must map to one character");
        assert!(
            !s.contains('\u{FFFD}'),
            "CP437 has no gaps: no replacement character should appear"
        );
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
