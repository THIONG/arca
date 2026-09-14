# Migración de la UI a GPUI Kit

Estado: en curso. Las fases 1 a 6 están hechas; queda la 7, que solo retira
andamiaje sin uso.

- `590862d` — `Root` del kit como vista raíz de la ventana.
- `0ae9f27` — confirmación de borrado con el `Dialog` del kit.

## Por qué existe este documento

`arca-gui` decía estar migrada a GPUI Kit, pero solo unos pocos componentes lo
estaban. El resto eran `div()` a mano con la forma de un componente que el kit
ya trae. El recuento de partida en `gpui_shell/mod.rs` (6067 líneas) era de 95
`div()` y 6 `uniform_list` contra 33 usos de componentes del kit.

## Lo que ya era del kit

`Theme` y `gpui_component::init`, `TitleBar`, `Sidebar`, `StatusBar`,
`Separator`, `Icon`, `Tooltip`, el `Button` del menú de desbordamiento con su
`DropdownMenu`, y el `Input` del filtro.

## Lo que falta por migrar

| Hecho a mano | Componente del kit |
| --- | --- |
| `dialog_overlay` y los once diálogos que cuelgan de él | `dialog` |
| `row_menu_view` | `menu` (ContextMenu) |
| `file_table`, `column_header`, `column_edge`, `file_row` | `table` y `resizable` |
| Migas de pan y su menú de ocultas | `breadcrumb` |
| `button`, `icon_button`, `dialog_button`, `menu_item` | `Button` |
| Fila de progreso como texto | `progress` (hecho) |
| `FilterInput` en `gpui_shell/input.rs` | `Input` y `InputState` |
| Atajos como texto plano | `kbd` (hecho) |
| Controles del panel de ajustes | `radio` y menú desplegable (hecho) |
| `uniform_list` del visor | `Scrollbar` del kit (hecho) |
| Campo de renombrado en la fila | `Input` (hecho) |
| `button` e `icon_button` | `Button` (hecho) |
| Menú del botón derecho | `ContextMenu` (hecho) |
| Tabla, cabecera y anchos | `DataTable` y `TableState` (hecho) |
| Campos de texto de los diálogos | `Input` y `InputState` (hecho) |
| Carpetas ocultas de las migas | `dropdown_menu` (hecho) |
| Árbol de carpetas sobre `SidebarMenuItem` | `tree` (hecho) |

## Decisiones tomadas

- La tabla se migra entera al `table` del kit y los gestos propios —banda de
  selección, arrastre fuera de la ventana, anchos por columna— se reconstruyen
  encima.
- Todos los diálogos adoptan el cierre estándar del kit: escape, clic en el
  fondo y botón de cerrar.
- Una fase, un commit.

## Reglas que gobiernan el resto de la migración

Las tres salieron de fallos reales durante el piloto y están anotadas en el
código. Saltárselas aborta el proceso, no da un error de compilación.

1. Las capas del kit —diálogo, hoja y notificación— se montan en `Frame`, la
   vista hermana del shell bajo `Root`, nunca dentro de `GpuiShell::render`. El
   cuerpo de un diálogo lee el shell, y hacerlo mientras el shell está prestado
   en exclusiva termina en `cannot read ... while it is already being updated`.
2. El constructor de un diálogo solo lee el shell. Quien escribe es el manejador
   (`on_ok`, `on_close`), porque un clic se despacha entre fotogramas.
3. `on_close` responde la pregunta pendiente solo si el estado sigue en ese
   modal. Ese guardia evita a la vez el doble despacho, cuando ya respondieron
   aceptar o cancelar, y el ciclo de abrir y cerrar sin fin.
4. Antes de abrir un diálogo hay que llevar el foco al shell. El kit enfoca su
   diálogo un fotograma más tarde, y hasta entonces el teclado se queda en una
   fila del fondo que ya ha salido del árbol de accesibilidad; marcar ese mismo
   nodo como descendiente activo aborta con `set_active_descendant called on
   the focused node`.

La tercera es la que sostiene el cierre estándar del kit: descartar un diálogo
cancela la operación de verdad en lugar de dejar al worker esperando una
respuesta que no llega.

## Lo que el kit no sabe de una lista de ficheros

La tabla del kit selecciona una fila. Esta marca muchas, las barre con una
banda y las arrastra fuera de la ventana. Nada de eso desaparece al migrar,
pero tampoco lo pone el kit:

- El marcado sigue siendo del shell. `render_tr` pinta el fondo y engancha el
  clic, que pasa por `select_row` con sus modificadores.
