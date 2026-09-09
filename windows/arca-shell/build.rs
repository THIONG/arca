// Le da al enlazador la lista de lo que el DLL exporta.
//
// Ver `arca-shell.def`: son los dos puntos de entrada de COM y van marcados
// PRIVATE. Solo con MSVC, que es el unico enlazador que entiende un fichero de
// definicion y el unico con el que se compila esto.
fn main() {
    println!("cargo:rerun-if-changed=arca-shell.def");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    // Absoluta: el enlazador no corre necesariamente desde esta carpeta.
    let here = std::env::var("CARGO_MANIFEST_DIR").expect("cargo da esto siempre");
    println!("cargo:rustc-cdylib-link-arg=/DEF:{here}/arca-shell.def");
}
