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
    Integrity { name: String, esperado: u32, obtenido: u32 },
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "error de E/S: {e}"),
            Error::Format(m) => write!(f, "archivo mal formado: {m}"),
            Error::Limit(m) => write!(f, "limite excedido: {m}"),
            Error::Integrity { name, esperado, obtenido } => write!(
                f,
                "fallo de integridad en «{name}»: CRC esperado {esperado:08x}, obtenido {obtenido:08x}"
            ),
            Error::Unsupported(m) => write!(f, "no soportado: {m}"),
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
    pub fn nombre(self) -> &'static str {
        match self {
            Method::Store => "store",
            Method::Deflate => "deflate",
            Method::Zstd => "zstd",
        }
    }

    pub fn codigo(self) -> u16 {
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
    pub fn a_flate2(self) -> u32 {
        match self {
            Level::Store => 0,
            Level::Fast => 1,
            Level::Normal => 6,
            Level::Best => 9,
        }
    }

    pub fn a_zstd(self) -> i32 {
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
    datos: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(datos: &'a [u8]) -> Self {
        Cursor { datos, pos: 0 }
    }

    pub fn en(datos: &'a [u8], pos: usize) -> Result<Self> {
        if pos > datos.len() {
            return Err(Error::Format(format!(
                "desplazamiento {pos} fuera del archivo ({} bytes)",
                datos.len()
            )));
        }
        Ok(Cursor { datos, pos })
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn restantes(&self) -> usize {
        self.datos.len().saturating_sub(self.pos)
    }

    fn tomar(&mut self, n: usize, que: &str) -> Result<&'a [u8]> {
        let fin = self.pos.checked_add(n).ok_or_else(|| {
            Error::Format(format!("desbordamiento al leer {que}"))
        })?;
        if fin > self.datos.len() {
            return Err(Error::Format(format!(
                "archivo truncado al leer {que}: faltan {} bytes",
                fin - self.datos.len()
            )));
        }
        let s = &self.datos[self.pos..fin];
        self.pos = fin;
        Ok(s)
    }

    pub fn u16le(&mut self, que: &str) -> Result<u16> {
        let b = self.tomar(2, que)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32le(&mut self, que: &str) -> Result<u32> {
        let b = self.tomar(4, que)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64le(&mut self, que: &str) -> Result<u64> {
        let b = self.tomar(8, que)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn bytes(&mut self, n: usize, que: &str) -> Result<&'a [u8]> {
        self.tomar(n, que)
    }

    pub fn saltar(&mut self, n: usize, que: &str) -> Result<()> {
        self.tomar(n, que).map(|_| ())
    }
}

pub fn crc32(datos: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(datos);
    h.finalize()
}

pub fn dos_a_unix(fecha: u16, hora: u16) -> Option<i64> {
    let anyo = 1980i64 + ((fecha >> 9) & 0x7f) as i64;
    let mes = ((fecha >> 5) & 0x0f) as i64;
    let dia = (fecha & 0x1f) as i64;
    let h = ((hora >> 11) & 0x1f) as i64;
    let m = ((hora >> 5) & 0x3f) as i64;
    let s = ((hora & 0x1f) * 2) as i64;
    if !(1..=12).contains(&mes) || !(1..=31).contains(&dia) || h > 23 || m > 59 || s > 59 {
        return None;
    }
    let y = if mes <= 2 { anyo - 1 } else { anyo };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mes + 9) % 12;
    let doy = (153 * mp + 2) / 5 + dia - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let dias = era * 146_097 + doe - 719_468;
    Some(dias * 86_400 + h * 3600 + m * 60 + s)
}

pub fn unix_a_dos(ts: i64) -> (u16, u16) {
    let dias = ts.div_euclid(86_400);
    let resto = ts.rem_euclid(86_400);
    let z = dias + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let dia = doy - (153 * mp + 2) / 5 + 1;
    let mes = if mp < 10 { mp + 3 } else { mp - 9 };
    let anyo = if mes <= 2 { y + 1 } else { y };
    let anyo_dos = (anyo - 1980).clamp(0, 127);
    let fecha = ((anyo_dos as u16) << 9) | ((mes as u16) << 5) | dia as u16;
    let hora = (((resto / 3600) as u16) << 11)
        | ((((resto % 3600) / 60) as u16) << 5)
        | (((resto % 60) / 2) as u16);
    (fecha, hora)
}

pub fn nombre_seguro(nombre: &str) -> Result<std::path::PathBuf> {
    if nombre.len() > limits::MAX_NAME {
        return Err(Error::Limit(format!("nombre de {} bytes", nombre.len())));
    }
    if nombre.contains('\0') {
        return Err(Error::Format("nombre con byte nulo".into()));
    }
    let normalizado = nombre.replace('\\', "/");
    let mut salida = std::path::PathBuf::new();
    for parte in normalizado.split('/') {
        match parte {
            "" | "." => continue,
            ".." => {
                return Err(Error::Format(format!(
                    "la entrada «{nombre}» intenta salir del directorio de destino"
                )))
            }
            p => {
                if p.len() >= 2 && p.as_bytes()[1] == b':' {
                    return Err(Error::Format(format!(
                        "la entrada «{nombre}» lleva letra de unidad"
                    )));
                }
                salida.push(p);
            }
        }
    }
    if salida.as_os_str().is_empty() {
        return Err(Error::Format(format!("nombre vacio: «{nombre}»")));
    }
    Ok(salida)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_trunca_sin_panic() {
        let d = [1u8, 2, 3];
        let mut c = Cursor::new(&d);
        assert!(c.u16le("x").is_ok());
        assert!(matches!(c.u32le("y"), Err(Error::Format(_))));
    }

    #[test]
    fn cursor_rechaza_offset_fuera() {
        let d = [0u8; 4];
        assert!(Cursor::en(&d, 99).is_err());
    }

    #[test]
    fn zip_slip_rechazado() {
        assert!(nombre_seguro("../../etc/passwd").is_err());
        assert!(nombre_seguro("..\\..\\windows\\system32").is_err());
        assert!(nombre_seguro("C:/tmp/x").is_err());
        assert!(nombre_seguro("a/b/c.txt").is_ok());
        assert_eq!(
            nombre_seguro("./a//b.txt").unwrap(),
            std::path::PathBuf::from("a/b.txt")
        );
    }

    #[test]
    fn fechas_dos_ida_y_vuelta() {
        let ts = 1_710_498_600i64;
        let (f, h) = unix_a_dos(ts);
        let vuelta = dos_a_unix(f, h).unwrap();
        assert!((vuelta - ts).abs() <= 2, "{vuelta} vs {ts}");
    }
}
