# Formato RAR (solo lectura)

Estado: sin empezar, con recomendación. A diferencia del de 7z, aquí la
decisión de fondo ya está tomada por la licencia: **se lee, no se escribe**.
Lo que queda por decidir es con qué.

## Por qué existe este documento

El de 7z salió de querer ocultar los nombres. Este sale de algo más simple: los
`.rar` están ahí fuera. Descargas, adjuntos, cómics en `.cbr`, archivos de hace
quince años. Arca hoy no los abre, y el usuario tiene que instalar otra cosa
para algo que un archivador se supone que hace.

No es una función nueva, es el hueco más visible que queda en la lista de
formatos.

## Lo que la licencia permite

La licencia de UnRAR es explícita:

> UnRAR source code may be used in any software to handle RAR archives without
> limitations free of charge, but cannot be used to develop RAR (WinRAR)
> compatible archiver and to re-create RAR compression algorithm, which is
> proprietary.

Abrir, listar, probar y extraer: permitido. Crear: no. Así que Arca lee `.rar`
y no los escribe, y eso no es una limitación temporal a la espera de una fase
2: es el final del camino.

Conviene que quede escrito también en el README cuando esto entre, porque un
formato que aparece al abrir y no al comprimir parece un olvido si no se
explica.

## Las tres vías, comprobadas

| Vía | Qué es | El pero |
| --- | --- | --- |
| Delegar en el binario instalado | Llamar a `unrar.exe` o `7z.exe` si está en el sistema | Cero dependencias, cero licencia que arrastrar, y los bytes hostiles se analizan en **otro proceso**. Pero solo funciona si el usuario ya lo tiene, que es justo el usuario que no nos necesita |
| FFI a unrar (crate `unrar`) | Lo que usa casi todo el mundo, y lo más probado que hay | C++ analizando archivos ajenos **dentro** de nuestro proceso. Choca con `unsafe_code = "forbid"` y con la regla de no analizar bytes no confiables con `unsafe`. Además hay que compilar C++ en las tres plataformas y meter la licencia UnRAR en el instalador |
| Rust puro (`rars`) | Implementación limpia del formato, sin código de RARLAB | Es joven. Ese es todo el pero, y no es pequeño |

### Datos de `rars`, comprobados en su árbol y no supuestos

- Versión 0.9.4, Apache-2.0. No arrastra la licencia UnRAR porque no usa su
  código: tiene su propio repositorio de investigación del formato.
- `unsafe_code = "forbid"` en los lints del workspace. La misma regla que aquí.
- Dependencias: `aes`, `hmac`, `sha1`, `sha2`, `zeroize`, `getrandom`,
  `aho-corasick`, `rayon`. Rust puro, y la mitad ya está en el árbol de Arca.
- Carpeta `fuzz/` con objetivos de fuzzing, que para un analizador de archivos
  ajenos es lo que uno quiere ver antes de confiarle nada.
- `rust-version = "1.87"`, edition 2021. Sube el MSRV desde el 1.75 de hoy,
  pero bastante menos que el 1.93 que pide `sevenz-rust2`.
- Cubre de RAR 1.3 a RAR 7, sólido, cifrado por archivo y cabecera cifrada.
- **También escribe**, y no hay bandera para dejar el escritor fuera del
  build. No nos obliga a nada —RAR no aparecería en el diálogo de comprimir—
  pero ese código se enlaza aunque no se llame.
- Su propio README dice que nació como experimento de desarrollo agéntico y que
  «could use more testing at volume». Hay que leerlo y creerlo: es la razón de
  la bandera de compilación de la fase 1.

Recomendación: `rars`, solo lectura, detrás de una bandera de características
que permita retirarlo en una versión si sale mal. Un fallo suyo llega como
error en safe Rust, no como corrupción de memoria, que es exactamente la línea
que el proyecto se ha trazado.

## Lo que toca en el árbol actual

Medido sobre el código de hoy:

- **Sería el primer formato de solo lectura.** Hoy `Format` da por hecho que
  todo lo que se abre también se crea: aparece en el desplegable de comprimir,
  en `extension()` y en los `match format` de
  `arca-gui/src/archive_ops/io.rs` (7 de ellos). Hay que partirlo en dos
  conjuntos, los que se leen y los que se escriben. Y el enum está duplicado en
  `arca-gui/src/model.rs:15` y `arca-cli/src/main.rs:162`, con 60 usos de
  `Format::*`: antes de añadir nada, plantearse si debería vivir una sola vez
  en `arca-core`.
- **`arca-core::Method`** (`arca-core/src/lib.rs:76`) solo conoce `Store`,
  `Deflate` y `Zstd`. RAR trae los suyos por generación —15, 20, 29, 50— más
  PPMd. Lo que pinta la columna *Method* del GUI es `name()`, así que basta con
  que sepa decirlo; `code()` es un número de ZIP y no significa nada aquí.
- **`Entry`** vale tal cual: `encrypted` a cierto y `zipcrypto` a falso.
- **El flujo de la contraseña se invierte**, igual que con 7z: con la cabecera
  cifrada no hay listado que enseñar hasta que la contraseña sea correcta, y
  hoy (`arca-gui/src/controller/mod.rs`, `Message::Listing`) se lista primero y
  se pregunta después.
- **La extracción en paralelo por entrada no aplica** en archivos sólidos: sacar
  la entrada 40 obliga a descomprimir las 39 anteriores. Igual que en 7z.
- **Multivolumen**, y esto sí es nuevo. Un `.part1.rar` o un `.r00` obliga a
  abrir los hermanos que están al lado en el disco. Todo el modelo actual
  asume «un archivo abierto es una ruta»: eso hay que tocarlo, y también la
  comprobación de qué pasa si falta un volumen.

Tres de estos cinco puntos son los mismos que pide el 7z. Quien vaya primero
los paga, y el segundo llega a un árbol que ya los tiene.

## Fases

1. Listar `.rar` (y `.cbr`) sin contraseña, detrás de bandera de compilación.
2. Extraer, incluidos los sólidos.
3. Contraseña: por archivo y con cabecera cifrada, con el flujo del diálogo ya
   invertido.
4. Multivolumen.

Nada de escribir. Nunca.

## Comprobaciones

- `interop.sh` gana una sección de solo lectura: archivos creados con `rar` y
  con `7z`, extraídos con Arca y comparados byte a byte contra lo que saca
  `unrar` de verdad. Con y sin contraseña, sólido y no sólido.
- Un `.rar` truncado en cada corte posible no puede provocar un `panic`, igual
  que los tests que ya existen para ZIP. Que el análisis lo haga una biblioteca
  no quita el test: comprueba que el error llega como error.
- Un volumen que falta tiene que decirse con su nombre, no fallar de cualquier
  manera.
- Contraseña incorrecta: mensaje claro y ni un byte escrito en el destino.

## Lo que cuesta no hacerlo

Más que con 7z. El que se encuentra un `.rar` y ve que Arca no lo abre no
piensa «qué pena, es un formato propietario»: instala otra cosa y se queda con
ella. Abrir RAR no hace a Arca mejor archivador, pero no abrirlo le cuesta el
usuario entero.
