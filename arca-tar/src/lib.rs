#![forbid(unsafe_code)]

use arca_core::{limits, Entry, Error, Method, Result};
use std::io::{self, Read, Write};

const BLOCK: usize = 512;

fn octal(field: &[u8], what: &str) -> Result<u64> {
    let s: Vec<u8> = field
        .iter()
        .copied()
        .take_while(|&b| b != 0)
        .filter(|&b| b != b' ')
        .collect();
    if s.is_empty() {
        return Ok(0);
    }
    let mut v: u64 = 0;
    for b in s {
        if !(b'0'..=b'7').contains(&b) {
            return Err(Error::Format(format!("{what}: invalid octal")));
        }
        v = v
            .checked_mul(8)
            .and_then(|x| x.checked_add((b - b'0') as u64))
            .ok_or_else(|| Error::Limit(format!("{what}: overflow")))?;
    }
    Ok(v)
}

fn write_octal(dest: &mut [u8], value: u64) {
    let width = dest.len() - 1;
    let s = format!("{value:0width$o}", width = width);
    let b = s.as_bytes();
    let n = b.len().min(width);
    dest[..n].copy_from_slice(&b[b.len() - n..]);
    dest[width] = 0;
}

fn checksum(header: &[u8; BLOCK]) -> u32 {
    let mut s: u32 = 0;
    for (i, &b) in header.iter().enumerate() {
        s += if (148..156).contains(&i) { 32 } else { b as u32 };
    }
    s
}

fn text_field(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

pub struct TarEntry {
    pub entry: Entry,
    pub data: u64,
}

pub struct TarReader<R: Read> {
    source: R,
    pos: u64,
    finished: bool,
}

impl<R: Read> TarReader<R> {
    pub fn new(source: R) -> Self {
        TarReader {
            source,
            pos: 0,
            finished: false,
        }
    }

    pub fn next_entry(&mut self) -> Result<Option<TarEntry>> {
        if self.finished {
            return Ok(None);
        }
        let mut header = [0u8; BLOCK];
        match self.source.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.finished = true;
                return Ok(None);
            }
            Err(e) => return Err(Error::Io(e)),
        }
        self.pos += BLOCK as u64;

        if header.iter().all(|&b| b == 0) {
            self.finished = true;
            return Ok(None);
        }

        let declared = octal(&header[148..156], "checksum")? as u32;
        let actual = checksum(&header);
        if declared != actual {
            return Err(Error::Format(format!(
                "invalid header checksum: {declared} declared, {actual} computed"
            )));
        }

        let base_name = text_field(&header[0..100]);
        let prefix = if &header[257..262] == b"ustar" {
            text_field(&header[345..500])
        } else {
            String::new()
        };
        let name = if prefix.is_empty() {
            base_name
        } else {
            format!("{prefix}/{base_name}")
        };
        if name.len() > limits::MAX_NAME {
            return Err(Error::Limit("name too long".into()));
        }

        let size = octal(&header[124..136], "size")?;
        let mtime = octal(&header[136..148], "mtime")? as i64;
        let kind = header[156];
        let is_dir = kind == b'5' || name.ends_with('/');

        if kind == b'L' || kind == b'K' {
            return Err(Error::Unsupported(
                "GNU tar long headers are not supported yet".into(),
            ));
        }

        let entry = Entry {
            name,
            size,
            compressed_size: size,
            method: Method::Store,
            crc32: 0,
            is_dir,
            mtime: Some(mtime),
            offset: self.pos,
            encrypted: false,
        };
        Ok(Some(TarEntry {
            entry,
            data: if is_dir { 0 } else { size },
        }))
    }

    pub fn copy_data<W: Write>(&mut self, e: &TarEntry, dest: &mut W) -> Result<u64> {
        let n = io::copy(&mut (&mut self.source).take(e.data), dest)?;
        self.pos += n;
        let padding = (BLOCK - (e.data as usize % BLOCK)) % BLOCK;
        if padding > 0 {
            let mut discard = vec![0u8; padding];
            self.source.read_exact(&mut discard)?;
            self.pos += padding as u64;
        }
        Ok(n)
    }

    pub fn skip_data(&mut self, e: &TarEntry) -> Result<()> {
        self.copy_data(e, &mut io::sink()).map(|_| ())
    }
}

