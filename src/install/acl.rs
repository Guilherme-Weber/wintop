//! ACL e links: o que o wintop elevado executa ou carrega só pode ficar em pasta que ninguém
//! sem elevação altera.

use super::*;

/// Escrita, apagar e trocar permissões/dono (em arquivo e em pasta).
pub(super) const WRITE_BITS: u32 = 0x0002 // FILE_WRITE_DATA / FILE_ADD_FILE
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
pub(super) const CREATOR_OWNER: &str = "S-1-3-0";
/// Donos aceitos: Administradores, SYSTEM, TrustedInstaller.
pub(super) const OWNERS: [&str; 3] = [
    "S-1-5-32-544",
    "S-1-5-18",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// `path` é um link (junction, symlink): o instalador não segue links.
pub(super) fn is_link(path: &Path) -> bool {
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW};
    let w = wide(&path.to_string_lossy());
    // SAFETY: caminho terminado em zero; o atributo é o do próprio link (não segue).
    let a = unsafe { GetFileAttributesW(PCWSTR(w.as_ptr())) };
    a != u32::MAX && a & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

/// `path` (e tudo dentro, se for pasta) sem links e sem escrita para quem não é
/// Administradores/SYSTEM/TrustedInstaller.
pub(super) fn check_tree(path: &Path) -> Result<(), String> {
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
pub(super) fn secure(path: &Path, dacl: bool) -> Result<(), String> {
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
pub(super) fn create_protected(path: &Path, data: &[u8], users_read: bool) -> Result<(), String> {
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
    if let Err(e) = f.write_all(data) {
        drop(f);
        let _ = std::fs::remove_file(path);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

/// Dono = Administradores em tudo dentro de `path` (a ACL vem herdada da pasta protegida).
pub(super) fn secure_owner_tree(path: &Path) -> Result<(), String> {
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
    if !crate::win::is_local_absolute(file) {
        return Err(format!("{} is not a local path (refused)", file.display()));
    }
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
