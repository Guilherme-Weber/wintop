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

use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows::core::PCWSTR;

use crate::win::{SecurityDescriptor, known_folder, sid_string, wide};

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

/// Caminho para comparar: sem diferenciar maiúsculas e com links resolvidos (como o Windows).
pub fn canon(p: &Path) -> String {
    std::fs::canonicalize(p).map_or_else(
        |_| p.to_string_lossy().to_lowercase(),
        |c| c.to_string_lossy().to_lowercase(),
    )
}

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

/// Dono = Administradores e, com `dacl`, ACL protegida herdável: SYSTEM e Administradores com
/// controle total, Usuários só leitura e execução. Num processo elevado, o dono padrão do que
/// ele cria pode ser o próprio usuário (e o CREATOR OWNER herdado de Program Files daria a ele
/// controle total: processos sem elevação poderiam trocar o executável).
fn secure(path: &Path, dacl: bool) -> Result<(), String> {
    use windows::Win32::Security::Authorization::SetNamedSecurityInfoW;
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    let sd = SecurityDescriptor::from_sddl(
        "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)",
    )?;
    let mut owner = PSID::default();
    let mut acl: *mut ACL = std::ptr::null_mut();
    let (mut present, mut defaulted) = (windows::core::BOOL(0), windows::core::BOOL(0));
    // SAFETY: descritor válido enquanto `sd` vive; os ponteiros apontam para dentro dele.
    unsafe {
        GetSecurityDescriptorOwner(sd.raw(), &raw mut owner, &raw mut defaulted).and_then(|()| {
            GetSecurityDescriptorDacl(sd.raw(), &raw mut present, &raw mut acl, &raw mut defaulted)
        })
    }
    .map_err(|e| e.to_string())?;
    let w = wide(&path.to_string_lossy());
    let info = if dacl {
        OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        OWNER_SECURITY_INFORMATION
    };
    // SAFETY: caminho terminado em zero; dono e DACL válidos dentro de `sd`.
    let e = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(w.as_ptr()),
            SE_FILE_OBJECT,
            info,
            Some(owner),
            None,
            dacl.then_some(acl.cast_const()),
            None,
        )
    };
    if e.is_ok() {
        Ok(())
    } else {
        Err(format!("{}: set security: {e:?}", path.display()))
    }
}

/// Cria `path` novo (exclusivo) já com dono Administradores e controle total só de SYSTEM e
/// Administradores: não existe instante em que o usuário seja dono (e possa mudar a ACL).
/// `users_read`: Usuários leem e executam, como no que a instalação copia (o exe e os módulos:
/// o Explorer lê o ícone e `--version` roda sem elevação); sem ele, só SYSTEM e Administradores
/// (o XML da tarefa).
fn create_protected(path: &Path, data: &[u8], users_read: bool) -> Result<(), String> {
    use std::io::Write;
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_MODE,
    };
    let sd = SecurityDescriptor::from_sddl(if users_read {
        "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;BU)"
    } else {
        "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)"
    })?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.raw().0,
        bInheritHandle: false.into(),
    };
    let w = wide(&path.to_string_lossy());
    // SAFETY: caminho terminado em zero; atributos válidos enquanto `sd` vive.
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            Some(&raw const sa),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;
    drop(sd);
    // SAFETY: handle recém-criado, só nosso; `own` o fecha se algo falhar.
    let mut f = std::fs::File::from(unsafe { crate::win::own(h) });
    f.write_all(data)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Dono = Administradores em tudo dentro de `path` (a ACL vem herdada da pasta protegida).
fn secure_owner_tree(path: &Path) -> Result<(), String> {
    if is_link(path) {
        return Err(format!("{} is a link: refusing to install", path.display()));
    }
    secure(path, false)?;
    if path.is_dir() {
        for e in std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))? {
            secure_owner_tree(&e.map_err(|e| e.to_string())?.path())?;
        }
    }
    Ok(())
}

