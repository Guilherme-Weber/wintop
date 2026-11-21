//! Atalho sem UAC (Etapa 7): `wintop --install` e `wintop --uninstall`.
//!
//! Como funciona: uma tarefa agendada ("wintop") com privilégio máximo, só com o login do
//! usuário e sem gatilhos, roda `C:\Program Files\wintop\wintop.exe`; o atalho do Menu Iniciar
//! chama `schtasks /run`. Assim o wintop abre elevado sem a janela do UAC.
//!
//! Isso é uma porta de elevação se alguém sem elevação puder trocar o executável. Por isso:
//! - só vale para o executável em `<Program Files>\wintop` (o instalador copia para lá);
//! - antes de criar a tarefa, a ACL da pasta e do executável é conferida: nenhuma entrada dá
//!   escrita a Todos, Usuários, Usuários autenticados, Interativo ou ao próprio usuário, e o
//!   dono é Administradores, SYSTEM ou TrustedInstaller;
//! - o XML da tarefa é escrito dentro dessa pasta protegida (nunca no `%TEMP%`, onde outro
//!   processo poderia trocá-lo entre a escrita e o `schtasks`).

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows::core::PCWSTR;

use crate::win::{known_folder, sid_string, wide};

const TASK: &str = "wintop";

/// Escrita, apagar e trocar permissões/dono (em arquivo e em pasta).
const WRITE_BITS: u32 = 0x0002 // FILE_WRITE_DATA / FILE_ADD_FILE
    | 0x0004 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
    | 0x0010 // FILE_WRITE_EA
    | 0x0040 // FILE_DELETE_CHILD
    | 0x0100 // FILE_WRITE_ATTRIBUTES
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x4000_0000 // GENERIC_WRITE
    | 0x1000_0000; // GENERIC_ALL

/// CREATOR OWNER (só em ACEs herdáveis: vale como o dono do que for criado).
const CREATOR_OWNER: &str = "S-1-3-0";
/// Donos aceitos: Administradores, SYSTEM, TrustedInstaller.
const OWNERS: [&str; 3] = [
    "S-1-5-32-544",
    "S-1-5-18",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// `<Program Files>\wintop`.
pub fn install_dir() -> Result<PathBuf, String> {
    Ok(known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramFiles)?.join("wintop"))
}

/// Pasta do Menu Iniciar do usuário: (pasta pai, nome) para o `userfile` (é do usuário e
/// redirecionável sem elevação: nada de caminho direto como administrador).
fn programs() -> Result<(PathBuf, String), String> {
    let p = known_folder(&windows::Win32::UI::Shell::FOLDERID_Programs)?;
    let name = p
        .file_name()
        .ok_or("no Start menu folder name")?
        .to_string_lossy()
        .to_string();
    let parent = p.parent().ok_or("no Start menu parent")?.to_path_buf();
    Ok((parent, name))
}

const LNK: &str = "wintop.lnk";

/// `path` é um link (junction, symlink): o instalador não segue links.
fn is_link(path: &Path) -> bool {
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW};
    let w = wide(&path.to_string_lossy());
    // SAFETY: caminho terminado em zero; o atributo é o do próprio link (não segue).
    let a = unsafe { GetFileAttributesW(PCWSTR(w.as_ptr())) };
    a != u32::MAX && a & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

/// `path` (e tudo dentro, se for pasta) sem links e sem escrita para quem não é
/// Administradores/SYSTEM/TrustedInstaller.
fn check_tree(path: &Path) -> Result<(), String> {
    if is_link(path) {
        return Err(format!("{} is a link: refusing to install", path.display()));
    }
    let problems = unsafe_acl(path)?;
    if !problems.is_empty() {
        return Err(format!(
            "{} can be changed without elevation, refusing to create an elevated task: {}",
            path.display(),
            problems.join("; ")
        ));
    }
    if path.is_dir() {
        for e in std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))? {
            let e = e.map_err(|e| e.to_string())?;
            check_tree(&e.path())?;
        }
    }
    Ok(())
}

