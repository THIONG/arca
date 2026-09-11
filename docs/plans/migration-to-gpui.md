# Plan de migración de `arca-gui` a GPUI Kit

## Resumen

Migrar únicamente la interfaz de Arca —no `arca-core`, `arca-zip`, `arca-tar`, CLI ni los formatos— a GPUI Kit, conservando el comportamiento actual y haciendo la transición por fases. La dependencia de GPUI queda fijada por `Cargo.lock` para evitar cambios involuntarios de API.

El punto de partida real es `arca-gui`: una ventana de aproximadamente 4.600 líneas en `src/main.rs`, más `theme.rs`, `tree.rs`, `glyphs.rs`, `clipboard.rs`, `i18n.rs` y `build.rs`. La UI actual incluye exploración jerárquica de ZIP/TAR/TAR.GZ, tabla con columnas configurables, selección de filas y carpetas, ordenación/redimensionado, filtro, breadcrumbs, doble clic, atajos, temas claro/oscuro/sistema, inglés/español, progreso, diálogos, drag-and-drop, clipboard Windows y drag-out mediante `arca-drag`.

## Fases

### 1. Línea base y spike de GPUI

- Registrar el estado actual con:
  - `cargo test --workspace`.
  - `cargo build --release`.
  - compilación/prueba específica en Windows.
  - prueba manual de ZIP, TAR y TAR.GZ, contraseña, conflictos, selección, clipboard y drag-and-drop.
- Capturar una matriz de comportamiento y dimensiones mínimas de ventana para usarla como criterio de paridad.
- Añadir GPUI Kit como dependencia de `arca-gui` desde la versión fijada.
- Crear una ventana mínima GPUI que compile y arranque en las plataformas soportadas.
- Verificar en este spike:
  - versión mínima de Rust requerida;
  - backend de ventana/renderizado en Windows, Linux y macOS;
  - texto, fuentes, imágenes, teclado, rueda, selección, menús, diálogos, drag-and-drop y accesibilidad;
  - integración con `rfd`, `clipboard-win` y `arca-drag`.
- Si el commit viable no soporta Rust 1.75, elevar `rust-version` y actualizar CI/documentación al mínimo requerido por GPUI. No se mantendrá una compatibilidad artificial con una versión de Rust que GPUI no soporte.

### 2. Separar el estado de aplicación del toolkit anterior

Sin reescribir la lógica de compresión:

- Extraer de `Arca` un estado/controlador de aplicación independiente del toolkit para:
  - archivo abierto, entradas, carpeta actual e historial;
  - selección, cursor, filtro y ordenación;
  - configuración, idioma, tema y columnas;
  - trabajos activos, progreso, errores y avisos;
  - estados pendientes de contraseña, conflicto, borrado y drop.
- Mantener `Job`, `Message`, `Answer`, `Pending`, `Format`, `Columns`, `SortColumn` y las funciones de archivo en Rust normal, aislados de las APIs visuales.
- Mantener el controlador desacoplado mediante eventos/acciones explícitos: abrir, extraer, comprimir, borrar, añadir, copiar, pegar, navegar y cancelar.
- Mantener los workers en hilos separados y el canal de mensajes; GPUI solo recibirá eventos y solicitará actualización de la vista. Ninguna operación de disco o compresión debe bloquear el hilo de UI.
- Conservar las pruebas existentes de `tree.rs` y de `main.rs`; trasladar a pruebas puras las reglas de selección, navegación, ordenación, nombres libres, progreso y transiciones de diálogos.

### 3. Shell de aplicación GPUI

G3 queda implementado con GPUI Kit como backend único. El shell usa
`AppController`, toolbar, tabla y diálogos GPUI. GPUI requiere Rust 1.97.1.

- Usar el ciclo de aplicación, ventana y root view de GPUI Kit.
- Preservar:
  - título dinámico `nombre — Arca`;
  - tamaño inicial compacto para acciones de línea de comandos;
  - tamaño inicial normal para navegación;
  - tamaño mínimo de ventana;
  - icono incluido mediante `build.rs` en Windows;
  - cierre automático de operaciones lanzadas desde el shell y ventana de resultados para operaciones interactivas.
