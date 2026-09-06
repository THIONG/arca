#![forbid(unsafe_code)]

use arca_core::{limits, Entry, Error, Method, Result};
use std::io::{self, Read, Write};

const BLOQUE: usize = 512;

fn octal(campo: &[u8], que: &str) -> Result<u64> {
    let s: Vec<u8> = campo
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
            return Err(Error::Format(format!("{que}: octal invalido")));
        }
        v = v
            .checked_mul(8)
            .and_then(|x| x.checked_add((b - b'0') as u64))
            .ok_or_else(|| Error::Limit(format!("{que}: desbordamiento")))?;
    }
    Ok(v)
}

fn escribir_octal(destino: &mut [u8], valor: u64) {
    let ancho = destino.len() - 1;
    let s = format!("{valor:0ancho$o}", ancho = ancho);
    let b = s.as_bytes();
    let n = b.len().min(ancho);
    destino[..n].copy_from_slice(&b[b.len() - n..]);
    destino[ancho] = 0;
}

fn checksum(cab: &[u8; BLOQUE]) -> u32 {
    let mut s: u32 = 0;
    for (i, &b) in cab.iter().enumerate() {
        s += if (148..156).contains(&i) { 32 } else { b as u32 };
    }
    s
}

fn cadena(campo: &[u8]) -> String {
    let fin = campo.iter().position(|&b| b == 0).unwrap_or(campo.len());
    String::from_utf8_lossy(&campo[..fin]).into_owned()
}

pub struct TarEntry {
    pub entry: Entry,
    pub datos: u64,
}

pub struct TarReader<R: Read> {
    fuente: R,
    pos: u64,
    terminado: bool,
}

impl<R: Read> TarReader<R> {
    pub fn new(fuente: R) -> Self {
        TarReader { fuente, pos: 0, terminado: false }
    }

    pub fn next_entry(&mut self) -> Result<Option<TarEntry>> {
        if self.terminado {
            return Ok(None);
        }
        let mut cab = [0u8; BLOQUE];
        match self.fuente.read_exact(&mut cab) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.terminado = true;
                return Ok(None);
            }
            Err(e) => return Err(Error::Io(e)),
        }
        self.pos += BLOQUE as u64;

        if cab.iter().all(|&b| b == 0) {
            self.terminado = true;
            return Ok(None);
        }

        let esperado = octal(&cab[148..156], "checksum")? as u32;
        let real = checksum(&cab);
        if esperado != real {
            return Err(Error::Format(format!(
                "checksum de cabecera invalido: {esperado} declarado, {real} calculado"
            )));
        }

        let nombre_base = cadena(&cab[0..100]);
        let prefijo = if &cab[257..262] == b"ustar" { cadena(&cab[345..500]) } else { String::new() };
        let name = if prefijo.is_empty() {
            nombre_base
        } else {
            format!("{prefijo}/{nombre_base}")
        };
        if name.len() > limits::MAX_NAME {
            return Err(Error::Limit("nombre demasiado largo".into()));
        }

        let size = octal(&cab[124..136], "tamano")?;
        let mtime = octal(&cab[136..148], "mtime")? as i64;
        let tipo = cab[156];
        let is_dir = tipo == b'5' || name.ends_with('/');

        if tipo == b'L' || tipo == b'K' {
            return Err(Error::Unsupported(
                "cabeceras largas de GNU tar todavia no soportadas".into(),
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
        };
        Ok(Some(TarEntry { entry, datos: if is_dir { 0 } else { size } }))
    }

    pub fn copiar_datos<W: Write>(&mut self, e: &TarEntry, destino: &mut W) -> Result<u64> {
        let n = io::copy(&mut (&mut self.fuente).take(e.datos), destino)?;
        self.pos += n;
        let relleno = (BLOQUE - (e.datos as usize % BLOQUE)) % BLOQUE;
        if relleno > 0 {
            let mut basura = vec![0u8; relleno];
            self.fuente.read_exact(&mut basura)?;
            self.pos += relleno as u64;
        }
        Ok(n)
    }

    pub fn saltar_datos(&mut self, e: &TarEntry) -> Result<()> {
        self.copiar_datos(e, &mut io::sink()).map(|_| ())
    }
}

