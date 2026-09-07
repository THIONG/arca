use aes::cipher::{KeyIvInit, StreamCipher};
use arca_core::{Error, Result};
use hmac::{Hmac, Mac};
use sha1::Sha1;

pub const METHOD_AE: u16 = 99;
pub const EXTRA_AE: u16 = 0x9901;
pub const VENDOR_AE: u16 = 0x4541;
pub const AE2: u16 = 2;
pub const STRENGTH_256: u8 = 3;
pub const SALT_256: usize = 16;
pub const KEY_256: usize = 32;
pub const VERIFIER: usize = 2;
pub const AUTH_CODE: usize = 10;
pub const OVERHEAD: usize = SALT_256 + VERIFIER + AUTH_CODE;

const ITERATIONS: u32 = 1000;

type Ctr = ctr::Ctr128LE<aes::Aes256>;
type HmacSha1 = Hmac<Sha1>;

pub struct Keys {
    cipher: [u8; KEY_256],
    mac: [u8; KEY_256],
    verifier: [u8; VERIFIER],
}

// PBKDF2-HMAC-SHA1 with 1000 iterations, exactly what WinZip AES specifies.
// The derived block is the encryption key, then the authentication key, then
// two bytes that only serve to tell a wrong password apart quickly.
pub fn derive(password: &str, salt: &[u8]) -> Keys {
    let mut out = [0u8; KEY_256 * 2 + VERIFIER];
    pbkdf2::pbkdf2_hmac::<Sha1>(password.as_bytes(), salt, ITERATIONS, &mut out);
    let mut k = Keys {
        cipher: [0u8; KEY_256],
        mac: [0u8; KEY_256],
        verifier: [0u8; VERIFIER],
    };
    k.cipher.copy_from_slice(&out[..KEY_256]);
    k.mac.copy_from_slice(&out[KEY_256..KEY_256 * 2]);
    k.verifier.copy_from_slice(&out[KEY_256 * 2..]);
    k
}

