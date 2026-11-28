//! Entrada em "Aplicativos instalados" e desinstalação (inclusive rodando de dentro da pasta).

use super::*;

/// Chave de "Aplicativos instalados" (Adicionar ou remover programas).
pub(super) const UNINSTALL_KEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\wintop";

/// Registra o wintop em "Aplicativos instalados": desinstalar ali roda o `--uninstall` do exe
/// instalado (o manifesto pede a elevação), com confirmação no console.
pub(super) fn register_uninstall(dir: &Path, exe: &Path) -> Result<(), String> {
    let quoted = format!("\"{}\"", exe.display());
    let size_kb = std::fs::read_dir(dir.join("modules"))
        .into_iter()
        .flatten()
        .flatten()
        .chain(std::fs::read_dir(dir).into_iter().flatten().flatten())
        .filter_map(|e| e.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|m| m.len())
        .sum::<u64>()
        / 1024;
    let key = crate::win::RegKey::create_hklm(UNINSTALL_KEY)?;
    key.set_sz("DisplayName", "wintop")?;
    key.set_sz("DisplayVersion", env!("CARGO_PKG_VERSION"))?;
    key.set_sz("Publisher", "Guilherme da Luz Weber")?;
    key.set_sz("DisplayIcon", &format!("{quoted},0"))?;
    key.set_sz("InstallLocation", &dir.display().to_string())?;
    key.set_sz("UninstallString", &format!("{quoted} --uninstall --prompt"))?;
    key.set_sz("QuietUninstallString", &format!("{quoted} --uninstall"))?;
    key.set_sz("URLInfoAbout", "https://github.com/Guilherme-Weber/wintop")?;
    key.set_dword("NoModify", 1)?;
    key.set_dword("NoRepair", 1)?;
    key.set_dword("EstimatedSize", u32::try_from(size_kb).unwrap_or(u32::MAX))?;
    Ok(())
}

