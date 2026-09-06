#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use arca_core::{Codec, Entry, Level};
use arca_tar::{TarReader, TarWriter};
use arca_zip::{ZipArchive, ZipWriter};
use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

const BUF: usize = 256 * 1024;
const ALTO_FILA: f32 = 20.0;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Formato {
    Zip,
    Tar,
    TarGz,
}

impl Formato {
    fn extension(self) -> &'static str {
        match self {
            Formato::Zip => "zip",
            Formato::Tar => "tar",
            Formato::TarGz => "tar.gz",
        }
    }

    fn etiqueta(self) -> &'static str {
        match self {
            Formato::Zip => "ZIP",
            Formato::Tar => "TAR",
            Formato::TarGz => "TAR.GZ",
        }
    }
}

fn detectar(p: &Path) -> Option<Formato> {
    let n = p.to_string_lossy().to_ascii_lowercase();
    if n.ends_with(".zip") {
        Some(Formato::Zip)
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some(Formato::TarGz)
    } else if n.ends_with(".tar") {
        Some(Formato::Tar)
    } else {
        None
    }
}

fn humano(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Columna {
    Nombre,
    Tamano,
    Comprimido,
    Metodo,
    Ratio,
}

enum Mensaje {
    Listado(PathBuf, Vec<Entry>),
    Progreso(usize, usize),
    Hecho(String),
    Fallo(String),
}

enum Estado {
    Libre,
    Trabajando { hechas: usize, total: usize },
}

fn abrir_fuente(archivo: &Path, formato: Formato) -> std::io::Result<Box<dyn Read>> {
    let f = BufReader::with_capacity(BUF, File::open(archivo)?);
    Ok(match formato {
        Formato::TarGz => Box::new(flate2::read::GzDecoder::new(f)),
        _ => Box::new(f),
    })
}

fn listar(archivo: &Path) -> arca_core::Result<Vec<Entry>> {
    let Some(formato) = detectar(archivo) else {
        return Err(arca_core::Error::Unsupported(format!(
            "no reconozco la extension de «{}»",
            archivo.display()
        )));
    };
    match formato {
        Formato::Zip => Ok(ZipArchive::open(File::open(archivo)?)?.entries().to_vec()),
        _ => {
            let mut r = TarReader::new(abrir_fuente(archivo, formato)?);
            let mut v = Vec::new();
            while let Some(e) = r.next_entry()? {
                v.push(e.entry.clone());
                r.skip_data(&e)?;
            }
            Ok(v)
        }
    }
}

fn ruta_destino(destino: &Path, nombre: &str, es_dir: bool) -> arca_core::Result<Option<PathBuf>> {
    let ruta = destino.join(arca_core::safe_name(nombre)?);
    if es_dir {
        fs::create_dir_all(&ruta)?;
        return Ok(None);
    }
    if let Some(p) = ruta.parent() {
        fs::create_dir_all(p)?;
    }
    Ok(Some(ruta))
}

fn extraer(
    archivo: &Path,
    destino: &Path,
    quiere: &[bool],
    avisar: &dyn Fn(usize, usize),
) -> arca_core::Result<u64> {
    let Some(formato) = detectar(archivo) else {
        return Err(arca_core::Error::Unsupported("formato desconocido".into()));
    };
    fs::create_dir_all(destino)?;
    let mut bytes = 0u64;

    match formato {
        Formato::Zip => {
            let mut a = ZipArchive::open(File::open(archivo)?)?;
            let total = a.len();
            for i in 0..total {
                avisar(i, total);
                if !quiere.get(i).copied().unwrap_or(true) {
                    continue;
                }
                let e = a.entries()[i].clone();
                if let Some(ruta) = ruta_destino(destino, &e.name, e.is_dir)? {
                    let f = BufWriter::with_capacity(BUF, File::create(&ruta)?);
                    bytes += a.extract_to(i, f)?;
                }
            }
            avisar(total, total);
        }
        _ => {
            let mut r = TarReader::new(abrir_fuente(archivo, formato)?);
            let total = quiere.len();
            let mut i = 0usize;
            while let Some(e) = r.next_entry()? {
                avisar(i, total);
                if !quiere.get(i).copied().unwrap_or(true) {
                    r.skip_data(&e)?;
                    i += 1;
                    continue;
                }
                match ruta_destino(destino, &e.entry.name, e.entry.is_dir)? {
                    Some(ruta) => {
                        let mut w = BufWriter::with_capacity(BUF, File::create(&ruta)?);
                        bytes += r.copy_data(&e, &mut w)?;
                        w.flush()?;
                    }
                    None => r.skip_data(&e)?,
                }
                i += 1;
            }
            avisar(total, total);
        }
    }
    Ok(bytes)
}

fn probar(archivo: &Path, avisar: &dyn Fn(usize, usize)) -> arca_core::Result<(usize, Vec<String>)> {
    let Some(formato) = detectar(archivo) else {
        return Err(arca_core::Error::Unsupported("formato desconocido".into()));
    };
    let mut bien = 0usize;
    let mut malas = Vec::new();

    match formato {
        Formato::Zip => {
            let mut a = ZipArchive::open(File::open(archivo)?)?;
            let total = a.len();
            for i in 0..total {
                avisar(i, total);
                if a.entries()[i].is_dir {
                    continue;
                }
                let nombre = a.entries()[i].name.clone();
                match a.extract_to(i, std::io::sink()) {
                    Ok(_) => bien += 1,
                    Err(e) => malas.push(format!("{nombre}: {e}")),
                }
            }
            avisar(total, total);
        }
        _ => {
            let mut r = TarReader::new(abrir_fuente(archivo, formato)?);
            let mut i = 0usize;
            while let Some(e) = r.next_entry()? {
                avisar(i, i + 1);
                if e.entry.is_dir {
                    r.skip_data(&e)?;
                } else {
                    match r.copy_data(&e, &mut std::io::sink()) {
                        Ok(_) => bien += 1,
                        Err(err) => malas.push(format!("{}: {err}", e.entry.name)),
                    }
                }
                i += 1;
            }
        }
    }
    Ok((bien, malas))
}

fn recolectar(entradas: &[PathBuf]) -> std::io::Result<Vec<(PathBuf, String)>> {
    fn recorrer(p: &Path, base: &Path, salida: &mut Vec<(PathBuf, String)>) -> std::io::Result<()> {
        let meta = fs::symlink_metadata(p)?;
        let rel = p.strip_prefix(base).unwrap_or(p);
        let nombre = rel.to_string_lossy().replace('\\', "/");
        if meta.is_dir() {
            let mut hijos: Vec<_> = fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
            hijos.sort_by_key(|d| d.file_name());
            for h in hijos {
                recorrer(&h.path(), base, salida)?;
            }
        } else if meta.is_file() {
            salida.push((p.to_path_buf(), nombre));
        }
        Ok(())
    }

    let mut v = Vec::new();
    for e in entradas {
        let base = e.parent().unwrap_or(Path::new(""));
        recorrer(e, base, &mut v)?;
    }
    Ok(v)
}

fn comprimir(
    salida: &Path,
    entradas: &[PathBuf],
    formato: Formato,
    codec: Codec,
    nivel: Level,
    avisar: &dyn Fn(usize, usize),
) -> arca_core::Result<(u64, u64)> {
    let ficheros = recolectar(entradas)?;
    let total = ficheros.len();
    let mut origen = 0u64;

    match formato {
        Formato::Zip => {
            let mut w = ZipWriter::new(BufWriter::with_capacity(BUF, File::create(salida)?));
            for (i, (ruta, nombre)) in ficheros.iter().enumerate() {
                avisar(i, total);
                let meta = fs::metadata(ruta)?;
                let f = BufReader::with_capacity(BUF, File::open(ruta)?);
                w.add(nombre, f, codec, nivel, None)?;
                origen += meta.len();
            }
            w.finish()?;
        }
        _ => {
            let bruto = BufWriter::with_capacity(BUF, File::create(salida)?);
            let destino: Box<dyn Write> = if formato == Formato::TarGz {
                Box::new(flate2::write::GzEncoder::new(
                    bruto,
                    flate2::Compression::new(nivel.to_flate2()),
                ))
            } else {
                Box::new(bruto)
            };
            let mut w = TarWriter::new(destino);
            for (i, (ruta, nombre)) in ficheros.iter().enumerate() {
                avisar(i, total);
                let meta = fs::metadata(ruta)?;
                let f = BufReader::with_capacity(BUF, File::open(ruta)?);
                w.add(nombre, meta.len(), 0, 0o644, f)?;
                origen += meta.len();
            }
            w.finish()?;
        }
    }
    avisar(total, total);
    let final_ = fs::metadata(salida).map(|m| m.len()).unwrap_or(0);
    Ok((origen, final_))
}

struct Arca {
    archivo: Option<PathBuf>,
    entradas: Vec<Entry>,
    marcadas: Vec<bool>,
    filtro: String,
    orden: (Columna, bool),
    estado: Estado,
    canal: Option<Receiver<Mensaje>>,
    aviso: String,
    error: bool,
    formato: Formato,
    codec: Codec,
    nivel: Level,
    en_subcarpeta: bool,
}

impl Default for Arca {
    fn default() -> Self {
        Arca {
            archivo: None,
            entradas: Vec::new(),
            marcadas: Vec::new(),
            filtro: String::new(),
            orden: (Columna::Nombre, true),
            estado: Estado::Libre,
            canal: None,
            aviso: "Arrastra un archivo aquí, o pulsa «Abrir»".into(),
            error: false,
            formato: Formato::Zip,
            codec: Codec::Deflate,
            nivel: Level::Normal,
            en_subcarpeta: false,
        }
    }
}

fn nombre_codec(c: Codec) -> &'static str {
    match c {
        Codec::Store => "Sin comprimir",
        Codec::Deflate => "Deflate",
        Codec::Zstd => "Zstandard",
    }
}

