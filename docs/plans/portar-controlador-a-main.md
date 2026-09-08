# Portar `AppController` al `main.rs` de hoy

Estado: análisis hecho, transformación sin empezar.

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

`cargo test --workspace`, `cargo test -p arca-gui --features gpui`, y a mano
que las funciones que trajo `main` siguen ahí: renombrar con F2, el visor,
la vista plana, elegir grupo por máscara y las columnas. Los tests no cubren la
UI de egui, así que si la extracción se come una de esas funciones, la suite
pasaría igual.
