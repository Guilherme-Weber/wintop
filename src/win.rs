//! Utilitários do Win32 usados pelos coletores.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;

use std::ffi::c_void;
use windows::Win32::Foundation::{FILETIME, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenUser,
};
use windows::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_NOEXPAND, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ,
    RRF_SUBKEY_WOW6432KEY, RegGetValueW,
};
use windows::Win32::System::SystemInformation::{GetSystemDirectoryW, GetSystemWindowsDirectoryW};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

/// Converte para UTF-16 terminado em zero.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Converte um `OsStr` para UTF-16 terminado em zero sem perda de surrogates.
pub fn wide_os(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Texto de um buffer UTF-16 terminado em zero (ou cheio).
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Texto vindo de fora (nomes de dispositivos, interfaces): controles viram '?', para não
/// chegarem ao terminal como sequências.
pub fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Comparação de prefixo ASCII sem diferenciar maiúsculas e sem alocar `String`.
pub fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
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

/// Dono de um `PSECURITY_DESCRIPTOR` alocado pela API Win32: libera com `LocalFree` no `Drop`.
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    /// Constrói a partir de uma string SDDL (`ConvertStringSecurityDescriptorToSecurityDescriptorW`).
    pub fn from_sddl(sddl: &str) -> Result<Self, String> {
        let w = wide(sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: SDDL terminado em zero; `sd` passa a ser liberado pelo Drop de `Self`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(w.as_ptr()),
                SDDL_REVISION_1,
                &raw mut sd,
                None,
            )
        }
        .map_err(|e| format!("SDDL: {e}"))?;
        Ok(Self(sd))
    }

    /// Assume a posse de um descritor alocado pela API (ex.: `GetSecurityInfo`,
    /// `GetNamedSecurityInfoW`).
    ///
    /// # Safety
    /// `raw` tem de ter sido alocado para liberação por `LocalFree` (ou ser nulo) e não ter outro
    /// dono.
    pub unsafe fn from_raw(raw: PSECURITY_DESCRIPTOR) -> Self {
        Self(raw)
    }

    pub fn raw(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: descritor alocado pela API Win32 e liberado uma única vez.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.0)));
            }
        }
    }
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

/// Pasta do Windows pela API (como `system_dir`).
pub fn windows_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    // SAFETY: buffer local com o tamanho informado.
    let n = unsafe { GetSystemWindowsDirectoryW(Some(&mut buf)) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// Caminho com os nomes curtos 8.3 expandidos (`RUNNER~1` -> `runneradmin`), sem resolver links;
/// o original se a expansão falhar. A conferência do caminho final do `userfile` compara com o
/// nome longo.
pub fn long_path(p: &std::path::Path) -> PathBuf {
    use windows::Win32::Storage::FileSystem::GetLongPathNameW;
    // Só caminho local: o processo elevado não toca um compartilhamento de rede nem um
    // dispositivo para expandir nomes (o que não é local é recusado depois, sem acesso).
    if !is_local_absolute(p) {
        return p.to_path_buf();
    }
    let w = wide(&p.to_string_lossy());
    let mut buf = vec![0u16; 512];
    for _ in 0..2 {
        // SAFETY: entrada terminada em zero; buffer local com o tamanho informado.
        let n = unsafe { GetLongPathNameW(PCWSTR(w.as_ptr()), Some(&mut buf)) } as usize;
        if n == 0 {
            break;
        }
        if n < buf.len() {
            return PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        }
        buf.resize(n, 0);
    }
    p.to_path_buf()
}

/// Letra da unidade do Windows (ex.: `'C'`).
pub fn windows_drive() -> Option<char> {
    windows_dir().and_then(|d| drive_letter(&d.to_string_lossy()))
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

fn reg_sz_with_flags(key: &str, value: &str, extra: REG_ROUTINE_FLAGS) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: strings terminadas em zero; buffer local com o tamanho em bytes informado.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(k.as_ptr()),
            PCWSTR(v.as_ptr()),
            // NOEXPAND: REG_EXPAND_SZ é recusado (o ambiente é herdado de quem abriu o wintop).
            REG_ROUTINE_FLAGS(RRF_RT_REG_SZ.0 | RRF_NOEXPAND.0 | extra.0),
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut len),
        )
    };
    if r.is_err() {
        return None;
    }
    let s = from_wide(&buf[..(len as usize / 2).min(buf.len())]);
    (!s.is_empty()).then_some(s)
}

/// Valor `REG_SZ` de `HKLM\<key>` na visão nativa (64 bits), recusando `REG_EXPAND_SZ`.
pub fn reg_sz(key: &str, value: &str) -> Option<String> {
    reg_sz_with_flags(key, value, REG_ROUTINE_FLAGS(0))
}

