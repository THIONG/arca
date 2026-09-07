# Arca

Archivador multiplataforma en Rust. **Fases F01 y parte de F03**: núcleo, ZIP y TAR,
Zstandard, compresión multihilo y línea de comandos.

Los parsers de contenedor están escritos en safe Rust con `#![forbid(unsafe_code)]`
a nivel de crate. Un archivo malformado produce un error, nunca corrupción de memoria.

## Construir

```sh
cargo build --release      # binario en target/release/arca
cargo test --workspace     # 41 pruebas
bash interop.sh            # criterio de aceptación de la fase
```

Dos perfiles, como fija la sección 03 del diseño:

```sh
cargo build --release                              # codecs-native: incluye libzstd (C)
cargo build --release --no-default-features        # Rust puro: compila a cualquier objetivo
```

## Uso

```sh
arca create copia.zip mis-ficheros/ -l normal   # store | fast | normal | best
arca create copia.zip mis-ficheros/ -c zstd    # auto | store | deflate | zstd
arca create copia.zip mis-ficheros/ -j 8       # hilos; 0 = todos los núcleos
arca create copia.tar.gz mis-ficheros/
arca create copia.zip mis-ficheros/ -p clave     # cifra con AES-256
arca list copia.zip --time
arca extract copia.zip -o destino/
arca extract copia.zip -o destino/ -p clave     # archivo cifrado
arca extract copia.zip -o destino/ -j 8        # hilos; 0 = todos los núcleos
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

Esos números son de otra máquina y no se deben leer como una ventaja general.
Medido en Windows 11 con 16 hilos, deflate contra deflate, mejor de 3:

| Corpus | Arca | 7-Zip `-mx5` | Tamaño Arca | Tamaño 7-Zip |
|---|---|---|---|---|
| 5358 ficheros de código, 54,8 MB | 0,710 s | **0,627 s** | 13 729 308 | 13 747 705 |
| 16 ficheros, 287 MB | **0,402 s** | 2,285 s | 56 757 284 | 54 753 868 |

```
arca create c1.zip src
7z a -tzip -mx5 c2.zip src
```

Es decir: con ficheros grandes Arca comprime 5,7× más rápido a cambio de un 3,7 %
de tamaño, y con muchos ficheros pequeños 7-Zip va algo por delante con el mismo
tamaño. No hay un ganador único, depende del corpus.

### Extracción en paralelo

Un `.zip` es de acceso aleatorio: el directorio central dice dónde empieza cada
entrada, así que un hilo por núcleo puede abrir el archivo y descomprimir una
entrada distinta. Un `.tar` es un flujo único y ahí no hay nada que repartir.

Medido en Windows 11, 16 hilos, sobre 287 MB en 16 ficheros de texto:

| | Tiempo |
|---|---|
| **Arca, 16 hilos** | **0,207 s** |
| Arca, `-j 1` | 0,686 s |
| 7-Zip | 1,020 s |

```
arca create big.zip big
arca extract big.zip -o out -j 1
arca extract big.zip -o out
7z x -o"out" big7z.zip
```
Mejor de 3, borrando `out` antes de cada pasada.

Sobre muchos ficheros pequeños el reparto no cambia nada, y conviene decir por
qué: extraer 5358 ficheros de código fuente tarda 4,5 s, pero descomprimir esos
mismos 55 MB tarda 0,128 s (`arca test bench.zip`). El 97 % del tiempo se va en
crear ficheros en NTFS, no en descomprimir. 7-Zip tarda lo mismo (4,6 s) porque
choca contra el mismo muro.

**Aviso sobre Zstandard en ZIP:** es el método 93, registrado en la especificación
pero que `unzip` clásico todavía no lee. Por eso `-c auto` usa deflate en `.zip`:
un zip existe para que lo abra cualquiera. Zstandard se pide a mano, y será el
valor por defecto cuando exista formato propio.

## Cifrado

AES-256 en `.zip`, con el esquema WinZip AE-2: PBKDF2-HMAC-SHA1 de 1000 rondas
para derivar la clave, AES-256 en modo CTR, y un HMAC-SHA1 que autentica el
texto cifrado. Es lo mismo que escriben 7-Zip, WinRAR y NanaZip, y `interop.sh`
lo comprueba en las dos direcciones contra 7-Zip.

Cada entrada lleva su propia sal aleatoria de 16 bytes. Reutilizar una sal entre
entradas reutilizaría el flujo de clave, y dos ficheros iguales se verían iguales
en el archivo.

Se cifra después de comprimir, que es el orden que manda la especificación: al
revés el compresor no encontraría nada que comprimir. El CRC se guarda a cero, lo
que dice AE-2: es una suma del contenido en claro y no tiene por qué estar ahí
cuando el HMAC ya responde por los datos.

Un byte alterado no sale como contenido, falla el código de autenticación. El
descifrado va en flujo, así que ese veredicto llega cuando los bytes ya están
escritos: quien llama debe tirar lo que escribió si la extracción falla.

Lo que **no** hace: ZipCrypto, el esquema antiguo de contraseña, que está roto y
no se lee ni se escribe. Los nombres de los ficheros no se cifran, porque el
formato ZIP no lo permite: se ve la lista del contenido sin la contraseña.

```sh
arca create secreto.zip carpeta/ -p "una clave"
arca extract secreto.zip -o destino/ -p "una clave"
7z t -p"una clave" secreto.zip        # lo lee 7-Zip
```

En la interfaz gráfica hay un campo de contraseña al crear, y al abrir un archivo
cifrado la ventana la pide antes de extraer.

## Interoperabilidad

`interop.sh` comprueba 27 casos verificando el SHA-256 del contenido:

- Lo que escribe Arca lo leen `unzip`, `tar` y 7-Zip, en los cuatro niveles
- Lo que escriben `zip`, `tar` y 7-Zip lo lee Arca sin perder un byte
- Un archivo cifrado con AES-256 por Arca lo abre 7-Zip, y al revés
- Un byte alterado se detecta por CRC, o por el HMAC si está cifrado
- Una entrada con `../../` se rechaza en vez de escribir fuera del destino

## Windows

`windows/` contiene la extensión del menú contextual del Explorador: el menú
moderno de Windows 11 (`IExplorerCommand` + paquete MSIX disperso) y el clásico
(`IContextMenu` + registro). Compilada, instalada y verificada en Windows 11.
El instalador se genera con Inno Setup desde `windows/arca.iss`. Ver
`windows/LEEME.md`.

## Todavía no

Formato 7z, xz/LZMA2, enlaces simbólicos, nombres largos de GNU tar,
archivo sólido (comprimir todos los ficheros como un flujo, que es de donde sale
la mayor ganancia de ratio) e integración de escritorio.
