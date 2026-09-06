# F02 · Menú contextual de Windows

## Aviso: este código no está compilado

Todo lo demás en Arca se entregó verificado —14 pruebas, 20 comprobaciones de
interoperabilidad, mediciones reales—. **Esto no.** Se escribió en un contenedor
Linux sin acceso al target de Windows, así que nadie lo ha compilado todavía.

Espera errores de compilación la primera vez. El punto flaco previsible es la
versión del crate `windows`: las firmas de `IExplorerCommand_Impl` y la macro
`#[implement]` cambian entre versiones menores. Está pinado a `0.58`; si lo subes,
revisa las firmas antes de tocar nada más.

Ese es exactamente el motivo por el que F02 iba antes que la interfaz gráfica en
el plan: es la parte que no se puede validar sin la máquina.

## Qué hay aquí

| Fichero | Qué es |
|---|---|
| `arca-shell/src/lib.rs` | La DLL: `IExplorerCommand` con submenú de tres acciones |
| `AppxManifest.xml` | Paquete MSIX disperso que da identidad a la extensión |
| `construir.ps1` | Compila, empaqueta y registra en modo desarrollo |

## Cómo se prueba

Necesitas Windows 11 (build 22000 o superior), Rust con
`rustup target add x86_64-pc-windows-msvc`, Visual Studio Build Tools y el
Windows SDK. Además, **Modo de desarrollador activado** en Configuración, o
`Add-AppxPackage -Register` fallará.

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

Si los seis pasan, F02 está superada y el riesgo alto del proyecto queda cerrado.

## Por qué esta DLL sí usa `unsafe`

Es el único crate de Arca que lo permite, y no hay alternativa: COM exige punteros
crudos y convenciones de llamada de C.

Lo importante es que **aquí no se parsea ningún archivo**. Esta DLL recoge las
rutas seleccionadas y lanza `arca.exe`; todo el trabajo con datos de origen
desconocido ocurre en el proceso hijo, en los crates que prohíben `unsafe`. Aunque
esta extensión tuviera un fallo de memoria, no sería explotable con un archivo
malicioso, porque nunca llega a abrirlo.

## Lo que falta

- **Windows 10** usa el mecanismo antiguo, `IContextMenu` registrado en el
  registro. Son dos implementaciones distintas y hay que escribir las dos.
- **Iconos de verdad**: `construir.ps1` genera PNG de 1×1 para que el paquete
  valide.
- **Firma**: para distribuirlo hace falta `makeappx` + `signtool` con un
  certificado. Sin él, SmartScreen avisa a todo el que lo descargue.
- **GUID propio**: el CLSID del código es de ejemplo. Genera el tuyo con
  `[guid]::NewGuid()` y cámbialo en los dos sitios, `lib.rs` y `AppxManifest.xml`.
