//! Cópia e troca do executável e dos módulos do PawnIO na pasta instalada (instalação e
//! atualização), com o exe em uso posto de lado como `.old`.

use super::*;

/// Versão da cópia instalada (`wintop.exe --version`), se houver uma.
pub(super) fn installed_version(exe: &Path) -> Option<String> {
    if !exe.is_file() {
        return None;
    }
    let out = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().strip_prefix("wintop ").map(str::to_string)
}

/// Teto para leitura do próprio executável (o binário distribuído tem ~2 MB).
pub(super) const MAX_OWN_EXE_BYTES: u64 = 64 << 20;

/// Caminho real da imagem em execução pelo kernel (`QueryFullProcessImageNameW`), que acompanha
/// renomeações (diferente de `current_exe()`, que lê o cache estático do PEB).
pub(super) fn process_image_path() -> Result<PathBuf, String> {
    // SAFETY: pseudo-handle do processo atual.
    image_path(unsafe { windows::Win32::System::Threading::GetCurrentProcess() })
}

/// Caminho Win32 da imagem do processo `process` (handle com consulta limitada basta).
pub(super) fn image_path(process: HANDLE) -> Result<PathBuf, String> {
    use windows::Win32::System::Threading::{PROCESS_NAME_WIN32, QueryFullProcessImageNameW};
    let mut buf = vec![0u16; 1024];
    loop {
        let mut len = buf.len() as u32;
        // SAFETY: handle de processo válido e buffer local com capacidade em caracteres.
        match unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &raw mut len,
            )
        } {
            Ok(()) => {
                let s = String::from_utf16_lossy(&buf[..len as usize]);
                return Ok(PathBuf::from(s));
            }
            Err(e) if buf.len() < 32_768 => {
                let _ = e;
                buf.resize(buf.len() * 2, 0);
            }
            Err(e) => return Err(format!("QueryFullProcessImageNameW: {e}")),
        }
    }
}

