# Portar `AppController` al `main.rs` de hoy

Estado: análisis hecho, transformación sin empezar.

## Dónde está cada cosa

| | qué es |
| --- | --- |
| `gpui-kit-redesign-v2` | **la rama viva**. Sale del `main` de hoy. Trae GPUI Kit, el spike, los planes y `gpui_shell.rs`/`gpui_theme.rs` en el árbol pero **sin declarar** en `main.rs`, así que no se compilan. Verde en `cargo test --workspace` y en `cargo check -p arca-gui --features gpui`. |
| `gpui-kit-redesign` | la rama vieja, sobre `c4c0354`. **No borrar**: su `main.rs` es la implementación de referencia de la extracción. |
| PR #1 | apunta a la rama vieja, en borrador y en conflicto. Cuando la v2 esté completa, se repunta o se abre otra. |

**La implementación de referencia está en la rama vieja.** `AppState`,
`AppController`, `AppAction` y `dispatch` ya se escribieron una vez, sobre la
base de 4.441 líneas. No hay que inventarlos, hay que rehacerlos sobre un
fichero que creció:

```sh
git show gpui-kit-redesign:arca-gui/src/main.rs > /tmp/referencia.rs
```

Ahí están `struct AppState` (55 campos), `struct AppController { state:
AppState }`, `struct Arca { controller, icons, band, wheel }`, el `enum
AppAction` completo y los 48 métodos del controlador, ya con los receptores
reescritos. La diferencia contra lo que hay que hacer ahora es que `main`
añadió 8 campos y 10 métodos más.

### Regenerar los inventarios

Las tablas de más abajo se sacaron con esto, por si el fichero se mueve otra
vez:

```sh
# contrato que el shell necesita
rg -o 'controller\.state\.([a-z_0-9]+)' -r '$1' arca-gui/src/gpui_shell.rs | sort -u
rg -o 'controller\.([a-z_]+)\(' -r '$1'    arca-gui/src/gpui_shell.rs | sort -u
rg -o 'AppAction::([A-Za-z_]+)' -r '$1'    arca-gui/src/gpui_shell.rs | sort -u

# metodos de impl Arca, en orden
rg -n '^    (pub )?fn [a-z_0-9]+' arca-gui/src/main.rs
```

La rama `gpui-kit-redesign` original se hizo sobre `c4c0354`. Mientras vivía,
`main` avanzó 18 commits que reescriben `arca-gui/src/main.rs`. Los dos lados
partieron de un fichero de 4.441 líneas:

| | `main.rs` | qué añadió |
| --- | --- | --- |
| base `c4c0354` | 4.441 | — |
| `origin/main` | 6.103 | +1.662: renombrar en el archivo, visor, vista plana, grupos por máscara, columnas, árbol de carpetas, modo oscuro B/N |
| `gpui-kit-redesign` | 5.269 | +828: sacar el estado a `AppController` |

Los 46 hunks del merge son el mismo conflicto repetido: la rama renombró
*todos* los accesos al estado y `main` escribió 1.662 líneas nuevas contra la
forma vieja. Resolverlo no es elegir un lado, es rehacer la extracción encima
del `main` de hoy. Esta es la razón de hacerlo así y no resolviendo el merge:
**el compilador verifica la transformación**. Un hunk mal resuelto compila; un
`self.archive` que se escape de la reescritura, no.

## Contrato que `gpui_shell.rs` necesita

No es negociable: si algo de esto no existe, la ventana GPUI no compila.

**14 métodos de `AppController`**
`can_go_back`, `can_go_forward`, `codec_name`, `cut_landed`, `dispatch`,
`drag_out`, `is_checked`, `level_name`, `open`, `receive`, `run_job`, `s`,
`summary`, `visible_rows`

**29 campos de `controller.state`**
`add_password`, `archive`, `busy`, `checked`, `codec`, `confirm_delete`,
`confirm_drop`, `conflict`, `current_dir`, `current_file`, `cursor`,
`cut_pending`, `done_count`, `entries`, `error`, `filter`, `format`, `level`,
`notice`, `order`, `output_name`, `password_input`, `pending_inputs`,
`replies`, `settings`, `show_password`, `total_count`, `view`,
`waiting_on_password`, `window_title`

**28 variantes de `AppAction`**
`AnswerConflict`, `AnswerDrop`, `Back`, `BeginPasswordChange`, `CancelJob`,
`CancelPassword`, `ClearSelection`, `ConfirmDelete`, `Copy`, `Drop`,
`ExtractTo`, `Forward`, `InvertVisible`, `Navigate`, `Open`, `OpenFile`,
`Paste`, `PrepareCompress`, `RequestDelete`, `Run`, `SelectAllVisible`,
`SetChecked`, `SetFilter`, `SetPasswordInput`, `Sort`, `SubmitPassword`,
`ToggleColumn`, `TogglePasswordVisibility`

## Reparto de campos

`struct Arca` en `origin/main` tiene 63 campos. El reparto es:

- **`Arca` se queda 4**: `controller`, `icons`, `band`, `wheel`. Son los únicos
  con tipos de egui (`TextureHandle`, `Pos2`) o estado de gesto del ratón.
- **`AppState` se lleva los otros 60**, más 3 que añadió la extracción y que no
  existen ni en la base ni en `main`: `cancel_token`, `extract_dialog`,
  `window_title`.