fn nombre_nivel(n: Level) -> &'static str {
    match n {
        Level::Store => "Ninguno",
        Level::Fast => "Rápido",
        Level::Normal => "Normal",
        Level::Best => "Máximo",
    }
}

impl Arca {
    fn ocupado(&self) -> bool {
        matches!(self.estado, Estado::Trabajando { .. })
    }

    fn resumen(&self) -> String {
        let n = self.entradas.iter().filter(|e| !e.is_dir).count();
        let sin: u64 = self.entradas.iter().map(|e| e.size).sum();
        let con: u64 = self.entradas.iter().map(|e| e.compressed_size).sum();
        let ratio = if sin == 0 {
            0.0
        } else {
            (1.0 - con as f64 / sin as f64) * 100.0
        };
        format!(
            "{n} ficheros · {} sin comprimir · {} en el archivo · {ratio:.1}% ahorrado",
            humano(sin),
            humano(con)
        )
    }

    fn visibles(&self) -> Vec<usize> {
        let f = self.filtro.to_lowercase();
        let mut v: Vec<usize> = (0..self.entradas.len())
            .filter(|&i| f.is_empty() || self.entradas[i].name.to_lowercase().contains(&f))
            .collect();
        let (col, asc) = self.orden;
        v.sort_by(|&a, &b| {
            let x = &self.entradas[a];
            let y = &self.entradas[b];
            let o = match col {
                Columna::Nombre => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                Columna::Tamano => x.size.cmp(&y.size),
                Columna::Comprimido => x.compressed_size.cmp(&y.compressed_size),
                Columna::Metodo => x.method.name().cmp(y.method.name()),
                Columna::Ratio => x
                    .ratio()
                    .partial_cmp(&y.ratio())
                    .unwrap_or(std::cmp::Ordering::Equal),
            };
            if asc {
                o
            } else {
                o.reverse()
            }
        });
        v
    }