/// Problemas de ACL que deixariam alguém sem elevação trocar `path`.
pub fn unsafe_acl(path: &Path) -> Result<Vec<String>, String> {
    let w = wide(&path.to_string_lossy());
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: caminho terminado em zero; o descritor é liberado com LocalFree abaixo.
    let r = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(w.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            Some(&raw mut dacl),
            None,
            &raw mut sd,
        )
    };
    if r.is_err() {
        return Err(format!("{}: security info: {r:?}", path.display()));
    }
    let mut problems = Vec::new();
    // SAFETY: `owner` aponta para dentro do descritor, vivo até o LocalFree.
    let owner_s = unsafe { sid_string(owner) }.unwrap_or_default();
    if !OWNERS.contains(&owner_s.as_str()) {
        problems.push(format!(
            "owner {owner_s} is not Administrators/SYSTEM/TrustedInstaller"
        ));
    }
    if dacl.is_null() {
        problems.push("no DACL (everyone has full access)".into());
    } else {
        // SAFETY: DACL válida dentro do descritor.
        let count = u32::from(unsafe { (*dacl).AceCount });
        for i in 0..count {
            let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: índice < AceCount; `ace` aponta para dentro da DACL.
            if unsafe { GetAce(dacl, i, &raw mut ace) }.is_err() {
                // Falha fechada: uma ACE que não dá para ler pode ser justamente a que permite.
                problems.push(format!("ACE {i} could not be read"));
                continue;
            }
            // SAFETY: toda ACE começa com o cabeçalho.
            let h = unsafe { *ace.cast::<ACE_HEADER>() };
            const ACCESS_ALLOWED: u8 = 0;
            const ACCESS_DENIED: u8 = 1;
            const INHERIT_ONLY: u8 = 0x08;
            match h.AceType {
                ACCESS_ALLOWED => {}
                ACCESS_DENIED => continue,
                // Objeto, condicional, callback...: não avaliados = recusados.
                t => {
                    problems.push(format!("ACE {i} of type {t} is not evaluated"));
                    continue;
                }
            }
            let a = ace.cast::<ACCESS_ALLOWED_ACE>();
            // SAFETY: ACE de permissão: a máscara e o SID seguem o cabeçalho.
            let mask = unsafe { (*a).Mask };
            // SAFETY: o SID começa em `SidStart`, dentro da ACE.
            let sid = PSID(unsafe { (&raw mut (*a).SidStart).cast() });
            // SAFETY: SID válido dentro da ACE.
            let Some(s) = (unsafe { sid_string(sid) }) else {
                problems.push(format!("ACE {i} has an unreadable SID"));
                continue;
            };
            // Lista branca: só os donos aceitos podem escrever (também nas ACEs que valem só
            // para o que for criado dentro: o XML da tarefa e os módulos herdam delas). A
            // exceção é CREATOR OWNER herdável: o dono do que o instalador cria é Administradores.
            if h.AceFlags & INHERIT_ONLY != 0 && s == CREATOR_OWNER {
                continue;
            }
            if mask & WRITE_BITS != 0 && !OWNERS.contains(&s.as_str()) {
                problems.push(format!("{s} can write ({mask:#x})"));
            }
        }
    }
    // SAFETY: descritor alocado pela API; liberado uma vez.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    Ok(problems)
}

/// XML da tarefa (o `schtasks` quer UTF-16). `user`: SID de quem roda.
pub fn task_xml(exe: &Path, user: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>wintop elevated without the UAC prompt (wintop --install).</Description>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
    </Exec>
  </Actions>
</Task>
"#,
        esc(&exe.to_string_lossy())
    )
}

fn schtasks(args: &[&str]) -> Result<(), String> {
    let exe = crate::win::system_dir()
        .ok_or("System32 not found")?
        .join("schtasks.exe");
    let out = std::process::Command::new(&exe)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "schtasks {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Copia `from` para `to` (pastas recursivamente).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(drop)
    }
}

