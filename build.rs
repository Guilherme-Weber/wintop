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

    copy_module();

    let mut res = winresource::WindowsResource::new();
    res.set_manifest_file("wintop.exe.manifest");
    res.set("FileDescription", "wintop - monitor de sistema em terminal");
    res.set("ProductName", "wintop");
    if let Err(e) = res.compile_for("wintop") {
        panic!("falha ao embutir o manifesto: {e}");
    }
}

/// Copia o módulo do PawnIO (LGPL-2.1, arquivo separado) e a licença para
/// `target\<perfil>\modules\`, o mesmo layout da instalação (`<pasta do exe>\modules\`): o
/// executável elevado nunca procura o módulo num caminho gravado na compilação.
fn copy_module() {
    const FILES: [&str; 2] = ["AMDFamily17.bin", "COPYING-LGPL-2.1"];
    for f in FILES {
        println!("cargo:rerun-if-changed=modules/{f}");
    }
    // OUT_DIR = target\<perfil>\build\wintop-<hash>\out (layout antigo) ou
    // target\<perfil>\build\wintop\<hash>\out (atual): o perfil é o pai da pasta `build`.
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let profile = out
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "build"))
        .and_then(std::path::Path::parent)
        .unwrap_or_else(|| panic!("pasta `build` acima de OUT_DIR ({})", out.display()));
    let dest = profile.join("modules");
    std::fs::create_dir_all(&dest).expect("criar target/<perfil>/modules");
    for f in FILES {
        std::fs::copy(std::path::Path::new("modules").join(f), dest.join(f))
            .unwrap_or_else(|e| panic!("copiar modules/{f}: {e}"));
    }
}
