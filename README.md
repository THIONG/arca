# Arca

Archivador multiplataforma en Rust. **Fases F01 y parte de F03**: núcleo, ZIP y TAR,
Zstandard, compresión multihilo y línea de comandos.

Los parsers de contenedor están escritos en safe Rust con `#![forbid(unsafe_code)]`
a nivel de crate. Un archivo malformado produce un error, nunca corrupción de memoria.

## Construir

```sh
cargo build --release      # binario en target/release/arca
cargo test --workspace     # 14 pruebas
bash interop.sh            # criterio de aceptación de la fase
```

Dos perfiles, como fija la sección 03 del diseño:

```sh
cargo build --release                              # codecs-native: incluye libzstd (C)
cargo build --release --no-default-features        # Rust puro: compila a cualquier objetivo
```

## Uso

```sh
arca create copia.zip mis-ficheros/ -n normal   # store | fast | normal | best
arca create copia.zip mis-ficheros/ -c zstd    # auto | store | deflate | zstd
arca create copia.zip mis-ficheros/ -j 8       # hilos; 0 = todos los núcleos
arca create copia.tar.gz mis-ficheros/
arca list copia.zip --tiempo
arca extract copia.zip -o destino/
arca test copia.zip                              # verifica CRC sin escribir en disco
arca bench copia.zip                             # mide los requisitos R1 y R2
```

Aliases cortos: `c`, `l`, `x`, `t`.

## Estructura

| Crate | Qué hace | `unsafe` |
|---|---|---|
| `arca-core` | Errores, límites, lectura acotada de cabeceras, fechas MS-DOS, defensa Zip Slip | prohibido |
| `arca-zip` | ZIP con Zip64; store, deflate sobre zlib-rs y Zstandard | prohibido |
| `arca-tar` | TAR ustar con verificación de checksum | prohibido |
| `arca-cli` | Binario `arca` | permitido, sin usar |

## Medido en esta máquina

Xeon 2,80 GHz, 2 núcleos, Linux.

| Requisito | Objetivo | Medido | |
|---|---|---|---|
| R1 arranque en frío | < 15 ms | **1,6 ms** | unzip 2,0 · 7z 3,7 |
| R2 listar 6000 entradas | < 200 ms | **4,5 ms** | unzip 17,2 · 7z 45,4 |
| R3 escalado a 2 hilos | ≥ 1,6× | **1,89×** | 94 % de eficiencia |

Comprimir 81 MB en 120 ficheros distintos, 2 hilos:

| | Tiempo | Tamaño |
|---|---|---|
| **Arca, zstd** | **330 ms** | **26,16 MB** |
| Arca, deflate | 941 ms | 28,35 MB |
| `zip -6` | 3934 ms | 27,65 MB |
| 7-Zip zip, 2 hilos | 5358 ms | 26,88 MB |

Con Zstandard, Arca es 11,9× más rápido que `zip` y 16,2× más rápido que 7-Zip,
y además produce el archivo más pequeño de los cuatro. Con deflate es 4,2× más
rápido que `zip` a cambio de un 2,5 % de tamaño: el compromiso conocido de zlib-rs.

**Aviso sobre Zstandard en ZIP:** es el método 93, registrado en la especificación
pero que `unzip` clásico todavía no lee. Por eso `-c auto` usa deflate en `.zip`:
un zip existe para que lo abra cualquiera. Zstandard se pide a mano, y será el
valor por defecto cuando exista formato propio.

## Interoperabilidad

`interop.sh` comprueba 20 casos verificando el SHA-256 del contenido:

- Lo que escribe Arca lo leen `unzip`, `tar` y 7-Zip, en los cuatro niveles
- Lo que escriben `zip`, `tar` y 7-Zip lo lee Arca sin perder un byte
- Un byte alterado se detecta por CRC
- Una entrada con `../../` se rechaza en vez de escribir fuera del destino

## Windows

`windows/` contiene la fase F02: la extensión del menú contextual del Explorador
(`IExplorerCommand` + paquete MSIX disperso). **Ese código todavía no se ha
compilado nunca** — se escribió sin acceso a una máquina Windows. Ver
`windows/LEEME.md`, que incluye la lista de comprobación para validarlo.

## Todavía no

Cifrado, formato 7z, xz/LZMA2, enlaces simbólicos, nombres largos de GNU tar,
archivo sólido (comprimir todos los ficheros como un flujo, que es de donde sale
la mayor ganancia de ratio) e integración de escritorio.
