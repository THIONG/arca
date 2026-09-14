# Formato 7z

Estado: sin empezar. Documento de decisión, no de diseño cerrado: lo primero
que hay que resolver es si el formato se escribe a mano o se toma de una
biblioteca, y eso condiciona todo lo demás.

Va en `docs/todos/` junto al resto de pendientes; cuando se decida el camino y
haya fases con orden, el diseño se mueve a `docs/plans/`.

## Por qué existe este documento

Salió de una pregunta sobre un ZIP cifrado: los nombres de los archivos se ven
sin contraseña. No es un fallo de Arca ni del programa que creó el archivo. El
directorio central de un ZIP nunca va cifrado, ni con ZipCrypto ni con AES-256:
el formato no tiene sitio donde meterlo. Nombres, tamaños, fechas, CRC y la
estructura de carpetas se leen siempre.

Si alguna vez hace falta que no se vea ni la lista, hay que salir de ZIP. Y de
los dos formatos que lo hacen, solo uno es viable:

| Formato | Oculta los nombres | Se puede implementar |
| --- | --- | --- |
| ZIP (ZipCrypto o AES-256) | No, nunca | Ya está hecho |
| 7z con `-mhe=on` | Sí, la cabecera va cifrada | Sí: formato abierto, implementación de referencia LGPL, SDK de LZMA en dominio público |
| RAR con `-hp` | Sí | No: el algoritmo es de win.rar GmbH. Publican el código de `unrar`, pero su licencia prohíbe expresamente usarlo para hacer un compresor compatible. Leer arrastraría esa licencia y código C; escribir requiere licencia comercial |

RAR queda descartado **como formato que Arca escriba**. Leerlo sí es legal y
tiene su propio documento: `formato-rar.md`. Tres de los cambios de fondo que
pide este trabajo son los mismos que pide aquel; quien vaya primero los paga.

## Lo que 7z aporta además de ocultar los nombres

- **Mejor ratio.** LZMA2 comprime bastante más que deflate en texto y binarios.
- **Compresión sólida.** Las entradas se comprimen como un flujo continuo, lo
  que en una carpeta de archivos parecidos cambia el resultado de forma
  notable. Tiene un coste, y es el que rompe el modelo actual: ver abajo.
- **Un formato que la gente espera.** Es la tercera casilla del diálogo de
  crear, después de ZIP y TAR.

## Las dos decisiones que hay que tomar antes de escribir código

### 1. A mano o con `sevenz-rust2`

`arca-zip` son 2.900 líneas escritas aquí; `arca-tar`, 299. Un `arca-7z` a
mano no se parece al segundo: LZMA2, el modelo de *folders* (cadenas de
codificadores encadenados), la cabecera codificada y AES-256-SHA-256 son un
formato entero, no una variante del que ya hay.

Datos de `sevenz-rust2`, comprobados en su `Cargo.toml` y su `CHANGELOG.md`,
no supuestos:

- Licencia Apache-2.0. Compatible con el proyecto.
- Rust puro. En 0.8.0 (2025-02-25) retiraron el `unsafe` que quedaba, tanto en
  `sevenz-rust2` como en `lzma-rust2`. Esto es lo que decide la regla del
  proyecto sobre analizar bytes de archivos ajenos sin `unsafe`, y hay que
  volver a comprobarlo en la versión concreta que se fije.
- Cabecera cifrada soportada: «Added support for encrypted headers» en 0.6.0 y
  «Support write encoded header» en 0.4.3. Es decir, `-mhe=on` de lectura y de
  escritura, que es justo lo que motivó este documento.
- Cifrado: solo AES-256-SHA-256, que es el único que usa 7-Zip en la práctica.
  La contraseña se valida al analizar la cabecera, igual que hace
  `arca_zip::check_password`, así que el diálogo del GUI no cambia.
- Sus dependencias por defecto incluyen `bzip2` y `zstd`, que son C. Si se
  toma, van detrás de la bandera `codecs-native` que ya existe, y el build
  `--no-default-features` sigue siendo Rust puro.
