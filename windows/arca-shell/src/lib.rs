//! Extensión del menú contextual del Explorador de Windows.
//!
//! # Aviso
//!
//! Este es el único crate de Arca que usa `unsafe`, y es inevitable: COM exige
//! punteros crudos y convenciones de llamada de C. Está aislado a propósito.
//! **Aquí no se parsea ningún archivo**: esta DLL solo recoge las rutas que el
//! usuario ha seleccionado y lanza `arca.exe`. Todo el trabajo con datos de
//! origen desconocido ocurre en el proceso hijo, en los crates que sí prohíben
//! `unsafe`. Si esta extensión tuviera un fallo de memoria, no sería explotable
//! con un archivo malicioso, porque nunca lo abre.
//!
//! # Cómo encaja en Windows
//!
//! Windows 11 usa `IExplorerCommand`, registrado mediante un paquete MSIX
//! disperso. Windows 10 usa el mecanismo antiguo, `IContextMenu` con la DLL
//! registrada en el registro; se añadirá en un segundo módulo.

use std::ffi::c_void;
use std::path::PathBuf;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;

/// CLSID de la extensión. Debe coincidir, letra por letra, con el que aparece
/// en `AppxManifest.xml`. Genera el tuyo propio antes de publicar nada.
const CLSID_ARCA: GUID = GUID::from_u128(0xe075ad96_f5bd_4bff_8c33_a29d05352efa);

/// Acciones que ofrece el menú.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Accion {
    ExtraerAqui,
    ExtraerACarpeta,
    ComprimirZip,
}

impl Accion {
    fn titulo(self) -> PCWSTR {
        match self {
            Accion::ExtraerAqui => w!("Extraer aquí"),
            Accion::ExtraerACarpeta => w!("Extraer a una carpeta nueva"),
            Accion::ComprimirZip => w!("Comprimir a .zip con Arca"),
        }
    }

    /// ¿Tiene sentido esta acción para lo que hay seleccionado?
    fn aplica(self, rutas: &[PathBuf]) -> bool {
        match self {
            Accion::ComprimirZip => !rutas.is_empty(),
            _ => rutas.iter().any(|p| es_archivo_comprimido(p)),
        }
    }
}

/// Solo las extensiones que `arca.exe` sabe abrir de verdad. Ofrecer «Extraer
/// aquí» sobre un .7z o un .rar seria una trampa: se lanza sin ventana de
/// consola, asi que el fallo del proceso hijo no lo veria nadie. Cuando se
/// implementen esos formatos, se añaden aqui y en `detectar()` del CLI.
fn es_archivo_comprimido(p: &std::path::Path) -> bool {
    let n = p.to_string_lossy().to_ascii_lowercase();
    [".zip", ".tar", ".tar.gz", ".tgz"]
        .iter()
        .any(|e| n.ends_with(e))
}

/// Copia una cadena al montón de COM, que es quien la liberará.
fn a_pwstr(s: PCWSTR) -> Result<PWSTR> {
    unsafe { SHStrDupW(s) }
}

/// Extrae las rutas seleccionadas del array que entrega el Explorador.
fn rutas_de(items: Option<&IShellItemArray>) -> Vec<PathBuf> {
    let Some(items) = items else {
        return Vec::new();
    };
    let mut v = Vec::new();
    unsafe {
        let Ok(n) = items.GetCount() else {
            return v;
        };
        for i in 0..n {
            let Ok(item) = items.GetItemAt(i) else { continue };
            let Ok(nombre) = item.GetDisplayName(SIGDN_FILESYSPATH) else { continue };
            if let Ok(s) = nombre.to_string() {
                v.push(PathBuf::from(s));
            }
            // GetDisplayName reserva con CoTaskMemAlloc y nos cede la
            // propiedad: si no liberamos, el Explorador acumula fugas.
            CoTaskMemFree(Some(nombre.0 as *const c_void));
        }
    }
    v
}

