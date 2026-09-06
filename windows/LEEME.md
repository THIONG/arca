# F02 · Menú contextual de Windows

## Estado: compilado y verificado

Esto se escribió en un contenedor Linux y durante un tiempo nadie lo compiló.
Ya no: compila, se registra y las seis comprobaciones de abajo pasan en
Windows 11 build 26200, con MSVC 14.44 y Windows SDK 10.0.26100.

El aviso que había aquí anunciaba que el punto flaco sería la versión del crate
`windows`. **No lo era.** El pin a `0.58` era el correcto y las ocho firmas de
`IExplorerCommand_Impl` cuadraban tal cual. Lo que fallaba era otra cosa, y
queda anotado por si vuelve a morder:

1. Faltaba la feature `implement` del crate `windows`. Los traits `*_Impl` viven
   en un `impl.rs` que solo se incluye tras `#[cfg(feature = "implement")]`, así
   que sin ella no existen: cuatro `cannot find trait in this scope`.
2. El trait va sobre el tipo que genera la macro (`Comando_Impl`), no sobre el
   original. El vtable se construye con `Vtbl::new::<Self, OFFSET>()` dentro de
   `impl Comando_Impl`, de modo que el bound recae en el tipo generado. El
   ejemplo de la documentación de `#[implement]` dice lo contrario, pero es un
   bloque `rust,ignore` que no se compila y está obsoleto. Los cuerpos no
   cambian: la macro genera `Deref` hacia el original.
3. `IEnumExplorerCommand_Impl::Skip` y `::Reset` devuelven `Result<()>`, no
   `HRESULT`; el vtable generado ya hace `.into()`. Solo `Next` devuelve
   `HRESULT`, porque ahí `S_FALSE` es un valor legítimo y no un error.
4. `AppxManifest.xml` no declaraba `uap10:AllowExternalContent`, sin lo cual un
   paquete disperso con `-ExternalLocation` falla con `0x80073D2E`. Este no se ve
   compilando: solo aparece al intentar registrar.

Si algún día subes la versión del crate, ese es el orden en que conviene mirar.

## Qué hay aquí

| Fichero | Qué es |
|---|---|
| `arca-shell/src/lib.rs` | La DLL: `IExplorerCommand` y `IContextMenu`, con submenú de tres acciones |
| `AppxManifest.xml` | Paquete MSIX disperso que da identidad a la extensión |
| `construir.ps1` | Compila, empaqueta, registra los dos menús |

## Dos menús, dos interfaces, dos CLSID

Windows 11 tiene dos menús contextuales y **no comparten mecanismo**:

- El **moderno**, el que sale al hacer clic derecho, usa `IExplorerCommand` y se
  registra declarándolo en el `AppxManifest.xml` del paquete MSIX.
- El **clásico**, el de «Mostrar más opciones» —y el único que existe en
  Windows 10—, usa `IContextMenu` + `IShellExtInit` y **no se puede declarar en
  el manifiesto**: va por registro. `construir.ps1` escribe las claves en
  `HKCU\Software\Classes`, que no necesita permisos de administrador.

Son dos objetos COM distintos, con su propio CLSID, en la misma DLL.
`DllGetClassObject` reparte según cuál le pidan.

### La trampa de `QueryContextMenu`

El contrato clásico de `IContextMenu::QueryContextMenu` es devolver el número de
elementos añadidos codificado en el HRESULT, con `MAKE_HRESULT(SEVERITY_SUCCESS,
0, n)`. Pero en `windows` 0.58 el trait declara `-> Result<()>`, y el vtable
generado convierte `Ok(())` en `HRESULT(0)`, que significaría «no he añadido
nada» y dejaría el menú roto.

La salida es que `From<Result<T>> for HRESULT` devuelve el código del `Err` tal
cual, y `nonzero_hresult` solo sustituye el cero. Así que la forma de devolver
«he puesto tres entradas» es:

```rust
Err(Error::from(HRESULT(3)))
```

Parece un error y no lo es: `HRESULT(3)` tiene el bit de severidad a cero, o sea
éxito. Se ve raro; el código no lleva comentarios por convención del proyecto, y
por eso queda explicado aquí.

## Cómo se prueba

Necesitas Windows 11 (build 22000 o superior), Rust con
`rustup target add x86_64-pc-windows-msvc`, Visual Studio Build Tools y el
Windows SDK. Además, **Modo de desarrollador activado**, o `Add-AppxPackage
-Register` fallará con `0x80073CFF`, que menciona una licencia de desarrollador
sin decir dónde se activa. En las builds recientes está en **Sistema › Opciones
avanzadas › Para programadores**; se llega directo con `start ms-settings:developers`.

