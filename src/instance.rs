//! Instância única por sessão de usuário (requisito do usuário).
//!
//! O mutex fica num namespace privado cuja fronteira exige o SID de Administradores habilitado
//! no token: um processo sem elevação não consegue criar nem abrir esse namespace, então não tem
//! como criar o mutex antes do wintop e impedir que ele abra (o que um mutex em `Local\`
//! permitia). O nome do mutex leva a sessão do Windows (uma instância por sessão). O mutex e o
//! namespace ficam com o processo até ele terminar (inclusive com erro: o Windows libera).

use std::ffi::c_void;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    CreateWellKnownSid, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TokenSessionId, WinBuiltinAdministratorsSid,
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

/// Mantém a instância enquanto viver: o mutex e depois o namespace (nessa ordem no Drop).
pub struct Guard {
    _mutex: OwnedHandle,
    namespace: HANDLE,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: handle de namespace aberto por `acquire`, fechado uma vez (sem destruí-lo:
        // outra instância pode estar usando).
        unsafe {
            let _ = ClosePrivateNamespace(self.namespace, 0);
        }
    }
}

const ALIAS: PCWSTR = w!("wintop");

pub fn acquire() -> Result<Guard, Error> {
    let namespace = open_namespace()?;
    let name = crate::win::wide(&format!("wintop\\instance_s{}", session_id()?));
    // SAFETY: nome terminado em zero dentro do namespace aberto; o handle passa a ter dono.
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) };
    let handle = match handle {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: namespace aberto acima, fechado uma vez.
            unsafe {
                let _ = ClosePrivateNamespace(namespace, 0);
            }
            return Err(Error::Os(e));
        }
    };
    // Lido antes de qualquer outra chamada.
    // SAFETY: sem pré-condições.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let guard = Guard {
        // SAFETY: CreateMutexW devolveu um handle válido, com um único dono a partir daqui.
        _mutex: unsafe { OwnedHandle::from_raw_handle(handle.0) },
        namespace,
    };
    if existed {
        return Err(Error::AlreadyRunning);
    }
    Ok(guard)
}

/// Cria o namespace (só Administradores e SYSTEM o acessam) ou abre o que outra instância
/// elevada já criou.
fn open_namespace() -> Result<HANDLE, Error> {
    // SAFETY: nome estático; o descritor é apagado no fim desta função.
    let mut boundary = unsafe { CreateBoundaryDescriptorW(ALIAS, 0) };
    if boundary.is_invalid() {
        return Err(Error::Os(windows::core::Error::from_thread()));
    }
    let r = (|| {
        let mut sid = [0u8; 68]; // SECURITY_MAX_SID_SIZE
        let mut len = sid.len() as u32;
        // SAFETY: buffer local com o tamanho informado.
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
        let sddl = w!("D:P(A;;GA;;;BA)(A;;GA;;;SY)");
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: SDDL estático; o descritor é liberado abaixo.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl,
                SDDL_REVISION_1,
                &raw mut sd,
                None,
            )
        }
        .map_err(Error::Os)?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: atributos e descritor válidos durante a chamada.
        let created =
            unsafe { CreatePrivateNamespaceW(Some(&raw const sa), boundary.0.cast_const(), ALIAS) };
        // SAFETY: descritor alocado pela API; liberado uma vez.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        if !created.is_invalid() {
            return Ok(created);
        }
        // Já existe (outra instância elevada, em qualquer sessão): abre o mesmo.
        // SAFETY: descritor válido.
        let opened = unsafe { OpenPrivateNamespaceW(boundary.0.cast_const(), ALIAS) };
        if opened.is_invalid() {
            return Err(Error::Os(windows::core::Error::from_thread()));
        }
        Ok(opened)
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
        if std::env::var_os("WINTOP_INSTANCE_CHILD").is_none() {
            return;
        }
        match acquire() {
            Ok(_) => println!("CHILD:acquired"),
            Err(Error::AlreadyRunning) => println!("CHILD:already"),
            Err(Error::Os(e)) => println!("CHILD:os {e}"),
        }
    }

    fn child() -> String {
        let out = std::process::Command::new(std::env::current_exe().expect("exe do teste"))
            .args([
                "--ignored",
                "--exact",
                "instance::tests::sonda_filha",
                "--nocapture",
            ])
            .env("WINTOP_INSTANCE_CHILD", "1")
            .output()
            .expect("processo filho");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("CHILD:").map(str::to_string))
            .unwrap_or_default()
    }

    /// Com uma instância viva, outro processo da sessão recebe "já em execução"; solta a
    /// primeira, o outro adquire.
    #[test]
    fn instancia_unica_entre_processos() {
        // O namespace exige o SID de Administradores habilitado: só num processo elevado.
        if !crate::win::is_elevated() {
            return;
        }
        let first = acquire().ok().expect("primeira instância");
        assert_eq!(child(), "already");
        drop(first);
        assert_eq!(child(), "acquired");
    }
}
