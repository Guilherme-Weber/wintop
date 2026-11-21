//! Arquivos do usuário lidos e gravados pelo wintop **elevado** (o `config.toml`).
//!
//! Com o atalho sem UAC, qualquer processo sem elevação do usuário consegue disparar o wintop
//! elevado; e a pasta `%APPDATA%\wintop` é desse usuário. Um caminho comum seria uma porta para
//! gravar como administrador onde o usuário não pode (junction na pasta, symlink do gerenciador
//! de objetos, AppData redirecionado para uma pasta do sistema). Por isso:
//!
//! - toda abertura usa `FILE_FLAG_OPEN_REPARSE_POINT` (um reparse point é recusado, não seguido)
//!   e confere o caminho final do handle contra o esperado;
//! - a pasta base (`AppData\Roaming`) tem de ter o próprio usuário como dono (AppData apontado
//!   para uma pasta do sistema é recusado);
//! - as pastas ficam abertas sem `FILE_SHARE_DELETE` durante a operação (não podem ser trocadas
//!   por uma junction no meio dela);
//! - a gravação vai para um temporário novo e exclusivo (`CREATE_NEW`) e é trocada pelo próprio
//!   handle (`FileRenameInfo`), nunca reabrindo pelo caminho;
//! - a leitura usa o handle já conferido, com teto de tamanho.

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_CREATION_DISPOSITION,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_NAME_NORMALIZED, FILE_RENAME_INFO, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileRenameInfo, GetFileInformationByHandle, GetFinalPathNameByHandleW, OPEN_EXISTING,
    SetFileInformationByHandle,
};
use windows::core::PCWSTR;

use crate::win::wide;

/// Handle aberto sem seguir reparse points, conferido.
struct Checked {
    handle: OwnedHandle,
}

impl Checked {
    fn raw(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }
}