Los 8 campos nuevos de `main` van **todos** a `AppState`: son datos puros.

| campo | por qué a `AppState` |
| --- | --- |
| `types` | caché de texto por extensión, no una textura (esa es `icons`) |
| `renaming`, `rename_fresh` | ruta y texto a medio escribir; GPUI también renombrará |
| `folders` | `tree::Folder`, el árbol de carpetas |
| `viewing` | `Viewed` no lleva tipos de egui |
| `picking_group`, `mask` | selección por máscara |
| `geometry` | cuatro `f32` para que `on_exit` tenga qué escribir |

## Reparto de los 57 métodos de `impl Arca`

**A `impl AppController` (35)**
`s`, `level_name`, `codec_name`, `summary`, `visible_rows`, `spawn`,
`remember`, `open`, `run_job`, `receive`, `extract_here`, `ask_extract`,
`selected_names`, `selected_roots`, `copy_to_clipboard`, `cut_landed`,
`dragged_files`, `drag_out` (×2), `paste_from_clipboard`, `add_files`,
`dropped`, `view_entry`, `cancel_password`, `rename_to`, `set_checked`,
`open_file`, `go_to`, `clear_picked`, `can_go_back`, `can_go_forward`,
`go_back`, `go_forward`, `is_checked`

**Se quedan en `impl Arca` (22)**
`tree_panel`, `settings_row`, `format_row`, `drop_hint`,
`confirm_drop_window`, `shortcuts`, `viewer_window`, `group_window`,
`confirm_delete_window`, `password_window`, `conflict_window`, `toolbar`,
`breadcrumb`, `shortcuts_window`, `settings_window`, `add_view`,
`running_view`, `column_edges`, `wheel_scroll`, `rubber_band`, `keyboard`,
`table`

**`new` se parte en dos**: `AppController::new(settings)` y `Arca::new(...)`.

## El punto que no es mecánico

`spawn` toma `&egui::Context` en `main` y llama a `ctx.request_repaint()`. La
extracción tiene que quitarle ese parámetro y sustituir el repintado por algo
que los dos backends puedan pedir. Es el único sitio donde la transformación no
es renombrar: lo demás es mover el método y reescribir el receptor.

## Orden de ejecución

1. Partir `struct Arca` en `AppState` + `AppController` + `Arca`.
2. Partir `impl Arca` en dos bloques moviendo los 22 métodos de egui al final.
3. Reescribir receptores, que es una regla por bloque:
   - en `impl AppController`: `self.<campo>` → `self.state.<campo>`
   - en `impl Arca`: `self.<campo>` → `self.controller.state.<campo>` y
     `self.<metodo_movido>()` → `self.controller.<metodo_movido>()`
4. Añadir `enum AppAction` y `dispatch`.
5. Quitar `&egui::Context` de `spawn`.
6. Compilar y arreglar hasta que `cargo check` calle. **Este es el paso que
   verifica**: cada acceso que se escape sale como error.
7. `mod gpui_shell` / `mod gpui_theme` y el `main` con `#[cfg(feature = "gpui")]`.
8. Simplificar `gpui_shell.rs`: borrar el `struct Folder` y su caché hechos a
   mano y usar `tree::folders_of` y `state.folders`, que `main` ya trae.

## Lo que hay que comprobar al final

Automático:

```sh
cargo test --workspace
cargo test -p arca-gui --features gpui
cargo fmt -p arca-gui -- --check
```

Y **a mano, que es lo que de verdad comprueba esto**: `cargo test` no toca la
UI de egui, así que si la extracción se come una de las funciones que trajo
`main`, la suite pasa igual de verde. Hay que abrir la ventana de egui
(`cargo run -p arca-gui`) y probar una por una:

- [ ] renombrar una entrada con F2 y desde el menú
- [ ] mirar un fichero sin sacarlo del archivo (el visor)
- [ ] la vista plana
- [ ] coger un grupo por máscara, y probar sólo lo elegido
- [ ] extraer aquí
- [ ] las columnas: ajustar, ordenar, y que sigan como se dejaron al reabrir
- [ ] el árbol de carpetas del panel lateral
- [ ] el modo oscuro en blanco y negro

Y la ventana GPUI (`cargo run -p arca-gui --features gpui`), que antes del
conflicto quedó funcionando: barra de acciones, ruta, árbol lateral, tabla,
barra de estado, claro y oscuro.

## Lo que quedaba pendiente de la fase 7, aparte de esto

No se pierde de vista por el desvío del merge; está en
`migration-to-gpui.md`, sección 7, apartado «Estado»:

1. Cambiar los widgets hechos a mano por los de `gpui-component`: `Input`
   (borra ~400 líneas de `FilterInput` y su contrato UTF-16 con el IME),
   `Modal`/`Root`, `Popover`, `Table`, `Notification`.
2. Los menús flotan con `absolute` y offsets fijos (`top(38.) right(232.)`),
   no anclados al disparador. Se rompe si cambia el ancho del filtro o el
   tamaño de fuente.
3. No hay diálogo de configuración en la superficie GPUI.
4. Matriz de plataforma: sólo Windows GNU.
5. Lector de pantalla sin validar con Narrator/NVDA.
6. Tres etiquetas de botón reconstruidas a mano en `gpui_shell.rs` que merecen
   una mirada: `"Set Password"`/`"Unlock"`, `"Keep Both"`/`"Keep Both Always"`
   y `"Delete"`.