    fn ordenar_por(&mut self, col: Columna) {
        if self.orden.0 == col {
            self.orden.1 = !self.orden.1;
        } else {
            self.orden = (col, true);
        }
    }

    fn lanzar<F>(&mut self, ctx: &egui::Context, total: usize, trabajo: F)
    where
        F: FnOnce(&Sender<Mensaje>) + Send + 'static,
    {
        let (tx, rx) = channel();
        self.canal = Some(rx);
        self.estado = Estado::Trabajando { hechas: 0, total };
        self.error = false;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            trabajo(&tx);
            ctx.request_repaint();
        });
    }

    fn abrir(&mut self, ctx: &egui::Context, ruta: PathBuf) {
        let ctx2 = ctx.clone();
        self.lanzar(ctx, 0, move |tx| {
            let m = match listar(&ruta) {
                Ok(v) => Mensaje::Listado(ruta, v),
                Err(e) => Mensaje::Fallo(e.to_string()),
            };
            let _ = tx.send(m);
            ctx2.request_repaint();
        });
    }

    fn recibir(&mut self) {
        let Some(rx) = &self.canal else { return };
        let mut cerrar = false;
        while let Ok(m) = rx.try_recv() {
            match m {
                Mensaje::Listado(ruta, v) => {
                    self.marcadas = vec![true; v.len()];
                    self.entradas = v;
                    if let Some(f) = detectar(&ruta) {
                        self.formato = f;
                    }
                    self.archivo = Some(ruta);
                    self.aviso = self.resumen();
                    self.estado = Estado::Libre;
                    cerrar = true;
                }
                Mensaje::Progreso(hechas, total) => {
                    self.estado = Estado::Trabajando { hechas, total };
                }
                Mensaje::Hecho(texto) => {
                    self.aviso = texto;
                    self.estado = Estado::Libre;
                    cerrar = true;
                }
                Mensaje::Fallo(texto) => {
                    self.aviso = texto;
                    self.error = true;
                    self.estado = Estado::Libre;
                    cerrar = true;
                }
            }
        }
        if cerrar {
            self.canal = None;
        }
    }

    fn pedir_extraer(&mut self, ctx: &egui::Context, solo_marcadas: bool) {
        let Some(archivo) = self.archivo.clone() else { return };
        let Some(mut destino) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        if self.en_subcarpeta {
            if let Some(tallo) = archivo.file_stem() {
                let limpio = tallo.to_string_lossy().replace(".tar", "");
                destino = destino.join(limpio);
            }
        }
        let quiere: Vec<bool> = if solo_marcadas {
            self.marcadas.clone()
        } else {
            vec![true; self.entradas.len()]
        };
        let total = quiere.iter().filter(|b| **b).count();
        let ctx2 = ctx.clone();
        self.lanzar(ctx, total, move |tx| {
            let avisar = |i: usize, n: usize| {
                let _ = tx.send(Mensaje::Progreso(i, n));
                ctx2.request_repaint();
            };
            let _ = tx.send(match extraer(&archivo, &destino, &quiere, &avisar) {
                Ok(bytes) => Mensaje::Hecho(format!(
                    "Extraído {} en {}",
                    humano(bytes),
                    destino.display()
                )),
                Err(e) => Mensaje::Fallo(e.to_string()),
            });
        });
    }

    fn pedir_probar(&mut self, ctx: &egui::Context) {
        let Some(archivo) = self.archivo.clone() else { return };
        let total = self.entradas.len();
        let ctx2 = ctx.clone();
        self.lanzar(ctx, total, move |tx| {
            let avisar = |i: usize, n: usize| {
                let _ = tx.send(Mensaje::Progreso(i, n));
                ctx2.request_repaint();
            };
            let _ = tx.send(match probar(&archivo, &avisar) {
                Ok((bien, malas)) if malas.is_empty() => {
                    Mensaje::Hecho(format!("{bien} entradas verificadas, sin errores"))
                }
                Ok((bien, malas)) => Mensaje::Fallo(format!(
                    "{bien} correctas, {} con errores: {}",
                    malas.len(),
                    malas.join("; ")
                )),
                Err(e) => Mensaje::Fallo(e.to_string()),
            });
        });
    }

    fn pedir_comprimir(&mut self, ctx: &egui::Context) {
        let Some(entradas) = rfd::FileDialog::new().pick_files() else {
            return;
        };
        if entradas.is_empty() {
            return;
        }
        let formato = self.formato;
        let ext = formato.extension();
        let sugerido = entradas[0]
            .file_stem()
            .map(|s| format!("{}.{ext}", s.to_string_lossy()))
            .unwrap_or_else(|| format!("archivo.{ext}"));
        let Some(salida) = rfd::FileDialog::new()
            .set_file_name(sugerido)
            .add_filter(formato.etiqueta(), &[ext])
            .save_file()
        else {
            return;
        };
        let codec = self.codec;
        let nivel = self.nivel;
        let ctx2 = ctx.clone();
        self.lanzar(ctx, entradas.len(), move |tx| {
            let avisar = |i: usize, n: usize| {
                let _ = tx.send(Mensaje::Progreso(i, n));
                ctx2.request_repaint();
            };
            let r = comprimir(&salida, &entradas, formato, codec, nivel, &avisar);
            let _ = tx.send(match r {
                Ok((origen, final_)) => {
                    let ahorro = if origen == 0 {
                        0.0
                    } else {
                        (1.0 - final_ as f64 / origen as f64) * 100.0
                    };
                    Mensaje::Hecho(format!(
                        "Creado {}: {} → {} ({ahorro:.1}% ahorrado)",
                        salida.display(),
                        humano(origen),
                        humano(final_)
                    ))
                }
                Err(e) => Mensaje::Fallo(e.to_string()),
            });
        });
    }

    fn barra(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.ocupado(), |ui| {
                if ui.button("  Abrir…  ").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Archivos comprimidos", &["zip", "tar", "gz", "tgz"])
                        .pick_file()
                    {
                        self.abrir(ctx, p);
                    }
                }
                if ui.button("  Comprimir…  ").clicked() {
                    self.pedir_comprimir(ctx);
                }
                ui.separator();
                let hay = self.archivo.is_some();
                if ui.add_enabled(hay, egui::Button::new("  Extraer todo  ")).clicked() {
                    self.pedir_extraer(ctx, false);
                }
                let marcadas = self.marcadas.iter().filter(|b| **b).count();
                if ui
                    .add_enabled(hay && marcadas > 0, egui::Button::new("  Extraer selección  "))
                    .clicked()
                {
                    self.pedir_extraer(ctx, true);
                }
                if ui.add_enabled(hay, egui::Button::new("  Probar  ")).clicked() {
                    self.pedir_probar(ctx);
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.filtro)
                        .hint_text("filtrar por nombre")
                        .desired_width(180.0),
                );
            });
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.ocupado(), |ui| {
                ui.label("Formato:");
                egui::ComboBox::from_id_salt("formato")
                    .selected_text(self.formato.etiqueta())
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        for f in [Formato::Zip, Formato::Tar, Formato::TarGz] {
                            ui.selectable_value(&mut self.formato, f, f.etiqueta());
                        }
                    });

                ui.add_space(8.0);
                ui.label("Compresor:");
                let habilita_codec = self.formato == Formato::Zip;
                ui.add_enabled_ui(habilita_codec, |ui| {
                    egui::ComboBox::from_id_salt("codec")
                        .selected_text(nombre_codec(self.codec))
                        .width(130.0)
                        .show_ui(ui, |ui| {
                            for c in [Codec::Store, Codec::Deflate, Codec::Zstd] {
                                ui.selectable_value(&mut self.codec, c, nombre_codec(c));
                            }
                        });
                });

                ui.add_space(8.0);
                ui.label("Nivel:");
                egui::ComboBox::from_id_salt("nivel")
                    .selected_text(nombre_nivel(self.nivel))
                    .width(100.0)
                    .show_ui(ui, |ui| {
                        for n in [Level::Store, Level::Fast, Level::Normal, Level::Best] {
                            ui.selectable_value(&mut self.nivel, n, nombre_nivel(n));
                        }
                    });

                ui.add_space(12.0);
                ui.checkbox(&mut self.en_subcarpeta, "Extraer a subcarpeta");
            });
        });
        ui.add_space(6.0);
    }

    fn tabla(&mut self, ui: &mut egui::Ui) {
        let visibles = self.visibles();

        ui.horizontal(|ui| {
            if ui.small_button("Marcar todo").clicked() {
                for i in &visibles {
                    self.marcadas[*i] = true;
                }
            }
            if ui.small_button("Desmarcar todo").clicked() {
                for i in &visibles {
                    self.marcadas[*i] = false;
                }
            }
            ui.separator();
            let marcadas = self.marcadas.iter().filter(|b| **b).count();
            ui.label(format!(
                "{} visibles de {} · {marcadas} marcadas",
                visibles.len(),
                self.entradas.len()
            ));
        });
        ui.add_space(4.0);

        let mut pedido: Option<Columna> = None;
        let orden = self.orden;
        let cabecera = |ui: &mut egui::Ui, texto: &str, col: Columna| -> bool {
            let flecha = if orden.0 != col {
                ""
            } else if orden.1 {
                " ^"
            } else {
                " v"
            };
            ui.add(
                egui::Label::new(egui::RichText::new(format!("{texto}{flecha}")).strong())
                    .sense(egui::Sense::click()),
            )
            .on_hover_text("Ordenar por esta columna")
            .clicked()
        };

        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(26.0))
            .column(Column::initial(90.0).at_least(70.0))
            .column(Column::initial(90.0).at_least(70.0))
            .column(Column::initial(80.0).at_least(60.0))
            .column(Column::initial(60.0).at_least(50.0))
            .column(Column::remainder().at_least(120.0))
            .header(22.0, |mut cab| {
                cab.col(|_| {});
                cab.col(|ui| {
                    if cabecera(ui, "Tamaño", Columna::Tamano) {
                        pedido = Some(Columna::Tamano);
                    }
                });
                cab.col(|ui| {
                    if cabecera(ui, "Comprimido", Columna::Comprimido) {
                        pedido = Some(Columna::Comprimido);
                    }
                });
                cab.col(|ui| {
                    if cabecera(ui, "Método", Columna::Metodo) {
                        pedido = Some(Columna::Metodo);
                    }
                });
                cab.col(|ui| {
                    if cabecera(ui, "Ahorro", Columna::Ratio) {
                        pedido = Some(Columna::Ratio);
                    }
                });
                cab.col(|ui| {
                    if cabecera(ui, "Nombre", Columna::Nombre) {
                        pedido = Some(Columna::Nombre);
                    }
                });
            })
            .body(|cuerpo| {
                cuerpo.rows(ALTO_FILA, visibles.len(), |mut fila| {
                    let i = visibles[fila.index()];
                    let e = self.entradas[i].clone();
                    fila.col(|ui| {
                        ui.checkbox(&mut self.marcadas[i], "");
                    });
                    fila.col(|ui| {
                        ui.monospace(humano(e.size));
                    });
                    fila.col(|ui| {
                        ui.monospace(humano(e.compressed_size));
                    });
                    fila.col(|ui| {
                        ui.label(e.method.name());
                    });
                    fila.col(|ui| {
                        let pct = e.ratio() * 100.0;
                        let redondeado = if pct.abs() < 0.5 { 0.0 } else { pct };
                        ui.monospace(format!("{redondeado:.0}%"));
                    });
                    fila.col(|ui| {
                        if e.is_dir {
                            ui.weak(&e.name);
                        } else {
                            ui.label(&e.name);
                        }
                    });
                });
            });

        if let Some(c) = pedido {
            self.ordenar_por(c);
        }
    }
}

