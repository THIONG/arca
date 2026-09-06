#![forbid(unsafe_code)]

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
const CD_FIJO: usize = 46;
const LFH_FIJO: usize = 30;
const EXTRA_Z64: usize = 20;
const MAX_COMENTARIO: usize = 65_535;
const BUF_FLUJO: usize = 256 * 1024;

struct CrcWriter<W: Write> {
    inner: W,
    hasher: crc32fast::Hasher,
    escritos: u64,
}

impl<W: Write> CrcWriter<W> {
    fn new(inner: W) -> Self {
        CrcWriter { inner, hasher: crc32fast::Hasher::new(), escritos: 0 }
    }
    fn finalizar(self) -> (W, u32, u64) {
        (self.inner, self.hasher.finalize(), self.escritos)
    }
}

impl<W: Write> Write for CrcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.escritos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub struct ZipArchive<R: Read + Seek> {
    fuente: R,
    entradas: Vec<Entry>,
}

impl<R: Read + Seek> ZipArchive<R> {
    pub fn open(mut fuente: R) -> Result<Self> {
        let total = fuente.seek(SeekFrom::End(0))?;
        if total < EOCD_MIN as u64 {
            return Err(Error::Format("demasiado corto para ser un ZIP".into()));
        }

        let cola_len = (EOCD_MIN + MAX_COMENTARIO).min(total as usize);
        let cola_ini = total - cola_len as u64;
        fuente.seek(SeekFrom::Start(cola_ini))?;
        let mut cola = vec![0u8; cola_len];
        fuente.read_exact(&mut cola)?;

        let pos_eocd = buscar_hacia_atras(&cola, SIG_EOCD).ok_or_else(|| {
            Error::Format("no se encontro el fin del directorio central (¿es un ZIP?)".into())
        })?;

        let mut c = Cursor::en(&cola, pos_eocd)?;
        c.saltar(4, "firma EOCD")?;
        c.saltar(4, "numeros de disco")?;
        let _ent_disco = c.u16le("entradas en disco")?;
        let mut n_entradas = c.u16le("entradas totales")? as u64;
        let mut cd_tam = c.u32le("tamano del directorio")? as u64;
        let mut cd_off = c.u32le("desplazamiento del directorio")? as u64;

        if n_entradas == 0xFFFF || cd_tam == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
            if let Some(p) = buscar_hacia_atras(&cola[..pos_eocd], SIG_LOC64) {
                let mut l = Cursor::en(&cola, p)?;
                l.saltar(4, "firma del localizador Zip64")?;
                l.saltar(4, "disco del EOCD64")?;
                let off64 = l.u64le("desplazamiento del EOCD64")?;
                if off64 >= total {
                    return Err(Error::Format("el localizador Zip64 apunta fuera del archivo".into()));
                }
                fuente.seek(SeekFrom::Start(off64))?;
                let mut r64 = [0u8; 56];
                fuente.read_exact(&mut r64)?;
                let mut z = Cursor::new(&r64);
                if z.u32le("firma EOCD64")? != SIG_EOCD64 {
                    return Err(Error::Format("firma Zip64 invalida".into()));
                }
                z.saltar(20, "cabecera del EOCD64")?;
                z.saltar(8, "entradas en disco")?;
                n_entradas = z.u64le("entradas totales (Zip64)")?;
                cd_tam = z.u64le("tamano del directorio (Zip64)")?;
                cd_off = z.u64le("desplazamiento del directorio (Zip64)")?;
            }
        }

        if n_entradas > limits::MAX_ENTRIES {
            return Err(Error::Limit(format!("{n_entradas} entradas declaradas")));
        }
        if cd_off > total || cd_tam > total || cd_off + cd_tam > total {
            return Err(Error::Format(
                "el directorio central queda fuera del archivo".into(),
            ));
        }

        fuente.seek(SeekFrom::Start(cd_off))?;
        let mut cd = vec![0u8; cd_tam as usize];
        fuente.read_exact(&mut cd)?;