// WinZip AES counts the CTR block as a little endian integer starting at one,
// which is not what the usual big endian CTR does. Getting this backwards
// produces archives that look fine here and that no other tool can read.
fn stream(key: &[u8; KEY_256]) -> Ctr {
    let mut iv = [0u8; 16];
    iv[0] = 1;
    Ctr::new(key.into(), &iv.into())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn random_salt() -> Result<[u8; SALT_256]> {
    let mut salt = [0u8; SALT_256];
    getrandom::getrandom(&mut salt)
        .map_err(|e| Error::Format(format!("no randomness available: {e}")))?;
    Ok(salt)
}

// Encrypts in place and returns salt, verifier and authentication code, which
// is what wraps the entry data on disk.
pub fn encrypt(password: &str, data: &mut [u8]) -> Result<([u8; SALT_256], [u8; VERIFIER], [u8; AUTH_CODE])> {
    let salt = random_salt()?;
    let keys = derive(password, &salt);
    stream(&keys.cipher).apply_keystream(data);
    let mut mac = <HmacSha1 as Mac>::new_from_slice(&keys.mac)
        .map_err(|_| Error::Format("bad HMAC key length".into()))?;
    mac.update(data);
    let tag = mac.finalize().into_bytes();
    let mut auth = [0u8; AUTH_CODE];
    auth.copy_from_slice(&tag[..AUTH_CODE]);
    Ok((salt, keys.verifier, auth))
}

// The authentication code is checked before returning a single byte: a wrong
// or tampered archive must not reach the caller as plausible looking data.
pub fn decrypt(password: &str, body: &[u8]) -> Result<Vec<u8>> {
    if body.len() < OVERHEAD {
        return Err(Error::Format(format!(
            "encrypted entry of {} bytes, shorter than the {OVERHEAD} bytes of its own header",
            body.len()
        )));
    }
    let salt = &body[..SALT_256];
    let verifier = &body[SALT_256..SALT_256 + VERIFIER];
    let auth = &body[body.len() - AUTH_CODE..];
    let cipher_text = &body[SALT_256 + VERIFIER..body.len() - AUTH_CODE];

    let keys = derive(password, salt);
    if !constant_time_eq(verifier, &keys.verifier) {
        return Err(Error::Format("wrong password".into()));
    }

    let mut mac = <HmacSha1 as Mac>::new_from_slice(&keys.mac)
        .map_err(|_| Error::Format("bad HMAC key length".into()))?;
    mac.update(cipher_text);
    let tag = mac.finalize().into_bytes();
    if !constant_time_eq(auth, &tag[..AUTH_CODE]) {
        return Err(Error::Tampered { name: "encrypted entry".into() });
    }

    let mut plain = cipher_text.to_vec();
    stream(&keys.cipher).apply_keystream(&mut plain);
    Ok(plain)
}

// The 0x9901 extra field: version, vendor, strength and the compression method
// that method 99 is standing in for.
pub fn extra_field(real_method: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(11);
    v.extend_from_slice(&EXTRA_AE.to_le_bytes());
    v.extend_from_slice(&7u16.to_le_bytes());
    v.extend_from_slice(&AE2.to_le_bytes());
    v.extend_from_slice(&VENDOR_AE.to_le_bytes());
    v.push(STRENGTH_256);
    v.extend_from_slice(&real_method.to_le_bytes());
    v
}

pub struct AesInfo {
    pub strength: u8,
    pub real_method: u16,
}

pub fn parse_extra(extra: &[u8]) -> Option<AesInfo> {
    let mut c = arca_core::Cursor::new(extra);
    while c.remaining() >= 4 {
        let id = c.u16le("extra field id").ok()?;
        let size = c.u16le("extra field size").ok()? as usize;
        if size > c.remaining() {
            return None;
        }
        let body = c.bytes(size, "extra field").ok()?;
        if id == EXTRA_AE && body.len() >= 7 {
            return Some(AesInfo {
                strength: body[4],
                real_method: u16::from_le_bytes([body[5], body[6]]),
            });
        }
    }
    None
}

// Encrypts and authenticates as the bytes go through, so a large entry never
// has to be held in memory. Requirement R6 says the peak stays bounded, and
// buffering the whole file to encrypt it would have broken that quietly.
pub struct AesWriter<W: std::io::Write> {
    inner: W,
    stream: Ctr,
    mac: HmacSha1,
    pub written: u64,
    scratch: Vec<u8>,
}

impl<W: std::io::Write> AesWriter<W> {
    pub fn new(inner: W, keys: &Keys) -> Result<Self> {
        Ok(AesWriter {
            inner,
            stream: stream(&keys.cipher),
            mac: <HmacSha1 as Mac>::new_from_slice(&keys.mac)
                .map_err(|_| Error::Format("bad HMAC key length".into()))?,
            written: 0,
            scratch: Vec::new(),
        })
    }

    pub fn finish(self) -> (W, [u8; AUTH_CODE], u64) {
        let tag = self.mac.finalize().into_bytes();
        let mut auth = [0u8; AUTH_CODE];
        auth.copy_from_slice(&tag[..AUTH_CODE]);
        (self.inner, auth, self.written)
    }
}

impl<W: std::io::Write> std::io::Write for AesWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.scratch.clear();
        self.scratch.extend_from_slice(buf);
        self.stream.apply_keystream(&mut self.scratch);
        self.mac.update(&self.scratch);
        self.inner.write_all(&self.scratch)?;
        self.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub fn verifier_of(keys: &Keys) -> [u8; VERIFIER] {
    keys.verifier
}

pub fn verifier_matches(keys: &Keys, given: &[u8]) -> bool {
    constant_time_eq(&keys.verifier, given)
}

pub fn auth_matches(computed: &[u8; AUTH_CODE], given: &[u8]) -> bool {
    constant_time_eq(computed, given)
}

// Decrypts as the bytes come off the disk, so the decompressor downstream never
// waits for the whole entry to be in memory. The price is that the
// authentication code can only be checked once everything has gone through: the
// caller gets the plaintext first and the verdict after, and must treat what it
// wrote as suspect until `finish` says otherwise.
pub struct AesReader<R: std::io::Read> {
    inner: R,
    stream: Ctr,
    mac: HmacSha1,
    remaining: u64,
}

impl<R: std::io::Read> AesReader<R> {
    pub fn new(inner: R, keys: &Keys, cipher_len: u64) -> Result<Self> {
        Ok(AesReader {
            inner,
            stream: stream(&keys.cipher),
            mac: <HmacSha1 as Mac>::new_from_slice(&keys.mac)
                .map_err(|_| Error::Format("bad HMAC key length".into()))?,
            remaining: cipher_len,
        })
    }

    // A decompressor stops at the end of its own stream, which need not be the
    // end of the ciphertext. Whatever is left still belongs under the MAC.
    pub fn finish(mut self) -> Result<[u8; AUTH_CODE]> {
        let mut skip = [0u8; 8192];
        while self.remaining > 0 {
            let want = skip.len().min(self.remaining as usize);
            let n = self.inner.read(&mut skip[..want])?;
            if n == 0 {
                break;
            }
            self.mac.update(&skip[..n]);
            self.remaining -= n as u64;
        }
        let tag = self.mac.finalize().into_bytes();
        let mut auth = [0u8; AUTH_CODE];
        auth.copy_from_slice(&tag[..AUTH_CODE]);
        Ok(auth)
    }
}

impl<R: std::io::Read> std::io::Read for AesReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Ok(0);
        }
        self.mac.update(&buf[..n]);
        self.stream.apply_keystream(&mut buf[..n]);
        self.remaining -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_recovers_the_bytes() {
        let mut data = b"something worth hiding".repeat(50);
        let original = data.clone();
        let (salt, verifier, auth) = encrypt("correct horse", &mut data).unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(&salt);
        body.extend_from_slice(&verifier);
        body.extend_from_slice(&data);
        body.extend_from_slice(&auth);
        assert_eq!(decrypt("correct horse", &body).unwrap(), original);
    }

    #[test]
    fn a_wrong_password_is_rejected() {
        let mut data = b"secret".to_vec();
        let (salt, verifier, auth) = encrypt("right", &mut data).unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(&salt);
        body.extend_from_slice(&verifier);
        body.extend_from_slice(&data);
        body.extend_from_slice(&auth);
        assert!(decrypt("wrong", &body).is_err());
    }

    #[test]
    fn a_tampered_byte_is_caught_by_the_authentication_code() {
        let mut data = b"secret payload here".to_vec();
        let (salt, verifier, auth) = encrypt("key", &mut data).unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(&salt);
        body.extend_from_slice(&verifier);
        body.extend_from_slice(&data);
        body.extend_from_slice(&auth);
        let middle = SALT_256 + VERIFIER + 3;
        body[middle] ^= 0xFF;
        assert!(
            matches!(decrypt("key", &body), Err(Error::Tampered { .. })),
            "a flipped bit must not decrypt to plausible data"
        );
    }

    #[test]
    fn two_encryptions_of_the_same_text_differ() {
        let mut a = b"same".to_vec();
        let mut b = b"same".to_vec();
        let (salt_a, _, _) = encrypt("k", &mut a).unwrap();
        let (salt_b, _, _) = encrypt("k", &mut b).unwrap();
        assert_ne!(salt_a, salt_b, "the salt must be random on every entry");
        assert_ne!(a, b, "a repeated salt would leak that the contents match");
    }

    #[test]
    fn garbage_does_not_panic() {
        for n in 0..64usize {
            let body: Vec<u8> = (0..n).map(|i| (i * 37) as u8).collect();
            let _ = decrypt("whatever", &body);
        }
        for n in 0..40usize {
            let extra: Vec<u8> = (0..n).map(|i| (i * 91) as u8).collect();
            let _ = parse_extra(&extra);
        }
    }

    #[test]
    fn the_extra_field_says_what_it_should() {
        let f = extra_field(8);
        let info = parse_extra(&f).expect("must parse back");
        assert_eq!(info.strength, STRENGTH_256);
        assert_eq!(info.real_method, 8);
    }
}
