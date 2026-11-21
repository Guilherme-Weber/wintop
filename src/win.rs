//! Utilitários do Win32 usados pelos coletores.

use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::PathBuf;

use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_RT_REG_MULTI_SZ, RegGetValueW,
};
use windows::Win32::System::SystemInformation::{GetSystemDirectoryW, GetSystemWindowsDirectoryW};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;

/// Converte para UTF-16 terminado em zero.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Texto de um buffer UTF-16 terminado em zero (ou cheio).
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Dono de um `HANDLE` do Win32: fecha no `Drop` (inclusive em caminhos de erro).
///
/// # Safety
/// `h` tem de ser um handle aberto, válido (nem nulo nem `INVALID_HANDLE_VALUE`) e sem outro
/// dono: depois desta chamada só o `OwnedHandle` o fecha.
pub unsafe fn own(h: HANDLE) -> OwnedHandle {
    // SAFETY: garantido por quem chama (ver acima).
    unsafe { OwnedHandle::from_raw_handle(h.0) }
}

/// Processo elevado: DLLs carregadas em tempo de execução só de System32 (ou por caminho
/// absoluto), nunca do diretório atual nem da pasta do executável. Chamar no início de `main`.
pub fn harden_dll_search() {
    // SAFETY: sem pré-condições; afeta só as cargas seguintes deste processo.
    if let Err(e) = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } {
        eprintln!("warning: SetDefaultDllDirectories: {e}");
    }
}

/// Pasta System32 pela API (a variável de ambiente `SystemRoot` é herdável e manipulável).
pub fn system_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    // SAFETY: buffer local com o tamanho informado.
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// Letra da unidade do Windows (ex.: `'C'`).
pub fn windows_drive() -> Option<char> {
    let mut buf = [0u16; 260];
    // SAFETY: buffer local com o tamanho informado.
    let n = unsafe { GetSystemWindowsDirectoryW(Some(&mut buf)) } as usize;
    drive_letter(&String::from_utf16_lossy(buf.get(..n)?))
}

/// Letra de unidade no início de um caminho (`C:\...`, `\??\C:\...`), em maiúscula.
pub fn drive_letter(path: &str) -> Option<char> {
    let p = path.strip_prefix(r"\??\").unwrap_or(path);
    let mut it = p.chars();
    let letter = it.next()?;
    (letter.is_ascii_alphabetic() && it.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

/// Valor `REG_MULTI_SZ` de `HKLM\<key>`.
pub fn reg_multi_sz(key: &str, value: &str) -> Option<Vec<String>> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 4096];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: strings terminadas em zero; buffer local com o tamanho em bytes informado.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(k.as_ptr()),
            PCWSTR(v.as_ptr()),
            REG_ROUTINE_FLAGS(RRF_RT_REG_MULTI_SZ.0),
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut len),
        )
    };
    if r.is_err() {
        return None;
    }
    buf.truncate(len as usize / 2);
    Some(
        buf.split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect(),
    )
}

/// `true` quando o processo atual roda elevado.
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: token recebe um handle válido em caso de sucesso e passa a ter dono logo abaixo.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.is_err() {
        return false;
    }
    // SAFETY: OpenProcessToken teve sucesso; handle válido e sem outro dono.
    let _token = unsafe { own(token) };
    let mut elev = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: buffer do tamanho de TOKEN_ELEVATION.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elev).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut len,
        )
    }
    .is_ok();
    ok && elev.TokenIsElevated != 0
}

/// `FILETIME` em unidades de 100 ns.
pub fn filetime(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letra_de_unidade() {
        assert_eq!(drive_letter(r"C:\Windows"), Some('C'));
        assert_eq!(drive_letter(r"\??\d:\pagefile.sys"), Some('D'));
        assert_eq!(drive_letter(r"\\?\Volume{x}"), None);
        assert_eq!(drive_letter("1"), None);
    }

    #[test]
    fn texto_utf16() {
        assert_eq!(from_wide(&[0x41, 0x42, 0, 0x43]), "AB");
        assert_eq!(from_wide(&[0x41]), "A");
    }
}
