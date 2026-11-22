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
//!   só no caminho padrão dentro do perfil, sem redirecionamento (AppData apontado para uma
//!   pasta do sistema ou de Administradores é recusado);
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
    use windows::Win32::Storage::FileSystem::{SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT};
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
    match verify(&c, path, dir, single_name) {
        Ok(()) => Ok(c),
        Err(e) => {
            // Criado agora e recusado (ex.: uma junction trocada no caminho depois da checagem
            // da pasta): não fica para trás onde o processo elevado não deveria ter escrito.
            if disposition == CREATE_NEW && access & DELETE.0 != 0 {
                let _ = delete_on_close(c.raw());
            }
            Err(e)
        }
    }
}

/// Conferências de `open` sobre o handle já aberto.
fn verify(c: &Checked, path: &Path, dir: bool, single_name: bool) -> Result<(), OpenError> {
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_TYPE_DISK, GetFileType,
    };
    let refuse = |m: String| OpenError::Refused(m);
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
    Ok(())
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

/// Bases padrão (sem redirecionamento) dentro do perfil: o AppData e o Menu Iniciar do usuário.
const DEFAULT_BASES: [&str; 2] = [
    r"AppData\Roaming",
    r"AppData\Roaming\Microsoft\Windows\Start Menu",
];

/// A base é do usuário: dono = o próprio usuário, ou Administradores só numa base **padrão** do
/// perfil (perfis criados por imagem ou por processo elevado, como nos runners do GitHub). Um
/// redirecionamento pelo HKCU para outra pasta de Administradores (`Program Files\wintop`, ou
/// uma subpasta protegida do perfil) é recusado. `base` e `profile` já normalizados (`norm`).
fn trusted_owner(own: &str, user: &str, base: &str, profile: Option<&str>) -> bool {
    if own == user {
        return true;
    }
    let Some(profile) = profile else {
        return false;
    };
    own == ADMINISTRATORS
        && DEFAULT_BASES
            .iter()
            .any(|d| base == norm(&format!(r"{profile}\{d}")))
}

/// Perfil do usuário normalizado pelo caminho final (nomes longos, sem `\\?\`): o
/// `ProfileImagePath` do HKLM pode ter nome curto 8.3. Segue links de propósito: a origem
/// (HKLM) não é do usuário, e a base é conferida contra o caminho final dela.
fn profile_final() -> Option<String> {
    use windows::Win32::Storage::FileSystem::{FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE};
    let p = crate::win::profile_dir()?;
    let w = wide(&p.to_string_lossy());
    // SAFETY: caminho terminado em zero; o handle passa a ter dono logo abaixo.
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .ok()?;
    // SAFETY: CreateFileW teve sucesso; handle válido e sem outro dono.
    let h = unsafe { crate::win::own(h) };
    final_path(HANDLE(h.as_raw_handle())).ok()
}

/// Arquivo-âncora novo dentro de `dir`, apagado quando o handle fecha.
fn pin_file(dir: &Path) -> Result<Checked, String> {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = dir.join(format!(".wintop.{}.{n}.pin", std::process::id()));
    let c = open(
        &p,
        GENERIC_READ.0 | DELETE.0,
        FILE_SHARE_READ,
        CREATE_NEW,
        false,
        true,
    )
    .map_err(String::from)?;
    delete_on_close(c.raw()).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(c)
}

