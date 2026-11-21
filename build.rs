//! Embute o manifesto (requireAdministrator, UTF-8) e a versão no wintop.exe.
//!
//! O recurso vai só no binário `wintop` (`compile_for`): testes e exemplos não herdam o
//! `requireAdministrator`, então `cargo test` roda num terminal comum.

fn main() {
    println!("cargo:rerun-if-changed=wintop.exe.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    assert!(
        arch == "x86_64",
        "o wintop só suporta x86_64 (manifesto amd64, CPUID); alvo: {arch}"
    );

    // A versão do manifesto acompanha a do Cargo.toml (X.Y.Z.0).
    let manifest = std::fs::read_to_string("wintop.exe.manifest").expect("ler wintop.exe.manifest");
    let expected = format!("version=\"{}.0\"", env!("CARGO_PKG_VERSION"));
    assert!(
        manifest.contains(&expected),
        "wintop.exe.manifest: assemblyIdentity deve ter {expected} (versão do Cargo.toml)"
    );

    let mut res = winresource::WindowsResource::new();
    res.set_manifest_file("wintop.exe.manifest");
    res.set("FileDescription", "wintop - monitor de sistema em terminal");
    res.set("ProductName", "wintop");
    if let Err(e) = res.compile_for("wintop") {
        panic!("falha ao embutir o manifesto: {e}");
    }
}