/// Valor `REG_SZ` de `HKLM\<key>` na visão de 32 bits (`WOW6432Node`), como os programas de
/// 32 bits (RTSS) gravam.
pub fn reg_sz_32(key: &str, value: &str) -> Option<String> {
    reg_sz_with_flags(key, value, RRF_SUBKEY_WOW6432KEY)
}

/// Caminho local e absoluto (`C:\...`): sem UNC (`\\servidor\...`), sem `\\?\` e sem `..`.
/// Para caminhos lidos do registro antes de carregar ou executar algo elevado.
pub fn is_local_absolute(p: &std::path::Path) -> bool {
    use std::path::{Component, Prefix};
    let mut c = p.components();
    // A raiz sozinha (`C:\`) não vale: pelo menos uma pasta.
    matches!(c.next(), Some(Component::Prefix(x)) if matches!(x.kind(), Prefix::Disk(_)))
        && matches!(c.next(), Some(Component::RootDir))
        && c.next().is_some_and(|x| matches!(x, Component::Normal(_)))
        && c.all(|x| matches!(x, Component::Normal(_)))
}

/// Token do processo atual aberto para leitura (`TOKEN_QUERY`).
fn current_token() -> Option<OwnedHandle> {
    let mut token = HANDLE::default();
    // SAFETY: `token` recebe um handle válido em caso de sucesso e passa a ter dono logo abaixo.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.ok()?;
    // SAFETY: OpenProcessToken teve sucesso; handle válido e sem outro dono.
    Some(unsafe { own(token) })
}

/// `true` quando o processo atual roda elevado.
pub fn is_elevated() -> bool {
    let Some(token) = current_token() else {
        return false;
    };
    let mut elev = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: token válido; buffer do tamanho de TOKEN_ELEVATION.
    let ok = unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenElevation,
            Some((&raw mut elev).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut len,
        )
    }
    .is_ok();
    ok && elev.TokenIsElevated != 0
}

/// Pasta conhecida do Windows (`FOLDERID_*`) pela API, não por variável de ambiente.
pub fn known_folder(id: &windows::core::GUID) -> Result<PathBuf, String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    // SAFETY: GUID válido; a string devolvida é liberada com CoTaskMemFree.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)
            .map_err(|e| format!("known folder: {e}"))?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0.cast()));
        s.map(PathBuf::from).map_err(|e| e.to_string())
    }
}

/// SID em texto (S-1-5-...), ou `None`.
///
/// # Safety
/// `sid` aponta para um SID válido durante a chamada.
pub unsafe fn sid_string(sid: PSID) -> Option<String> {
    let mut text = PWSTR::null();
    // SAFETY: SID válido (garantido por quem chama); o texto é liberado com LocalFree.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }.ok()?;
    // SAFETY: `text` aponta para uma string terminada em zero alocada pela API.
    let s = unsafe { text.to_string() }.ok();
    // SAFETY: liberado uma única vez.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    s
}

/// SID do usuário do token atual, em texto (S-1-5-21-...).
pub fn user_sid() -> Option<String> {
    let token = current_token()?;
    // TOKEN_USER (16 bytes) + SECURITY_MAX_SID_SIZE (68 bytes) = 84 bytes; 128 bytes na pilha.
    let mut buf = [0u64; 16];
    let mut len = 0u32;
    // SAFETY: token válido; buffer local alinhado com o tamanho informado.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            size_of_val(&buf) as u32,
            &raw mut len,
        )
    }
    .ok()?;
    // SAFETY: GetTokenInformation preencheu um TOKEN_USER no início do buffer alinhado.
    let user = unsafe { buf.as_ptr().cast::<TOKEN_USER>().read() };
    // SAFETY: SID válido enquanto `buf` vive.
    unsafe { sid_string(user.User.Sid) }
}

/// Pasta do perfil do usuário do token atual (ex.: `C:\Users\nome`). Vem do `ProfileList` do
/// HKLM (não do ambiente nem do HKCU): o usuário não consegue redirecioná-la.
pub fn profile_dir() -> Option<PathBuf> {
    use windows::Win32::UI::Shell::GetUserProfileDirectoryW;
    let token = current_token()?;
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: token válido; buffer local com o tamanho (em caracteres) informado.
    unsafe {
        GetUserProfileDirectoryW(
            HANDLE(token.as_raw_handle()),
            Some(PWSTR(buf.as_mut_ptr())),
            &raw mut len,
        )
    }
    .ok()?;
    let n = buf.iter().position(|&c| c == 0)?;
    let p = PathBuf::from(String::from_utf16(&buf[..n]).ok()?);
    is_local_absolute(&p).then_some(p)
}

/// Maximiza a janela do console clássico (conhost, onde o atalho sem UAC abre o wintop com
/// 120x30). No Windows Terminal a janela do console é um pseudoconsole oculto: nada muda.
/// Devolve se maximizou.
pub fn maximize_classic_console() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{SW_MAXIMIZE, ShowWindow};
    let Some(hwnd) = classic_console_window() else {
        return false;
    };
    // SAFETY: janela do console deste processo. O retorno diz se ela estava visível antes,
    // não se deu certo.
    let _ = unsafe { ShowWindow(hwnd, SW_MAXIMIZE) };
    true
}