- Crear una única vista raíz GPUI que derive su representación del estado independiente y procese acciones generadas por los componentes.
- Reemplazar `request_repaint`/`request_repaint_after` por el mecanismo de invalidación/notificación y temporizador de GPUI, manteniendo actualizaciones de progreso aproximadamente cada 100 ms y redibujado continuo solo cuando sea necesario.

### 4. Migración de la UI por superficies

Implementar y validar cada superficie con GPUI Kit:

1. **Toolbar y navegación**
   - Abrir, comprimir, extraer todo, extraer selección, contraseña y menú de overflow.
   - Campo de filtro con foco, placeholder y atajos.
   - Atrás, adelante, subir y breadcrumbs con truncado y menú de carpetas ocultas.
   - Contador de visibles/seleccionados.

2. **Listado de archivos**
   - Usar la lista virtualizada de GPUI para la tabla dentro de `arca-gui`.
   - Mantener columnas Nombre, Tamaño, Packed, Método, Ahorro, Modificado y CRC32.
   - Mantener columnas configurables y persistencia en `gui.conf`.
   - Mantener ordenación, indicador triangular, redimensionado desde cabecera, filas de carpetas antes que archivos y renderizado de iconos.
   - Mantener selección simple, Ctrl/Cmd, Shift, selección de carpeta, cursor de teclado, Home/End/PageUp/PageDown y scroll al cursor.
   - Mantener doble clic, Enter, menú contextual, goma de selección y autoscroll durante selección.

3. **Estados vacíos y barra de estado**
   - Empty state con indicación de drop.
   - Progreso, archivo actual, errores, avisos y resumen del archivo abierto.
   - Vista de operación con progreso, duración, resultado y cierre.

4. **Diálogos y overlays**
   - Configuración de idioma, tema, formato, codec, nivel y extracción a subcarpeta.
   - Contraseña nueva/actual/necesaria, visibilidad de contraseña y Enter/Escape.
   - Conflicto de destino con Replace/Skip/Rename y variantes “all”.
   - Confirmación de borrado.
   - Confirmación de abrir o añadir un archivo arrastrado sobre otro archivo.
   - Ventana de atajos.
   - Todos los diálogos serán modales o bloquearán explícitamente las acciones de fondo mientras esperan respuesta.

### 5. Tema, tipografía, iconos y pintura

- Convertir `theme.rs` en tokens de tema propios de Arca: colores, fondos, bordes, selección, cursor, radios, espaciado y tamaños tipográficos.
- Mantener tema claro, oscuro y sistema, y guardar/cargar la preferencia existente.
- Reutilizar las fuentes del sistema en Windows y las fuentes de fallback del backend GPUI.
- Portar `glyphs.rs` a la primitiva de dibujo de GPUI disponible; no añadir una librería de iconos para sustituir ocho figuras ya dibujadas.
- Portar el icono de tipo de archivo y la caché por extensión. La caché deberá guardar el equivalente GPUI de textura/imagen y recordar fallos igual que ahora.
- Portar las formas especiales: triángulo de ordenación, cursor, selección de goma, overlay de drop y puntero de autoscroll.
- Revisar contraste y semántica accesible de botones, filas, menús, campos y diálogos mediante AccessKit/GPUI.

### 6. Integraciones de plataforma

- Mantener `rfd` para selección de archivos y carpetas.
- Mantener `clipboard-win` y su implementación CF_HDROP para copiar/cortar/pegar archivos en Windows.
- Mantener `arca-drag` para drag-out en Windows y preservar su manejo de liberación/cancelación.
- Mantener drop de archivos hacia la ventana en las plataformas donde GPUI lo exponga; adaptar solo el puente de eventos.
- Mantener apertura mediante la aplicación del sistema y el comportamiento de archivos temporales.
- Si una capacidad de GPUI no tiene equivalente directo, encapsular únicamente ese puente en un módulo de plataforma; no contaminar el estado de negocio con APIs GPUI.