        let mut entradas = Vec::with_capacity(n_entradas.min(4096) as usize);
        let mut c = Cursor::new(&cd);
        for i in 0..n_entradas {
            if c.restantes() < CD_FIJO {
                break;
            }
            match leer_cabecera_central(&mut c) {
                Ok(e) => entradas.push(e),
                Err(e) => {
                    return Err(Error::Format(format!(
                        "entrada {i} del directorio central: {e}"
                    )))
                }
            }
        }

        Ok(ZipArchive { fuente, entradas })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entradas
    }

    pub fn len(&self) -> usize {
        self.entradas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entradas.is_empty()
    }

    pub fn extract_to<W: Write>(&mut self, idx: usize, destino: W) -> Result<u64> {
        let e = self
            .entradas
            .get(idx)
            .ok_or_else(|| Error::Format(format!("no existe la entrada {idx}")))?
            .clone();

        self.fuente.seek(SeekFrom::Start(e.offset))?;
        let mut lfh = [0u8; LFH_FIJO];
        self.fuente.read_exact(&mut lfh)?;
        let mut c = Cursor::new(&lfh);
        if c.u32le("firma de cabecera local")? != SIG_LFH {
            return Err(Error::Format(format!(
                "«{}»: no hay cabecera local en el desplazamiento {}",
                e.name, e.offset
            )));
        }
        c.saltar(2, "version")?;
        let flags = c.u16le("flags")?;
        if flags & 1 != 0 {
            return Err(Error::Unsupported(format!("«{}» esta cifrada", e.name)));
        }
        c.saltar(18, "resto de la cabecera local")?;
        let n_len = c.u16le("longitud del nombre")? as u64;
        let x_len = c.u16le("longitud de extra")? as u64;

        let inicio_datos = e.offset + LFH_FIJO as u64 + n_len + x_len;
        self.fuente.seek(SeekFrom::Start(inicio_datos))?;
        let acotado = (&mut self.fuente).take(e.compressed_size);

        let mut cw = CrcWriter::new(destino);
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
                        "«{}» usa Zstandard y este binario se compilo sin el".into(),
                    ));
                }
            }
        }
        let (_, crc, escritos) = cw.finalizar();

        if crc != e.crc32 {
            return Err(Error::Integrity { name: e.name.clone(), esperado: e.crc32, obtenido: crc });
        }
        if escritos != e.size {
            return Err(Error::Format(format!(
                "«{}»: se esperaban {} bytes y salieron {escritos}",
                e.name, e.size
            )));
        }
        Ok(escritos)
    }
}

fn buscar_hacia_atras(buf: &[u8], firma: u32) -> Option<usize> {
    let f = firma.to_le_bytes();
    if buf.len() < 4 {
        return None;
    }
    (0..=buf.len() - 4).rev().find(|&i| buf[i..i + 4] == f)
}

#[rustfmt::skip]
const CP437_ALTO: [char; 128] = [
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

fn de_cp437(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii() {
                b as char
            } else {
                CP437_ALTO[usize::from(b) - 0x80]
            }
        })
        .collect()
}