impl eframe::App for Arca {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.recibir();

        let soltados: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if let Some(p) = soltados.into_iter().next() {
            if !self.ocupado() {
                self.abrir(ctx, p);
            }
        }

        egui::TopBottomPanel::top("barra").show(ctx, |ui| {
            let ctx2 = ctx.clone();
            self.barra(ui, &ctx2);
        });

        egui::TopBottomPanel::bottom("estado").show(ctx, |ui| {
            ui.add_space(5.0);
            match self.estado {
                Estado::Trabajando { hechas, total } => {
                    let f = if total == 0 {
                        0.0
                    } else {
                        hechas as f32 / total as f32
                    };
                    ui.add(
                        egui::ProgressBar::new(f)
                            .text(format!("{hechas} de {total}"))
                            .desired_width(ui.available_width()),
                    );
                }
                Estado::Libre => {
                    let color = if self.error {
                        egui::Color32::from_rgb(220, 90, 90)
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.colored_label(color, &self.aviso);
                }
            }
            ui.add_space(5.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.entradas.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("Arrastra aquí un .zip, .tar o .tar.gz")
                            .size(16.0)
                            .weak(),
                    );
                });
                return;
            }
            if let Some(a) = &self.archivo {
                ui.horizontal(|ui| {
                    ui.strong(
                        a.file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    );
                    ui.weak(a.parent().map(|p| p.display().to_string()).unwrap_or_default());
                });
                ui.add_space(2.0);
            }
            self.tabla(ui);
        });
    }
}

fn main() -> eframe::Result<()> {
    let inicial = std::env::args().nth(1).map(PathBuf::from);
    let opciones = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 660.0])
            .with_min_inner_size([620.0, 360.0])
            .with_title("Arca"),
        ..Default::default()
    };
    eframe::run_native(
        "Arca",
        opciones,
        Box::new(move |cc| {
            let mut estilo = (*cc.egui_ctx.style()).clone();
            estilo.spacing.item_spacing = egui::vec2(8.0, 6.0);
            estilo.spacing.button_padding = egui::vec2(8.0, 4.0);
            estilo.visuals.striped = true;
            cc.egui_ctx.set_style(estilo);

            let mut app = Arca::default();
            if let Some(p) = inicial {
                app.abrir(&cc.egui_ctx, p);
            }
            Ok(Box::new(app))
        }),
    )
}
