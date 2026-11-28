//! Instância única por sessão de usuário (requisito do usuário).
//!
//! O mutex fica num namespace privado cuja fronteira exige o SID de Administradores habilitado
//! no token: um processo sem elevação não consegue criar nem abrir esse namespace, então não tem
//! como criar o mutex antes do wintop e impedir que ele abra (o que um mutex em `Local\`
//! permitia). O nome do mutex leva a sessão do Windows (uma instância por sessão). O mutex e o
//! namespace ficam com o processo até ele terminar (inclusive com erro: o Windows libera).

use std::ffi::c_void;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::Security::{
    CreateWellKnownSid, GetTokenInformation, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TokenSessionId, WinBuiltinAdministratorsSid,
};
use windows::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateMutexW,
    CreatePrivateNamespaceW, DeleteBoundaryDescriptor, GetCurrentProcess, OpenPrivateNamespaceW,
    OpenProcessToken,
};
use windows::core::{PCWSTR, w};

pub enum Error {
    AlreadyRunning,
    Os(windows::core::Error),
}

/// Handle do namespace privado fechado no `Drop` (sem destruir: outra instância pode usar).
struct PrivateNamespace(HANDLE);

impl Drop for PrivateNamespace {
    fn drop(&mut self) {
        // SAFETY: handle de namespace aberto por `open_namespace`, fechado uma vez.
        unsafe {
            let _ = ClosePrivateNamespace(self.0, 0);
        }
    }
}

/// Mantém a instância enquanto viver: os campos caem na ordem de declaração (`_mutex` antes de
/// `_namespace`, garantindo que o mutex fecha antes do namespace).
pub struct Guard {
    _mutex: OwnedHandle,
    _namespace: PrivateNamespace,
}

const ALIAS: PCWSTR = w!("wintop");

pub fn acquire() -> Result<Guard, Error> {
    let sid = session_id()?;
    acquire_named(&format!("wintop\\instance_s{sid}"))
}

fn acquire_named(mutex_name: &str) -> Result<Guard, Error> {
    let namespace = open_namespace()?;
    let name = crate::win::wide(mutex_name);
    // SAFETY: nome terminado em zero dentro do namespace aberto; o handle passa a ter dono.
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.map_err(Error::Os)?;
    // Lido antes de qualquer outra chamada.
    // SAFETY: sem pré-condições.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let guard = Guard {
        // SAFETY: CreateMutexW devolveu um handle válido, com um único dono a partir daqui.
        _mutex: unsafe { OwnedHandle::from_raw_handle(handle.0) },
        _namespace: namespace,
    };
    if existed {
        return Err(Error::AlreadyRunning);
    }
    Ok(guard)
}

/// Cria o namespace (só Administradores e SYSTEM o acessam) ou abre o que outra instância
/// elevada já criou.
fn open_namespace() -> Result<PrivateNamespace, Error> {
    // SAFETY: nome estático; o descritor é apagado no fim desta função.
    let mut boundary = unsafe { CreateBoundaryDescriptorW(ALIAS, 0) };
    if boundary.is_invalid() {
        return Err(Error::Os(windows::core::Error::from_thread()));
    }
    let r = (|| {
        // Alinhado a 4 bytes (DWORD) para `SID`, com 68 bytes (`SECURITY_MAX_SID_SIZE`).
        let mut sid = [0u32; 17];
        let mut len = size_of_val(&sid) as u32;
        // SAFETY: buffer local alinhado com o tamanho informado.
        unsafe {
            CreateWellKnownSid(
                WinBuiltinAdministratorsSid,
                None,
                Some(PSID(sid.as_mut_ptr().cast())),
                &raw mut len,
            )
        }
        .map_err(Error::Os)?;
        // SAFETY: descritor válido; o SID é copiado para dentro dele.
        unsafe { AddSIDToBoundaryDescriptor(&raw mut boundary, PSID(sid.as_mut_ptr().cast())) }
            .map_err(Error::Os)?;
        let sd = crate::win::SecurityDescriptor::from_sddl("D:P(A;;GA;;;BA)(A;;GA;;;SY)")
            .map_err(|_| Error::Os(windows::core::Error::from_thread()))?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.raw().0,
            bInheritHandle: false.into(),
        };
        // SAFETY: atributos e descritor válidos durante a chamada (`sd` vive até o drop).
        let created =
            unsafe { CreatePrivateNamespaceW(Some(&raw const sa), boundary.0.cast_const(), ALIAS) };
        drop(sd);
        if !created.is_invalid() {
            return Ok(PrivateNamespace(created));
        }
        // Já existe (outra instância elevada, em qualquer sessão): abre o mesmo.
        // SAFETY: descritor válido.
        let opened = unsafe { OpenPrivateNamespaceW(boundary.0.cast_const(), ALIAS) };
        if opened.is_invalid() {
            return Err(Error::Os(windows::core::Error::from_thread()));
        }
        Ok(PrivateNamespace(opened))
    })();
    // SAFETY: descritor criado acima, apagado uma vez.
    unsafe { DeleteBoundaryDescriptor(boundary) };
    r
}

/// Sessão do Windows do processo atual (pelo token).
pub(crate) fn session_id() -> Result<u32, Error> {
    let mut token = HANDLE::default();
    // SAFETY: o token passa a ter dono logo abaixo.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }
        .map_err(Error::Os)?;
    // SAFETY: OpenProcessToken teve sucesso; handle válido e sem outro dono.
    let _token = unsafe { crate::win::own(token) };
    let mut id = 0u32;
    let mut len = 0u32;
    // SAFETY: buffer de 4 bytes (DWORD) com o tamanho informado.
    unsafe {
        GetTokenInformation(
            token,
            TokenSessionId,
            Some((&raw mut id).cast::<c_void>()),
            size_of::<u32>() as u32,
            &raw mut len,
        )
    }
    .map_err(Error::Os)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Processo filho do teste abaixo: tenta adquirir e diz o resultado.
    #[test]
    #[ignore = "chamado pelo teste instancia_unica_entre_processos"]
    fn sonda_filha() {
        let Ok(name) = std::env::var("WINTOP_INSTANCE_CHILD") else {
            return;
        };
        match acquire_named(&name) {
            Ok(_) => println!("CHILD:acquired"),
            Err(Error::AlreadyRunning) => println!("CHILD:already"),
            Err(Error::Os(e)) => println!("CHILD:os {e}"),
        }
    }

    fn child(name: &str) -> String {
        let out = std::process::Command::new(std::env::current_exe().expect("exe do teste"))
            .args([
                "--ignored",
                "--exact",
                "instance::tests::sonda_filha",
                "--nocapture",
            ])
            .env("WINTOP_INSTANCE_CHILD", name)
            .output()
            .expect("processo filho");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("CHILD:").map(str::to_string))
            .unwrap_or_default()
    }

    /// Com uma instância viva, outro processo da sessão recebe "já em execução"; solta a
    /// primeira, o outro adquire (nome isolado por PID para não colidir com um wintop aberto).
    #[test]
    fn instancia_unica_entre_processos() {
        // O namespace exige o SID de Administradores habilitado: só num processo elevado.
        if !crate::win::is_elevated() {
            return;
        }
        let name = format!("wintop\\instance_test_{}", std::process::id());
        let first = acquire_named(&name).ok().expect("primeira instância");
        assert_eq!(child(&name), "already");
        drop(first);
        assert_eq!(child(&name), "acquired");
    }
}
