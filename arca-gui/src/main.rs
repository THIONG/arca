#![forbid(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use arca_core::{Codec, Entry, Level};
use arca_tar::{TarReader, TarWriter};
use arca_zip::{ZipArchive, ZipWriter};
use eframe::egui;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

const BUF: usize = 256 * 1024;
const ALTO_FILA: f32 = 22.0;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Formato {
    Zip,
    Tar,
    TarGz,
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
                r.saltar_datos(&e)?;
            }
            Ok(v)
        }
    }
}

fn escribir_entrada(destino: &Path, nombre: &str, es_dir: bool) -> arca_core::Result<Option<PathBuf>> {
    let ruta = destino.join(arca_core::nombre_seguro(nombre)?);
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
                if let Some(ruta) = escribir_entrada(destino, &e.name, e.is_dir)? {
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
                    r.saltar_datos(&e)?;
                    i += 1;
                    continue;
                }
                match escribir_entrada(destino, &e.entry.name, e.entry.is_dir)? {
                    Some(ruta) => {
                        let mut w = BufWriter::with_capacity(BUF, File::create(&ruta)?);
                        bytes += r.copiar_datos(&e, &mut w)?;
                        w.flush()?;
                    }
                    None => r.saltar_datos(&e)?,
                }
                i += 1;
            }
            avisar(total, total);
        }
    }
    Ok(bytes)
}