fn leer_cabecera_central(c: &mut Cursor<'_>) -> Result<Entry> {
    if c.u32le("firma del directorio")? != SIG_CD {
        return Err(Error::Format("firma de entrada invalida".into()));
    }
    c.saltar(4, "versiones")?;
    let flags = c.u16le("flags")?;
    let metodo = c.u16le("metodo")?;
    let hora = c.u16le("hora")?;
    let fecha = c.u16le("fecha")?;
    let crc = c.u32le("crc32")?;
    let mut comp = c.u32le("tamano comprimido")? as u64;
    let mut sin_comp = c.u32le("tamano sin comprimir")? as u64;
    let n_len = c.u16le("longitud del nombre")? as usize;
    let x_len = c.u16le("longitud de extra")? as usize;
    let k_len = c.u16le("longitud del comentario")? as usize;
    c.saltar(4, "disco y atributos internos")?;
    let _ext_attr = c.u32le("atributos externos")?;
    let mut offset = c.u32le("desplazamiento local")? as u64;

    if n_len > limits::MAX_NAME {
        return Err(Error::Limit(format!("nombre de {n_len} bytes")));
    }
    if x_len > limits::MAX_EXTRA {
        return Err(Error::Limit(format!("campo extra de {x_len} bytes")));
    }

    let nombre_bytes = c.bytes(n_len, "nombre")?;
    let extra = c.bytes(x_len, "campo extra")?;
    c.saltar(k_len, "comentario")?;

    if sin_comp == 0xFFFF_FFFF || comp == 0xFFFF_FFFF || offset == 0xFFFF_FFFF {
        let mut x = Cursor::new(extra);
        while x.restantes() >= 4 {
            let id = x.u16le("id de campo extra")?;
            let tam = x.u16le("tamano de campo extra")? as usize;
            if tam > x.restantes() {
                break;
            }
            if id == 0x0001 {
                let mut z = Cursor::new(x.bytes(tam, "datos Zip64")?);
                if sin_comp == 0xFFFF_FFFF && z.restantes() >= 8 {
                    sin_comp = z.u64le("tamano sin comprimir Zip64")?;
                }
                if comp == 0xFFFF_FFFF && z.restantes() >= 8 {
                    comp = z.u64le("tamano comprimido Zip64")?;
                }
                if offset == 0xFFFF_FFFF && z.restantes() >= 8 {
                    offset = z.u64le("desplazamiento Zip64")?;
                }
                break;
            }
            x.saltar(tam, "campo extra")?;
        }
    }

    let method = match metodo {
        0 => Method::Store,
        8 => Method::Deflate,
        93 => Method::Zstd,
        otro => {
            return Err(Error::Unsupported(format!(
                "metodo de compresion {otro} (se admiten store, deflate y zstd)"
            )))
        }
    };

    let name = if flags & 0x800 != 0 {
        String::from_utf8_lossy(nombre_bytes).into_owned()
    } else {
        de_cp437(nombre_bytes)
    };
    let is_dir = name.ends_with('/') || name.ends_with('\\');

    Ok(Entry {
        name,
        size: sin_comp,
        compressed_size: comp,
        method,
        crc32: crc,
        is_dir,
        mtime: arca_core::dos_a_unix(fecha, hora),
        offset,
    })
}

struct Registro {
    nombre: Vec<u8>,
    crc: u32,
    comp: u64,
    sin_comp: u64,
    offset: u64,
    metodo: u16,
    fecha: u16,
    hora: u16,
    dir: bool,
}

pub struct ZipWriter<W: Write + Seek> {
    salida: W,
    registros: Vec<Registro>,
    pos: u64,
}

impl<W: Write + Seek> ZipWriter<W> {
    pub fn new(salida: W) -> Self {
        ZipWriter { salida, registros: Vec::new(), pos: 0 }
    }