/// Ruta a `arca.exe`, buscado junto a esta DLL.
fn ruta_del_binario() -> Result<PathBuf> {
    use windows::Win32::System::LibraryLoader::*;
    let mut buf = [0u16; 32_768];
    unsafe {
        let mut modulo = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(ruta_del_binario as *const u16),
            &mut modulo,
        )?;
        let n = GetModuleFileNameW(modulo, &mut buf) as usize;
        if n == 0 || n >= buf.len() {
            return Err(E_FAIL.into());
        }
        let dll = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        Ok(dll.with_file_name("arca.exe"))
    }
}

/// Lanza `arca.exe` sin ventana de consola y sin esperar a que termine:
/// el Explorador no puede quedarse bloqueado (requisito R5).
fn lanzar(accion: Accion, rutas: &[PathBuf]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = ruta_del_binario()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.creation_flags(CREATE_NO_WINDOW);

    match accion {
        Accion::ExtraerAqui => {
            for r in rutas.iter().filter(|p| es_archivo_comprimido(p)) {
                let destino = r.parent().map(PathBuf::from).unwrap_or_default();
                let mut c = std::process::Command::new(ruta_del_binario()?);
                c.creation_flags(CREATE_NO_WINDOW)
                    .arg("extract")
                    .arg(r)
                    .arg("-o")
                    .arg(destino);
                let _ = c.spawn().map_err(|_| Error::from(E_FAIL))?;
            }
            return Ok(());
        }
        Accion::ExtraerACarpeta => {
            for r in rutas.iter().filter(|p| es_archivo_comprimido(p)) {
                let carpeta = r.with_extension("");
                let mut c = std::process::Command::new(ruta_del_binario()?);
                c.creation_flags(CREATE_NO_WINDOW)
                    .arg("extract")
                    .arg(r)
                    .arg("-o")
                    .arg(carpeta);
                let _ = c.spawn().map_err(|_| Error::from(E_FAIL))?;
            }
            return Ok(());
        }
        Accion::ComprimirZip => {
            let Some(primero) = rutas.first() else {
                return Ok(());
            };
            let salida = primero.with_extension("zip");
            cmd.arg("create").arg(salida);
            for r in rutas {
                cmd.arg(r);
            }
        }
    }

    cmd.spawn().map_err(|_| Error::from(E_FAIL))?;
    Ok(())
}

// ---------------------------------------------------------------- comandos

#[implement(IExplorerCommand)]
struct Comando(Accion);

impl IExplorerCommand_Impl for Comando_Impl {
    fn GetTitle(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        a_pwstr(self.0.titulo())
    }