/// Caminho final do handle, sem o prefixo `\\?\`, em minúsculas (para comparar).
fn final_path(h: HANDLE) -> Result<String, String> {
    let mut buf = vec![0u16; 1024];
    // SAFETY: handle válido; buffer local com o tamanho informado.
    let n = unsafe { GetFinalPathNameByHandleW(h, &mut buf, FILE_NAME_NORMALIZED) } as usize;
    if n == 0 || n >= buf.len() {
        return Err("could not resolve the final path".into());
    }
    let s = String::from_utf16_lossy(&buf[..n]);
    Ok(norm(s.strip_prefix(r"\\?\").unwrap_or(&s)))
}

fn norm(p: &str) -> String {
    p.trim_end_matches('\\').to_lowercase()
}

/// Abre `path` sem seguir reparse points e confere que o handle é dele mesmo (não de um
/// redirecionamento) e que não é um reparse point.
fn open(
    path: &Path,
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: FILE_CREATION_DISPOSITION,
    dir: bool,
) -> Result<Checked, String> {
    let w = wide(&path.to_string_lossy());
    let mut flags: FILE_FLAGS_AND_ATTRIBUTES = FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL;
    if dir {
        flags |= FILE_FLAG_BACKUP_SEMANTICS;
    }
    // SAFETY: caminho terminado em zero; o handle passa a ter dono logo abaixo.
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            access,
            share,
            None,
            disposition,
            flags,
            None,
        )
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: CreateFileW teve sucesso; handle válido e sem outro dono.
    let handle = unsafe { crate::win::own(h) };
    let c = Checked { handle };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: handle válido; estrutura local.
    unsafe { GetFileInformationByHandle(c.raw(), &raw mut info) }
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(format!("{} is a link (refused)", path.display()));
    }
    let got = final_path(c.raw())?;
    if got != norm(&path.to_string_lossy()) {
        return Err(format!(
            "{} resolves elsewhere ({got}): refused",
            path.display()
        ));
    }
    Ok(c)
}

/// Dono do objeto aberto, em texto.
fn owner(c: &Checked) -> Result<String, String> {
    let mut owner = PSID::default();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: handle válido; o descritor é liberado abaixo.
    let r = unsafe {
        GetSecurityInfo(
            c.raw(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            None,
            None,
            Some(&raw mut sd),
        )
    };
    if r.is_err() {
        return Err(format!("owner: {r:?}"));
    }
    // SAFETY: `owner` aponta para dentro do descritor, vivo até o LocalFree.
    let s = unsafe { crate::win::sid_string(owner) };
    // SAFETY: descritor alocado pela API; liberado uma vez.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    s.ok_or_else(|| "owner SID".into())
}

/// Pastas abertas e conferidas para uma operação em `<base>\<dir>`.
pub struct UserDir {
    _base: Checked,
    dir: Checked,
    path: PathBuf,
}

impl UserDir {
    /// `base` (ex.: `AppData\Roaming`) precisa ser do usuário atual; `name` é criada se faltar.
    pub fn open(base: &Path, name: &str, create: bool) -> Result<Self, String> {
        // Sem FILE_SHARE_DELETE: ninguém renomeia nem troca a pasta enquanto ela está aberta.
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
        let b = open(base, GENERIC_READ.0, share, OPEN_EXISTING, true)?;
        let user = crate::win::user_sid().ok_or("could not read the user SID")?;
        let own = owner(&b)?;
        if own != user {
            return Err(format!(
                "{} is not owned by the current user ({own}): refused",
                base.display()
            ));
        }
        let path = base.join(name);
        if create {
            let w = wide(&path.to_string_lossy());
            // SAFETY: caminho terminado em zero; pasta já existente não é erro aqui.
            let _ = unsafe { CreateDirectoryW(PCWSTR(w.as_ptr()), None) };
        }
        let dir = open(&path, GENERIC_READ.0, share, OPEN_EXISTING, true)?;
        Ok(Self {
            _base: b,
            dir,
            path,
        })
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Lê `name` (no máximo `max` bytes); `Ok(None)` se não existe.
    pub fn read(&self, name: &str, max: u64) -> Result<Option<String>, String> {
        let p = self.file(name);
        if !p.exists() {
            return Ok(None);
        }
        let c = open(&p, GENERIC_READ.0, FILE_SHARE_READ, OPEN_EXISTING, false)?;
        let _ = &self.dir;
        // SAFETY: o handle conferido passa para o File (dono único).
        let f = unsafe { File::from_raw_handle(c.handle.as_raw_handle()) };
        std::mem::forget(c.handle);
        let mut s = String::new();
        let n = f
            .take(max + 1)
            .read_to_string(&mut s)
            .map_err(|e| format!("{}: {e}", p.display()))?;
        if n as u64 > max {
            return Err(format!("{} is larger than {} KB", p.display(), max >> 10));
        }
        Ok(Some(s))
    }

    /// Grava `name` atomicamente: temporário novo e exclusivo, trocado pelo próprio handle.
    pub fn write(&self, name: &str, text: &str) -> Result<(), String> {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = self.file(&format!("{name}.{}.{n}.tmp", std::process::id()));
        let c = open(
            &tmp,
            GENERIC_WRITE.0 | DELETE.0,
            FILE_SHARE_MODE(0),
            CREATE_NEW,
            false,
        )?;
        let r = (|| -> Result<(), String> {
            // SAFETY: handle conferido e só nosso; o File não o fecha (forget abaixo).
            let mut f = std::mem::ManuallyDrop::new(unsafe {
                File::from_raw_handle(c.handle.as_raw_handle())
            });
            f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
            rename_over(c.raw(), &self.file(name))
        })();
        if r.is_err() {
            // Marca o temporário para apagar ao fechar o handle.
            delete_on_close(c.raw());
        }
        r
    }
}

/// Troca o arquivo de `h` pelo destino `to` (substituindo), pelo handle.
fn rename_over(h: HANDLE, to: &Path) -> Result<(), String> {
    let name: Vec<u16> = to.to_string_lossy().encode_utf16().collect();
    let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = header + (name.len() + 1) * 2;
    let mut buf = vec![0u64; size.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: buffer alinhado com espaço para o cabeçalho e o nome; campos escritos no lugar.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = HANDLE::default();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            name.len(),
        );
        SetFileInformationByHandle(h, FileRenameInfo, info.cast(), size as u32)
    }
    .map_err(|e| format!("rename to {}: {e}", to.display()))
}

fn delete_on_close(h: HANDLE) {
    use windows::Win32::Storage::FileSystem::{FILE_DISPOSITION_INFO, FileDispositionInfo};
    let d = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: handle aberto com DELETE; estrutura local.
    unsafe {
        let _ = SetFileInformationByHandle(
            h,
            FileDispositionInfo,
            (&raw const d).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pasta Roaming do perfil (do usuário, como o AppData real), com nomes de teste.
    fn roaming() -> PathBuf {
        crate::config::default_path()
            .and_then(|p| p.parent()?.parent().map(Path::to_path_buf))
            .expect("Roaming")
    }

    fn name(tag: &str) -> String {
        format!("wintop_test_{}_{tag}", std::process::id())
    }

    #[test]
    fn grava_e_le_pelo_handle() {
        let b = roaming();
        let n = name("rw");
        let d = UserDir::open(&b, &n, true).expect("pasta do usuário");
        assert_eq!(d.read("config.toml", 1024).expect("leitura"), None);
        d.write("config.toml", "a = 1\n").expect("gravação");
        d.write("config.toml", "a = 2\n").expect("substitui");
        assert_eq!(
            d.read("config.toml", 1024).expect("leitura").as_deref(),
            Some("a = 2\n")
        );
        // Teto de tamanho no handle.
        assert!(d.read("config.toml", 3).is_err());
        // Nenhum temporário sobrando.
        let left: Vec<_> = std::fs::read_dir(b.join(&n)).expect("lista").collect();
        assert_eq!(left.len(), 1, "{left:?}");
        drop(d);
        let _ = std::fs::remove_dir_all(b.join(&n));
    }

    #[test]
    fn junction_na_pasta_e_recusada() {
        let b = roaming();
        let n = name("junction");
        let target = std::env::temp_dir().join(name("alvo"));
        std::fs::create_dir_all(&target).expect("alvo");
        // Junction (não precisa de privilégio): wintop -> alvo.
        let st = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(b.join(&n))
            .arg(&target)
            .output()
            .expect("mklink");
        assert!(
            st.status.success(),
            "{}",
            String::from_utf8_lossy(&st.stderr)
        );
        let r = UserDir::open(&b, &n, true);
        // Remove a junction (rmdir não segue o link) e o alvo.
        let _ = std::fs::remove_dir(b.join(&n));
        let empty = std::fs::read_dir(&target).expect("alvo").next().is_none();
        let _ = std::fs::remove_dir_all(&target);
        assert!(r.is_err(), "junction seguida");
        assert!(empty, "nada gravado no alvo");
    }

    #[test]
    fn base_de_outro_dono_e_recusada() {
        // System32 não é do usuário (AppData redirecionado para lá não vale).
        let sys = crate::win::system_dir().expect("System32");
        assert!(UserDir::open(&sys, "wintop", false).is_err());
    }
}