pub struct TarWriter<W: Write> {
    out: W,
}

impl<W: Write> TarWriter<W> {
    pub fn new(out: W) -> Self {
        TarWriter { out }
    }

    pub fn add<R: Read>(
        &mut self,
        name: &str,
        size: u64,
        mtime: i64,
        mode: u32,
        mut data: R,
    ) -> Result<()> {
        let (prefix, base) = split_name(name)?;
        let mut header = [0u8; BLOCK];
        header[..base.len()].copy_from_slice(base.as_bytes());
        write_octal(&mut header[100..108], mode as u64);
        write_octal(&mut header[108..116], 0);
        write_octal(&mut header[116..124], 0);
        write_octal(&mut header[124..136], size);
        write_octal(&mut header[136..148], mtime.max(0) as u64);
        header[156] = if name.ends_with('/') { b'5' } else { b'0' };
        header[257..262].copy_from_slice(b"ustar");
        header[262] = 0;
        header[263..265].copy_from_slice(b"00");
        if !prefix.is_empty() {
            header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        }
        let sum = checksum(&header);
        let s = format!("{sum:06o}");
        header[148..154].copy_from_slice(s.as_bytes());
        header[154] = 0;
        header[155] = b' ';

        self.out.write_all(&header)?;
        let written = io::copy(&mut data, &mut self.out)?;
        if written != size {
            return Err(Error::Format(format!(
                "'{name}': {size} bytes declared but {written} written"
            )));
        }
        let padding = (BLOCK - (size as usize % BLOCK)) % BLOCK;
        if padding > 0 {
            self.out.write_all(&vec![0u8; padding])?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        self.out.write_all(&[0u8; BLOCK * 2])?;
        self.out.flush()?;
        Ok(self.out)
    }
}

fn split_name(name: &str) -> Result<(&str, &str)> {
    if name.len() <= 100 {
        return Ok(("", name));
    }
    let cut = name[..name.len().min(156)]
        .rfind('/')
        .ok_or_else(|| Error::Limit(format!("name of {} bytes with no split point", name.len())))?;
    let (p, b) = name.split_at(cut);
    let b = &b[1..];
    if p.len() > 155 || b.len() > 100 {
        return Err(Error::Limit(format!("name too long for ustar: {name}")));
    }
    Ok((p, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let data = b"test payload".repeat(100);
        let mut w = TarWriter::new(Vec::new());
        w.add("dir/f.txt", data.len() as u64, 1_700_000_000, 0o644, &data[..])
            .unwrap();
        let buf = w.finish().unwrap();
        assert_eq!(buf.len() % BLOCK, 0);

        let mut r = TarReader::new(&buf[..]);
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!(e.entry.name, "dir/f.txt");
        assert_eq!(e.entry.size, data.len() as u64);
        let mut out = Vec::new();
        r.copy_data(&e, &mut out).unwrap();
        assert_eq!(out, data);
        assert!(r.next_entry().unwrap().is_none());
    }

    #[test]
    fn bad_checksum_is_detected() {
        let mut w = TarWriter::new(Vec::new());
        w.add("a.txt", 3, 0, 0o644, &b"abc"[..]).unwrap();
        let mut buf = w.finish().unwrap();
        buf[10] ^= 0xFF;
        let mut r = TarReader::new(&buf[..]);
        assert!(r.next_entry().is_err());
    }

    #[test]
    fn garbage_does_not_panic() {
        for seed in 0u32..1000 {
            let n = (seed as usize % 1500) + 1;
            let data: Vec<u8> = (0..n)
                .map(|i| ((seed.wrapping_mul(2_246_822_519) >> (i % 24)) & 0xFF) as u8)
                .collect();
            let mut r = TarReader::new(&data[..]);
            let _ = r.next_entry();
        }
    }
}