fn create_shortcut(lnk: &Path, exe: &Path) -> Result<(), String> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IPersistFile,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWMINNOACTIVE;
    use windows::core::Interface;
    let schtasks = crate::win::system_dir()
        .ok_or("System32 not found")?
        .join("schtasks.exe");
    let w = |s: &str| wide(s);
    // SAFETY: COM iniciado nesta thread só para criar o atalho; objetos liberados no Drop.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| format!("COM: {e}"))?;
        let r = (|| -> windows::core::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            let target = w(&schtasks.to_string_lossy());
            link.SetPath(PCWSTR(target.as_ptr()))?;
            let args = w(&format!("/run /tn \"{TASK}\""));
            link.SetArguments(PCWSTR(args.as_ptr()))?;
            let icon = w(&exe.to_string_lossy());
            link.SetIconLocation(PCWSTR(icon.as_ptr()), 0)?;
            let desc = w("wintop (elevated, no UAC prompt)");
            link.SetDescription(PCWSTR(desc.as_ptr()))?;
            // O schtasks só dispara a tarefa: janela minimizada (sem piscar).
            link.SetShowCmd(SW_SHOWMINNOACTIVE)?;
            let file: IPersistFile = link.cast()?;
            let p = w(&lnk.to_string_lossy());
            file.Save(PCWSTR(p.as_ptr()), true)
        })();
        CoUninitialize();
        r.map_err(|e| format!("shortcut {}: {e}", lnk.display()))
    }
}