fn recolectar(entradas: &[PathBuf]) -> std::io::Result<Vec<(PathBuf, String)>> {
    fn recorrer(
        p: &Path,
        base: &Path,
        salida: &mut Vec<(PathBuf, String)>,
    ) -> std::io::Result<()> {
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
    avisar: &dyn Fn(usize, usize),
) -> arca_core::Result<u64> {
    let ficheros = recolectar(entradas)?;
    let total = ficheros.len();
    let mut bytes = 0u64;
    let es_tar = detectar(salida) != Some(Formato::Zip);

    if es_tar {
        let mut w = TarWriter::new(BufWriter::with_capacity(BUF, File::create(salida)?));
        for (i, (ruta, nombre)) in ficheros.iter().enumerate() {
            avisar(i, total);
            let meta = fs::metadata(ruta)?;
            let f = BufReader::with_capacity(BUF, File::open(ruta)?);
            w.add(nombre, meta.len(), 0, 0o644, f)?;
            bytes += meta.len();
        }
        w.finish()?;
    } else {
        let mut w = ZipWriter::new(BufWriter::with_capacity(BUF, File::create(salida)?));
        for (i, (ruta, nombre)) in ficheros.iter().enumerate() {
            avisar(i, total);
            let meta = fs::metadata(ruta)?;
            let f = BufReader::with_capacity(BUF, File::open(ruta)?);
            w.add(nombre, f, Codec::Deflate, Level::Normal, None)?;
            bytes += meta.len();
        }
        w.finish()?;
    }
    avisar(total, total);
    Ok(bytes)
}

struct Arca {
    archivo: Option<PathBuf>,
    entradas: Vec<Entry>,
    marcadas: Vec<bool>,
    filtro: String,
    estado: Estado,
    canal: Option<Receiver<Mensaje>>,
    aviso: String,
    error: bool,
}

impl Default for Arca {
    fn default() -> Self {
        Arca {
            archivo: None,
            entradas: Vec::new(),
            marcadas: Vec::new(),
            filtro: String::new(),
            estado: Estado::Libre,
            canal: None,
            aviso: "Arrastra un archivo aquí, o pulsa «Abrir»".into(),
            error: false,
        }
    }
}

impl Arca {
    fn ocupado(&self) -> bool {
        matches!(self.estado, Estado::Trabajando { .. })
    }

    fn visibles(&self) -> Vec<usize> {
        if self.filtro.is_empty() {
            return (0..self.entradas.len()).collect();
        }
        let f = self.filtro.to_lowercase();
        (0..self.entradas.len())
            .filter(|&i| self.entradas[i].name.to_lowercase().contains(&f))
            .collect()
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
        self.lanzar(ctx, 0, move |tx| match listar(&ruta) {
            Ok(v) => {
                let _ = tx.send(Mensaje::Listado(ruta, v));
                ctx2.request_repaint();
            }
            Err(e) => {
                let _ = tx.send(Mensaje::Fallo(e.to_string()));
                ctx2.request_repaint();
            }
        });
    }

    fn recibir(&mut self) {
        let Some(rx) = &self.canal else { return };
        let mut cerrar = false;
        while let Ok(m) = rx.try_recv() {
            match m {
                Mensaje::Listado(ruta, v) => {
                    self.aviso = if v.len() == 1 {
                        "1 entrada".to_string()
                    } else {
                        format!("{} entradas", v.len())
                    };
                    self.marcadas = vec![true; v.len()];
                    self.entradas = v;
                    self.archivo = Some(ruta);
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
        let Some(destino) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
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
            let r = extraer(&archivo, &destino, &quiere, &avisar);
            let _ = tx.send(match r {
                Ok(bytes) => Mensaje::Hecho(format!("Extraído: {}", humano(bytes))),
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
        let sugerido = entradas[0]
            .file_stem()
            .map(|s| format!("{}.zip", s.to_string_lossy()))
            .unwrap_or_else(|| "archivo.zip".into());
        let Some(salida) = rfd::FileDialog::new()
            .set_file_name(sugerido)
            .add_filter("ZIP", &["zip"])
            .add_filter("TAR", &["tar"])
            .save_file()
        else {
            return;
        };
        let ctx2 = ctx.clone();
        self.lanzar(ctx, entradas.len(), move |tx| {
            let avisar = |i: usize, n: usize| {
                let _ = tx.send(Mensaje::Progreso(i, n));
                ctx2.request_repaint();
            };
            let r = comprimir(&salida, &entradas, &avisar);
            let _ = tx.send(match r {
                Ok(bytes) => Mensaje::Hecho(format!("Comprimido: {} de origen", humano(bytes))),
                Err(e) => Mensaje::Fallo(e.to_string()),
            });
        });
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
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!self.ocupado(), |ui| {
                    if ui.button("Abrir…").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("Archivos", &["zip", "tar", "gz", "tgz"])
                            .pick_file()
                        {
                            self.abrir(ctx, p);
                        }
                    }
                    if ui.button("Comprimir…").clicked() {
                        self.pedir_comprimir(ctx);
                    }
                    ui.separator();
                    let hay = self.archivo.is_some();
                    if ui.add_enabled(hay, egui::Button::new("Extraer todo")).clicked() {
                        self.pedir_extraer(ctx, false);
                    }
                    let marcadas = self.marcadas.iter().filter(|b| **b).count();
                    if ui
                        .add_enabled(hay && marcadas > 0, egui::Button::new("Extraer selección"))
                        .clicked()
                    {
                        self.pedir_extraer(ctx, true);
                    }
                });
                ui.separator();
                ui.label("Filtro:");
                ui.add(egui::TextEdit::singleline(&mut self.filtro).desired_width(160.0));
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("estado").show(ctx, |ui| {
            ui.add_space(4.0);
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
                        egui::Color32::from_rgb(220, 80, 80)
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.colored_label(color, &self.aviso);
                }
            }
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.entradas.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("Arrastra aquí un .zip, .tar o .tar.gz");
                });
                return;
            }

            let visibles = self.visibles();
            ui.horizontal(|ui| {
                if ui.button("Marcar todo").clicked() {
                    for i in &visibles {
                        self.marcadas[*i] = true;
                    }
                }
                if ui.button("Desmarcar todo").clicked() {
                    for i in &visibles {
                        self.marcadas[*i] = false;
                    }
                }
                ui.label(format!("{} de {}", visibles.len(), self.entradas.len()));
            });
            ui.separator();

            egui::ScrollArea::vertical().show_rows(ui, ALTO_FILA, visibles.len(), |ui, rango| {
                for fila in rango {
                    let i = visibles[fila];
                    let e = &self.entradas[i];
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.marcadas[i], "");
                        ui.add_sized(
                            [90.0, ALTO_FILA],
                            egui::Label::new(humano(e.size)).halign(egui::Align::RIGHT),
                        );
                        ui.add_sized(
                            [70.0, ALTO_FILA],
                            egui::Label::new(e.method.nombre()),
                        );
                        ui.add_sized(
                            [60.0, ALTO_FILA],
                            egui::Label::new(format!("{:.0}%", e.ratio() * 100.0))
                                .halign(egui::Align::RIGHT),
                        );
                        ui.label(&e.name);
                    });
                }
            });
        });
    }
}

fn main() -> eframe::Result<()> {
    let inicial = std::env::args().nth(1).map(PathBuf::from);
    let opciones = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 620.0])
            .with_min_inner_size([560.0, 320.0])
            .with_title("Arca"),
        ..Default::default()
    };
    eframe::run_native(
        "Arca",
        opciones,
        Box::new(move |cc| {
            let mut app = Arca::default();
            if let Some(p) = inicial {
                app.abrir(&cc.egui_ctx, p);
            }
            Ok(Box::new(app))
        }),
    )
}
