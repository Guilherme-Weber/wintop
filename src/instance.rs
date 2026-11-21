//! Instância única por sessão de usuário (requisito do usuário): um mutex nomeado em
//! `Local\`. O mutex fica com o processo até ele terminar (inclusive se terminar com erro: o
//! Windows libera o handle).

use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

pub enum Error {
    AlreadyRunning,
    Os(windows::core::Error),
}

/// Mantém a instância enquanto viver.
pub struct Guard(#[allow(dead_code)] OwnedHandle);

pub fn acquire() -> Result<Guard, Error> {
    // SAFETY: nome estático; o handle passa a ter dono logo abaixo.
    let handle =
        unsafe { CreateMutexW(None, false, w!("Local\\wintop_instance")) }.map_err(Error::Os)?;
    // SAFETY: CreateMutexW devolveu um handle válido, com um único dono a partir daqui.
    let guard = Guard(unsafe { OwnedHandle::from_raw_handle(handle.0) });
    // SAFETY: sem pré-condições; lido logo após CreateMutexW.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(Error::AlreadyRunning);
    }
    Ok(guard)
}