/// Lê os bytes do próprio executável em execução sem TOCTOU de renomeação: abre o caminho real
/// informado pelo kernel sem `FILE_SHARE_DELETE`/`FILE_SHARE_WRITE`, confere que não é link nem
/// hardlink e revalida o caminho final contra a imagem atual com o handle travado.
pub(super) fn read_own_exe() -> Result<(PathBuf, Vec<u8>), String> {
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
        FILE_SHARE_READ, FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType,
        GetFinalPathNameByHandleW, OPEN_EXISTING, SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT,
    };

    let image = process_image_path()?;
    if !crate::win::is_local_absolute(&image) {
        return Err(format!("{} is not a local path (refused)", image.display()));
    }
    let w = crate::win::wide_os(image.as_os_str());
    // SAFETY: caminho terminado em zero; sem FILE_SHARE_DELETE/WRITE trava contra rename.
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
            None,
        )
    }
    .map_err(|e| format!("{}: {e}", image.display()))?;
    // SAFETY: CreateFileW teve sucesso; handle válido e sem outro dono.
    let owned = unsafe { crate::win::own(h) };
    let raw = HANDLE(owned.as_raw_handle());
    // SAFETY: handle válido.
    if unsafe { GetFileType(raw) } != FILE_TYPE_DISK {
        return Err(format!("{} is not a disk file (refused)", image.display()));
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: handle válido; estrutura local.
    unsafe { GetFileInformationByHandle(raw, &raw mut info) }
        .map_err(|e| format!("{}: {e}", image.display()))?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT.0 | FILE_ATTRIBUTE_DIRECTORY.0) != 0 {
        return Err(format!("{} is a link or folder (refused)", image.display()));
    }
    if info.nNumberOfLinks > 1 {
        return Err(format!("{} is a hardlink (refused)", image.display()));
    }
    // Com o handle segurando sem FILE_SHARE_DELETE, o caminho final não pode mudar: confere
    // contra a imagem real do processo novamente para provar que lemos o próprio binário.
    let mut fbuf = vec![0u16; 1024];
    // SAFETY: handle válido e buffer local.
    let n = unsafe { GetFinalPathNameByHandleW(raw, &mut fbuf, FILE_NAME_NORMALIZED) } as usize;
    if n == 0 || n >= fbuf.len() {
        return Err(format!("{}: could not verify final path", image.display()));
    }
    let final_s = String::from_utf16_lossy(&fbuf[..n]);
    let final_clean = final_s.strip_prefix(r"\\?\").unwrap_or(&final_s);
    let image_now = process_image_path()?;
    if !final_clean.eq_ignore_ascii_case(&image_now.to_string_lossy()) {
        return Err("running executable was moved during install (refused)".into());
    }
    let mut bytes = Vec::new();
    let read = std::fs::File::from(owned)
        .take(MAX_OWN_EXE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", image.display()))?;
    if read as u64 > MAX_OWN_EXE_BYTES {
        return Err(format!(
            "{} is larger than {} MB",
            image.display(),
            MAX_OWN_EXE_BYTES >> 20
        ));
    }
    Ok((image_now, bytes))
}

/// Troca o exe instalado mesmo com o wintop aberto: o Windows deixa renomear um exe em
/// execução (não sobrescrever). O novo entra como `.new` já protegido (dono Administradores), o
/// atual vira `.old` e o novo assume o nome; a instância aberta segue na versão antiga até fechar.
pub(super) fn replace_exe(exe_bytes: &[u8], exe: &Path) -> Result<(), String> {
    let new = exe.with_extension("exe.new");
    if is_link(&new) {
        return Err(format!("{} is a link (refused)", new.display()));
    }
    let _ = std::fs::remove_file(&new);
    create_protected(&new, exe_bytes, true)?;
    if swap_in(&new, exe)? {
        println!("wintop is open: the new version starts the next time it opens");
    }
    Ok(())
}

/// Nome único de sobra `.old` quando `wintop.exe.old` ainda está em execução.
pub(crate) fn unique_old_name() -> String {
    format!("wintop.exe.{}.{}.old", std::process::id(), stamp())
}

/// Protocolo de troca atômica do exe (`exe.new` -> `exe`, atual -> `exe.old` com rollback em
/// erro). Compartilhado entre a instalação (`Program Files`) e a atualização portátil (`userfile`).
pub(crate) fn swap_exe(
    free_old: impl FnOnce() -> Result<(), String>,
    move_current_to_old: impl FnOnce() -> Result<(), String>,
    move_new_to_current: impl FnOnce() -> Result<(), String>,
    restore_old_to_current: impl FnOnce(),
    remove_new: impl FnOnce(),
    cleanup_old: impl FnOnce(),
) -> Result<(), String> {
    if let Err(e) = free_old() {
        remove_new();
        return Err(e);
    }
    if let Err(e) = move_current_to_old() {
        remove_new();
        return Err(e);
    }
    if let Err(e) = move_new_to_current() {
        restore_old_to_current();
        remove_new();
        return Err(e);
    }
    cleanup_old();
    Ok(())
}

/// Tira o `exe.old` do caminho: apagado se ninguém o usa; em uso (o processo que reabriu a
/// versão nova espera por ela rodando do `.old`), renomeado para um nome único (o Windows deixa
/// renomear um exe em execução), limpo depois por `remove_stale_old`.
pub(super) fn free_old(old: &Path) -> Result<(), String> {
    if is_link(old) {
        return Err(format!("{} is a link (refused)", old.display()));
    }
    if !old.exists() || std::fs::remove_file(old).is_ok() {
        return Ok(());
    }
    let unique = old.parent().map_or_else(
        || PathBuf::from(unique_old_name()),
        |d| d.join(unique_old_name()),
    );
    std::fs::rename(old, &unique).map_err(|e| format!("{}: {e}", old.display()))
}

/// Marca de tempo para nomes únicos.
pub(super) fn stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// `wintop.exe.old` e `wintop.exe.<pid>.<marca>.old` (sobras de trocas com o exe em uso).
pub fn is_old_exe(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "wintop.exe.old"
        || n.strip_prefix("wintop.exe.")
            .and_then(|r| r.strip_suffix(".old"))
            .is_some_and(|mid| {
                mid.split('.').count() == 2
                    && mid
                        .split('.')
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            })
}

/// Apaga (melhor esforço) as sobras `.old` da pasta instalada que ninguém usa mais.
pub fn remove_stale_old() {
    let Ok(dir) = install_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if is_old_exe(&e.file_name().to_string_lossy()) && !is_link(&p) {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// `exe.new` (já gravado) assume o lugar de `exe`; o atual vira `exe.old` (apagado se não estiver
/// em uso). Devolve `true` se havia uma instância rodando a partir de `exe`.
pub(super) fn swap_in(new: &Path, exe: &Path) -> Result<bool, String> {
    let old = exe.with_extension("exe.old");
    let mut was_running = false;
    swap_exe(
        || free_old(&old),
        || {
            if exe.exists() {
                std::fs::rename(exe, &old).map_err(|e| format!("{}: {e}", exe.display()))?;
                was_running = running_from(exe);
            }
            Ok(())
        },
        || std::fs::rename(new, exe).map_err(|e| format!("{}: {e}", exe.display())),
        || {
            let _ = std::fs::rename(&old, exe);
        },
        || {
            let _ = std::fs::remove_file(new);
        },
        || {
            let _ = std::fs::remove_file(&old);
        },
    )?;
    Ok(was_running)
}

/// Grava a lista de módulos na pasta `mdir` protegida, cada um via arquivo exclusivo + rename.
pub(super) fn write_protected_modules(
    mdir: &Path,
    modules: &[(String, Vec<u8>)],
) -> Result<(), String> {
    if is_link(mdir) {
        return Err(format!("{} is a link (refused)", mdir.display()));
    }
    std::fs::create_dir_all(mdir).map_err(|e| format!("{}: {e}", mdir.display()))?;
    for (name, data) in modules {
        if !MODULE_FILES.contains(&name.as_str()) {
            continue;
        }
        let dst = mdir.join(name);
        let tmp = mdir.join(format!("{name}.new"));
        if tmp.exists() || is_link(&tmp) {
            std::fs::remove_file(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        }
        create_protected(&tmp, data, true)?;
        let r = if is_link(&dst) {
            Err(format!("{} is a link (refused)", dst.display()))
        } else {
            std::fs::rename(&tmp, &dst).map_err(|e| format!("{}: {e}", dst.display()))
        };
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r?;
    }
    Ok(())
}

/// Atualização automática da cópia instalada (o processo elevado roda dela): o exe e os módulos
/// novos (já conferidos pela assinatura) são gravados protegidos na própria pasta e trocados no
/// lugar; a tarefa e o atalho não mudam. A ACL da pasta é conferida antes e depois.
pub fn update_installed(exe_bytes: &[u8], modules: &[(String, Vec<u8>)]) -> Result<(), String> {
    let dir = install_dir()?;
    check_tree(&dir)?;
    let exe = dir.join("wintop.exe");
    let new = exe.with_extension("exe.new");
    // Módulos antes do exe: um módulo novo com o exe antigo é inofensivo (mesmo formato), o
    // contrário pode não ser.
    write_protected_modules(&dir.join("modules"), modules)?;
    if new.exists() || is_link(&new) {
        std::fs::remove_file(&new).map_err(|e| format!("{}: {e}", new.display()))?;
    }
    create_protected(&new, exe_bytes, true)?;
    let _ = swap_in(&new, &exe)?;
    secure_owner_tree(&dir)?;
    check_tree(&dir)
}

/// Há um wintop aberto a partir de `exe` (pelo `.old` ainda travado depois da troca).
pub(super) fn running_from(exe: &Path) -> bool {
    let old = exe.with_extension("exe.old");
    // Exe em execução não pode ser apagado: se o .old some, ninguém o usava.
    old.exists() && std::fs::OpenOptions::new().write(true).open(&old).is_err()
}

/// Arquivos de `modules\` que a instalação leva (o que o wintop usa e a licença dele).
/// Os `.bin` são obrigatórios (os módulos do PawnIO: AMD e Intel).
pub const MODULE_FILES: [&str; 4] = [
    "AMDFamily17.bin",
    "IntelMSR.bin",
    "COPYING-LGPL-2.1",
    "README.md",
];

/// Teto de tamanho de cada arquivo de `modules\` lido da pasta de origem.
pub(super) const MAX_MODULE_BYTES: u64 = 16 << 20;

/// Lê a lista fechada de `modules\` na memória pelo `userfile` (sem seguir links nem hardlinks,
/// ancorando a pasta contra troca entre a validação e a leitura). O módulo é obrigatório.
pub(super) fn read_modules(from: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let src = crate::userfile::UserDir::open_exe_dir(from).map_err(|e| {
        if from.exists() {
            e
        } else {
            format!(
                "{} not found: extract the whole zip next to wintop.exe",
                from.join(MODULE_FILES[0]).display()
            )
        }
    })?;
    let mut out = Vec::new();
    for f in MODULE_FILES {
        match src.read_bytes(f, MAX_MODULE_BYTES)? {
            Some(data) => out.push((f.to_string(), data)),
            None if f.ends_with(".bin") => {
                return Err(format!(
                    "{} not found: extract the whole zip next to wintop.exe",
                    from.join(f).display()
                ));
            }
            None => {}
        }
    }
    Ok(out)
}