Si no tienes las Build Tools, `winget install Microsoft.VisualStudio.2022.BuildTools`
con `--override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`
trae el enlazador y el SDK. Sin ellas no compila **nada** del proyecto, ni
siquiera las pruebas del núcleo: `rustc` no tiene con qué enlazar. Y ojo si lanzas
`cargo` desde Git Bash, porque el `link` de coreutils que hay en `/usr/bin`
eclipsa al `link.exe` de MSVC y el error que sale no lo aparenta.

```powershell
cd windows
.\construir.ps1
Stop-Process -Name explorer -Force
```

Y la lista de comprobación:

1. Clic derecho sobre un `.zip` → aparece **Arca** con tres opciones
2. Clic derecho sobre un `.txt` → **no** aparece «Extraer aquí»
3. «Extraer aquí» descomprime junto al archivo, sin ventana de consola
4. Seleccionar varios ficheros → «Comprimir a .zip» los mete todos
5. El Explorador no se congela ni un instante (requisito R5)
6. `.\construir.ps1 -Quitar` lo deja todo como estaba

Y las del menú clásico, que es una implementación aparte:

7. «Mostrar más opciones» sobre un `.zip` → **Arca** con las tres
8. «Mostrar más opciones» sobre un `.txt` → **Arca** solo con «Comprimir»
9. `-Quitar` borra también las claves de `HKCU\Software\Classes`

Los seis pasan. Las cuatro primeras se comprobaron además conduciendo el objeto
COM a mano —cargando la DLL y llamando a `GetState` e `Invoke` como haría el
Explorador—, que es lo que permite decir *qué* falla y no solo que «no sale el
menú». `GetState`, que es la llamada que el Explorador hace al construir el
menú, tarda **3–23 µs**; el presupuesto de R5 son 16 ms.

`Invoke` tardaba **5,0 ms**, que es lo que cuesta un `CreateProcess`, y
`lanzar()` los encadenaba en serie: con cuatro `.zip` seleccionados eran unos
20 ms, por encima de R5. Ahora `lanzar()` deja el trabajo en un hilo aparte y
devuelve en **68 µs**, sin depender de cuántos ficheros haya seleccionados.

## Por qué esta DLL sí usa `unsafe`

Es el único crate de Arca que lo permite, y no hay alternativa: COM exige punteros
crudos y convenciones de llamada de C.

Lo importante es que **aquí no se parsea ningún archivo**. Esta DLL recoge las
rutas seleccionadas y lanza `arca.exe`; todo el trabajo con datos de origen
desconocido ocurre en el proceso hijo, en los crates que prohíben `unsafe`. Aunque
esta extensión tuviera un fallo de memoria, no sería explotable con un archivo
malicioso, porque nunca llega a abrirlo.

## Lo que falta

- **Windows 10 sin probar**: el `IContextMenu` que necesita ya está escrito y
  funciona en el menú clásico de Windows 11, que es el mismo mecanismo. Pero
  nadie lo ha ejecutado en un Windows 10 de verdad.
- **Iconos de verdad**: `construir.ps1` genera PNG de 1×1 para que el paquete
  valide. El menú clásico tampoco pone icono en sus entradas; haría falta
  `MENUITEMINFOW` con un bitmap.
- **`GetCommandString` devuelve `E_NOTIMPL`**: el Explorador se queda sin texto
  de ayuda en la barra de estado para las entradas del menú clásico.
- **Firma**: para distribuirlo hace falta `makeappx` + `signtool` con un
  certificado. Sin él, SmartScreen avisa a todo el que lo descargue. El
  `Publisher` del manifiesto sigue siendo `CN=CAMBIAME` y debe coincidir letra
  por letra con el asunto del certificado.
- **Formatos**: `es_archivo_comprimido` solo ofrece las extensiones que el CLI
  sabe abrir (`.zip`, `.tar`, `.tar.gz`, `.tgz`). Al añadir 7z o rar hay que
  tocar esa lista y `detectar()` del CLI a la vez, o el menú ofrecerá algo que
  falla en silencio: el proceso hijo se lanza sin ventana de consola.
- **R5 con selecciones grandes**: `lanzar()` encadena un `CreateProcess` por
  archivo, ~5 ms cada uno. Conviene agrupar antes de que alguien seleccione diez.