    pub fn add<R: Read>(
        &mut self,
        nombre: &str,
        mut datos: R,
        codec: Codec,
        nivel: Level,
        mtime: Option<i64>,
    ) -> Result<()> {
        let (fecha, hora) = arca_core::unix_a_dos(mtime.unwrap_or(0));
        let nombre_b = comprobar_nombre(nombre)?;
        let metodo = metodo_de(codec);
        let offset = self.pos;
        self.escribir_lfh(&nombre_b, metodo.codigo(), fecha, hora)?;
        let extra_off = offset + LFH_FIJO as u64 + nombre_b.len() as u64;

        let mut hasher = crc32fast::Hasher::new();
        let mut sin_comp: u64 = 0;
        let comp: u64;
        let mut buf = vec![0u8; BUF_FLUJO];

        match metodo {
            Method::Store => {
                loop {
                    let n = datos.read(&mut buf)?;
                    if n == 0 { break; }
                    hasher.update(&buf[..n]);
                    self.salida.write_all(&buf[..n])?;
                    sin_comp += n as u64;
                }
                comp = sin_comp;
            }
            Method::Deflate => {
                let mut enc = DeflateEncoder::new(
                    Contador::new(&mut self.salida),
                    Compression::new(nivel.a_flate2()),
                );
                loop {
                    let n = datos.read(&mut buf)?;
                    if n == 0 { break; }
                    hasher.update(&buf[..n]);
                    enc.write_all(&buf[..n])?;
                    sin_comp += n as u64;
                }
                comp = enc.finish()?.escritos;
            }
            Method::Zstd => {
                #[cfg(feature = "codecs-native")]
                {
                    let mut enc = zstd::stream::write::Encoder::new(
                        Contador::new(&mut self.salida),
                        nivel.a_zstd(),
                    )
                    .map_err(Error::Io)?;
                    let _ = enc.multithread(hilos_disponibles());
                    loop {
                        let n = datos.read(&mut buf)?;
                        if n == 0 { break; }
                        hasher.update(&buf[..n]);
                        enc.write_all(&buf[..n])?;
                        sin_comp += n as u64;
                    }
                    comp = enc.finish().map_err(Error::Io)?.escritos;
                }
                #[cfg(not(feature = "codecs-native"))]
                {
                    return Err(Error::Unsupported(
                        "este binario se compilo sin Zstandard".into(),
                    ));
                }
            }
        }

        let crc = hasher.finalize();
        self.cerrar_entrada(
            nombre_b, offset, extra_off, crc, comp, sin_comp, metodo, fecha, hora,
            nombre.ends_with('/'),
        )
    }