    fn GetIcon(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        // El icono sale de arca.exe; el índice 0 es el principal.
        let exe = ruta_del_binario()?;
        let s: Vec<u16> = format!("{},0", exe.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        a_pwstr(PCWSTR(s.as_ptr()))
    }

    fn GetToolTip(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        // Devolver E_NOTIMPL es lo correcto: le dice al Explorador que use
        // el título, en vez de mostrar un tooltip vacío.
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(GUID::zeroed())
    }

    fn GetState(&self, items: Option<&IShellItemArray>, _ocultar: BOOL) -> Result<u32> {
        let rutas = rutas_de(items);
        Ok(if self.0.aplica(&rutas) {
            ECS_ENABLED.0 as u32
        } else {
            // Oculto, no deshabilitado: un menú lleno de opciones en gris
            // es peor que un menú corto.
            ECS_HIDDEN.0 as u32
        })
    }

    fn Invoke(&self, items: Option<&IShellItemArray>, _ctx: Option<&IBindCtx>) -> Result<()> {
        let rutas = rutas_de(items);
        if rutas.is_empty() {
            return Ok(());
        }
        lanzar(self.0, &rutas)
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

/// Entrada raíz: un submenú «Arca» que agrupa las tres acciones.
#[implement(IExplorerCommand)]
struct Raiz;

impl IExplorerCommand_Impl for Raiz_Impl {
    fn GetTitle(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        a_pwstr(w!("Arca"))
    }

    fn GetIcon(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        let exe = ruta_del_binario()?;
        let s: Vec<u16> = format!("{},0", exe.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        a_pwstr(PCWSTR(s.as_ptr()))
    }

    fn GetToolTip(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(GUID::zeroed())
    }

    fn GetState(&self, items: Option<&IShellItemArray>, _ocultar: BOOL) -> Result<u32> {
        let rutas = rutas_de(items);
        Ok(if rutas.is_empty() {
            ECS_HIDDEN.0 as u32
        } else {
            ECS_ENABLED.0 as u32
        })
    }

    fn Invoke(&self, _items: Option<&IShellItemArray>, _ctx: Option<&IBindCtx>) -> Result<()> {
        // Una entrada con submenú no se invoca directamente.
        Ok(())
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_HASSUBCOMMANDS.0 as u32)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        let hijos: Vec<IExplorerCommand> = vec![
            Comando(Accion::ExtraerAqui).into(),
            Comando(Accion::ExtraerACarpeta).into(),
            Comando(Accion::ComprimirZip).into(),
        ];
        Ok(Enumerador::new(hijos).into())
    }
}

/// Enumerador de subcomandos. COM no acepta un `Vec`, quiere un `IEnum*`.
#[implement(IEnumExplorerCommand)]
struct Enumerador {
    items: Vec<IExplorerCommand>,
    pos: std::cell::Cell<usize>,
}

impl Enumerador {
    fn new(items: Vec<IExplorerCommand>) -> Self {
        Enumerador { items, pos: std::cell::Cell::new(0) }
    }
}

impl IEnumExplorerCommand_Impl for Enumerador_Impl {
    fn Next(
        &self,
        pedidos: u32,
        salida: *mut Option<IExplorerCommand>,
        entregados: *mut u32,
    ) -> HRESULT {
        let mut n = 0u32;
        unsafe {
            while n < pedidos && self.pos.get() < self.items.len() {
                let item = self.items[self.pos.get()].clone();
                *salida.add(n as usize) = Some(item);
                self.pos.set(self.pos.get() + 1);
                n += 1;
            }
            if !entregados.is_null() {
                *entregados = n;
            }
        }
        if n == pedidos {
            S_OK
        } else {
            S_FALSE
        }
    }

    // En `windows` 0.58 estas dos devuelven Result<()>, no HRESULT: el vtable
    // generado ya hace `.into()` sobre lo que retornan. Solo Next devuelve
    // HRESULT, porque ahi S_FALSE es un valor legitimo y no un error.
    fn Skip(&self, cuantos: u32) -> Result<()> {
        self.pos.set((self.pos.get() + cuantos as usize).min(self.items.len()));
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        self.pos.set(0);
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumExplorerCommand> {
        let copia = Enumerador::new(self.items.clone());
        copia.pos.set(self.pos.get());
        Ok(copia.into())
    }
}

// ------------------------------------------------------------ fábrica COM

#[implement(IClassFactory)]
struct Fabrica;

impl IClassFactory_Impl for Fabrica_Impl {
    fn CreateInstance(
        &self,
        exterior: Option<&IUnknown>,
        iid: *const GUID,
        objeto: *mut *mut c_void,
    ) -> Result<()> {
        if exterior.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if objeto.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *objeto = std::ptr::null_mut();
            let raiz: IExplorerCommand = Raiz.into();
            raiz.query(iid, objeto).ok()
        }
    }

    fn LockServer(&self, _bloquear: BOOL) -> Result<()> {
        Ok(())
    }
}

// -------------------------------------------------------- exportaciones DLL

/// Punto de entrada que llama el Explorador para construir el objeto.
#[no_mangle]
pub extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    iid: *const GUID,
    objeto: *mut *mut c_void,
) -> HRESULT {
    if clsid.is_null() || iid.is_null() || objeto.is_null() {
        return E_POINTER;
    }
    unsafe {
        *objeto = std::ptr::null_mut();
        if *clsid != CLSID_ARCA {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let fabrica: IClassFactory = Fabrica.into();
        fabrica.query(iid, objeto)
    }
}

/// Devolvemos S_FALSE siempre: es más seguro que el Explorador mantenga la DLL
/// cargada que arriesgarse a descargarla con objetos vivos.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