/// Pastas abertas e conferidas para uma operação em `<base>\<dir>`.
pub struct UserDir {
    _base: Checked,
    dir: Checked,
    /// Âncora: arquivo aberto dentro da pasta enquanto ela está aberta (apagado ao fechar).
    _pin: Checked,
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
        // `open` já conferiu que o caminho final da base é ela mesma.
        let based = norm(&base.to_string_lossy());
        if own != user && !trusted_owner(&own, &user, &based, profile_final().as_deref()) {
            return Err(format!(
                "{} is not owned by the current user ({own}): refused",
                base.display()
            ));
        }
        // A base também ancorada antes de criar a subpasta: um AppData redirecionado para uma
        // pasta vazia do usuário poderia virar junction e o CreateDirectoryW criaria a pasta no
        // alvo. Solta quando a subpasta estiver aberta e ancorada (ela prende a base).
        let base_pin = pin_file(base)?;
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
        // Só uma pasta vazia vira junction (FSCTL_SET_REPARSE_POINT): com um arquivo nosso
        // aberto dentro dela, sem FILE_SHARE_DELETE, ela não esvazia enquanto está aberta, e um
        // arquivo criado ou trocado nela não vai parar no alvo de uma junction posta depois da
        // conferência. A base também fica presa: a subpasta aberta não a deixa esvaziar.
        let pin = pin_file(&path)?;
        drop(base_pin);
        Ok(Some(Self {
            _base: b,
            dir,
            _pin: pin,
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
        // Falha ao marcar (ex.: arquivo somente leitura) é erro, não "removido".
        delete_on_close(c.raw()).map_err(|e| format!("{}: {e}", p.display()))?;
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
            // Marca o temporário para apagar ao fechar o handle (melhor esforço: o erro que
            // vale é o da gravação).
            let _ = delete_on_close(c.raw());
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

/// Marca o arquivo de `h` para apagar quando o último handle fechar.
fn delete_on_close(h: HANDLE) -> windows::core::Result<()> {
    use windows::Win32::Storage::FileSystem::{FILE_DISPOSITION_INFO, FileDispositionInfo};
    let d = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: handle aberto com DELETE; estrutura local.
    unsafe {
        SetFileInformationByHandle(
            h,
            FileDispositionInfo,
            (&raw const d).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
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
        // Nenhum temporário sobrando (fechada a pasta, a âncora também some).
        drop(d);
        let left: Vec<_> = std::fs::read_dir(b.join(&n)).expect("lista").collect();
        assert_eq!(left.len(), 1, "{left:?}");
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

    /// Transforma a pasta (vazia) numa junction para `target`, como faria um processo sem
    /// elevação do usuário (FSCTL_SET_REPARSE_POINT; não exige privilégio).
    fn make_junction(dir: &Path, target: &Path) -> windows::core::Result<()> {
        use windows::Win32::Storage::FileSystem::{FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA};
        use windows::Win32::System::IO::DeviceIoControl;
        use windows::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
        const MOUNT_POINT: u32 = 0xA000_0003;
        let sub: Vec<u16> = format!(r"\??\{}", target.display())
            .encode_utf16()
            .collect();
        // PathBuffer: nome substituto + 0 + nome de exibição vazio + 0.
        let mut path = sub.clone();
        path.extend([0, 0]);
        let data_len = 8 + path.len() * 2;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend(MOUNT_POINT.to_le_bytes());
        buf.extend((data_len as u16).to_le_bytes());
        buf.extend(0u16.to_le_bytes());
        buf.extend(0u16.to_le_bytes()); // SubstituteNameOffset
        buf.extend(((sub.len() * 2) as u16).to_le_bytes());
        buf.extend(((sub.len() * 2 + 2) as u16).to_le_bytes()); // PrintNameOffset
        buf.extend(0u16.to_le_bytes()); // PrintNameLength
        buf.extend(path.iter().flat_map(|c| c.to_le_bytes()));
        let w = wide(&dir.to_string_lossy());
        // SAFETY: caminho terminado em zero; o handle passa a ter dono logo abaixo.
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                FILE_WRITE_DATA.0 | FILE_WRITE_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }?;
        // SAFETY: CreateFileW teve sucesso.
        let h = unsafe { crate::win::own(h) };
        // SAFETY: handle válido; buffer local com o tamanho informado.
        unsafe {
            DeviceIoControl(
                HANDLE(h.as_raw_handle()),
                FSCTL_SET_REPARSE_POINT,
                Some(buf.as_ptr().cast()),
                buf.len() as u32,
                None,
                0,
                None,
                None,
            )
        }
    }

    /// Com a pasta aberta, ela não vira junction (o arquivo-âncora a mantém não vazia), e
    /// gravar e trocar o arquivo dentro dela funciona; fechada, a âncora some. Sem a âncora, a
    /// mesma pasta vazia vira junction (o teste realmente exercita o ataque).
    #[test]
    fn pasta_aberta_nao_vira_junction() {
        let b = roaming();
        let n = name("junction_tarde");
        let target = std::env::temp_dir();
        let d = UserDir::create(&b, &n).expect("pasta");
        let while_open = make_junction(&b.join(&n), &target);
        d.write("config.toml", "a = 1\n").expect("gravação");
        d.write("config.toml", "a = 2\n").expect("troca");
        assert_eq!(
            d.read("config.toml", 64).expect("leitura").as_deref(),
            Some("a = 2\n")
        );
        drop(d);
        let left: Vec<_> = std::fs::read_dir(b.join(&n))
            .expect("pasta")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        let _ = std::fs::remove_file(b.join(&n).join("config.toml"));
        let empty_ok = make_junction(&b.join(&n), &target);
        let _ = std::fs::remove_dir(b.join(&n));
        assert!(while_open.is_err(), "pasta aberta virou junction");
        assert_eq!(left, ["config.toml"], "a âncora ficou para trás");
        empty_ok.expect("controle: a pasta vazia vira junction");
    }

    #[test]
    fn dono_administradores_so_nas_bases_padrao() {
        let user = "S-1-5-21-1-2-3-1001";
        let prof = Some(r"c:\users\nome");
        let roam = r"c:\users\nome\appdata\roaming";
        let menu = r"c:\users\nome\appdata\roaming\microsoft\windows\start menu";
        assert!(trusted_owner(user, user, roam, prof));
        assert!(trusted_owner(ADMINISTRATORS, user, roam, prof));
        assert!(trusted_owner(ADMINISTRATORS, user, menu, prof));
        // Redirecionado: fora do perfil, para uma subpasta do perfil, outro perfil, sem perfil.
        let fora = [
            r"c:\program files\wintop",
            r"c:\users\nome\appdata\roaming\protegida",
            r"c:\users\nome\appdata",
            r"c:\users\nome2\appdata\roaming",
        ];
        for b in fora {
            assert!(!trusted_owner(ADMINISTRATORS, user, b, prof), "{b}");
            // A pasta do próprio usuário redirecionada continua valendo.
            assert!(trusted_owner(user, user, b, prof));
        }
        assert!(!trusted_owner(ADMINISTRATORS, user, roam, None));
        // Outros donos (SYSTEM, TrustedInstaller), mesmo na base padrão: recusados.
        assert!(!trusted_owner("S-1-5-18", user, roam, prof));
    }

    #[test]
    fn perfil_normalizado_e_a_base_padrao_do_roaming() {
        let p = profile_final().expect("perfil");
        assert!(!p.starts_with(r"\\") && !p.contains('~'), "{p}");
        let roam = norm(&roaming().to_string_lossy());
        assert_eq!(roam, norm(&format!(r"{p}\{}", DEFAULT_BASES[0])));
    }

    #[test]
    fn base_de_outro_dono_e_recusada() {
        // System32 não é do usuário (AppData redirecionado para lá não vale).
        let sys = crate::win::system_dir().expect("System32");
        assert!(UserDir::open(&sys, "wintop", false).is_err());
    }
}