/// `wintop --uninstall [--prompt]`: remove a tarefa, o atalho, a entrada de "Aplicativos
/// instalados" e a pasta instalada (inclusive quando roda a partir dela, pela desinstalação do
/// Windows). Com `--prompt`, pede confirmação e espera o Enter no fim (a janela do console é
/// aberta só para isso). O `config.toml` do usuário fica.
pub fn uninstall(prompt: bool) -> Result<(), String> {
    if !crate::win::is_elevated() {
        return Err("run --uninstall elevated".into());
    }
    if prompt {
        use std::io::Write;
        print!(
            "Remove wintop (scheduled task, Start menu shortcut and {})? [y/N] ",
            install_dir()?.display()
        );
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "s" | "S" | "yes" | "sim") {
            println!("nothing was removed");
            return Ok(());
        }
    }
    let r = uninstall_all();
    if prompt {
        match &r {
            Ok(_) => println!("\nwintop removed. Press Enter to close."),
            Err(e) => println!("\nwintop --uninstall: {e}\nPress Enter to close."),
        }
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    // Por último (depois do Enter): o exe em uso e a pasta saem quando o processo terminar.
    match r {
        Ok(Some(dir)) => schedule_dir_removal(&dir),
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove tudo; `Some(pasta)` = rodando de dentro da pasta instalada (o exe e a pasta ficam
/// para `schedule_dir_removal`).
pub(super) fn uninstall_all() -> Result<Option<PathBuf>, String> {
    let dir = install_dir()?;
    let open = other_instances(&dir.join("wintop.exe"));
    if !open.is_empty() {
        let pids: Vec<String> = open.iter().map(u32::to_string).collect();
        return Err(format!(
            "wintop is open (PID {}): close it and run the uninstall again (nothing was removed)",
            pids.join(", ")
        ));
    }
    match delete_task() {
        Ok(()) => println!("scheduled task \"{TASK}\" removed"),
        Err(e) => println!("scheduled task: {e}"),
    }
    let (base, name) = programs()?;
    if let Some(menu) = crate::userfile::UserDir::open(&base, &name, false)?
        && menu.remove(LNK)?
    {
        println!("shortcut removed");
    }
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    // Por componentes (`wintop2` não está dentro de `wintop`).
    if Path::new(&canon(&me)).starts_with(Path::new(&canon(&dir))) {
        if is_link(&dir) || dir.parent().is_some_and(is_link) {
            return Err(format!("{} is a link: refusing to remove", dir.display()));
        }
        remove_others(&dir, &me)?;
        println!(
            "{} removed (the last file goes when this window closes)",
            dir.display()
        );
        return Ok(Some(dir));
    } else if is_link(&dir) || dir.parent().is_some_and(is_link) {
        return Err(format!("{} is a link: refusing to remove", dir.display()));
    } else if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        println!("{} removed", dir.display());
    }
    // Só com a pasta removida: se ela falhasse, a entrada continua e a desinstalação pode ser
    // repetida por "Aplicativos instalados".
    remove_uninstall_entry();
    Ok(None)
}

pub(super) fn remove_uninstall_entry() {
    match crate::win::RegKey::delete_hklm(UNINSTALL_KEY) {
        Ok(true) => println!("removed from Installed apps"),
        Ok(false) => {}
        Err(e) => println!("Installed apps entry: {e}"),
    }
}

/// PIDs de outros processos rodando `exe` (pelo caminho real da imagem). Abre cada
/// `wintop.exe` só com `PROCESS_QUERY_LIMITED_INFORMATION` (sem ler memória).
pub(super) fn other_instances(exe: &Path) -> Vec<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let me = std::process::id();
    let target = canon(exe);
    let mut out = Vec::new();
    // SAFETY: sem pré-condições; o handle passa a ter dono logo abaixo.
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return out;
    };
    // SAFETY: CreateToolhelp32Snapshot teve sucesso; handle válido e sem outro dono.
    let snap = unsafe { own(snap) };
    let snap = HANDLE(snap.as_raw_handle());
    let mut e = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: snapshot válido; estrutura local com `dwSize` preenchido.
    let mut ok = unsafe { Process32FirstW(snap, &raw mut e) }.is_ok();
    while ok {
        let n = e
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(e.szExeFile.len());
        let name = String::from_utf16_lossy(&e.szExeFile[..n]);
        if e.th32ProcessID != me && name.eq_ignore_ascii_case("wintop.exe") {
            let pid = e.th32ProcessID;
            // SAFETY: só consulta; o handle passa a ter dono.
            let opened = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
            if let Ok(h) = opened {
                // SAFETY: OpenProcess teve sucesso; handle válido e sem outro dono.
                let h = unsafe { own(h) };
                if image_path(HANDLE(h.as_raw_handle())).is_ok_and(|p| canon(&p) == target) {
                    out.push(e.th32ProcessID);
                }
            }
        }
        // SAFETY: snapshot válido; estrutura local.
        ok = unsafe { Process32NextW(snap, &raw mut e) }.is_ok();
    }
    out
}

/// Remove o conteúdo da pasta instalada menos o próprio exe (em uso).
pub(super) fn remove_others(dir: &Path, me: &Path) -> Result<(), String> {
    for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let p = e.map_err(|e| e.to_string())?.path();
        if same_file(&p, me) {
            continue;
        }
        let r = if p.is_dir() && !is_link(&p) {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        r.map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

/// Apaga o exe em uso e a pasta por um `cmd.exe` do System32, desanexado, depois que este
/// processo sair.
pub(super) fn schedule_dir_removal(dir: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let cmd = crate::win::system_dir()
        .ok_or("System32 not found")?
        .join("cmd.exe");
    let reg = crate::win::system_dir()
        .ok_or("System32 not found")?
        .join("reg.exe");
    // ~3 s para este processo sair (o exe destrava), apaga a pasta e, só se ela sumiu, a entrada
    // de "Aplicativos instalados"; o caminho é o fixo da instalação (`install_dir`), já conferido
    // sem links.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let d = dir.display();
    std::process::Command::new(cmd)
        .raw_arg(format!(
            "/d /c ping -n 4 127.0.0.1 >nul & rd /s /q \"{d}\" & if not exist \"{d}\\\" \"{}\" delete \"HKLM\\{UNINSTALL_KEY}\" /f /reg:64 >nul",
            reg.display()
        ))
        .current_dir(crate::win::system_dir().ok_or("System32 not found")?)
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("cmd.exe: {e}"))?;
    Ok(())
}