- La banda no cuelga de las filas sino de la ventana, así que sigue igual: lo
  único que necesitaba era la geometría de la lista, y el handle de scroll del
  kit es del mismo tipo que el que ya se usaba.
- La cebra se dibuja fila a fila en vez de con `stripe`, que raya el panel
  entero y hace pasar por filas lo que no lo es.
- Los roles de accesibilidad se ponen a mano en el delegate. Sin ellos la
  tabla desaparece del árbol: es lo primero que hay que volver a mirar si
  alguna vez se cambia cómo se dibuja una fila.

## El último menú dibujado a mano

Con las carpetas ocultas de las migas se fue el andamiaje que mantenía vivo un
menú hecho a mano: un `FocusHandle` por fila, las flechas arriba y abajo, la
trampa de foco con el tabulador, el escape que devuelve el teclado al botón y
el recorte de los handles cuando la ruta se acorta. Todo eso lo trae el menú
del kit; el botón `…` sólo tiene que decir qué carpetas hay.

Se fueron con él `menu_row`, `menu_item`, `menu_key_down`, `breadcrumbs_key_down`,
`trap_focus`, `sync_breadcrumb_item_focus`, `visible_menu_items`,
`focus_cycle_index`, `menu_target`, cuatro campos de estado y sus tres tests.
Los tests no prueban comportamiento del usuario sino la aritmética de un menú
que ya no se dibuja aquí: navegar con flechas dentro del menú sigue
funcionando, sólo que ahora es problema del kit.

`overflow_open` se fue también: quedó sin ponerse a `true` desde que el menú
de la barra pasó al kit, y seguía apareciendo en las condiciones que deciden
si la ventana está en reposo.

## El editor de texto que ya no hace falta

`gpui_shell/input.rs` eran 452 líneas para escribir en cuatro campos: el
contrato UTF-16 que esperan los IME del sistema, un elemento propio con su
medida y su pintado, y la cuenta de dónde está el cursor. Todo eso lo trae
`InputState`, que ya se usaba para el filtro y para el renombrado; los cuatro
campos que quedaban a mano (contraseña, nombre de salida, contraseña al
comprimir y el campo compartido de nombre) pasaron a suscribirse a
`InputEvent::Change` y el módulo entero desapareció.

Lo que se gana además de las líneas: deshacer, portapapeles, selección con el
ratón y un IME de verdad, que el campo a mano no tenía. `select_all` sí
existe en `InputState` — estaba mal anotado aquí — así que al renombrar el
nombre viejo ya sale seleccionado, como en cualquier explorador.

## El tema se pinta dos veces

`Theme` lleva la paleta por duplicado: los colores planos en `theme.colors`, que
es lo que lee el shell, y la copia resuelta en `theme.tokens`, que es lo que
leen los componentes del kit. `gpui_theme::paint` solo escribía la primera, así
que un diálogo del kit salía negro sobre negro con el botón primario blanco
mientras la ventana alrededor era gris. Al final de `paint` los tokens se
regeneran desde la paleta recién pintada; cualquier color nuevo tiene que
quedar por encima de esa línea.

## Cómo se reconcilian los dos modelos

El shell deriva su modal del estado del controlador y el kit mantiene una pila
imperativa. `sync_dialog` es el único punto donde se encuentran, y corre entre
fotogramas desde `on_next_frame`. `kit_dialog` es la lista blanca de los modales
ya migrados: lo que no está en ella sigue saliendo por los overlays antiguos, de
modo que el árbol queda utilizable después de cada fase.

## Fases

1. **Hecha.** `Root` como vista raíz y capas del kit en `Frame`.
2. **Hecha.** Los diálogos salen del kit. Con ellos se fueron
   `dialog_overlay`, `dialog_button`, `dialogs`, `modal_key_down`,
   `modal_enter`, `modal_focus_targets`, once manejadores de foco y la cadena
   `close`, que ya no tiene botón que la use. Renombrar dejó de ser un diálogo
   y se edita en la propia fila, así que `ModalKind::Rename` ya no existe.
3. **Hecha.** `button` e `icon_button` devuelven el `Button` del kit, y con
   ellos migran la barra, las flechas de navegación, las migas y los botones de
   la barra de progreso. `menu_item` se quedó dibujado a mano a propósito, bajo
   el nombre `menu_row`: un menú mantiene vivas las flechas guardando un manejo
   de foco por fila y el botón del kit se guarda el suyo, así que migrarlo
   ahora sería hacerlo dos veces. Por lo mismo se queda el botón `…` de las
   migas, que sostiene su propio menú.
