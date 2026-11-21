//! Embute o manifesto (requireAdministrator, UTF-8) e a versão no wintop.exe.

fn main() {
    println!("cargo:rerun-if-changed=wintop.exe.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_manifest_file("wintop.exe.manifest");
        res.set("FileDescription", "wintop - monitor de sistema em terminal");
        res.set("ProductName", "wintop");
        if let Err(e) = res.compile() {
            panic!("falha ao embutir o manifesto: {e}");
        }
    }
}
