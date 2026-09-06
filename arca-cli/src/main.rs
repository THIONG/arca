use arca_core::{Codec, Error, Level, Result};
use arca_tar::{TarReader, TarWriter};
use arca_zip::{comprimir_bloque, ZipArchive, ZipWriter};
use rayon::prelude::*;
use clap::{Parser, Subcommand, ValueEnum};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const BUF: usize = 256 * 1024;

type Bloque = (usize, Vec<u8>, arca_core::Method, u32);

#[derive(Parser)]
#[command(
    name = "arca",
    version,
    about = "Archivador rapido y seguro",
    long_about = "Arca comprime y extrae archivos. Los parsers estan escritos en safe Rust:\nun archivo malformado produce un error, nunca corrupcion de memoria."
)]
struct Cli {
    #[command(subcommand)]
    orden: Orden,
}

#[derive(Subcommand)]
enum Orden {
    #[command(visible_alias = "c")]
    Create {
        salida: PathBuf,
        #[arg(required = true)]
        entradas: Vec<PathBuf>,
        #[arg(short, long, value_enum, default_value_t = Nivel::Normal)]
        nivel: Nivel,
        #[arg(short, long, value_enum, default_value_t = Compresor::Auto)]
        codec: Compresor,
        #[arg(short = 'j', long, default_value_t = 0)]
        hilos: usize,
    },
    #[command(visible_alias = "l")]
    List {
        archivo: PathBuf,
        #[arg(short, long)]
        tiempo: bool,
    },
    #[command(visible_alias = "x")]
    Extract {
        archivo: PathBuf,
        #[arg(short = 'o', long, default_value = ".")]
        destino: PathBuf,
    },
    #[command(visible_alias = "t")]
    Test { archivo: PathBuf },
    Bench { archivo: PathBuf },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Compresor {
    Auto,
    Store,
    Deflate,
    Zstd,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Nivel {
    Store,
    Fast,
    Normal,
    Best,
}

impl From<Nivel> for Level {
    fn from(n: Nivel) -> Level {
        match n {
            Nivel::Store => Level::Store,
            Nivel::Fast => Level::Fast,
            Nivel::Normal => Level::Normal,
            Nivel::Best => Level::Best,
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Formato {
    Zip,
    Tar,
    TarGz,
}

fn detectar(p: &Path) -> Result<Formato> {
    let n = p.to_string_lossy().to_ascii_lowercase();
    if n.ends_with(".zip") {
        Ok(Formato::Zip)
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Ok(Formato::TarGz)
    } else if n.ends_with(".tar") {
        Ok(Formato::Tar)
    } else {
        Err(Error::Unsupported(format!(
            "no reconozco la extension de «{}» (se admiten .zip, .tar, .tar.gz)",
            p.display()
        )))
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = ejecutar(cli) {
        eprintln!("arca: {e}");
        std::process::exit(1);
    }
}

fn ejecutar(cli: Cli) -> Result<()> {
    match cli.orden {
        Orden::Create { salida, entradas, nivel, codec, hilos } => {
            crear(&salida, &entradas, nivel.into(), codec, hilos)
        }
        Orden::List { archivo, tiempo } => listar(&archivo, tiempo),
        Orden::Extract { archivo, destino } => extraer(&archivo, &destino),
        Orden::Test { archivo } => probar(&archivo),
        Orden::Bench { archivo } => bench(&archivo),
    }
}

fn recolectar(entradas: &[PathBuf]) -> Result<Vec<(PathBuf, String)>> {
    let mut v = Vec::new();
    for e in entradas {
        let base = e.parent().unwrap_or(Path::new(""));
        recorrer(e, base, &mut v)?;
    }
    Ok(v)
}

fn recorrer(p: &Path, base: &Path, salida: &mut Vec<(PathBuf, String)>) -> Result<()> {
    let meta = fs::symlink_metadata(p)?;
    let rel = p.strip_prefix(base).unwrap_or(p);
    let nombre = rel.to_string_lossy().replace('\\', "/");
    if meta.is_dir() {
        let mut hijos: Vec<_> = fs::read_dir(p)?.collect::<io::Result<Vec<_>>>()?;
        hijos.sort_by_key(|d| d.file_name());
        for h in hijos {
            recorrer(&h.path(), base, salida)?;
        }
    } else if meta.is_file() {
        salida.push((p.to_path_buf(), nombre));
    }
    Ok(())
}

fn mtime_de(m: &fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const EN_VUELO_POR_HILO: u64 = 32 * 1024 * 1024;

fn resolver_hilos(pedidos: usize) -> usize {
    if pedidos > 0 {
        return pedidos;
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

fn resolver_codec(c: Compresor, formato: Formato) -> Codec {
    match c {
        Compresor::Store => Codec::Store,
        Compresor::Deflate => Codec::Deflate,
        Compresor::Zstd => Codec::Zstd,
        Compresor::Auto => match formato {
            Formato::Zip => Codec::Deflate,
            _ => Codec::Deflate,
        },
    }
}

fn lotes(ficheros: &[(PathBuf, String, u64, i64)], tope: u64) -> Vec<Vec<usize>> {
    let mut v = Vec::new();
    let mut actual: Vec<usize> = Vec::new();
    let mut suma = 0u64;
    for (i, f) in ficheros.iter().enumerate() {
        if f.2 > tope {
            if !actual.is_empty() {
                v.push(std::mem::take(&mut actual));
                suma = 0;
            }
            v.push(vec![i]);
            continue;
        }
        if suma + f.2 > tope && !actual.is_empty() {
            v.push(std::mem::take(&mut actual));
            suma = 0;
        }
        actual.push(i);
        suma += f.2;
    }
    if !actual.is_empty() {
        v.push(actual);
    }
    v
}

fn crear(
    salida: &Path,
    entradas: &[PathBuf],
    nivel: Level,
    compresor: Compresor,
    hilos_pedidos: usize,
) -> Result<()> {
    let formato = detectar(salida)?;
    let codec = resolver_codec(compresor, formato);
    let hilos = resolver_hilos(hilos_pedidos);
    let brutos = recolectar(entradas)?;
    if brutos.is_empty() {
        return Err(Error::Format("no hay ficheros que anadir".into()));
    }

    let mut ficheros: Vec<(PathBuf, String, u64, i64)> = Vec::with_capacity(brutos.len());
    let mut total = 0u64;
    for (ruta, nombre) in brutos {
        let m = fs::metadata(&ruta)?;
        total += m.len();
        ficheros.push((ruta, nombre, m.len(), mtime_de(&m)));
    }

    let t0 = Instant::now();
    match formato {
        Formato::Zip => {
            let f = File::create(salida)?;
            let mut w = ZipWriter::new(BufWriter::with_capacity(BUF, f));
            let tope = EN_VUELO_POR_HILO * hilos as u64;
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(hilos)
                .build()
                .map_err(|e| Error::Format(format!("no se pudo crear el pool de hilos: {e}")))?;

            for lote in lotes(&ficheros, tope) {
                if lote.len() == 1 && ficheros[lote[0]].2 > tope {
                    let (ruta, nombre, _, mt) = &ficheros[lote[0]];
                    let entrada = BufReader::with_capacity(BUF, File::open(ruta)?);
                    w.add(nombre, entrada, codec, nivel, Some(*mt))?;
                    continue;
                }
                let hechos: Vec<Result<Bloque>> = pool.install(|| {
                    lote.par_iter()
                        .map(|&i| {
                            let datos = fs::read(&ficheros[i].0)?;
                            let (c, m, crc) = comprimir_bloque(&datos, codec, nivel)?;
                            Ok((i, c, m, crc))
                        })
                        .collect()
                });
                for h in hechos {
                    let (i, c, m, crc) = h?;
                    let (_, nombre, tam, mt) = &ficheros[i];
                    w.add_comprimido(nombre, &c, crc, *tam, m, Some(*mt))?;
                }
            }
            w.finish()?;
        }
        Formato::Tar | Formato::TarGz => {
            let f = BufWriter::with_capacity(BUF, File::create(salida)?);
            let destino: Box<dyn Write> = if formato == Formato::TarGz {
                Box::new(flate2::write::GzEncoder::new(
                    f,
                    flate2::Compression::new(nivel.a_flate2()),
                ))
            } else {
                Box::new(f)
            };
            let mut w = TarWriter::new(destino);
            for (ruta, nombre, tam, mt) in &ficheros {
                let entrada = BufReader::with_capacity(BUF, File::open(ruta)?);
                w.add(nombre, *tam, *mt, 0o644, entrada)?;
            }
            let mut d = w.finish()?;
            d.flush()?;
        }
    }

    let dt = t0.elapsed();
    let final_tam = fs::metadata(salida)?.len();
    let ratio = if total > 0 { 100.0 * (1.0 - final_tam as f64 / total as f64) } else { 0.0 };
    let mbs = if dt.as_secs_f64() > 0.0 {
        total as f64 / 1_048_576.0 / dt.as_secs_f64()
    } else {
        0.0
    };
    println!(
        "{}: {} ficheros, {} -> {} ({:.1} % menos) en {:.3} s · {:.0} MB/s · {} hilos",
        salida.display(),
        ficheros.len(),
        humano(total),
        humano(final_tam),
        ratio,
        dt.as_secs_f64(),
        mbs,
        hilos
    );
    Ok(())
}

fn listar(archivo: &Path, tiempo: bool) -> Result<()> {
    let t0 = Instant::now();
    let formato = detectar(archivo)?;
    let mut n = 0u64;
    let mut bytes = 0u64;

    let mut salida = BufWriter::new(io::stdout().lock());
    match formato {
        Formato::Zip => {
            let a = ZipArchive::open(File::open(archivo)?)?;
            for e in a.entries() {
                writeln!(
                    salida,
                    "{:>12}  {:>7}  {:>5.1}%  {}",
                    e.size,
                    e.method.nombre(),
                    e.ratio() * 100.0,
                    e.name
                )?;
                n += 1;
                bytes += e.size;
            }
        }
        Formato::Tar | Formato::TarGz => {
            let f = BufReader::with_capacity(BUF, File::open(archivo)?);
            let fuente: Box<dyn Read> = if formato == Formato::TarGz {
                Box::new(flate2::read::GzDecoder::new(f))
            } else {
                Box::new(f)
            };
            let mut r = TarReader::new(fuente);
            while let Some(e) = r.next_entry()? {
                writeln!(salida, "{:>12}  {:>7}  {:>5}   {}", e.entry.size, "store", "", e.entry.name)?;
                n += 1;
                bytes += e.entry.size;
                r.saltar_datos(&e)?;
            }
        }
    }
    salida.flush()?;

    if tiempo {
        eprintln!(
            "{n} entradas, {} sin comprimir, listado en {:.1} ms",
            humano(bytes),
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

fn extraer(archivo: &Path, destino: &Path) -> Result<()> {
    let formato = detectar(archivo)?;
    fs::create_dir_all(destino)?;
    let t0 = Instant::now();
    let mut n = 0u64;
    let mut bytes = 0u64;

    match formato {
        Formato::Zip => {
            let mut a = ZipArchive::open(File::open(archivo)?)?;
            let total = a.len();
            for i in 0..total {
                let e = a.entries()[i].clone();
                if e.is_dir {
                    fs::create_dir_all(destino.join(arca_core::nombre_seguro(&e.name)?))?;
                    continue;
                }
                let ruta = destino.join(arca_core::nombre_seguro(&e.name)?);
                if let Some(p) = ruta.parent() {
                    fs::create_dir_all(p)?;
                }
                let f = BufWriter::with_capacity(BUF, File::create(&ruta)?);
                bytes += a.extract_to(i, f)?;
                n += 1;
            }
        }
        Formato::Tar | Formato::TarGz => {
            let f = BufReader::with_capacity(BUF, File::open(archivo)?);
            let fuente: Box<dyn Read> = if formato == Formato::TarGz {
                Box::new(flate2::read::GzDecoder::new(f))
            } else {
                Box::new(f)
            };
            let mut r = TarReader::new(fuente);
            while let Some(e) = r.next_entry()? {
                let ruta = destino.join(arca_core::nombre_seguro(&e.entry.name)?);
                if e.entry.is_dir {
                    fs::create_dir_all(&ruta)?;
                    r.saltar_datos(&e)?;
                    continue;
                }
                if let Some(p) = ruta.parent() {
                    fs::create_dir_all(p)?;
                }
                let mut w = BufWriter::with_capacity(BUF, File::create(&ruta)?);
                bytes += r.copiar_datos(&e, &mut w)?;
                w.flush()?;
                n += 1;
            }
        }
    }

    println!(
        "{n} ficheros, {} escritos en {:.3} s",
        humano(bytes),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

fn probar(archivo: &Path) -> Result<()> {
    let formato = detectar(archivo)?;
    let t0 = Instant::now();
    let mut n = 0u64;
    let mut fallos = 0u64;

    match formato {
        Formato::Zip => {
            let mut a = ZipArchive::open(File::open(archivo)?)?;
            for i in 0..a.len() {
                if a.entries()[i].is_dir {
                    continue;
                }
                let nombre = a.entries()[i].name.clone();
                match a.extract_to(i, io::sink()) {
                    Ok(_) => n += 1,
                    Err(e) => {
                        eprintln!("  FALLO  {nombre}: {e}");
                        fallos += 1;
                    }
                }
            }
        }
        Formato::Tar | Formato::TarGz => {
            let f = BufReader::with_capacity(BUF, File::open(archivo)?);
            let fuente: Box<dyn Read> = if formato == Formato::TarGz {
                Box::new(flate2::read::GzDecoder::new(f))
            } else {
                Box::new(f)
            };
            let mut r = TarReader::new(fuente);
            while let Some(e) = r.next_entry()? {
                r.saltar_datos(&e)?;
                n += 1;
            }
        }
    }

    if fallos > 0 {
        return Err(Error::Format(format!("{fallos} entradas corruptas de {}", n + fallos)));
    }
    println!("{n} entradas verificadas, sin errores ({:.3} s)", t0.elapsed().as_secs_f64());
    Ok(())
}

fn bench(archivo: &Path) -> Result<()> {
    println!("Requisitos de rendimiento (documento de diseno, seccion 05)\n");

    let mut mejor = f64::MAX;
    let mut entradas = 0usize;
    for _ in 0..5 {
        let t = Instant::now();
        let a = ZipArchive::open(File::open(archivo)?)?;
        entradas = a.len();
        let d = t.elapsed().as_secs_f64() * 1000.0;
        if d < mejor {
            mejor = d;
        }
    }
    let tam = fs::metadata(archivo)?.len();
    let r2 = mejor < 200.0;
    println!("  R2  listar sin descomprimir");
    println!("      {} entradas de un archivo de {}", entradas, humano(tam));
    println!("      {:.1} ms   objetivo < 200 ms   {}", mejor, si_no(r2));
    println!();
    println!("  R1  arranque en frio: se mide desde fuera, con hyperfine");
    println!("      hyperfine --warmup 20 'arca --version'");
    Ok(())
}

fn si_no(ok: bool) -> &'static str {
    if ok {
        "CUMPLE"
    } else {
        "NO CUMPLE"
    }
}

fn humano(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