4. **Hecha.** El menú del botón derecho es el `ContextMenu` del kit: se abre
   donde está el puntero, anda con las flechas, cierra con escape o con un
   clic fuera y devuelve el foco, todo sin código nuestro. Al migrarlo salió a
   la luz que el menú de `…` dibujado a mano llevaba muerto desde que ese
   botón pasó al kit en la fase 3: `overflow_open` no se ponía a `true` en
   ningún sitio. Se fueron con él sus cuatrocientas líneas, tres manejos de
   foco, `overflow_key_down`, `recent_shown`, dos constantes de posición y una
   cadena de i18n.
5. **Hecha.** La tabla es la del kit. El delegate `FileTable` guarda su propia
   copia de lo que dibuja un fotograma, porque corre dentro del render del
   shell y leer el shell desde ahí lo tomaría prestado dos veces; `sync_table`
   la refresca justo antes de entregar la tabla. La banda sobrevivió intacta
   compartiendo un mismo `UniformListScrollHandle` con el kit. Lo que el kit no
   trae y se reconstruyó encima: marcar muchas filas a la vez, la cebra sólo
   bajo las filas que existen, los roles y descripciones de accesibilidad, y
   ordenar pulsando el nombre de la columna y no sólo su flechita.
6. **Hecha.** Migas de pan, progreso, atajos, controles de ajustes, campos de
   texto, visor y árbol lateral.
   - **Progreso**: barra del kit con su rol y su valor numérico. El nombre
     accesible pasó del contenedor a la barra: dos nodos con el mismo nombre lo
     anunciaban dos veces.
   - **Atajos**: la tabla guarda pulsaciones (`ctrl-o`), no cadenas escritas a
     mano, y el kit las deletrea y las encajona. Con eso se fue el `Supr` que
     la ventana pedía a un teclado cuya tecla dice Delete.
   - **Ajustes**: idioma y tema son grupos de radios; el formato, el compresor,
     el nivel y la página de códigos son menús que enseñan todas las opciones.
     Los cuatro eran botones que avanzaban al siguiente valor, así que volver
     al que se acababa de pasar era dar la vuelta entera y nada decía cuáles
     eran las demás opciones. `cycle_format`, `cycle_codec` y `cycle_level`
     desaparecieron, y con ellos cinco variantes de `SettingsControl`. El
     mismo selector lo usa el diálogo de añadir, que tenía los mismos tres
     botones.
   - **Visor**: barra de desplazamiento del kit colgada del `UniformListScrollHandle`
     que ya usaba. El `uniform_list` se queda: es la virtualización, y el
     `virtual_list` del kit es para filas de altura variable.
   - **Árbol lateral**: `tree` del kit. El panel reconstruía un
     `SidebarMenuItem` anidado por carpeta en cada fotograma y recortaba lo que
     no cabía, sin manera de llegar a ello. Ahora pulsar una carpeta a la vez
     va a ella y la abre —lo que hace el árbol del kit en todas partes—, y los
     roles se ponen a mano porque el árbol del kit no pone ninguno.
   - No se usó `Select`: necesita una entidad `SelectState` y una suscripción
     por control, y el propio kit dibuja sus ajustes desplegables con un
     `Button` y un `DropdownMenu`, que es lo que hay aquí.
7. Retirar `gpui_shell/input.rs` y el resto de andamiaje que quede sin uso
   (`settings_focus`, `viewer_focus` y las ramas de `sync_modal_focus`, que ya
   no se alcanzan porque todos los modales son del kit).

## Comprobación de cada fase

```sh
cargo fmt --all -- --check
cargo clippy -p arca-gui --all-targets -- -D warnings
cargo test -p arca-gui
```

Compilar no basta: los dos fallos del piloto compilaban y pasaban los tests.
Hay que abrir la ventana con un archivo de prueba y recorrer el camino que se
acaba de tocar. El piloto se dio por bueno al ver "2 removed" en la barra de
estado y el ZIP con cero entradas en disco.

## Avisos para quien automatice la interfaz

- Ni la acción `press` de accesibilidad ni un clic sintético por referencia
  activan los botones del kit. Con el teclado sí, y con un clic por coordenadas
  también.
- Borrar trabaja sobre las filas marcadas, no sobre la fila enfocada: hace falta
  `Ctrl+A` antes de `Supr`.
- Las teclas de función solo llegan con el foco dentro de la lista, y el arnés
  no siempre lo consigue a la primera.
