# F02 · Windows context menu

## Status: built and verified

This was written inside a Linux container and for a while nobody compiled it.
No longer: it builds, it registers, and the checks below pass on Windows 11
build 26200, with MSVC 14.44 and Windows SDK 10.0.26100.

The warning that used to sit here announced that the weak point would be the
version of the `windows` crate. **It was not.** Pinning to `0.58` was right and
the eight `IExplorerCommand_Impl` signatures matched as they were. What actually
broke was something else, written down here in case it bites again:

1. The `implement` feature of the `windows` crate was missing. The `*_Impl`
   traits live in an `impl.rs` that is only included behind
   `#[cfg(feature = "implement")]`, so without it they do not exist: four
   `cannot find trait in this scope`.
2. The trait goes on the type the macro generates (`Item_Impl`), not on the
   original. The vtable is built with `Vtbl::new::<Self, OFFSET>()` inside
   `impl Item_Impl`, so the bound lands on the generated type. The example in the
   `#[implement]` documentation says the opposite, but it is a `rust,ignore`
   block that never gets compiled and is out of date. The bodies do not change:
   the macro generates a `Deref` to the original.
3. `IEnumExplorerCommand_Impl::Skip` and `::Reset` return `Result<()>`, not
   `HRESULT`; the generated vtable already calls `.into()`. Only `Next` returns
   `HRESULT`, because there `S_FALSE` is a legitimate value and not an error.
4. `AppxManifest.xml` did not declare `uap10:AllowExternalContent`, without which
   a sparse package with `-ExternalLocation` fails with `0x80073D2E`. This one is
   invisible at compile time: it only shows up when registering.

If the crate version is ever raised, that is the order worth looking in.

## What is here

| File | What it is |
|---|---|
| `arca-shell/src/lib.rs` | The DLL: `IExplorerCommand` and `IContextMenu`, with a submenu of three actions |
| `AppxManifest.xml` | Sparse MSIX package giving the extension an identity |
| `arca.iss` | Inno Setup script that produces the installer |
| `construir.ps1` | Builds, packages and registers both menus for development |

## Two menus, two interfaces, two CLSIDs

Windows 11 has two context menus and **they share no mechanism**:

- The **modern** one, the menu a right click brings up, uses `IExplorerCommand`
  and is registered by declaring it in the MSIX package's `AppxManifest.xml`.
- The **classic** one, behind "Show more options" — and the only one that exists
  on Windows 10 — uses `IContextMenu` plus `IShellExtInit` and **cannot be
  declared in the manifest**: it goes through the registry, under
  `HKCU\Software\Classes`, which needs no administrator rights.

They are two separate COM objects, each with its own CLSID, in the same DLL.
`DllGetClassObject` hands out whichever is asked for.

### The `QueryContextMenu` trap

The classic contract for `IContextMenu::QueryContextMenu` is to return the number
of items added, encoded in the HRESULT with
`MAKE_HRESULT(SEVERITY_SUCCESS, 0, n)`. But in `windows` 0.58 the trait is
declared `-> Result<()>`, and the generated vtable turns `Ok(())` into
`HRESULT(0)`, which would mean "I added nothing" and leave the menu broken.

The way out is that `From<Result<T>> for HRESULT` returns the `Err` code
untouched, and `nonzero_hresult` only substitutes zero. So the way to say "I put
three entries in" is:

```rust
Err(Error::from(HRESULT(3)))
```

It looks like an error and is not: `HRESULT(3)` has the severity bit clear, which
means success. It reads oddly enough that the code carries a comment saying so.

## How it is tested

You need Windows 11 (build 22000 or later), Rust with
`rustup target add x86_64-pc-windows-msvc`, Visual Studio Build Tools and the
Windows SDK. On top of that, **Developer Mode enabled**, or `Add-AppxPackage
-Register` fails with `0x80073CFF`, which mentions a developer licence without
saying where to turn it on. On recent builds it is under **System › Advanced
options › For developers**; `start ms-settings:developers` goes straight there.

Without the Build Tools, `winget install Microsoft.VisualStudio.2022.BuildTools`
with `--override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`
brings the linker and the SDK. Without them **nothing** in the project builds,
not even the core tests: `rustc` has nothing to link with. And watch out when
running `cargo` from Git Bash, because the coreutils `link` in `/usr/bin`
shadows MSVC's `link.exe` and the resulting error does not look like it.

```powershell
cd windows
.\construir.ps1
Stop-Process -Name explorer -Force
```

And the checklist:

1. Right click a `.zip` → **Arca** appears with three options
2. Right click a `.txt` → "Extract here" does **not** appear
3. "Extract here" unpacks next to the archive, with no console window
4. Select several files → "Compress to .zip" puts them all in
5. Explorer does not freeze for an instant (requirement R5)
6. `.\construir.ps1 -Quitar` leaves everything as it was

And the classic menu ones, which are a separate implementation:

7. "Show more options" on a `.zip` → **Arca** with all three
8. "Show more options" on a `.txt` → **Arca** with "Compress" only
9. `-Quitar` also deletes the keys under `HKCU\Software\Classes`

All of them pass. The first four were additionally checked by driving the COM
object by hand — loading the DLL and calling `GetState` and `Invoke` the way
Explorer would — which is what makes it possible to say *what* fails rather than
just that "the menu does not show up". `GetState`, the call Explorer makes while
building the menu, takes **3–23 µs**; the R5 budget is 16 ms.

`Invoke` used to take **5.0 ms**, which is what a `CreateProcess` costs, and
`launch()` chained them one after another: with four `.zip` files selected that
was around 20 ms, over R5. `launch()` now leaves the work on its own thread and
returns in **68 µs**, regardless of how many files are selected.

## Why this DLL does use `unsafe`

It is the only crate in Arca that allows it, and there is no alternative: COM
requires raw pointers and C calling conventions.

What matters is that **no file is parsed here**. This DLL collects the selected
paths and launches `arca.exe`; all the work on data of unknown origin happens in
the child process, in the crates that forbid `unsafe`. Even if this extension had
a memory bug, a malicious archive could not reach it, because it never opens one.

## What is missing

- **Windows 10 untested**: the `IContextMenu` it needs is written and works in
  the Windows 11 classic menu, which is the same mechanism. But nobody has run it
  on a real Windows 10.
- **The menu does not use the window**: `arca-gui.exe` has `--extract-here`,
  `--extract-to-folder`, `--add` and `--add-quick` modes, with a progress bar and
  the overwrite prompt. The DLL still calls the CLI, so a right-click extraction
  runs silently and says nothing when it fails.
- **The labels are English only**: the window follows the system language, this
  does not.
- **Real icons**: `construir.ps1` generates 1×1 PNGs so the package validates.
  The classic menu puts no icon on its entries either; that would need
  `MENUITEMINFOW` with a bitmap.
- **`GetCommandString` returns `E_NOTIMPL`**: Explorer has no status-bar help
  text for the classic menu entries.
- **Signing**: distributing it needs `makeappx` plus `signtool` and a
  certificate. Without one, SmartScreen warns everyone who downloads it. The
  manifest's `Publisher` is still `CN=CAMBIAME` and has to match the certificate
  subject letter for letter.
- **Formats**: `is_archive` only offers the extensions the CLI can open (`.zip`,
  `.tar`, `.tar.gz`, `.tgz`). Adding 7z or rar means touching that list and the
  CLI's `detect()` at the same time, or the menu will offer something that fails
  silently: the child process runs with no console window.
- **R5 with large selections**: `launch()` chains one `CreateProcess` per file,
  about 5 ms each. Worth batching before somebody selects ten.
