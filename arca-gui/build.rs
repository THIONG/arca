// Embeds the icon into the executable. Without this the file associations,
// the Start Menu shortcut and the Explorer all fall back to the generic
// Windows icon, no matter what the installer points at.
fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../brand/arca.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../brand/arca.ico");
        res.set("FileDescription", "Arca archiver");
        res.set("ProductName", "Arca");
        if let Err(e) = res.compile() {
            // Not fatal: a build without rc.exe still produces a working
            // binary, just without an icon.
            println!("cargo:warning=icon not embedded: {e}");
        }
    }
}