### 7. Rediseño visual posterior con GPUI Kit

Después de alcanzar la paridad funcional, validar accesibilidad y completar la matriz de plataforma, hacer un rediseño visual completo de la superficie GPUI usando [GPUI Kit](https://gpui-kit.com/apps/) como referencia de componentes y dirección visual.

#### Decisiones tomadas al abrir la fase

- **GPUI Kit pasa a ser dependencia real, no solo referencia.** El pin a un rev de
  zed se retira: `gpui-component` se construye contra `gpui-pre ^0.3`, y mantener
  el rev de git dejaba dos copias de GPUI en el grafo, que no enlazan. `arca-gui`
  depende ahora de `gpui-pre` y `gpui-pre-platform` renombrados a `gpui` y
  `gpui_platform` en `Cargo.toml`, de modo que ningún `use gpui::...` cambia. La
  reproducibilidad la da `Cargo.lock`, igual que antes la daba el rev.
- **Monocromo con tinte de marca.** Seis grises por modo, hue 225 (el azul noche
  de `brand/BRAND.md`) al 8-12% de saturación, invertidos entre claro y oscuro.
- **Sin color de acento.** Selección, cursor de teclado y anillo de foco son el
  color de texto a distinta fuerza, así que el contraste está garantizado por
  construcción. Los únicos píxeles saturados son `danger` y `warning`, que
  distinguen "extraído" de "no extraído" y no son decoración.
- **Radio 4/6 px, fila de 26 px, fuente base 13 px.**
- Los tokens viven en `arca-gui/src/gpui_theme.rs` y son los de `gpui-component`;
  Arca no añade una capa de tokens propia.
- Mantener intactos `AppController`, `AppAction`, workers, formatos de `gui.conf` e integraciones de plataforma.
- Rediseñar toolbar, navegación, tabla, estados vacíos, progreso, notificaciones, menús y diálogos con un sistema coherente de tokens, espaciado, tipografía, iconos, estados hover/focus/disabled y responsive behavior.
- Conservar roles AccessKit, foco visible, teclado, contraste y soporte de lector de pantalla; el rediseño no puede degradar la matriz de accesibilidad.
- Comparar manualmente antes/después en ventanas compactas y normales, con archivos vacíos, listas grandes, errores, progreso y diálogos abiertos.
- Gestionar esta fase con un modelo especializado en diseño de interfaces y revisión visual, separando decisiones estéticas de cambios de lógica.

#### Estado

- **G7.1 hecho** — dependencia GPUI Kit, `gpui_theme.rs` monocromo claro/oscuro/
  sistema, y los 67 colores incrustados de `gpui_shell.rs` sustituidos por
  tokens. `cargo test --workspace` y `cargo test -p arca-gui`
  en verde.
- **G7.2 hecho** — layout. La referencia es **Nohrs** (mismo problema: un
  explorador de ficheros) con la densidad de **DBFlux**. La ventana deja de ser
  una pila de tiras flotando en padding y pasa a ser regiones a sangre separadas
  por líneas de 1 px:

  ```text
  ┌─ barra de acciones (40 px) ────────────────── [filtrar] ─┐
  ├─ ← → ↑ │ ruta (34 px) ─────────────────────────────────┤
  │ carpetas   │  tabla a sangre                            │
  │ (224 px)   │                                            │
  ├────────────┴─────────────────────────────────────────┤
  │ resumen del archivo            N visibles │ M elegidos │
  └───────────────────────────────────────────────────┘
  ```

  - **Barra lateral con el árbol de carpetas del archivo**
    (`gpui_component::sidebar`). Es contenido nuevo, no cromo: antes un archivo
    profundo solo se recorría descendiendo a doble clic y volviendo atrás. La
    rama de la carpeta actual se abre sola; el resto queda cerrado. El árbol se
    cachea por (ruta del archivo, número de entradas).
  - **Barra de estado** (`gpui_component::status_bar`) soldada abajo, con el
    resumen a la izquierda y los contadores a la derecha, que antes vivían en
    medio de la fila de navegación.
  - **Botones sin borde**, con el fondo apareciendo solo bajo el puntero. Una
    fila de siete cajas con contorno se leía como siete cosas compitiendo.
  - **Flechas con icono** (`IconName`, vía `gpui-kit-assets`) en vez de ‹ › ↑.
  - **Menús flotantes**: overflow y carpetas ocultas eran `absolute`, antes se
    dibujaban en el flujo y empujaban media ventana hacia abajo.
  - **Tabla a sangre**, sin borde ni radio propios, con cabecera fijada y
    franjas alternas.
- **G7.3 pendiente** — sustituir los widgets internos por los de
  `gpui-component`: `Input` (retira las ~400 líneas de `FilterInput` y su
  contrato UTF-16 con el IME), `Button`, `Modal`/`Root` para los diálogos,
  `Popover` real anclado al disparador en vez de `absolute` con offsets fijos,
  `Table` y `Notification`.
- **G7.4 pendiente** — el diálogo de configuración de la superficie GPUI, que es
  lo que permite cambiar tema e idioma sin editar `gui.conf`. Hasta entonces la
  preferencia se lee al arrancar y `System` sigue al escritorio.

### 8. Retirada del backend anterior

Cuando la vista GPUI alcance la matriz de paridad:

- Mantener únicamente GPUI y GPUI Kit en `Cargo.toml` y regenerar `Cargo.lock`.
- Retirar imports y tipos del toolkit anterior de los módulos de interfaz.
- Eliminar el adaptador y los tests que dependan de coordenadas del backend anterior; conservar sus invariantes como pruebas del nuevo modelo.
- Revisar comentarios para documentar GPUI o el comportamiento de Arca, no la implementación anterior.
- Actualizar README y cualquier documentación de build con la nueva dependencia, MSRV y requisitos de plataforma.

## Criterios de aceptación

- `cargo test --workspace` pasa sin regresiones.
- `cargo build --release` pasa en las plataformas soportadas y la build Windows conserva icono, clipboard y drag-out.
- La UI arranca con y sin archivo, y también en los modos `--extract-here`, `--extract-to-folder`, `--test`, `--add` y `--add-quick`.
- Se conserva la paridad de ZIP, TAR, TAR.GZ, cifrado AES-256, extracción, compresión, test, borrado y adición.
- Se conserva la matriz de interacción: teclado, ratón, doble clic, selección múltiple, cursor, filtro, ordenación, redimensionado, rueda/autoscroll, menús y Escape.
- Se conserva la configuración existente de idioma, tema y columnas sin cambiar el formato `gui.conf`.
- Se validan lectores de pantalla y foco de teclado en Windows.
- Se comprueba que ninguna operación pesada se ejecuta en el hilo de UI y que el progreso sigue actualizándose durante compresión/extracción.
- Se realiza una comparación visual manual de toolbar, tabla, diálogos, temas y estados vacíos contra la línea base, aceptando solo diferencias propias de GPUI que no alteren jerarquía ni legibilidad.

## Supuestos fijados

- El alcance es todo `arca-gui`, no una migración de los crates de compresión.
- Se busca paridad completa, no un prototipo ni una reducción temporal de funciones.
- La transición será incremental, pero GPUI será el backend final único.
- GPUI se fijará a un commit/tag reproducible del repositorio de Zed; la selección exacta se hará en el spike de compilación y quedará registrada en `Cargo.toml`/`Cargo.lock`.
- Se reutilizarán dependencias existentes (`rfd`, `clipboard-win`, `arca-drag`) y no se añadirá una librería de tabla o iconos salvo que el spike demuestre que GPUI no puede cubrir una capacidad imprescindible.
- No se migrará la lógica de archivos ni se introducirán abstracciones de negocio nuevas: solo se separará el estado mínimo necesario para que la vista no dependa del toolkit.