pub struct TarWriter<W: Write> {
    salida: W,
}

impl<W: Write> TarWriter<W> {
    pub fn new(salida: W) -> Self {
        TarWriter { salida }
    }

    pub fn add<R: Read>(
        &mut self,
        nombre: &str,
        tam: u64,
        mtime: i64,
        modo: u32,
        mut datos: R,
    ) -> Result<()> {
        let (prefijo, base) = partir_nombre(nombre)?;
        let mut cab = [0u8; BLOQUE];
        cab[..base.len()].copy_from_slice(base.as_bytes());
        escribir_octal(&mut cab[100..108], modo as u64);
        escribir_octal(&mut cab[108..116], 0);
        escribir_octal(&mut cab[116..124], 0);
        escribir_octal(&mut cab[124..136], tam);
        escribir_octal(&mut cab[136..148], mtime.max(0) as u64);
        cab[156] = if nombre.ends_with('/') { b'5' } else { b'0' };
        cab[257..262].copy_from_slice(b"ustar");
        cab[262] = 0;
        cab[263..265].copy_from_slice(b"00");
        if !prefijo.is_empty() {
            cab[345..345 + prefijo.len()].copy_from_slice(prefijo.as_bytes());
        }
        let suma = checksum(&cab);
        let s = format!("{suma:06o}");
        cab[148..154].copy_from_slice(s.as_bytes());
        cab[154] = 0;
        cab[155] = b' ';

        self.salida.write_all(&cab)?;
        let escritos = io::copy(&mut datos, &mut self.salida)?;
        if escritos != tam {
            return Err(Error::Format(format!(
                "«{nombre}»: se declararon {tam} bytes y se escribieron {escritos}"
            )));
        }
        let relleno = (BLOQUE - (tam as usize % BLOQUE)) % BLOQUE;
        if relleno > 0 {
            self.salida.write_all(&vec![0u8; relleno])?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        self.salida.write_all(&[0u8; BLOQUE * 2])?;
        self.salida.flush()?;
        Ok(self.salida)
    }
}

fn partir_nombre(nombre: &str) -> Result<(&str, &str)> {
    if nombre.len() <= 100 {
        return Ok(("", nombre));
    }
    let corte = nombre[..nombre.len().min(156)]
        .rfind('/')
        .ok_or_else(|| Error::Limit(format!("nombre de {} bytes sin punto de corte", nombre.len())))?;
    let (p, b) = nombre.split_at(corte);
    let b = &b[1..];
    if p.len() > 155 || b.len() > 100 {
        return Err(Error::Limit(format!("nombre demasiado largo para ustar: {nombre}")));
    }
    Ok((p, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ida_y_vuelta() {
        let datos = b"contenido de prueba".repeat(100);
        let mut w = TarWriter::new(Vec::new());
        w.add("dir/f.txt", datos.len() as u64, 1_700_000_000, 0o644, &datos[..]).unwrap();
        let buf = w.finish().unwrap();
        assert_eq!(buf.len() % BLOQUE, 0);

        let mut r = TarReader::new(&buf[..]);
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!(e.entry.name, "dir/f.txt");
        assert_eq!(e.entry.size, datos.len() as u64);
        let mut salida = Vec::new();
        r.copiar_datos(&e, &mut salida).unwrap();
        assert_eq!(salida, datos);
        assert!(r.next_entry().unwrap().is_none());
    }

    #[test]
    fn checksum_malo_se_detecta() {
        let mut w = TarWriter::new(Vec::new());
        w.add("a.txt", 3, 0, 0o644, &b"abc"[..]).unwrap();
        let mut buf = w.finish().unwrap();
        buf[10] ^= 0xFF;
        let mut r = TarReader::new(&buf[..]);
        assert!(r.next_entry().is_err());
    }

    #[test]
    fn basura_no_hace_panic() {
        for semilla in 0u32..1000 {
            let n = (semilla as usize % 1500) + 1;
            let datos: Vec<u8> = (0..n)
                .map(|i| ((semilla.wrapping_mul(2_246_822_519) >> (i % 24)) & 0xFF) as u8)
                .collect();
            let mut r = TarReader::new(&datos[..]);
            let _ = r.next_entry();
        }
    }
}
