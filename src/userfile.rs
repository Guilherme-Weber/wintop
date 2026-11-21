//! Arquivos do usuário lidos e gravados pelo wintop **elevado** (o `config.toml`).
//!
//! Com o atalho sem UAC, qualquer processo sem elevação do usuário consegue disparar o wintop
//! elevado; e a pasta `%APPDATA%\wintop` é desse usuário. Um caminho comum seria uma porta para
//! gravar como administrador onde o usuário não pode (junction na pasta, symlink do gerenciador
//! de objetos, AppData redirecionado para uma pasta do sistema). Por isso:
//!
//! - toda abertura usa `FILE_FLAG_OPEN_REPARSE_POINT` (um reparse point é recusado, não seguido)
//!   e confere o caminho final do handle contra o esperado;
//! - a pasta base (`AppData\Roaming`) tem de ter como dono o próprio usuário, ou Administradores
//!   dentro do perfil dele (AppData apontado para uma pasta do sistema é recusado);
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

/// Falha de abertura: "não existe" (decidido pelo código do `CreateFileW`, sem `exists()`, que
/// segue links) ou qualquer outra recusa.
enum OpenError {
    NotFound,
    Refused(String),
}

impl From<OpenError> for String {
    fn from(e: OpenError) -> Self {
        match e {
            OpenError::NotFound => "not found".into(),
            OpenError::Refused(s) => s,
        }
    }
}