/// O processo está no console clássico (conhost), que não faz fallback de fonte. No Windows
/// Terminal e em outros pseudoconsoles a janela do console é oculta, de outra classe.
pub fn is_classic_console() -> bool {
    classic_console_window().is_some()
}

fn classic_console_window() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::System::Console::GetConsoleWindow;
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    // SAFETY: sem pré-condições; janela nula = sem console.
    let hwnd = unsafe { GetConsoleWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    let mut buf = [0u16; 64];
    // SAFETY: janela válida e buffer local com o tamanho informado.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    (String::from_utf16_lossy(&buf[..n.min(buf.len())]) == "ConsoleWindowClass").then_some(hwnd)
}

/// Taxa de atualização (Hz) do monitor principal, pelo modo de vídeo atual. `None` quando o
/// Windows não informa (0 ou 1 = "padrão do hardware").
pub fn primary_refresh_hz() -> Option<u32> {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `dm` é um DEVMODEW válido com `dmSize` preenchido; dispositivo nulo = o monitor
    // da thread atual (o principal).
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut dm) };
    (ok.as_bool() && dm.dmDisplayFrequency > 1).then_some(dm.dmDisplayFrequency)
}

/// `FILETIME` em unidades de 100 ns.
pub fn filetime(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Caminhos quaisquer (do registro, no processo elevado): o que passa no filtro é absoluto,
    /// com letra de unidade, sem `..`/`.` e sem prefixo de dispositivo ou rede.
    #[test]
    fn fuzz_caminhos() {
        use std::path::{Component, Path};
        let seeds = [
            r"C:\Program Files\PawnIO",
            r"\??\D:\pagefile.sys",
            r"\\srv\s\x",
        ];
        crate::fuzz::run_text("caminhos", &seeds, 300, |s| {
            if let Some(c) = drive_letter(s) {
                assert!(c.is_ascii_uppercase());
            }
            let p = Path::new(s);
            if is_local_absolute(p) {
                assert!(drive_letter(s).is_some() && !s.starts_with(r"\\"));
                assert!(
                    p.components()
                        .all(|c| !matches!(c, Component::ParentDir | Component::CurDir))
                );
            }
        });
    }

    /// Nome curto 8.3 vira o longo (o teste de troca portátil quebrava na CI com `RUNNER~1`);
    /// caminho inexistente volta igual.
    #[test]
    fn caminho_longo() {
        use windows::Win32::Storage::FileSystem::GetShortPathNameW;
        let dir = std::env::temp_dir().join(format!("wintop_nome_longo_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = wide(&dir.to_string_lossy());
        let mut buf = [0u16; 512];
        // SAFETY: entrada terminada em zero; buffer local com o tamanho informado.
        let n = unsafe { GetShortPathNameW(PCWSTR(w.as_ptr()), Some(&mut buf)) } as usize;
        let short = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        // O próprio TEMP pode vir curto (CI: RUNNER~1): compara com a forma longa dele.
        let long = long_path(&short);
        assert_eq!(long, long_path(&dir), "curto: {}", short.display());
        assert!(!long.to_string_lossy().contains('~'), "{}", long.display());
        let none = dir.join("nao-existe");
        assert_eq!(long_path(&none), none);
        let unc = PathBuf::from(r"\\127.0.0.1\c$\PROGRA~1");
        assert_eq!(long_path(&unc), unc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn letra_de_unidade() {
        assert_eq!(drive_letter(r"C:\Windows"), Some('C'));
        assert_eq!(drive_letter(r"\??\d:\pagefile.sys"), Some('D'));
        assert_eq!(drive_letter(r"\\?\Volume{x}"), None);
        assert_eq!(drive_letter("1"), None);
    }

    #[test]
    fn caminho_local_absoluto() {
        use std::path::Path;
        assert!(is_local_absolute(Path::new(r"C:\Program Files\PawnIO")));
        assert!(!is_local_absolute(Path::new(r"\\server\share\RTSS.exe")));
        assert!(!is_local_absolute(Path::new(r"\\?\C:\x")));
        assert!(!is_local_absolute(Path::new(r"C:\a\..\b")));
        assert!(!is_local_absolute(Path::new(r"C:relative")));
        assert!(!is_local_absolute(Path::new(r"relative\x")));
        assert!(!is_local_absolute(Path::new(r"C:\")));
    }

    #[test]
    fn texto_utf16() {
        assert_eq!(from_wide(&[0x41, 0x42, 0, 0x43]), "AB");
        assert_eq!(from_wide(&[0x41]), "A");
    }
}
