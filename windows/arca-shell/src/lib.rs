use std::ffi::c_void;
use std::path::PathBuf;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;

const CLSID_ARCA: GUID = GUID::from_u128(0xe075ad96_f5bd_4bff_8c33_a29d05352efa);

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

    fn aplica(self, rutas: &[PathBuf]) -> bool {
        match self {
            Accion::ComprimirZip => !rutas.is_empty(),
            _ => rutas.iter().any(|p| es_archivo_comprimido(p)),
        }
    }
}

fn es_archivo_comprimido(p: &std::path::Path) -> bool {
    let n = p.to_string_lossy().to_ascii_lowercase();
    [".zip", ".tar", ".tar.gz", ".tgz"]
        .iter()
        .any(|e| n.ends_with(e))
}

fn a_pwstr(s: PCWSTR) -> Result<PWSTR> {
    unsafe { SHStrDupW(s) }
}

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
            CoTaskMemFree(Some(nombre.0 as *const c_void));
        }
    }
    v
}

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

#[implement(IExplorerCommand)]
struct Comando(Accion);

impl IExplorerCommand_Impl for Comando_Impl {
    fn GetTitle(&self, _items: Option<&IShellItemArray>) -> Result<PWSTR> {
        a_pwstr(self.0.titulo())
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
        Ok(if self.0.aplica(&rutas) {
            ECS_ENABLED.0 as u32
        } else {
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

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