/// `wintop --install`: copia para `<Program Files>\wintop`, confere a ACL, cria a tarefa e o
/// atalho. Mensagens para o console (em inglês, como a interface).
pub fn install() -> Result<(), String> {
    if !crate::win::is_elevated() {
        return Err("run --install elevated (the executable asks for it)".into());
    }
    let dir = install_dir()?;
    let exe = dir.join("wintop.exe");
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let here = me.parent().ok_or("no executable folder")?;
    // Antes de gravar qualquer coisa: a pasta pai (Program Files) e a de destino, se já existe,
    // sem links e sem escrita para quem não é administrador (senão a cópia em si já seria
    // gravar como administrador num lugar que outro controla).
    let parent = dir.parent().ok_or("no Program Files folder")?;
    if is_link(parent) {
        return Err(format!(
            "{} is a link: refusing to install",
            parent.display()
        ));
    }
    let p = unsafe_acl(parent)?;
    if !p.is_empty() {
        return Err(format!(
            "{} is writable without elevation: {}",
            parent.display(),
            p.join("; ")
        ));
    }
    if dir.exists() {
        check_tree(&dir).map_err(|e| format!("{e} (remove {} and try again)", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    check_tree(&dir)?;
    if !same_file(&me, &exe) {
        std::fs::copy(&me, &exe).map_err(|e| format!("copy to {}: {e}", exe.display()))?;
        let modules = here.join("modules");
        if modules.is_dir() {
            copy_tree(&modules, &dir.join("modules")).map_err(|e| format!("modules: {e}"))?;
        }
        println!("copied to {}", dir.display());
    }
    // Depois da cópia: tudo o que o wintop elevado executa ou carrega (o exe e os módulos do
    // PawnIO) continua protegido.
    check_tree(&dir)?;
    let user = crate::win::user_sid().ok_or("could not read the user SID")?;
    let xml = dir.join("wintop-task.xml");
    let utf16: Vec<u8> = std::iter::once(0xFEFFu16)
        .chain(task_xml(&exe, &user).encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    // Arquivo novo e exclusivo (um que já exista, ou um link no lugar, é apagado antes).
    if xml.exists() || is_link(&xml) {
        std::fs::remove_file(&xml).map_err(|e| format!("{}: {e}", xml.display()))?;
    }
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&xml)
            .map_err(|e| format!("{}: {e}", xml.display()))?;
        f.write_all(&utf16)
            .map_err(|e| format!("{}: {e}", xml.display()))?;
    }
    check_tree(&xml)?;
    let r = schtasks(&["/create", "/tn", TASK, "/xml", &xml.to_string_lossy(), "/f"]);
    let _ = std::fs::remove_file(&xml);
    r?;
    println!(
        "scheduled task \"{TASK}\" created (runs {} elevated)",
        exe.display()
    );
    // O atalho é gerado na pasta protegida e copiado para o Menu Iniciar pelo `userfile`
    // (sem seguir links nem redirecionamentos da pasta do usuário).
    let tmp = dir.join("wintop.lnk.tmp");
    if tmp.exists() || is_link(&tmp) {
        std::fs::remove_file(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    }
    create_shortcut(&tmp, &exe)?;
    let bytes = std::fs::read(&tmp).map_err(|e| format!("{}: {e}", tmp.display()));
    let _ = std::fs::remove_file(&tmp);
    let (base, name) = programs()?;
    let menu = crate::userfile::UserDir::open(&base, &name, false)?
        .ok_or("Start menu folder not found")?;
    menu.write_bytes(LNK, &bytes?)?;
    println!("Start menu shortcut: {}", menu.file(LNK).display());
    Ok(())
}

/// `wintop --uninstall`: remove a tarefa, o atalho e a pasta instalada.
pub fn uninstall() -> Result<(), String> {
    if !crate::win::is_elevated() {
        return Err("run --uninstall elevated".into());
    }
    match schtasks(&["/delete", "/tn", TASK, "/f"]) {
        Ok(()) => println!("scheduled task \"{TASK}\" removed"),
        Err(e) => println!("scheduled task: {e}"),
    }
    let (base, name) = programs()?;
    if let Some(menu) = crate::userfile::UserDir::open(&base, &name, false)?
        && menu.remove(LNK)?
    {
        println!("shortcut removed");
    }
    let dir = install_dir()?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    // Comparação sem diferenciar maiúsculas e com links resolvidos (como o Windows).
    let canon = |p: &Path| {
        std::fs::canonicalize(p).map_or_else(
            |_| p.to_string_lossy().to_lowercase(),
            |c| c.to_string_lossy().to_lowercase(),
        )
    };
    // Por componentes (`wintop2` não está dentro de `wintop`).
    if Path::new(&canon(&me)).starts_with(Path::new(&canon(&dir))) {
        println!(
            "{} is in use by this process: delete it after closing wintop",
            dir.display()
        );
    } else if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        println!("{} removed", dir.display());
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_da_tarefa() {
        let x = task_xml(
            Path::new(r"C:\Program Files\wintop\wintop.exe"),
            "S-1-5-21-1-2-3-1001",
        );
        assert!(x.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(x.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(
            x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
            "sem limite de tempo"
        );
        assert!(x.contains(r"<Command>C:\Program Files\wintop\wintop.exe</Command>"));
        assert!(!x.contains("<Triggers>"), "só sob demanda");
        // Caracteres especiais do caminho escapados.
        let y = task_xml(Path::new(r"C:\a&b<c>\w.exe"), "S-1");
        assert!(y.contains(r"C:\a&amp;b&lt;c&gt;\w.exe"), "{y}");
    }

    #[test]
    fn acl_da_system32_e_segura_e_a_da_pasta_do_usuario_nao() {
        let sys = crate::win::system_dir().expect("System32");
        assert_eq!(unsafe_acl(&sys).expect("acl"), Vec::<String>::new());
        // Program Files (a pasta pai da instalação) também.
        let pf = install_dir().expect("Program Files");
        assert_eq!(
            unsafe_acl(pf.parent().expect("pai")).expect("acl"),
            Vec::<String>::new()
        );
        let mine = std::env::temp_dir().join(format!("wintop_acl_{}", std::process::id()));
        std::fs::create_dir_all(&mine).expect("pasta");
        let p = unsafe_acl(&mine).expect("acl");
        let _ = std::fs::remove_dir_all(&mine);
        assert!(!p.is_empty(), "pasta do usuário é gravável sem elevação");
    }
}