    pub fn add_comprimido(
        &mut self,
        nombre: &str,
        comprimido: &[u8],
        crc: u32,
        sin_comp: u64,
        metodo: Method,
        mtime: Option<i64>,
    ) -> Result<()> {
        let (fecha, hora) = arca_core::unix_a_dos(mtime.unwrap_or(0));
        let nombre_b = comprobar_nombre(nombre)?;
        let offset = self.pos;
        self.escribir_lfh(&nombre_b, metodo.codigo(), fecha, hora)?;
        let extra_off = offset + LFH_FIJO as u64 + nombre_b.len() as u64;
        self.salida.write_all(comprimido)?;
        self.cerrar_entrada(
            nombre_b, offset, extra_off, crc, comprimido.len() as u64, sin_comp, metodo,
            fecha, hora, nombre.ends_with('/'),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn cerrar_entrada(
        &mut self,
        nombre_b: Vec<u8>,
        offset: u64,
        extra_off: u64,
        crc: u32,
        comp: u64,
        sin_comp: u64,
        metodo: Method,
        fecha: u16,
        hora: u16,
        dir: bool,
    ) -> Result<()> {
        let fin = extra_off + EXTRA_Z64 as u64 + comp;
        let sat_u = sin_comp >= 0xFFFF_FFFF;
        let sat_c = comp >= 0xFFFF_FFFF;
        self.salida.seek(SeekFrom::Start(offset + 14))?;
        self.salida.write_all(&crc.to_le_bytes())?;
        self.salida
            .write_all(&(if sat_c { 0xFFFF_FFFFu32 } else { comp as u32 }).to_le_bytes())?;
        self.salida
            .write_all(&(if sat_u { 0xFFFF_FFFFu32 } else { sin_comp as u32 }).to_le_bytes())?;
        self.salida.seek(SeekFrom::Start(extra_off + 4))?;
        self.salida.write_all(&sin_comp.to_le_bytes())?;
        self.salida.write_all(&comp.to_le_bytes())?;
        self.salida.seek(SeekFrom::Start(fin))?;
        self.pos = fin;
        self.registros.push(Registro {
            nombre: nombre_b,
            crc,
            comp,
            sin_comp,
            offset,
            metodo: metodo.codigo(),
            fecha,
            hora,
            dir,
        });
        Ok(())
    }

    fn escribir_lfh(&mut self, nombre: &[u8], metodo: u16, fecha: u16, hora: u16) -> Result<()> {
        let mut h = Vec::with_capacity(LFH_FIJO + nombre.len() + EXTRA_Z64);
        h.extend_from_slice(&SIG_LFH.to_le_bytes());
        h.extend_from_slice(&45u16.to_le_bytes());
        h.extend_from_slice(&(0x0800u16).to_le_bytes());
        h.extend_from_slice(&metodo.to_le_bytes());
        h.extend_from_slice(&hora.to_le_bytes());
        h.extend_from_slice(&fecha.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&(nombre.len() as u16).to_le_bytes());
        h.extend_from_slice(&(EXTRA_Z64 as u16).to_le_bytes());
        h.extend_from_slice(nombre);
        h.extend_from_slice(&0x0001u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        self.salida.write_all(&h)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        let cd_off = self.pos;
        let mut cd_tam = 0u64;

        for r in &self.registros {
            let sat_u = r.sin_comp >= 0xFFFF_FFFF;
            let sat_c = r.comp >= 0xFFFF_FFFF;
            let sat_o = r.offset >= 0xFFFF_FFFF;
            let extra: Vec<u8> = if sat_u || sat_c || sat_o {
                let cuerpo = 8 * (sat_u as usize + sat_c as usize + sat_o as usize);
                let mut e = Vec::with_capacity(4 + cuerpo);
                e.extend_from_slice(&0x0001u16.to_le_bytes());
                e.extend_from_slice(&(cuerpo as u16).to_le_bytes());
                if sat_u {
                    e.extend_from_slice(&r.sin_comp.to_le_bytes());
                }
                if sat_c {
                    e.extend_from_slice(&r.comp.to_le_bytes());
                }
                if sat_o {
                    e.extend_from_slice(&r.offset.to_le_bytes());
                }
                e
            } else {
                Vec::new()
            };

            let mut h = Vec::with_capacity(CD_FIJO + r.nombre.len() + extra.len());
            h.extend_from_slice(&SIG_CD.to_le_bytes());
            h.extend_from_slice(&(0x031Eu16).to_le_bytes());
            h.extend_from_slice(&45u16.to_le_bytes());
            h.extend_from_slice(&(0x0800u16).to_le_bytes());
            h.extend_from_slice(&r.metodo.to_le_bytes());
            h.extend_from_slice(&r.hora.to_le_bytes());
            h.extend_from_slice(&r.fecha.to_le_bytes());
            h.extend_from_slice(&r.crc.to_le_bytes());
            h.extend_from_slice(&(r.comp.min(0xFFFF_FFFF) as u32).to_le_bytes());
            h.extend_from_slice(&(r.sin_comp.min(0xFFFF_FFFF) as u32).to_le_bytes());
            h.extend_from_slice(&(r.nombre.len() as u16).to_le_bytes());
            h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            let modo: u32 = if r.dir { 0o040755 } else { 0o100644 };
            h.extend_from_slice(&((modo << 16) | if r.dir { 0x10 } else { 0 }).to_le_bytes());
            h.extend_from_slice(&(r.offset.min(0xFFFF_FFFF) as u32).to_le_bytes());
            h.extend_from_slice(&r.nombre);
            h.extend_from_slice(&extra);

            self.salida.write_all(&h)?;
            cd_tam += h.len() as u64;
        }

        let n = self.registros.len() as u64;
        let necesita_z64 = n > u16::MAX as u64 || cd_off >= 0xFFFF_FFFF || cd_tam >= 0xFFFF_FFFF;

        if necesita_z64 {
            let z_off = cd_off + cd_tam;
            let mut z = Vec::with_capacity(76);
            z.extend_from_slice(&SIG_EOCD64.to_le_bytes());
            z.extend_from_slice(&44u64.to_le_bytes());
            z.extend_from_slice(&(0x031Eu16).to_le_bytes());
            z.extend_from_slice(&45u16.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&n.to_le_bytes());
            z.extend_from_slice(&n.to_le_bytes());
            z.extend_from_slice(&cd_tam.to_le_bytes());
            z.extend_from_slice(&cd_off.to_le_bytes());
            z.extend_from_slice(&SIG_LOC64.to_le_bytes());
            z.extend_from_slice(&0u32.to_le_bytes());
            z.extend_from_slice(&z_off.to_le_bytes());
            z.extend_from_slice(&1u32.to_le_bytes());
            self.salida.write_all(&z)?;
        }

        let mut e = Vec::with_capacity(EOCD_MIN);
        e.extend_from_slice(&SIG_EOCD.to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        e.extend_from_slice(&(n.min(0xFFFF) as u16).to_le_bytes());
        e.extend_from_slice(&(n.min(0xFFFF) as u16).to_le_bytes());
        e.extend_from_slice(&(cd_tam.min(0xFFFF_FFFF) as u32).to_le_bytes());
        e.extend_from_slice(&(cd_off.min(0xFFFF_FFFF) as u32).to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        self.salida.write_all(&e)?;
        self.salida.flush()?;
        Ok(self.salida)
    }
}

struct Contador<W: Write> {
    inner: W,
    escritos: u64,
}

impl<W: Write> Contador<W> {
    fn new(inner: W) -> Self {
        Contador { inner, escritos: 0 }
    }
}

impl<W: Write> Write for Contador<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.escritos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn comprobar_nombre(nombre: &str) -> Result<Vec<u8>> {
    let b = nombre.as_bytes().to_vec();
    if b.len() > u16::MAX as usize {
        return Err(Error::Limit("nombre demasiado largo para ZIP".into()));
    }
    Ok(b)
}

fn metodo_de(c: Codec) -> Method {
    match c {
        Codec::Store => Method::Store,
        Codec::Deflate => Method::Deflate,
        Codec::Zstd => Method::Zstd,
    }
}

#[cfg(feature = "codecs-native")]
fn hilos_disponibles() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1)
}

pub fn comprimir_bloque(datos: &[u8], codec: Codec, nivel: Level) -> Result<(Vec<u8>, Method, u32)> {
    let crc = arca_core::crc32(datos);
    match codec {
        Codec::Store => Ok((datos.to_vec(), Method::Store, crc)),
        Codec::Deflate => {
            let mut e = DeflateEncoder::new(Vec::new(), Compression::new(nivel.a_flate2()));
            e.write_all(datos)?;
            Ok((e.finish()?, Method::Deflate, crc))
        }
        Codec::Zstd => {
            #[cfg(feature = "codecs-native")]
            {
                let c = zstd::bulk::compress(datos, nivel.a_zstd()).map_err(Error::Io)?;
                Ok((c, Method::Zstd, crc))
            }
            #[cfg(not(feature = "codecs-native"))]
            {
                Err(Error::Unsupported("este binario se compilo sin Zstandard".into()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;

    fn ida_y_vuelta(codec: Codec, nivel: Level) {
        let contenido = b"Arca. ".repeat(5000);
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        w.add("dir/fichero.txt", &contenido[..], codec, nivel, Some(1_700_000_000)).unwrap();
        let buf = w.finish().unwrap().into_inner();

        let mut a = ZipArchive::open(IoCursor::new(buf)).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a.entries()[0].name, "dir/fichero.txt");
        assert_eq!(a.entries()[0].size, contenido.len() as u64);
        let mut salida = Vec::new();
        a.extract_to(0, &mut salida).unwrap();
        assert_eq!(salida, contenido);
    }

    #[test]
    fn ida_y_vuelta_deflate() {
        ida_y_vuelta(Codec::Deflate, Level::Normal);
    }

    #[cfg(feature = "codecs-native")]
    #[test]
    fn ida_y_vuelta_zstd() {
        ida_y_vuelta(Codec::Zstd, Level::Normal);
    }

    #[test]
    fn ida_y_vuelta_store() {
        ida_y_vuelta(Codec::Store, Level::Store);
    }

    #[test]
    fn crc_corrupto_se_detecta() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        w.add("a.txt", &b"contenido original"[..], Codec::Store, Level::Store, None).unwrap();
        let mut buf = w.finish().unwrap().into_inner();
        let p = LFH_FIJO + "a.txt".len() + EXTRA_Z64 + 3;
        buf[p] ^= 0xFF;
        let mut a = ZipArchive::open(IoCursor::new(buf)).unwrap();
        let r = a.extract_to(0, &mut Vec::new());
        assert!(matches!(r, Err(Error::Integrity { .. })), "{r:?}");
    }

    #[test]
    fn basura_no_hace_panic() {
        for semilla in 0u32..2000 {
            let n = (semilla as usize % 300) + 1;
            let datos: Vec<u8> = (0..n)
                .map(|i| ((semilla.wrapping_mul(2_654_435_761) >> (i % 24)) & 0xFF) as u8)
                .collect();
            let _ = ZipArchive::open(IoCursor::new(datos));
        }
    }

    #[test]
    fn eocd_declarando_directorio_gigante() {
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
        assert!(r.is_err(), "deberia rechazar el directorio imposible");
    }

    #[test]
    fn muchas_entradas() {
        let mut w = ZipWriter::new(IoCursor::new(Vec::new()));
        for i in 0..500 {
            w.add(&format!("f{i:04}.txt"), &b"x"[..], Codec::Deflate, Level::Normal, None).unwrap();
        }
        let buf = w.finish().unwrap().into_inner();
        let a = ZipArchive::open(IoCursor::new(buf)).unwrap();
        assert_eq!(a.len(), 500);
        assert_eq!(a.entries()[499].name, "f0499.txt");
    }

    fn cabecera_central(flags: u16, nombre: &[u8]) -> Vec<u8> {
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
        b.extend_from_slice(&(nombre.len() as u16).to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(nombre);
        b
    }

    #[test]
    fn nombre_sin_bit_utf8_se_lee_como_cp437() {
        let crudo = b"caf\x82.txt";

        let b = cabecera_central(0, crudo);
        let e = leer_cabecera_central(&mut Cursor::new(&b)).unwrap();
        assert_eq!(e.name, "caf\u{00E9}.txt");

        let b = cabecera_central(0x800, crudo);
        let e = leer_cabecera_central(&mut Cursor::new(&b)).unwrap();
        assert_eq!(e.name, "caf\u{FFFD}.txt");
    }

    #[test]
    fn cp437_traga_los_256_bytes_sin_perder_nada() {
        let todos: Vec<u8> = (0..=255u8).collect();
        let s = de_cp437(&todos);
        assert_eq!(s.chars().count(), 256, "cada byte debe dar un caracter");
        assert!(
            !s.contains('\u{FFFD}'),
            "CP437 no tiene huecos: no deberia salir ningun reemplazo"
        );
    }

    #[test]
    fn cabecera_central_con_nombre_basura_no_hace_panic() {
        for semilla in 0u32..200 {
            let n = (semilla as usize % 40) + 1;
            let nombre: Vec<u8> = (0..n)
                .map(|i| ((semilla.wrapping_mul(2_654_435_761) >> (i % 24)) & 0xFF) as u8)
                .collect();
            for flags in [0u16, 0x800] {
                let b = cabecera_central(flags, &nombre);
                let _ = leer_cabecera_central(&mut Cursor::new(&b));
                for corte in 0..b.len() {
                    let _ = leer_cabecera_central(&mut Cursor::new(&b[..corte]));
                }
            }
        }
    }
}
