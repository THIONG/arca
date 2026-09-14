// PKWARE's original stream cipher, the one every ZIP tool has carried since
// 1989. It is broken: a few hundred known plaintext bytes recover the keys, and
// the password check is a single byte. Arca reads it so that old archives are
// not a dead end, and never writes it -- anything it creates is AES-256.
use arca_core::{Error, Result};
use std::io::Read;

pub const HEADER: usize = 12;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc32_byte(crc: u32, b: u8) -> u32 {
    CRC_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8)
}

pub struct Keys {
    k: [u32; 3],
}

impl Keys {
    pub fn new(password: &[u8]) -> Self {
        let mut keys = Keys {
            k: [0x1234_5678, 0x2345_6789, 0x3456_7890],
        };
        for b in password {
            keys.update(*b);
        }
        keys
    }

    // Every wrapping operation here is part of the cipher, not an overflow
    // being papered over: the spec defines these as 32 bit truncating steps.
    fn update(&mut self, plain: u8) {
        self.k[0] = crc32_byte(self.k[0], plain);
        self.k[1] = self.k[1]
            .wrapping_add(self.k[0] & 0xFF)
            .wrapping_mul(134_775_813)
            .wrapping_add(1);
        self.k[2] = crc32_byte(self.k[2], (self.k[1] >> 24) as u8);
    }

    fn stream_byte(&self) -> u8 {
        let t = ((self.k[2] | 2) & 0xFFFF) as u16;
        (t.wrapping_mul(t ^ 1) >> 8) as u8
    }

    pub fn decrypt(&mut self, buf: &mut [u8]) {
        for c in buf.iter_mut() {
            *c ^= self.stream_byte();
            self.update(*c);
        }
    }

    #[cfg(test)]
    pub fn encrypt(&mut self, buf: &mut [u8]) {
        for c in buf.iter_mut() {
            let keystream = self.stream_byte();
            self.update(*c);
            *c ^= keystream;
        }
    }
}

// Decrypts as the bytes come off the disk, the same shape as the AES reader, so
// the decompressor downstream never waits for the whole entry.
pub struct Reader<R: Read> {
    inner: R,
    keys: Keys,
}

// Eats the 12 byte header and checks its last byte against what the entry says
// it should be. That byte is the whole password check this scheme has: one byte
// in 256 lets a wrong password through, and then the CRC of the entry is what
// catches it.
pub fn open<R: Read>(mut inner: R, password: &str, check: u8, name: &str) -> Result<Reader<R>> {
    let mut keys = Keys::new(password.as_bytes());
    let mut head = [0u8; HEADER];
    inner.read_exact(&mut head)?;
    keys.decrypt(&mut head);
    if head[HEADER - 1] != check {
        return Err(Error::Format(format!("'{name}': wrong password")));
    }
    Ok(Reader { inner, keys })
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.keys.decrypt(&mut buf[..n]);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_it_encrypts_it_decrypts() {
        let plain = b"Arca reads the old scheme too.".repeat(10);
        let mut body = plain.clone();
        Keys::new(b"secreto").encrypt(&mut body);
        assert_ne!(body, plain);
        Keys::new(b"secreto").decrypt(&mut body);
        assert_eq!(body, plain);
    }

    #[test]
    fn a_short_header_is_an_error_and_not_a_panic() {
        let err = open(&b"only four"[..], "secreto", 0, "x.txt");
        assert!(err.is_err());
    }
}