/// Arquivo de outro programa que o wintop elevado executa ou carrega (RTSS.exe, PawnIOLib.dll):
/// nem ele nem as pastas acima dele podem ser links ou alteráveis sem elevação (um RTSS numa
/// pasta do usuário, ou abaixo de uma que o usuário renomeia, viraria um jeito de rodar código
/// como administrador pelo atalho sem UAC). A raiz do volume só não pode ser link: nela
/// Usuários criam pastas, mas não renomeiam nem apagam as existentes.
pub fn trusted_file(file: &Path) -> Result<(), String> {
    if file.parent().is_none() {
        return Err(format!("{} has no folder", file.display()));
    }
    for p in file.ancestors() {
        if is_link(p) {
            return Err(format!("{} is a link (refused)", p.display()));
        }
        if p.parent().is_none() {
            break;
        }
        let problems = unsafe_acl(p)?;
        if !problems.is_empty() {
            return Err(format!(
                "{} can be changed without elevation (refused): {}",
                p.display(),
                problems.join("; ")
            ));
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
    // SAFETY: descritor alocado por GetNamedSecurityInfoW; liberado no Drop.
    let _sd = unsafe { SecurityDescriptor::from_raw(sd) };
    let mut problems = Vec::new();
    // SAFETY: `owner` aponta para dentro do descritor, vivo até o fim da função.
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

/// Dispara a tarefa: abre a cópia instalada elevada (sem UAC).
pub fn run_task() -> Result<(), String> {
    schtasks(&["/run", "/tn", TASK])
}

/// Versão da cópia instalada (`wintop.exe --version`), se houver uma.
fn installed_version(exe: &Path) -> Option<String> {
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

/// Troca o exe instalado mesmo com o wintop aberto: o Windows deixa renomear um exe em
/// execução (não sobrescrever). O novo entra como `.new`, o atual vira `.old` e o novo assume o
/// nome; a instância aberta segue na versão antiga até fechar. O `.old` de uma troca anterior é
/// apagado antes (se ainda estiver em uso, fica para a próxima).
fn replace_exe(from: &Path, exe: &Path) -> Result<(), String> {
    let new = exe.with_extension("exe.new");
    if is_link(&new) {
        return Err(format!("{} is a link (refused)", new.display()));
    }
    let _ = std::fs::remove_file(&new);
    std::fs::copy(from, &new).map_err(|e| format!("copy to {}: {e}", new.display()))?;
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
fn free_old(old: &Path) -> Result<(), String> {
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
fn stamp() -> u128 {
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
fn swap_in(new: &Path, exe: &Path) -> Result<bool, String> {
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
fn write_protected_modules(mdir: &Path, modules: &[(String, Vec<u8>)]) -> Result<(), String> {
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
fn running_from(exe: &Path) -> bool {
    let old = exe.with_extension("exe.old");
    // Exe em execução não pode ser apagado: se o .old some, ninguém o usava.
    old.exists() && std::fs::OpenOptions::new().write(true).open(&old).is_err()
}

/// Arquivos de `modules\` que a instalação leva (o que o wintop usa e a licença dele).
pub const MODULE_FILES: [&str; 3] = ["AMDFamily17.bin", "COPYING-LGPL-2.1", "README.md"];

/// Teto de tamanho de cada arquivo de `modules\` lido da pasta de origem.
const MAX_MODULE_BYTES: u64 = 16 << 20;

/// Lê a lista fechada de `modules\` na memória pelo `userfile` (sem seguir links nem hardlinks,
/// ancorando a pasta contra troca entre a validação e a leitura). O módulo é obrigatório.
fn read_modules(from: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
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
    for (i, f) in MODULE_FILES.iter().enumerate() {
        match src.read_bytes(f, MAX_MODULE_BYTES)? {
            Some(data) => out.push(((*f).to_string(), data)),
            None if i == 0 => {
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

fn create_shortcut(lnk: &Path, exe: &Path) -> Result<(), String> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IPersistFile,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWMINNOACTIVE;
    use windows::core::Interface;

    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            // SAFETY: pareado com o `CoInitializeEx` bem-sucedido nesta thread.
            unsafe { CoUninitialize() };
        }
    }

    let schtasks = crate::win::system_dir()
        .ok_or("System32 not found")?
        .join("schtasks.exe");
    let w = |s: &str| wide(s);
    // SAFETY: COM iniciado nesta thread só para criar o atalho; `ComGuard` desfaz no Drop.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|e| format!("COM: {e}"))?;
    let _com = ComGuard;
    let r = (|| -> windows::core::Result<()> {
        // SAFETY: COM inicializado; strings UTF-16 vivas durante cada chamada; objetos liberados
        // no Drop antes do `_com`.
        unsafe {
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
        }
    })();
    r.map_err(|e| format!("shortcut {}: {e}", lnk.display()))
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
    } else {
        std::fs::create_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    // Sempre, mesmo com a pasta já existente: uma pasta criada à mão herda o CREATOR OWNER de
    // Program Files (o que o wintop criar nela seria do usuário, com controle total).
    secure(&dir, true)?;
    check_tree(&dir)?;
    if !same_file(&me, &exe) {
        // Os módulos são lidos na memória pelo `userfile` antes de tocar o destino: sem TOCTOU
        // entre a validação e a cópia, e sem deixar a instalação pela metade em caso de erro.
        let modules = read_modules(&here.join("modules"))?;
        let before = installed_version(&exe);
        write_protected_modules(&dir.join("modules"), &modules)?;
        replace_exe(&me, &exe)?;
        match before {
            Some(v) => println!(
                "updated {} ({v} -> {})",
                dir.display(),
                env!("CARGO_PKG_VERSION")
            ),
            None => println!("copied to {}", dir.display()),
        }
    }
    // Depois da cópia: tudo o que o wintop elevado executa ou carrega (o exe e os módulos do
    // PawnIO) com dono Administradores e protegido.
    secure_owner_tree(&dir)?;
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
    create_protected(&xml, &utf16, false)?;
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
    // Por componentes (`wintop2` não está dentro de `wintop`).
    if Path::new(&canon(&me)).starts_with(Path::new(&canon(&dir))) {
        println!(
            "{} is in use by this process: delete it after closing wintop",
            dir.display()
        );
    } else if is_link(&dir) || dir.parent().is_some_and(is_link) {
        return Err(format!("{} is a link: refusing to remove", dir.display()));
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

    /// Troca do exe: o novo assume o nome e não sobram `.new` nem `.old`.
    #[test]
    fn troca_do_exe_sem_sobras() {
        let dir = std::env::temp_dir().join(format!("wintop_replace_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("pasta");
        let (src, exe) = (dir.join("novo.bin"), dir.join("wintop.exe"));
        std::fs::write(&src, b"novo").expect("origem");
        std::fs::write(&exe, b"antigo").expect("instalado");
        replace_exe(&src, &exe).expect("troca");
        let left: Vec<String> = std::fs::read_dir(&dir)
            .expect("lista")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        let content = std::fs::read(&exe).expect("leitura");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(content, b"novo");
        let mut left = left;
        left.sort();
        assert_eq!(left, ["novo.bin", "wintop.exe"]);
    }

    /// Com o exe instalado em execução (aqui um ping copiado), a troca funciona: o processo
    /// aberto segue no arquivo antigo e o nome passa a ser do novo.
    #[test]
    fn troca_do_exe_em_execucao() {
        let dir = std::env::temp_dir().join(format!("wintop_running_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("pasta");
        let exe = dir.join("wintop.exe");
        let ping = crate::win::system_dir().expect("System32").join("PING.EXE");
        std::fs::copy(&ping, &exe).expect("cópia");
        let mut child = std::process::Command::new(&exe)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("processo");
        let src = dir.join("novo.bin");
        std::fs::write(&src, b"novo").expect("origem");
        let r = replace_exe(&src, &exe);
        let content = std::fs::read(&exe);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        r.expect("troca com o exe aberto");
        assert_eq!(content.expect("leitura"), b"novo");
    }

    /// Programa de outro projeto que o wintop elevado executaria: numa pasta do usuário é
    /// recusado; nas instalações reais (Program Files), aceito.
    #[test]
    fn arquivo_externo_so_de_pasta_protegida() {
        let tmp = std::env::temp_dir().join(format!("wintop_trusted_{}.exe", std::process::id()));
        std::fs::write(&tmp, b"x").expect("arquivo de teste");
        let r = trusted_file(&tmp);
        let _ = std::fs::remove_file(&tmp);
        assert!(r.is_err(), "pasta temporária do usuário aceita");
        let rtss = crate::win::reg_sz_32(r"SOFTWARE\Unwinder\RTSS", "InstallPath");
        if let Some(p) = rtss.map(PathBuf::from).filter(|p| p.is_file()) {
            trusted_file(&p).expect("RTSS em Program Files");
        }
        let pawn = crate::win::system_dir()
            .and_then(|s| {
                s.parent()?
                    .parent()
                    .map(|d| d.join(r"Program Files\PawnIO\PawnIOLib.dll"))
            })
            .filter(|p| p.is_file());
        if let Some(p) = pawn {
            trusted_file(&p).expect("PawnIO em Program Files");
        }
    }

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