- **Sube el MSRV.** Pide `rust-version = "1.93"` y `edition = "2024"`; el
  workspace está en 1.75 y 2021. Esto no se negocia con una bandera: sube para
  todo el árbol, incluido `windows/arca-shell`. Es el coste más real de esta
  opción y hay que aceptarlo antes de empezar.

Recomendación: tomarlo. Escribir un LZMA2 correcto no es donde está el valor de
Arca, y un LZMA2 casi correcto produce archivos que nadie más abre.

### 2. Qué se soporta, y en qué orden

Leer antes que escribir. Un 7z que se abre bien es útil el primer día; uno que
se escribe mal es un archivo perdido. El orden propuesto:

1. Listar y extraer `.7z` sin contraseña.
2. Extraer `.7z` con contraseña, incluida la cabecera cifrada.
3. Crear `.7z` con LZMA2 y los niveles que ya tiene la interfaz.
4. Crear `.7z` con contraseña y con la opción de ocultar los nombres.

Nada de cambiar la contraseña de un `.7z` existente en la primera vuelta: eso
en ZIP se resuelve sin recomprimir porque el cifrado va sobre los bytes ya
comprimidos y por entrada; en 7z con bloques sólidos hay que rehacer el bloque.

## Lo que toca en el árbol actual

Medido sobre el código de hoy, no estimado:

- **`arca-core::Method`** (`arca-core/src/lib.rs:76`) solo conoce `Store`,
  `Deflate` y `Zstd`. Faltan al menos LZMA2 y BZip2. Ojo con `Method::code()`:
  devuelve el número del método **de ZIP**, que en 7z no significa nada. O se
  documenta como específico de ZIP o se separa. Lo que pinta la interfaz es
  `name()`.
- **`Format` está duplicado**: `arca-gui/src/model.rs:15` y
  `arca-cli/src/main.rs:162`, con 60 usos de `Format::*` entre los dos. Añadir
  una variante toca ambos, más los 7 `match format` de
  `arca-gui/src/archive_ops/io.rs`. Antes de tocarlo, plantearse si el enum
  debería vivir una sola vez en `arca-core`.
- **`Entry`** vale tal cual. Para 7z todo lo cifrado es AES-256, así que
  `encrypted` es cierto y `zipcrypto` sigue en `false`.
- **El flujo de la contraseña en el GUI se invierte.** Hoy
  (`arca-gui/src/controller/mod.rs`, `Message::Listing`) el archivo se lista
  primero y la contraseña se pide después, porque en ZIP el listado se lee
  siempre. Con la cabecera cifrada no hay listado que enseñar hasta que la
  contraseña sea correcta: el orden pasa a ser pedir, abrir y solo entonces
  llenar la ventana. Este es el cambio de fondo del trabajo, no el códec.
- **La extracción en paralelo por entrada no aplica.** `arca-zip` abre el
  fichero una vez por hilo y saca una entrada distinta en cada uno, porque un
  ZIP es acceso directo por su directorio central. En un bloque sólido de 7z,
  sacar la entrada 40 obliga a descomprimir las 39 anteriores. El plan para el
  progreso y para la extracción selectiva hay que rehacerlo: se extrae por
  bloques, no por entradas.

## Comprobaciones

Lo mismo que ya se exige al resto, más lo propio del formato:

- `interop.sh` gana una sección: crear con Arca y verificar con `7z t`, crear
  con `7z` y extraer con Arca, byte a byte en ambos sentidos. Con contraseña,
  sin contraseña y con `-mhe=on`.
- Un `.7z` truncado en cada corte posible no puede provocar un `panic`, igual
  que los tests que ya existen para ZIP. Si el análisis lo hace una biblioteca,
  el test sigue siendo nuestro: comprueba que el error llega como error.
- Contraseña incorrecta: mensaje claro y ni un byte escrito en el destino.

## Lo que cuesta no hacerlo

Poco. ZIP con AES-256 cubre el caso de «que nadie lea el contenido», que es el
que pide casi todo el mundo. Ocultar los nombres es un requisito real pero
minoritario, y quien lo tiene suele tener ya 7-Zip instalado. Este documento
existe para que la decisión se tome con los números delante, no para que el
trabajo esté prometido.