/// Abre `path` sem seguir reparse points e confere: é dele mesmo (não um redirecionamento),
/// não é link, é um arquivo de disco do tipo pedido (pasta ou arquivo) e, se arquivo, sem
/// outros nomes (hardlink). Só caminho local absoluto; o handle não expõe o token elevado se o
/// nome for de um pipe (SQOS anônimo).
fn open(
    path: &Path,
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: FILE_CREATION_DISPOSITION,
    dir: bool,
    single_name: bool,
) -> Result<Checked, OpenError> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_TYPE_DISK, GetFileType, SECURITY_ANONYMOUS,
        SECURITY_SQOS_PRESENT,
    };
    let refuse = |m: String| OpenError::Refused(m);
    if !crate::win::is_local_absolute(path) {
        return Err(refuse(format!(
            "{} is not a local path (refused)",
            path.display()
        )));
    }
    let w = wide(&path.to_string_lossy());
    let mut flags: FILE_FLAGS_AND_ATTRIBUTES = FILE_FLAG_OPEN_REPARSE_POINT
        | FILE_ATTRIBUTE_NORMAL
        | SECURITY_SQOS_PRESENT
        | SECURITY_ANONYMOUS;
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
    .map_err(|e| {
        if e.code() == ERROR_FILE_NOT_FOUND.to_hresult()
            || e.code() == ERROR_PATH_NOT_FOUND.to_hresult()
        {
            OpenError::NotFound
        } else {
            refuse(format!("{}: {e}", path.display()))
        }
    })?;
    // SAFETY: CreateFileW teve sucesso; handle válido e sem outro dono.
    let handle = unsafe { crate::win::own(h) };
    let c = Checked { handle };
    // SAFETY: handle válido.
    if unsafe { GetFileType(c.raw()) } != FILE_TYPE_DISK {
        return Err(refuse(format!(
            "{} is not a disk file (refused)",
            path.display()
        )));
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: handle válido; estrutura local.
    unsafe { GetFileInformationByHandle(c.raw(), &raw mut info) }
        .map_err(|e| refuse(format!("{}: {e}", path.display())))?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(refuse(format!("{} is a link (refused)", path.display())));
    }
    let is_dir = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if is_dir != dir {
        return Err(refuse(format!(
            "{} is not a {} (refused)",
            path.display(),
            if dir { "folder" } else { "file" }
        )));
    }
    // Leitura e gravação: o arquivo não pode ter outros nomes (seria ler ou escrever o alvo
    // por outro caminho); apagar um nome é seguro.
    if !dir && single_name && info.nNumberOfLinks > 1 {
        return Err(refuse(format!(
            "{} has other names (hardlink, refused)",
            path.display()
        )));
    }
    let got = final_path(c.raw()).map_err(refuse)?;
    if got != norm(&path.to_string_lossy()) {
        return Err(refuse(format!(
            "{} resolves elsewhere ({got}): refused",
            path.display()
        )));
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

/// Administradores (dono padrão do que um processo elevado cria).
const ADMINISTRATORS: &str = "S-1-5-32-544";

/// A base é do usuário: dono = o próprio usuário, ou Administradores **dentro do perfil dele**
/// (perfis criados por imagem ou por processo elevado, como nos runners do GitHub). Fora do
/// perfil, uma pasta de Administradores (ex.: `Program Files\wintop`) seria um redirecionamento
/// do AppData pelo HKCU: recusada.
fn trusted_owner(own: &str, user: &str, base: &Path, profile: Option<&Path>) -> bool {
    if own == user {
        return true;
    }
    let Some(profile) = profile else {
        return false;
    };
    let p = format!("{}\\", norm(&profile.to_string_lossy()));
    own == ADMINISTRATORS && norm(&base.to_string_lossy()).starts_with(&p)
}

/// Pastas abertas e conferidas para uma operação em `<base>\<dir>`.
pub struct UserDir {
    _base: Checked,
    dir: Checked,
    path: PathBuf,
}

impl UserDir {
    /// `base` (ex.: `AppData\Roaming`) precisa ser do usuário atual; `name` é criada se faltar
    /// (`create`). `Ok(None)`: sem criar, a subpasta não existe (a base passou em tudo).
    pub fn open(base: &Path, name: &str, create: bool) -> Result<Option<Self>, String> {
        // Sem FILE_SHARE_DELETE: ninguém renomeia nem troca a pasta enquanto ela está aberta.
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
        let b =
            open(base, GENERIC_READ.0, share, OPEN_EXISTING, true, true).map_err(String::from)?;
        let user = crate::win::user_sid().ok_or("could not read the user SID")?;
        let own = owner(&b)?;
        let profile = crate::win::profile_dir();
        if !trusted_owner(&own, &user, base, profile.as_deref()) {
            return Err(format!(
                "{} is not owned by the current user ({own}): refused",
                base.display()
            ));
        }
        let path = base.join(name);
        if create {
            let w = wide(&path.to_string_lossy());
            // SAFETY: caminho terminado em zero; pasta já existente não é erro aqui (o que
            // existir no lugar é conferido na abertura abaixo).
            let _ = unsafe { CreateDirectoryW(PCWSTR(w.as_ptr()), None) };
        }
        let dir = match open(&path, GENERIC_READ.0, share, OPEN_EXISTING, true, true) {
            Ok(d) => d,
            Err(OpenError::NotFound) if !create => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(Self {
            _base: b,
            dir,
            path,
        }))
    }

    /// Como `open`, criando a subpasta se faltar.
    pub fn create(base: &Path, name: &str) -> Result<Self, String> {
        Self::open(base, name, true)?.ok_or_else(|| "folder not created".into())
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Lê `name` (no máximo `max` bytes); `Ok(None)` se não existe.
    pub fn read(&self, name: &str, max: u64) -> Result<Option<String>, String> {
        let p = self.file(name);
        let c = match open(
            &p,
            GENERIC_READ.0,
            FILE_SHARE_READ,
            OPEN_EXISTING,
            false,
            true,
        ) {
            Ok(c) => c,
            Err(OpenError::NotFound) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
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
        self.write_bytes(name, text.as_bytes())
    }

    /// Apaga `name` sem seguir links (um link no lugar é apagado como link). `Ok(false)`: não
    /// existia.
    pub fn remove(&self, name: &str) -> Result<bool, String> {
        let p = self.file(name);
        let c = match open(
            &p,
            DELETE.0,
            FILE_SHARE_MODE(0),
            OPEN_EXISTING,
            false,
            false,
        ) {
            Ok(c) => c,
            Err(OpenError::NotFound) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        delete_on_close(c.raw());
        Ok(true)
    }

    pub fn write_bytes(&self, name: &str, data: &[u8]) -> Result<(), String> {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = self.file(&format!("{name}.{}.{n}.tmp", std::process::id()));
        let c = open(
            &tmp,
            GENERIC_WRITE.0 | DELETE.0,
            FILE_SHARE_MODE(0),
            CREATE_NEW,
            false,
            true,
        )
        .map_err(String::from)?;
        let r = (|| -> Result<(), String> {
            // SAFETY: handle conferido e só nosso; o File não o fecha (forget abaixo).
            let mut f = std::mem::ManuallyDrop::new(unsafe {
                File::from_raw_handle(c.handle.as_raw_handle())
            });
            f.write_all(data).map_err(|e| e.to_string())?;
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
        let d = UserDir::create(&b, &n).expect("pasta do usuário");
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
        let r = UserDir::create(&b, &n);
        // Remove a junction (rmdir não segue o link) e o alvo.
        let _ = std::fs::remove_dir(b.join(&n));
        let empty = std::fs::read_dir(&target).expect("alvo").next().is_none();
        let _ = std::fs::remove_dir_all(&target);
        assert!(r.is_err(), "junction seguida");
        assert!(empty, "nada gravado no alvo");
    }

    #[test]
    fn hardlink_arquivo_no_lugar_da_pasta_e_unc_sao_recusados() {
        let b = roaming();
        // Hardlink: o config com outro nome não é lido (poderia ser um arquivo protegido).
        let n = name("hard");
        let d = UserDir::create(&b, &n).expect("pasta");
        d.write("config.toml", "a = 1\n").expect("gravação");
        std::fs::hard_link(d.file("config.toml"), d.file("outro.toml")).expect("hardlink");
        assert!(d.read("config.toml", 1024).is_err(), "hardlink lido");
        // `remove` apaga o nome, não o alvo.
        assert!(d.remove("outro.toml").expect("remove"));
        assert!(!d.remove("outro.toml").expect("remove"), "já não existe");
        assert!(d.read("config.toml", 1024).expect("leitura").is_some());
        drop(d);
        let _ = std::fs::remove_dir_all(b.join(&n));
        // Arquivo no lugar da pasta.
        let f = name("arquivo");
        std::fs::write(b.join(&f), "x").expect("arquivo");
        assert!(
            UserDir::create(&b, &f).is_err(),
            "arquivo aceito como pasta"
        );
        assert!(UserDir::open(&b, &f, false).is_err());
        let _ = std::fs::remove_file(b.join(&f));
        // Sem a pasta e sem criar: Ok(None), sem aviso.
        assert!(
            UserDir::open(&b, &name("nada"), false)
                .expect("base ok")
                .is_none()
        );
        // Base que não é caminho local (UNC, pipe): recusada antes de qualquer abertura.
        assert!(UserDir::open(Path::new(r"\127.0.0.1\c$"), "wintop", false).is_err());
        assert!(UserDir::open(Path::new(r"\.\pipe\x"), "wintop", false).is_err());
    }

    #[test]
    fn dono_administradores_so_dentro_do_perfil() {
        let user = "S-1-5-21-1-2-3-1001";
        let prof = Path::new(r"C:\Users\nome");
        let roam = Path::new(r"C:\Users\nome\AppData\Roaming");
        assert!(trusted_owner(user, user, roam, Some(prof)));
        assert!(trusted_owner(ADMINISTRATORS, user, roam, Some(prof)));
        // Fora do perfil (AppData redirecionado), prefixo parecido ou sem perfil: recusado.
        let pf = Path::new(r"C:\Program Files\wintop");
        assert!(!trusted_owner(ADMINISTRATORS, user, pf, Some(prof)));
        let irmao = Path::new(r"C:\Users\nome2\AppData\Roaming");
        assert!(!trusted_owner(ADMINISTRATORS, user, irmao, Some(prof)));
        assert!(!trusted_owner(ADMINISTRATORS, user, roam, None));
        // Outros donos (SYSTEM, TrustedInstaller), mesmo dentro do perfil: recusados.
        assert!(!trusted_owner("S-1-5-18", user, roam, Some(prof)));
    }

    #[test]
    fn base_de_outro_dono_e_recusada() {
        // System32 não é do usuário (AppData redirecionado para lá não vale).
        let sys = crate::win::system_dir().expect("System32");
        assert!(UserDir::open(&sys, "wintop", false).is_err());
    }
}
