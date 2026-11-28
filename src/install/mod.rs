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
//! - o registro, a remoção e o disparo da tarefa usam a API COM nativa `ITaskService` diretamente
//!   em memória (sem gravar XML temporário em disco nem abrir subprocesso `schtasks.exe`).

use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows::core::PCWSTR;

use crate::win::{SecurityDescriptor, known_folder, own, sid_string, wide};

mod acl;
mod exe;
mod task;
mod uninstall;

pub use acl::*;
pub use exe::*;
pub use task::*;
pub use uninstall::*;

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

/// `wintop --install`: copia para `<Program Files>\wintop`, confere a ACL, cria a tarefa e o
/// atalho. Mensagens para o console (em inglês, como a interface).
pub fn install() -> Result<(), String> {
    if !crate::win::is_elevated() {
        return Err("run --install elevated (the executable asks for it)".into());
    }
    let dir = install_dir()?;
    let exe = dir.join("wintop.exe");
    let (me, exe_bytes) = read_own_exe()?;
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
        replace_exe(&exe_bytes, &exe)?;
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
    register_task(&exe, &user)?;
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
    if let Err(e) = create_shortcut(&tmp, &exe) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let bytes = std::fs::read(&tmp).map_err(|e| format!("{}: {e}", tmp.display()));
    let _ = std::fs::remove_file(&tmp);
    let (base, name) = programs()?;
    let menu = crate::userfile::UserDir::open(&base, &name, false)?
        .ok_or("Start menu folder not found")?;
    menu.write_bytes(LNK, &bytes?)?;
    println!("Start menu shortcut: {}", menu.file(LNK).display());
    register_uninstall(&dir, &exe)?;
    println!("listed in Installed apps (uninstall from there or with --uninstall)");
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

    /// Outro processo rodando o exe da pasta (uma cópia do ping.exe como wintop.exe) aparece
    /// em `other_instances`; o próprio processo e outro caminho, não.
    #[test]
    fn outras_instancias_pelo_caminho_da_imagem() {
        let dir = std::env::temp_dir().join(format!("wintop-inst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("pasta");
        let exe = dir.join("wintop.exe");
        let ping = crate::win::system_dir().expect("System32").join("PING.EXE");
        std::fs::copy(&ping, &exe).expect("cópia");
        let mut child = std::process::Command::new(&exe)
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("processo");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let found = other_instances(&exe);
        let other = other_instances(&dir.join("outro").join("wintop.exe"));
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, vec![child.id()]);
        assert!(other.is_empty());
    }

    use super::*;

    /// Troca do exe: o novo assume o nome e não sobram `.new` nem `.old`.
    #[test]
    fn troca_do_exe_sem_sobras() {
        let dir = std::env::temp_dir().join(format!("wintop_replace_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("pasta");
        let (src, exe) = (dir.join("novo.bin"), dir.join("wintop.exe"));
        std::fs::write(&src, b"novo").expect("origem");
        std::fs::write(&exe, b"antigo").expect("instalado");
        replace_exe(b"novo", &exe).expect("troca");
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
        let r = replace_exe(b"novo", &exe);
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
        // Caracteres especiais do caminho e do usuário escapados.
        let y = task_xml(Path::new(r"C:\a&b<c>\w.exe"), "S-1&<x>");
        assert!(y.contains(r"C:\a&amp;b&lt;c&gt;\w.exe"), "{y}");
        assert!(y.contains("<UserId>S-1&amp;&lt;x&gt;</UserId>"), "{y}");
    }

    #[test]
    fn swap_exe_rollback_em_cada_falha() {
        let log = std::cell::RefCell::new(Vec::new());
        let push = |s: &'static str| log.borrow_mut().push(s);
        // Sucesso: chama cleanup_old, não chama restore nem remove_new.
        assert!(
            swap_exe(
                || {
                    push("free");
                    Ok(())
                },
                || {
                    push("cur->old");
                    Ok(())
                },
                || {
                    push("new->cur");
                    Ok(())
                },
                || push("restore"),
                || push("rm_new"),
                || push("clean_old"),
            )
            .is_ok()
        );
        assert_eq!(*log.borrow(), ["free", "cur->old", "new->cur", "clean_old"]);

        // Falha em free_old: só remove_new.
        log.borrow_mut().clear();
        assert!(
            swap_exe(
                || Err("busy".into()),
                || {
                    push("cur->old");
                    Ok(())
                },
                || Ok(()),
                || push("restore"),
                || push("rm_new"),
                || push("clean_old"),
            )
            .is_err()
        );
        assert_eq!(*log.borrow(), ["rm_new"]);

        // Falha em move_current_to_old: só remove_new.
        log.borrow_mut().clear();
        assert!(
            swap_exe(
                || Ok(()),
                || Err("locked".into()),
                || {
                    push("new->cur");
                    Ok(())
                },
                || push("restore"),
                || push("rm_new"),
                || push("clean_old"),
            )
            .is_err()
        );
        assert_eq!(*log.borrow(), ["rm_new"]);

        // Falha em move_new_to_current: restaura old e remove new.
        log.borrow_mut().clear();
        assert!(
            swap_exe(
                || Ok(()),
                || Ok(()),
                || Err("rename".into()),
                || push("restore"),
                || push("rm_new"),
                || push("clean_old"),
            )
            .is_err()
        );
        assert_eq!(*log.borrow(), ["restore", "rm_new"]);
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

    /// Conexão somente leitura ao Agendador de Tarefas via COM (`ITaskService`), sem alterar
    /// nenhuma tarefa (valida que o binding COM e a inicialização funcionam sem elevação).
    #[test]
    fn task_scheduler_com_conecta_na_pasta_raiz() {
        with_task_folder("connect", |folder| {
            // SAFETY: `folder` é um `ITaskFolder` válido retornado por `GetFolder("\\")`.
            let path = unsafe { folder.Path()? };
            assert_eq!(path.to_string(), "\\");
            Ok(())
        })
        .expect("ITaskService conectado");
    }
}
