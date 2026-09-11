# Arca

Archivador multiplataforma en Rust. El objetivo doble, y en este orden: **ser el
más rápido de extremo a extremo** (del clic al archivo listo) y que **un archivo
malicioso no pueda corromper la memoria del programa**.

## La regla que gobierna las decisiones

Se midió antes de empezar. Un códec reescrito desde cero en Rust pierde entre 3×
y 12× contra su equivalente en C; en cambio zlib-rs, que es maduro, **le gana** a
la zlib de C. La conclusión no es «Rust es lento», es que **gana la implementación
madura, sea del lenguaje que sea**.

De ahí sale la frontera del proyecto:

- **Lo que toca bytes de origen desconocido va en safe Rust.** Los parsers de
  contenedor procesan un archivo que te llegó por correo. Ahí es donde 7-Zip ha
  acumulado CVEs durante 25 años y ahí está todo el valor de Arca.
- **Lo que solo hace matemática pesada puede ser C.** Un códec procesa datos que
  el parser ya validó. Insistir en Rust puro ahí cuesta un orden de magnitud de
  rendimiento a cambio de seguridad en la parte que casi nunca falla.

Elección de códec por algoritmo:

| Algoritmo | Implementación | Por qué |
|---|---|---|
| DEFLATE | `zlib-rs` vía `flate2` | Rust, y **más rápido que C** |
| Zstandard | `libzstd` (C) | No existe encoder maduro en Rust |
| LZMA2 | `liblzma` (C), pendiente | Ídem |
| LZ4 | `lz4_flex`, pendiente | Rust competitivo |

## Estructura

| Crate | Qué hace | `unsafe` |
|---|---|---|
| `arca-core` | Errores, límites, lectura acotada de cabeceras, fechas MS-DOS, defensa Zip Slip | **prohibido** |
| `arca-zip` | ZIP con Zip64; store, deflate y Zstandard | **prohibido** |
| `arca-tar` | TAR ustar con checksum | **prohibido** |
| `arca-cli` | Binario `arca` | permitido, sin usar |
| `arca-gui` | Interfaz grafica con GPUI Kit | **permitido** |
| `windows/arca-shell` | Extensión del menú contextual (COM) | necesario |

`windows/arca-shell` está **excluido del workspace** para que `cargo build` siga
funcionando en Linux y macOS.

Todo el parseo de cabeceras pasa por `arca_core::Cursor`, que comprueba límites.
Nunca se indexa un slice directamente: una cabecera truncada devuelve
`Error::Format`, jamás un panic ni una lectura fuera de rango.

## Requisitos de rendimiento

No son aspiraciones, son criterios de aceptación. Se miden en cada commit.

| | Requisito | Objetivo | Medido (2 núcleos) |
|---|---|---|---|
| R1 | Arranque en frío | < 15 ms | **1,6 ms** |
| R2 | Listar sin descomprimir | < 200 ms | **4,5 ms** con 6000 entradas |
| R3 | Escalado multihilo | ≥ 0,8 × N | **1,89×** con 2 hilos |
| R4 | Saturar el disco en modo rápido | ±20 % del soporte | sin medir |
| R5 | Ningún frame > 16 ms en la UI | — | **3–23 µs** en `GetState` del menú |
| R6 | Pico de memoria acotado | < 2 × diccionario/hilo | tope de 32 MB en vuelo por hilo |

R2 se cumple porque el lector **no carga el archivo entero**: lee la cola para
localizar el EOCD y luego solo el directorio central.

Comprimir 81 MB en 120 ficheros distintos, 2 hilos: Arca con zstd **330 ms**,
`zip -6` 3934 ms, 7-Zip 5358 ms — y el archivo de Arca es el más pequeño.

## Convenciones

- **`src/` va sin comentarios**, ni `//` ni `///`. El código se explica con los
  nombres; el porqué de las decisiones vive en este documento y en
  `windows/LEEME.md`. Los ficheros de construcción —`Cargo.toml`, `interop.sh`,
  `construir.ps1`, los workflows— sí los llevan.
- Identificadores y strings internos de `src/` **en ASCII**, sin acentos. El
  texto que ve el usuario —menús, botones, avisos— sí lleva acentos: escribir
  «Extraccion» en un botón es español mal escrito.
- La documentación en Markdown lleva acentos siempre.
- Cada cambio en un parser necesita una prueba que le meta basura y compruebe
  que no hay panic.
- `cargo clippy --workspace --all-targets -- -D warnings` debe pasar limpio.
- Nunca publicar un número de rendimiento sin el comando que lo reproduce.

## Comprobar que nada se ha roto

```sh
cargo test --workspace              # 17 pruebas
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --no-default-features   # perfil Rust puro
bash interop.sh                     # 20 comprobaciones verificadas por hash
```

`interop.sh` es el criterio de aceptación real: comprueba con SHA-256 que
`unzip`, `zip`, `tar` y 7-Zip leen lo que escribe Arca y viceversa, que un byte
alterado se detecta por CRC, y que una entrada con `../../` se rechaza.

## Publicar una versión

```sh
git tag v0.1.0 && git push origin v0.1.0
```

`.github/workflows/release.yml` compila para Linux, Windows y macOS —Intel y
Apple Silicon—, ejecuta las pruebas en cada plataforma antes de empaquetar,
genera el SHA-256 de cada archivo y crea el Release. La etiqueta tiene que
coincidir con `version` de `Cargo.toml` o el flujo se para: un Release que
miente sobre lo que contiene es peor que no tenerlo.

Los binarios van **sin firmar**, así que SmartScreen y Gatekeeper avisarán. La
extensión del menú contextual no se distribuye ahí: necesita un MSIX firmado.

## Estado

**Verificado y medido:** núcleo, ZIP con Zip64, TAR, CLI, Zstandard, compresión
multihilo. 17 pruebas y 20 comprobaciones en verde.

**Verificado en máquina:** `windows/arca-shell`. Se escribió en un contenedor
Linux y estuvo un tiempo sin compilar; ya compila, se registra y pasa las seis
comprobaciones en Windows 11 build 26200. Ver `windows/LEEME.md`.

**Primera versión, arranca y funciona:** `arca-gui`. Abre zip y tar, lista,
filtra, extrae —todo o la selección— y comprime. El trabajo de disco va en un
hilo aparte y la tabla solo dibuja las filas visibles, las dos cosas por R5.

**Sin empezar:** formato 7z, xz/LZMA2, cifrado AES, archivo sólido, macOS
(Finder Sync), firma de código, iconos de verdad, winget y Homebrew.

**Escrito pero sin probar en su sitio:** Windows 10. El `IContextMenu` que
necesita funciona en el menú clásico de Windows 11, que es el mismo mecanismo,
pero nadie lo ha ejecutado en un Windows 10 real.

## Aviso sobre Zstandard en ZIP

Es el método 93, registrado en la especificación pero que `unzip` clásico aún no
lee. Por eso `-c auto` usa deflate en `.zip`: un zip existe para que lo abra
cualquiera. Zstandard se pide a mano y será el valor por defecto cuando exista
formato propio.
