# Marca de Arca

Azul noche: **#05060A → #1B2A4A**, degradado en diagonal de la esquina inferior izquierda
a la superior derecha.

## Geometria

Lienzo 64x64, cuadrado con radio 14.5.

| Pieza | Medidas |
|---|---|
| Tapa | x 10-54, y 14-24, radio 4 |
| Cuerpo | x 10-54, y 27-50, radio 4 |
| Maneta | x 21.5-42.5, y 34.8-42.2, radio 3.7 |

Tapa y cuerpo **comparten los dos bordes verticales** y el mismo radio. La maneta
va centrada en los dos ejes del cuerpo. Si tocas el glifo, respeta esos bordes.

La maneta va calada: deja ver el fondo a traves. Por eso la version plana funciona
con una sola tinta.

## Ficheros

| Fichero | Para que |
|---|---|
| `arca-isotipo.svg` | Icono del .exe y de los archivos asociados |
| `arca-isotipo-troquel.svg` | Alternativa: maneta troncoconica de caja de archivo |
| `arca-glifo-plano.svg` | Menu contextual y barras. Usa `currentColor` |
| `arca-logotipo.svg` | Isotipo mas la palabra, texto en #1B2A4A |
| `arca-logotipo-blanco.svg` | Igual, texto en blanco, para fondos oscuros |

## Pendiente

Convertir `arca-isotipo.svg` a `.ico` con 16, 24, 32, 48 y 256 dentro, para
sustituir los PNG de relleno que genera `windows/build.ps1`.
