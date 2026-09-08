# G0/G1 GPUI spike baseline

Estado: spike aislado; `arca-gui` sigue usando egui/eframe.

## G0 congelado

- Pin de G1: `zed-industries/zed@3384317a9931a21bb5ad8706f0f9d82cb02a71ec`.
  **Superado en G7**: `arca-gui` ya no depende de ese rev, sino de las cajas
  publicadas de GPUI Kit (`gpui-pre` / `gpui-pre-platform` / `gpui-component`),
  porque `gpui-component` se construye contra `gpui-pre ^0.3` y mantener el rev
  de git dejaba dos copias de GPUI en el grafo. `spikes/gpui` conserva el pin
  original: es el registro de lo que se validó en G1 y no se reescribe.
- Toolchain objetivo: Rust `1.97.1` (el árbol actual de Arca declara MSRV
  `1.75`; este spike no cambia ese contrato).
- Fixture: `spikes/gpui/fixtures/entries-6000.txt`, generado de forma
  determinista y validado por `cargo test` del spike.
- Capturas manuales que deben acompañar la ejecución local: `empty-window`,
  `fixture-6000`, `filter-ime`, `modal-blocking`, `file-drop` y
  `accessibility-list`. No se inventan capturas en CI; el resultado se anota
  en esta matriz con la plataforma, backend y fecha.

### Matriz manual

| Caso | Windows | Linux X11/Wayland | macOS | Resultado/fecha |
| --- | --- | --- | --- | --- |
| Ventana vacía y cierre | validado manualmente | pendiente | pendiente | ventana visible y cierre disponible en Windows GNU |
| 6.000 filas virtualizadas | código + test + validación visual | pendiente | pendiente | ventana ejecutada; lista virtualizada visible |
| Ctrl/Shift, cursor y scroll al cursor | código | pendiente | pendiente | validación manual completa pendiente |
| Columnas redimensionables | código | pendiente | pendiente | celdas reales y delta relativo; validación manual pendiente |
| Filtro: selección, IME, foco y Tab | código + tests + foco observado | pendiente | pendiente | campo Filter visible y editable; IME/Tab manual pendiente |
| Lector de pantalla: filas/campo/modal/progreso | código | pendiente | pendiente | Narrator/NVDA pendiente |
| Modal bloquea clicks del fondo y Escape | validado manualmente | pendiente | pendiente | modal apareció, ocultó fondo y Escape lo cerró |
| PNG RGBA cargado | validado manualmente | pendiente | pendiente | icono cargado en ventana Windows |
| Explorer/file-drop | pendiente | pendiente | pendiente | pendiente de prueba manual |
| `arca-drag` virtual, cancelación y extracción diferida | pendiente | N/A | N/A | pendiente de prueba manual Windows |

La ejecución disponible en esta máquina queda registrada así:

- Windows GNU: el toolchain `1.97.1-x86_64-pc-windows-gnu` está instalado y pasan
  `rustup run 1.97.1 cargo check --manifest-path spikes/gpui/Cargo.toml --locked`,
  `rustup run 1.97.1 cargo test --manifest-path spikes/gpui/Cargo.toml --locked`
  (4 pruebas) y `rustup run 1.97.1 cargo check --workspace --locked`. También
  pasan `cargo fmt --manifest-path spikes/gpui/Cargo.toml -- --check`, el check
  de `platform-probes` y la inspección manual de ventana, filtro, lista, icono,
  modal y Escape.
- Linux y macOS: no se marcan como compilados; los targets
  `x86_64-unknown-linux-gnu` y `x86_64-apple-darwin` no están instalados.
- Arca: `cargo check --workspace` y `cargo test --workspace` pasan. El
  `cargo fmt --all -- --check` existente falla en archivos de Arca por drift
  de formato; no se reformatean esos archivos para no tocar la UI productiva.

No se marca una plataforma como validada sin prueba manual real.

## Defecto de persistencia de `columns` (resuelto)

`arca-gui/src/main.rs::Settings::save` escribía `columns = ...` y
`Settings::load` no leía esa clave, así que los cambios de columnas se perdían
al reiniciar aunque `gui.conf` conservara la línea. Registrado aquí durante G1
y **no** corregido entonces a propósito: la UI productiva y el formato
`gui.conf` no debían cambiar durante el spike.

`Settings::load` ya lee la clave. El formato de `gui.conf` no cambió.

## G1 aislado

El binario está en `spikes/gpui`, fuera del workspace. Solo añade `gpui` y
`gpui_platform` desde el pin anterior; `gpui_platform` usa features por target.
El binario no importa crates productivos en su camino normal, no toca la
ventana productiva y no materializa entradas para drag-out. El feature
`platform-probes` mantiene probes de compilación Windows aislados para `rfd`,
`clipboard-win` y `arca-drag`; el check realizado demuestra que sus APIs
públicas compilan juntas, pero no sustituye una prueba interactiva de OLE,
clipboard o diálogos. Si falla input/IME, AccessKit, file-drop, `arca-drag` o
el loop de UI, se conserva egui y se detiene la migración.
