//! Instalação na primeira abertura e atualização automática.
//!
//! - **Modo**: o exe em `<Program Files>\wintop` é a cópia instalada; um `config.toml` ao lado
//!   do exe marca a cópia portátil; sem nenhum dos dois, a primeira abertura pergunta (instalar
//!   ou rodar portátil na pasta).
//! - **Versão nova**: o redirecionamento de `github.com/<repo>/releases/latest` dá a tag da
//!   última release publicada (sem API nem JSON). Uma consulta por abertura, numa thread, com
//!   tempo limite curto; sem rede, nada aparece.
//! - **Confiança**: o wintop roda elevado, então baixar e trocar o exe é o ponto mais sensível
//!   do programa. O zip só é usado se a assinatura ed25519 (`<zip>.sig`, gerada na release com a
//!   chave privada guardada num secret do GitHub) bater com a chave pública embutida aqui; a
//!   pasta dentro do zip assinado tem de ser a da versão anunciada (sem servir uma release
//!   antiga como nova). Do zip só saem `wintop.exe` e os arquivos conhecidos de `modules\`,
//!   lidos na memória (nada é extraído por caminho do arquivo).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_AUTOLOGON_SECURITY_LEVEL_HIGH,
    WINHTTP_DISABLE_COOKIES, WINHTTP_DISABLE_REDIRECTS, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_AUTOLOGON_POLICY, WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect,
    WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::core::PCWSTR;

use crate::win::wide;

const REPO: &str = "Guilherme-Weber/wintop";

/// Chave pública ed25519 das releases (a privada fica no secret `WINTOP_UPDATE_KEY` do GitHub;
/// `release.yml` confere a assinatura com esta mesma chave antes de anexar o `.sig`).
pub const PUBLIC_KEY: [u8; 32] = [
    0x07, 0x52, 0x5f, 0xff, 0xb1, 0xd1, 0xa5, 0xb4, 0xa8, 0xe7, 0x90, 0x4d, 0xf5, 0xc1, 0xe8, 0xff,
    0x84, 0x23, 0xf9, 0xea, 0x54, 0x93, 0xfc, 0xe8, 0x33, 0x9b, 0x71, 0x43, 0xce, 0x74, 0xc2, 0xb6,
];

/// Teto do zip baixado (o da 1.1.1 tem ~3 MB) e da assinatura.
const MAX_ZIP: usize = 64 << 20;
const MAX_SIG: usize = 1024;
/// Tempo limite de cada etapa (DNS, conexão, envio, resposta) da consulta de versão e do
/// download.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// De onde esta cópia roda.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `<Program Files>\wintop\wintop.exe`.
    Installed,
    /// Pasta do exe, com o `config.toml` dela.
    Portable(PathBuf),
    /// Pasta do exe, sem escolha feita ainda.
    Undecided(PathBuf),
}

/// Modo desta cópia (pelo caminho do exe e pelo `config.toml` ao lado).
pub fn mode() -> Mode {
    let Ok(me) = std::env::current_exe() else {
        return Mode::Undecided(PathBuf::new());
    };
    let dir = me.parent().map_or_else(PathBuf::new, crate::win::long_path);
    let canon = crate::install::canon;
    if let Ok(inst) = crate::install::install_dir()
        && canon(&dir) == canon(&inst)
    {
        return Mode::Installed;
    }
    if dir.join("config.toml").is_file() {
        Mode::Portable(dir)
    } else {
        Mode::Undecided(dir)
    }
}

/// (maior, menor, correção) de "x.y.z" (apenas dígitos ASCII, sem `+`).
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.split('.');
    let part = |p: &str| -> Option<u32> {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    };
    let v = (part(it.next()?)?, part(it.next()?)?, part(it.next()?)?);
    it.next().is_none().then_some(v)
}

/// `latest` é mais nova que `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Versão da tag no `Location` de `releases/latest` (`.../releases/tag/v1.2.0`).
fn version_from_location(loc: &str) -> Option<String> {
    let tag = loc.trim().rsplit_once("/releases/tag/")?.1;
    let v = tag.strip_prefix('v')?;
    parse_version(v).map(|_| v.to_string())
}

// ---------------------------------------------------------------------------------------------
// HTTP (WinHTTP: nativo, proxy do sistema, TLS do Windows)

/// Handle do WinHTTP fechado no Drop.
struct Hi(*mut core::ffi::c_void);

impl Drop for Hi {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: handle aberto pelo WinHTTP e fechado uma vez.
            let _ = unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

struct Response {
    status: u32,
    location: Option<String>,
    body: Vec<u8>,
}

/// Apenas `github.com` e subdomínios de `githubusercontent.com` (evita SSRF em redirects).
fn allowed_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    if h == "github.com" {
        return true;
    }
    let Some(sub) = h.strip_suffix(".githubusercontent.com") else {
        return false;
    };
    !sub.is_empty()
        && !sub.starts_with('.')
        && !sub.ends_with('.')
        && !sub.contains("..")
        && sub
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// `https://host/caminho` → (host, caminho). Só HTTPS para hosts do GitHub permitidos.
fn split_url(url: &str) -> Result<(&str, &str), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("not an https URL: {url}"))?;
    let (host, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
    if host.is_empty() || host.contains(['@', ':']) || !allowed_host(host) {
        return Err(format!("unexpected host in {url}"));
    }
    Ok((host, path))
}

/// Um GET, sem seguir redirecionamento; corpo lido até `max` bytes (passou disso = erro).
fn get_once(url: &str, max: usize, timeout: Duration) -> Result<Response, String> {
    let (host, path) = split_url(url)?;
    let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let agent = wide(concat!("wintop/", env!("CARGO_PKG_VERSION")));
    let (h, p, get) = (wide(host), wide(path), wide("GET"));

    // SAFETY: string UTF-16 terminada em zero; handle entregue ao `Hi` logo abaixo.
    let session = Hi(unsafe {
        WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    });
    if session.0.is_null() {
        return Err(format!(
            "WinHttpOpen: {}",
            windows::core::Error::from_thread()
        ));
    }
    // SAFETY: `session` é um handle WinHTTP válido.
    unsafe { WinHttpSetTimeouts(session.0, ms, ms, ms, ms) }.map_err(|e| e.to_string())?;
    // SAFETY: `session` válido e `host` terminado em zero; handle entregue ao `Hi`.
    let conn = Hi(unsafe { WinHttpConnect(session.0, PCWSTR(h.as_ptr()), 443, 0) });
    if conn.0.is_null() {
        return Err(format!("{host}: {}", windows::core::Error::from_thread()));
    }
    // SAFETY: `conn` válido e strings UTF-16 terminadas em zero; handle entregue ao `Hi`.
    let req = Hi(unsafe {
        WinHttpOpenRequest(
            conn.0,
            PCWSTR(get.as_ptr()),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        )
    });
    if req.0.is_null() {
        return Err(format!("{url}: {}", windows::core::Error::from_thread()));
    }
    // Redirecionamentos seguidos à mão (só para HTTPS em hosts do GitHub, com limite), sem
    // cookies e sem envio automático de credenciais NTLM/Negotiate do processo elevado.
    let off = (WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_COOKIES).to_le_bytes();
    // SAFETY: `req` válido e slice `off` com tamanho de um u32.
    unsafe { WinHttpSetOption(Some(req.0), WINHTTP_OPTION_DISABLE_FEATURE, Some(&off)) }
        .map_err(|e| e.to_string())?;
    let autologon = WINHTTP_AUTOLOGON_SECURITY_LEVEL_HIGH.to_le_bytes();
    // SAFETY: `req` válido e slice `autologon` com tamanho de um u32.
    unsafe {
        WinHttpSetOption(
            Some(req.0),
            WINHTTP_OPTION_AUTOLOGON_POLICY,
            Some(&autologon),
        )
    }
    .map_err(|e| e.to_string())?;
    // SAFETY: `req` válido, sem cabeçalhos extras nem corpo.
    unsafe { WinHttpSendRequest(req.0, None, None, 0, 0, 0) }
        .map_err(|e| format!("{host}: {e}"))?;
    // SAFETY: `req` já enviado.
    unsafe { WinHttpReceiveResponse(req.0, std::ptr::null_mut()) }
        .map_err(|e| format!("{host}: {e}"))?;

    let mut status = 0u32;
    let mut len = 4u32;
    // SAFETY: `req` com resposta pronta; buffer local de 4 bytes para `WINHTTP_QUERY_FLAG_NUMBER`.
    unsafe {
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &raw mut len,
            std::ptr::null_mut(),
        )
    }
    .map_err(|e| e.to_string())?;

    let mut location = None;
    if (300..400).contains(&status) {
        let mut buf = vec![0u16; 2048];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `req` válido; buffer local com tamanho em bytes informado.
        if unsafe {
            WinHttpQueryHeaders(
                req.0,
                WINHTTP_QUERY_LOCATION,
                PCWSTR::null(),
                Some(buf.as_mut_ptr().cast()),
                &raw mut bytes,
                std::ptr::null_mut(),
            )
        }
        .is_ok()
        {
            location = Some(String::from_utf16_lossy(
                &buf[..(bytes as usize / 2).min(buf.len())],
            ));
        }
        return Ok(Response {
            status,
            location,
            body: Vec::new(),
        });
    }
    // Erro (404 de um `.sig` ausente, 5xx): o corpo não interessa nem é baixado.
    if status != 200 {
        return Ok(Response {
            status,
            location: None,
            body: Vec::new(),
        });
    }
    let mut body = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let mut read = 0u32;
        // SAFETY: `req` válido e buffer local com tamanho informado.
        unsafe {
            WinHttpReadData(
                req.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &raw mut read,
            )
        }
        .map_err(|e| format!("{host}: {e}"))?;
        if read == 0 {
            break;
        }
        if body.len().saturating_add(read as usize) > max {
            return Err(format!("{url}: larger than {max} bytes"));
        }
        body.extend_from_slice(&chunk[..read as usize]);
    }
    Ok(Response {
        status,
        location,
        body,
    })
}

/// GET seguindo até 5 redirecionamentos HTTPS; só 200 é sucesso.
fn get(url: &str, max: usize, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut url = url.to_string();
    for _ in 0..=5 {
        let r = get_once(&url, max, timeout)?;
        match r.status {
            200 => return Ok(r.body),
            300..=399 => url = r.location.ok_or("redirect without Location")?,
            s => return Err(format!("HTTP {s}")),
        }
    }
    Err("too many redirects".into())
}

/// Versão da última release publicada (rascunhos e pré-lançamentos não contam).
pub fn latest_version() -> Result<String, String> {
    let r = get_once(
        &format!("https://github.com/{REPO}/releases/latest"),
        0,
        CHECK_TIMEOUT,
    )?;
    if !(300..400).contains(&r.status) {
        return Err(format!("releases/latest: HTTP {}", r.status));
    }
    let loc = r.location.ok_or("releases/latest without Location")?;
    version_from_location(&loc).ok_or_else(|| format!("unexpected release URL: {loc}"))
}

/// Versão desta cópia. Só no build de debug, `WINTOP_UPDATE_AS` finge outra (para testar a
/// atualização de ponta a ponta contra uma release publicada).
pub fn current() -> String {
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("WINTOP_UPDATE_AS") {
        return v;
    }
    env!("CARGO_PKG_VERSION").to_string()
}

/// Consulta numa thread: `Some(versão)` se houver uma mais nova que esta. Erros (sem rede,
/// GitHub fora) não aparecem: a próxima abertura tenta de novo.
pub fn spawn_check() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("wintop-update-check".into())
        .spawn(move || match latest_version() {
            Ok(v) if is_newer(&v, &current()) => {
                let _ = tx.send(v);
            }
            Ok(_) => {}
            Err(e) => crate::debug::log("update", &format!("check failed: {e}")),
        });
    rx
}

// ---------------------------------------------------------------------------------------------
// Pacote: assinatura e zip

/// O que a atualização troca.
#[derive(Debug)]
pub struct Package {
    pub exe: Vec<u8>,
    /// (nome do arquivo em `modules\`, conteúdo).
    pub modules: Vec<(String, Vec<u8>)>,
}

/// Arquivos de `modules\` que a atualização leva (os mesmos da instalação).
use crate::install::MODULE_FILES;

/// Prova de tipo de que os bytes do zip passaram em `verify()` (ed25519 `verify_strict`):
/// impede chamar `unpack` com bytes sem assinatura verificada.
#[derive(Clone, Copy)]
pub struct VerifiedZip<'a>(&'a [u8]);

/// Confere a assinatura ed25519 (`verify_strict`) de `data` com `key` e devolve `VerifiedZip`.
pub fn verify<'a>(data: &'a [u8], sig: &[u8], key: &[u8; 32]) -> Result<VerifiedZip<'a>, String> {
    use ed25519_dalek::{Signature, VerifyingKey};
    let sig: [u8; 64] = sig
        .try_into()
        .map_err(|_| format!("signature with {} bytes (expected 64)", sig.len()))?;
    let key = VerifyingKey::from_bytes(key).map_err(|e| format!("public key: {e}"))?;
    key.verify_strict(data, &Signature::from_bytes(&sig))
        .map(|()| VerifiedZip(data))
        .map_err(|_| "signature does not match the wintop release key".to_string())
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(i..i.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(i..i.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Entrada do diretório central do zip: (nome, método, tamanho comprimido, tamanho final,
/// posição do cabeçalho local).
type Entry = (String, u16, usize, usize, usize);

/// Entradas do diretório central do zip. Sem zip64 (o pacote tem poucos MB).
fn zip_entries(z: &[u8]) -> Result<Vec<Entry>, String> {
    const EOCD: u32 = 0x0605_4b50;
    const CEN: u32 = 0x0201_4b50;
    let bad = || "malformed zip".to_string();
    // Fim do diretório central: 22 bytes + comentário de até 64 KB, a partir do final.
    let min = z.len().saturating_sub(22 + 0xFFFF);
    let eocd = (min..=z.len().saturating_sub(22))
        .rev()
        .find(|&i| u32_at(z, i) == Some(EOCD))
        .ok_or_else(bad)?;
    let count = u16_at(z, eocd.checked_add(10).ok_or_else(bad)?).ok_or_else(bad)? as usize;
    let mut pos = u32_at(z, eocd.checked_add(16).ok_or_else(bad)?).ok_or_else(bad)? as usize;
    let mut out = Vec::with_capacity(count.min(256));
    for _ in 0..count {
        if u32_at(z, pos) != Some(CEN) {
            return Err(bad());
        }
        let method = u16_at(z, pos.checked_add(10).ok_or_else(bad)?).ok_or_else(bad)?;
        let csize = u32_at(z, pos.checked_add(20).ok_or_else(bad)?).ok_or_else(bad)? as usize;
        let usize_ = u32_at(z, pos.checked_add(24).ok_or_else(bad)?).ok_or_else(bad)? as usize;
        let nlen = u16_at(z, pos.checked_add(28).ok_or_else(bad)?).ok_or_else(bad)? as usize;
        let elen = u16_at(z, pos.checked_add(30).ok_or_else(bad)?).ok_or_else(bad)? as usize;
        let clen = u16_at(z, pos.checked_add(32).ok_or_else(bad)?).ok_or_else(bad)? as usize;
        let local = u32_at(z, pos.checked_add(42).ok_or_else(bad)?).ok_or_else(bad)? as usize;
        let name_start = pos.checked_add(46).ok_or_else(bad)?;
        let name_end = name_start.checked_add(nlen).ok_or_else(bad)?;
        let name = z.get(name_start..name_end).ok_or_else(bad)?;
        out.push((
            String::from_utf8_lossy(name).into_owned(),
            method,
            csize,
            usize_,
            local,
        ));
        pos = name_end
            .checked_add(elen)
            .and_then(|p| p.checked_add(clen))
            .ok_or_else(bad)?;
    }
    Ok(out)
}

/// Conteúdo de uma entrada (guardada ou deflate), com o tamanho final conferido.
fn zip_read(
    z: &[u8],
    method: u16,
    csize: usize,
    size: usize,
    local: usize,
) -> Result<Vec<u8>, String> {
    const LOC: u32 = 0x0403_4b50;
    let bad = || "malformed zip entry".to_string();
    if u32_at(z, local) != Some(LOC) || size > MAX_ZIP {
        return Err(bad());
    }
    let nlen = u16_at(z, local.checked_add(26).ok_or_else(bad)?).ok_or_else(bad)? as usize;
    let elen = u16_at(z, local.checked_add(28).ok_or_else(bad)?).ok_or_else(bad)? as usize;
    let start = local
        .checked_add(30)
        .and_then(|p| p.checked_add(nlen))
        .and_then(|p| p.checked_add(elen))
        .ok_or_else(bad)?;
    let end = start.checked_add(csize).ok_or_else(bad)?;
    let data = z.get(start..end).ok_or_else(bad)?;
    let out = match method {
        0 => data.to_vec(),
        8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, size).map_err(|_| bad())?,
        m => return Err(format!("zip method {m} not supported")),
    };
    if out.len() == size {
        Ok(out)
    } else {
        Err(bad())
    }
}

/// O exe e os módulos de um zip já conferido (`VerifiedZip`), que precisa ser o da `version`
/// (`wintop-<versão>-windows-x64/...`).
pub fn unpack(verified: VerifiedZip<'_>, version: &str) -> Result<Package, String> {
    if parse_version(version).is_none() {
        return Err(format!("invalid version: {version}"));
    }
    let z = verified.0;
    let root = format!("wintop-{version}-windows-x64/");
    let entries = zip_entries(z)?;
    if entries.iter().any(|e| !e.0.starts_with(&root)) {
        return Err(format!("the package is not wintop {version}"));
    }
    let find = |rel: &str| {
        entries
            .iter()
            .find(|e| e.0 == format!("{root}{rel}"))
            .map(|&(_, m, c, s, l)| zip_read(z, m, c, s, l))
    };
    let exe = find("wintop.exe").ok_or("wintop.exe not in the package")??;
    let mut modules = Vec::new();
    for f in MODULE_FILES {
        if let Some(r) = find(&format!("modules/{f}")) {
            modules.push((f.to_string(), r?));
        }
    }
    if !modules.iter().any(|(n, _)| n == MODULE_FILES[0]) {
        return Err(format!("modules/{} not in the package", MODULE_FILES[0]));
    }
    Ok(Package { exe, modules })
}

/// Baixa e confere a release `version` (rejeita versão inválida ou downgrade).
pub fn download(version: &str) -> Result<Package, String> {
    if parse_version(version).is_none() {
        return Err(format!("invalid version: {version}"));
    }
    let cur = current();
    if !is_newer(version, &cur) {
        return Err(format!(
            "refusing downgrade or same version ({cur} -> {version})"
        ));
    }
    let base = format!("https://github.com/{REPO}/releases/download/v{version}");
    let name = format!("wintop-{version}-windows-x64.zip");
    let sig = get(&format!("{base}/{name}.sig"), MAX_SIG, DOWNLOAD_TIMEOUT)
        .map_err(|e| format!("signature: {e}"))?;
    let zip = get(&format!("{base}/{name}"), MAX_ZIP, DOWNLOAD_TIMEOUT)?;
    let verified = verify(&zip, &sig, &PUBLIC_KEY)?;
    unpack(verified, version)
}

/// Atualização aplicada no disco, pronta para reabrir a versão nova.
///
/// Na cópia portátil, mantém a pasta e o exe travados (sem escrita nem rename por processos sem
/// elevação) até `relaunch` abrir o processo filho.
pub struct PendingRestart {
    exe: PathBuf,
    _held: Option<(crate::userfile::UserDir, crate::userfile::Held)>,
}

impl std::fmt::Debug for PendingRestart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRestart")
            .field("exe", &self.exe)
            .field("held", &self._held.is_some())
            .finish()
    }
}

impl PendingRestart {
    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Reabre a versão nova no mesmo console e espera por ela. Na cópia portátil o exe segue
    /// travado (e conferido) até o processo filho existir.
    pub fn relaunch(self) -> std::io::Result<std::process::ExitStatus> {
        let child = std::process::Command::new(&self.exe).spawn();
        drop(self._held);
        child?.wait()
    }
}

/// Troca a cópia portátil em `dir` (pasta do usuário: tudo pelo `userfile`, sem seguir links).
/// O exe novo é gravado inteiro como `.new` antes de mexer no atual: a pasta nunca fica sem
/// `wintop.exe` além do instante entre os dois renames.
fn apply_portable(dir: &Path, pkg: &Package) -> Result<PendingRestart, String> {
    let d = crate::userfile::UserDir::open_exe_dir(dir)?;
    if !pkg.modules.is_empty() {
        let m = d
            .sub("modules", true)?
            .ok_or("modules folder not created")?;
        for (f, data) in &pkg.modules {
            m.write_bytes(f, data)?;
        }
    }
    d.write_bytes("wintop.exe.new", &pkg.exe)?;
    crate::install::swap_exe(
        || free_old(&d),
        || d.rename("wintop.exe", "wintop.exe.old").map(|_| ()),
        || d.rename("wintop.exe.new", "wintop.exe").map(|_| ()),
        || {
            let _ = d.rename("wintop.exe.old", "wintop.exe");
        },
        || {
            let _ = d.remove("wintop.exe.new");
        },
        || {
            // Apagado se ninguém o usa (o exe em execução fica até a próxima abertura).
            let _ = d.remove("wintop.exe.old");
        },
    )?;
    let held = d.hold("wintop.exe", &pkg.exe)?;
    Ok(PendingRestart {
        exe: dir.join("wintop.exe"),
        _held: Some((d, held)),
    })
}

/// Tira o `wintop.exe.old` do caminho (ver `install::free_old`): apagado, ou renomeado para um
/// nome único se ainda estiver em uso.
fn free_old(d: &crate::userfile::UserDir) -> Result<(), String> {
    if d.remove("wintop.exe.old").is_ok() {
        return Ok(());
    }
    d.rename("wintop.exe.old", &crate::install::unique_old_name())
        .map(|_| ())
}

/// Na partida: apaga (melhor esforço) as sobras `.old` de trocas anteriores que ninguém usa.
pub fn remove_stale_old(mode: &Mode) {
    match mode {
        Mode::Installed => crate::install::remove_stale_old(),
        Mode::Portable(d) | Mode::Undecided(d) => {
            let Ok(entries) = std::fs::read_dir(d) else {
                return;
            };
            let old: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| crate::install::is_old_exe(n))
                .collect();
            if old.is_empty() {
                return;
            }
            if let Ok(dir) = crate::userfile::UserDir::open_exe_dir(d) {
                for n in old {
                    let _ = dir.remove(&n);
                }
            }
        }
    }
}

/// Handle da atualização em segundo plano: cancela antes da troca em disco se a TUI sair, ou
/// aguarda a troca concluir (evita o SO encerrar a thread no meio dos renames do exe).
pub struct ApplyHandle {
    rx: mpsc::Receiver<Result<PendingRestart, String>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    writing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ApplyHandle {
    pub fn try_recv(&self) -> Result<Result<PendingRestart, String>, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for ApplyHandle {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.cancel.store(true, Ordering::Release);
        // Só bloqueia no `join` se a fase de gravação/troca em disco já começou (rápida, ms).
        if self.writing.load(Ordering::Acquire)
            && let Some(j) = self.join.take()
        {
            let _ = j.join();
        }
    }
}

/// Baixa, confere e aplica `version` na cópia em uso.
fn apply_with_guards(
    version: &str,
    mode: &Mode,
    cancel: &std::sync::atomic::AtomicBool,
    writing: &std::sync::atomic::AtomicBool,
) -> Result<PendingRestart, String> {
    use std::sync::atomic::Ordering;
    let pkg = download(version)?;
    if cancel.load(Ordering::Acquire) {
        return Err("update cancelled".into());
    }
    writing.store(true, Ordering::Release);
    if cancel.load(Ordering::Acquire) {
        writing.store(false, Ordering::Release);
        return Err("update cancelled".into());
    }
    let res = match mode {
        Mode::Installed => {
            crate::install::update_installed(&pkg.exe, &pkg.modules)?;
            let exe = crate::install::install_dir()?.join("wintop.exe");
            Ok(PendingRestart { exe, _held: None })
        }
        Mode::Portable(d) | Mode::Undecided(d) => apply_portable(d, &pkg),
    };
    writing.store(false, Ordering::Release);
    res
}

/// `apply` numa thread (a TUI segue desenhando).
pub fn spawn_apply(version: String, mode: Mode) -> ApplyHandle {
    let (tx, rx) = mpsc::sync_channel(1);
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let c = std::sync::Arc::clone(&cancel);
    let w = std::sync::Arc::clone(&writing);
    let tx_err = tx.clone();
    let join = match std::thread::Builder::new()
        .name("wintop-update".into())
        .spawn(move || {
            let r = std::panic::catch_unwind(|| apply_with_guards(&version, &mode, &c, &w))
                .unwrap_or_else(|_| Err("update failed (panic)".into()));
            let _ = tx.send(r);
        }) {
        Ok(h) => Some(h),
        Err(e) => {
            let _ = tx_err.send(Err(format!("update thread did not start: {e}")));
            None
        }
    };
    ApplyHandle {
        rx,
        cancel,
        writing,
        join,
    }
}

// ---------------------------------------------------------------------------------------------
// Primeira abertura

/// Escolha na primeira abertura.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Install,
    Portable,
    Quit,
}

/// Pergunta no console (antes da TUI): instalar, portátil ou sair.
pub fn ask_first_run(dir: &Path) -> Choice {
    use std::io::{BufRead, Write};
    let target = crate::install::install_dir().map_or_else(
        |_| r"C:\Program Files\wintop".into(),
        |d| d.display().to_string(),
    );
    println!("wintop {}\n", env!("CARGO_PKG_VERSION"));
    println!("This copy is not installed. How do you want to run it?\n");
    println!("  [I] Install to {target}");
    println!("      Start menu shortcut that opens wintop elevated without the UAC prompt.");
    println!("  [P] Portable: run from this folder, settings saved next to wintop.exe");
    println!("      ({})", dir.display());
    println!("  [Q] Quit\n");
    let stdin = std::io::stdin();
    loop {
        print!("Choice [I/P/Q]: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            return Choice::Quit;
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "i" | "install" => return Choice::Install,
            "p" | "portable" => return Choice::Portable,
            "q" | "quit" => return Choice::Quit,
            _ => {}
        }
    }
}

/// Espera Enter (o console aberto pelo duplo clique fecha junto com o processo).
pub fn pause() {
    use std::io::{BufRead, Write};
    print!("Press Enter to close.");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().lock().read_line(&mut String::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versoes() {
        assert!(is_newer("1.2.0", "1.1.1"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.1.1", "1.1.1"));
        assert!(!is_newer("1.1.0", "1.1.1"));
        assert!(!is_newer("1.2", "1.1.1"));
        assert!(!is_newer("1.2.0-rc1", "1.1.1"));
        assert!(!is_newer("1.2.0.1", "1.1.1"));
        assert!(!is_newer("+1.2.0", "1.1.1"));
        assert_eq!(parse_version("+1.2.0"), None);
        assert_eq!(parse_version("1.+2.0"), None);
        // Anti-downgrade e validação de versão em `download`.
        assert!(download("0.0.1").is_err());
        assert!(download("+9.9.9").is_err());
    }

    #[test]
    fn location_da_release() {
        assert_eq!(
            version_from_location("https://github.com/Guilherme-Weber/wintop/releases/tag/v1.2.0")
                .as_deref(),
            Some("1.2.0")
        );
        assert_eq!(
            version_from_location("https://github.com/x/releases/tag/1.2.0"),
            None
        );
        assert_eq!(version_from_location("https://github.com/x/releases"), None);
        assert_eq!(
            version_from_location("https://github.com/x/releases/tag/vabc"),
            None
        );
    }

    #[test]
    fn url_so_https() {
        assert_eq!(
            split_url("https://github.com/a/b?c=1").unwrap(),
            ("github.com", "/a/b?c=1")
        );
        assert_eq!(
            split_url("https://objects.githubusercontent.com/pkg.zip").unwrap(),
            ("objects.githubusercontent.com", "/pkg.zip")
        );
        assert_eq!(
            split_url("https://github.com").unwrap(),
            ("github.com", "/")
        );
        assert!(split_url("http://github.com/a").is_err());
        assert!(split_url("https://user@evil/a").is_err());
        assert!(split_url("https://h:80/a").is_err());
        assert!(split_url("https:///a").is_err());
        // SSRF / redirect para fora do GitHub recusados.
        assert!(split_url("https://evil.example/a").is_err());
        assert!(split_url("https://evilgithub.com/a").is_err());
        assert!(split_url("https://evilgithubusercontent.com/a").is_err());
        assert!(split_url("https://.githubusercontent.com/a").is_err());
    }

    /// Zip com método `method` (0 = stored, 8 = raw deflate).
    fn zip_method(entries: &[(&str, &[u8], u16)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cen = Vec::new();
        for (name, data, method) in entries {
            let payload: Vec<u8> = if *method == 8 {
                miniz_oxide::deflate::compress_to_vec(data, 6)
            } else {
                data.to_vec()
            };
            let off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(&0u32.to_le_bytes()); // crc
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&payload);
            cen.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            cen.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            cen.extend_from_slice(&method.to_le_bytes());
            cen.extend_from_slice(&[0, 0, 0, 0]);
            cen.extend_from_slice(&0u32.to_le_bytes());
            cen.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            cen.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cen.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cen.extend_from_slice(&[0; 12]);
            cen.extend_from_slice(&off.to_le_bytes());
            cen.extend_from_slice(name.as_bytes());
        }
        let cen_off = out.len() as u32;
        out.extend_from_slice(&cen);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        let n = entries.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(cen.len() as u32).to_le_bytes());
        out.extend_from_slice(&cen_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Zip mínimo (método 0) com as entradas dadas.
    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let e: Vec<(&str, &[u8], u16)> = entries.iter().map(|&(n, d)| (n, d, 0)).collect();
        zip_method(&e)
    }

    #[test]
    fn zip_do_pacote() {
        let z = zip(&[
            ("wintop-1.2.0-windows-x64/README.md", b"r"),
            ("wintop-1.2.0-windows-x64/modules/AMDFamily17.bin", b"mod"),
            ("wintop-1.2.0-windows-x64/modules/source/x.c", b"src"),
            ("wintop-1.2.0-windows-x64/wintop.exe", b"MZexe"),
        ]);
        let p = unpack(VerifiedZip(&z), "1.2.0").unwrap();
        assert_eq!(p.exe, b"MZexe");
        assert_eq!(
            p.modules,
            vec![("AMDFamily17.bin".to_string(), b"mod".to_vec())]
        );
        // Deflate (método 8, o usado na release real) + método não suportado.
        let zd = zip_method(&[
            (
                "wintop-1.2.0-windows-x64/wintop.exe",
                b"MZexe-deflate-payload",
                8,
            ),
            (
                "wintop-1.2.0-windows-x64/modules/AMDFamily17.bin",
                b"mod-deflate",
                8,
            ),
        ]);
        let pd = unpack(VerifiedZip(&zd), "1.2.0").unwrap();
        assert_eq!(pd.exe, b"MZexe-deflate-payload");
        assert_eq!(pd.modules[0].1, b"mod-deflate");
        let z_bad_method = zip_method(&[
            ("wintop-1.2.0-windows-x64/wintop.exe", b"MZ", 99),
            ("wintop-1.2.0-windows-x64/modules/AMDFamily17.bin", b"m", 0),
        ]);
        assert!(unpack(VerifiedZip(&z_bad_method), "1.2.0").is_err());
        // Outra versão no zip assinado: recusado (release antiga servida como nova).
        assert!(unpack(VerifiedZip(&z), "1.3.0").is_err());
        // Sem o exe ou sem o módulo.
        let z = zip(&[("wintop-1.2.0-windows-x64/modules/AMDFamily17.bin", b"m")]);
        assert!(unpack(VerifiedZip(&z), "1.2.0").is_err());
        let z = zip(&[("wintop-1.2.0-windows-x64/wintop.exe", b"MZ")]);
        assert!(unpack(VerifiedZip(&z), "1.2.0").is_err());
        // Entrada fora da pasta da versão.
        let z = zip(&[
            ("wintop-1.2.0-windows-x64/wintop.exe", b"MZ"),
            ("wintop-1.2.0-windows-x64/modules/AMDFamily17.bin", b"m"),
            ("../evil.dll", b"x"),
        ]);
        assert!(unpack(VerifiedZip(&z), "1.2.0").is_err());
    }

    #[test]
    fn fuzz_zip_do_atualizador() {
        let z0 = zip(&[
            ("wintop-1.2.0-windows-x64/wintop.exe", b"MZexe"),
            ("wintop-1.2.0-windows-x64/modules/AMDFamily17.bin", b"mod"),
        ]);
        let z8 = zip_method(&[
            ("wintop-1.2.0-windows-x64/wintop.exe", b"MZexe", 8),
            (
                "wintop-1.2.0-windows-x64/modules/AMDFamily17.bin",
                b"mod",
                8,
            ),
        ]);
        for n in 0..z0.len() {
            let _ = unpack(VerifiedZip(&z0[..n]), "1.2.0");
        }
        crate::fuzz::run("zip_updater", &[z0, z8], 512, |_, b| {
            let _ = unpack(VerifiedZip(b), "1.2.0");
        });
    }

    /// Troca portátil com o exe em execução (uma cópia do ping.exe rodando como wintop.exe), na
    /// pasta criada por este processo (elevado na CI: dono Administradores, aceito na pasta do
    /// exe): o atual vira `.old`, o novo assume o nome travado contra troca até o reinício, os
    /// módulos são gravados; uma segunda troca com o `.old` ainda em uso o põe de lado com nome
    /// único, e a limpeza da partida apaga as sobras.
    #[test]
    fn troca_portatil_com_exe_aberto() {
        let tmp = std::env::temp_dir().join(format!("wintop-upd-{}", std::process::id()));
        let dir = tmp.join("wintop-port");
        std::fs::create_dir_all(&dir).unwrap();
        let ping = crate::win::system_dir().unwrap().join("PING.EXE");
        std::fs::copy(&ping, dir.join("wintop.exe")).unwrap();
        let mut child = std::process::Command::new(dir.join("wintop.exe"))
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pkg = Package {
            exe: b"MZnew".to_vec(),
            modules: vec![("AMDFamily17.bin".into(), b"mod".to_vec())],
        };
        let r = apply_portable(&dir, &pkg);
        if r.is_err() {
            let _ = child.kill();
        }
        let pending = r.unwrap();
        assert_eq!(pending.exe(), dir.join("wintop.exe"));
        assert_eq!(std::fs::read(dir.join("wintop.exe")).unwrap(), b"MZnew");
        assert!(dir.join("wintop.exe.old").is_file());
        assert_eq!(
            std::fs::read(dir.join("modules").join("AMDFamily17.bin")).unwrap(),
            b"mod"
        );
        // Travado até o reinício: nem gravar nem renomear o exe novo.
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(dir.join("wintop.exe"))
                .is_err()
        );
        assert!(std::fs::rename(dir.join("wintop.exe"), dir.join("x.exe")).is_err());
        drop(pending);
        // Segunda troca com o .old ainda em execução.
        let pkg2 = Package {
            exe: b"MZnew2".to_vec(),
            modules: Vec::new(),
        };
        let r = apply_portable(&dir, &pkg2);
        let _ = child.kill();
        let _ = child.wait();
        drop(r.unwrap());
        assert_eq!(std::fs::read(dir.join("wintop.exe")).unwrap(), b"MZnew2");
        let names = || -> Vec<String> {
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                .collect()
        };
        let n = names();
        assert!(
            n.iter()
                .any(|x| x != "wintop.exe.old" && crate::install::is_old_exe(x)),
            "{n:?}"
        );
        assert!(
            !n.iter()
                .any(|x| x.ends_with(".tmp") || x.ends_with(".new") || x.ends_with(".pin")),
            "{n:?}"
        );
        // Partida seguinte: sobras apagadas.
        remove_stale_old(&Mode::Portable(dir.clone()));
        let n = names();
        assert!(!n.iter().any(|x| crate::install::is_old_exe(x)), "{n:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// O exe travado pelo `hold` (sem compartilhar escrita nem exclusão) ainda abre como processo:
    /// o `relaunch` cria o filho com a trava posta.
    #[test]
    fn exe_travado_ainda_executa() {
        let dir = std::env::temp_dir().join(format!("wintop-hold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ping = crate::win::system_dir().unwrap().join("PING.EXE");
        let bytes = std::fs::read(&ping).unwrap();
        std::fs::write(dir.join("wintop.exe"), &bytes).unwrap();
        let d = crate::userfile::UserDir::open_exe_dir(&dir).unwrap();
        assert!(
            d.hold("wintop.exe", b"outro").is_err(),
            "conteúdo diferente aceito"
        );
        let held = d.hold("wintop.exe", &bytes).unwrap();
        let st = std::process::Command::new(dir.join("wintop.exe"))
            .args(["-n", "1", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .status();
        drop(held);
        drop(d);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(st.unwrap().success());
    }

    #[test]
    fn nomes_das_sobras_old() {
        use crate::install::is_old_exe;
        assert!(is_old_exe("wintop.exe.old"));
        assert!(is_old_exe("WINTOP.EXE.12.345.old"));
        assert!(!is_old_exe("wintop.exe"));
        assert!(!is_old_exe("wintop.exe.new"));
        assert!(!is_old_exe("config.old"));
        assert!(!is_old_exe("wintop.exe.1.old"));
        assert!(!is_old_exe("wintop.exe.a.b.old"));
        assert!(!is_old_exe("wintop.exe..1.old"));
    }

    /// A chave com que a release.yml confere o `.sig` é a embutida aqui.
    #[test]
    fn chave_igual_a_do_workflow() {
        let yml = include_str!("../.github/workflows/release.yml");
        let hex = yml
            .lines()
            .find_map(|l| l.trim().strip_prefix("UPDATE_PUBLIC_KEY: "))
            .expect("UPDATE_PUBLIC_KEY na release.yml");
        let ours: String = PUBLIC_KEY.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex.trim(), ours);
    }

    /// Vetor de teste 1 da RFC 8032 (mensagem vazia) e a chave das releases bem formada.
    #[test]
    fn assinatura_ed25519() {
        let key: [u8; 32] = [
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ];
        let sig = [
            0xe5, 0x56, 0x43, 0x00, 0xc3, 0x60, 0xac, 0x72, 0x90, 0x86, 0xe2, 0xcc, 0x80, 0x6e,
            0x82, 0x8a, 0x84, 0x87, 0x7f, 0x1e, 0xb8, 0xe5, 0xd9, 0x74, 0xd8, 0x73, 0xe0, 0x65,
            0x22, 0x49, 0x01, 0x55, 0x5f, 0xb8, 0x82, 0x15, 0x90, 0xa3, 0x3b, 0xac, 0xc6, 0x1e,
            0x39, 0x70, 0x1c, 0xf9, 0xb4, 0x6b, 0xd2, 0x5b, 0xf5, 0xf0, 0x59, 0x5b, 0xbe, 0x24,
            0x65, 0x51, 0x41, 0x43, 0x8e, 0x7a, 0x10, 0x0b,
        ];
        assert!(verify(b"", &sig, &key).is_ok());
        assert!(verify(b"x", &sig, &key).is_err());
        assert!(verify(b"", &sig[..63], &key).is_err());
        assert!(verify(b"", &sig, &PUBLIC_KEY).is_err());
        assert!(ed25519_dalek::VerifyingKey::from_bytes(&PUBLIC_KEY).is_ok());
    }
}

/// Provas com rede e com o pacote real (ignoradas na suíte normal).
#[cfg(test)]
mod real {
    use super::*;

    /// `releases/latest` do GitHub responde com a tag de uma versão.
    #[test]
    #[ignore = "rede: consulta o GitHub"]
    fn rede_ultima_versao() {
        let v = latest_version().expect("releases/latest");
        assert!(parse_version(&v).is_some(), "{v}");
        eprintln!("última release: {v}");
    }

    /// Atualização real da cópia instalada (`<Program Files>\wintop`, terminal elevado,
    /// `WINTOP_TEST_INSTALLED=1`): baixa a última release, troca o exe e os módulos, e a cópia
    /// responde na versão nova, com a pasta ainda protegida.
    #[test]
    #[ignore = "troca a cópia instalada: terminal elevado e WINTOP_TEST_INSTALLED=1"]
    fn atualiza_a_copia_instalada() {
        assert_eq!(
            std::env::var("WINTOP_TEST_INSTALLED").as_deref(),
            Ok("1"),
            "troca a cópia em Program Files: confirme com WINTOP_TEST_INSTALLED=1"
        );
        let v = latest_version().expect("releases/latest");
        let pkg = download(&v).expect("pacote");
        crate::install::update_installed(&pkg.exe, &pkg.modules).expect("troca");
        let exe = crate::install::install_dir().unwrap().join("wintop.exe");
        let out = std::process::Command::new(&exe)
            .arg("--version")
            .output()
            .expect("--version");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("wintop {v}")
        );
        assert_eq!(std::fs::read(&exe).unwrap(), pkg.exe);
    }

    /// `WINTOP_TEST_ZIP` (pacote real) + `<zip>.sig`: assinatura confere com a chave das
    /// releases e o pacote abre na versão do nome.
    #[test]
    #[ignore = "precisa de WINTOP_TEST_ZIP com o .sig ao lado"]
    fn pacote_real_assinado() {
        let zip = std::env::var("WINTOP_TEST_ZIP").expect("WINTOP_TEST_ZIP");
        let data = std::fs::read(&zip).expect("zip");
        let sig = std::fs::read(format!("{zip}.sig")).expect("sig");
        let verified = verify(&data, &sig, &PUBLIC_KEY).expect("assinatura");
        let name = Path::new(&zip)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let v = name
            .strip_prefix("wintop-")
            .and_then(|s| s.strip_suffix("-windows-x64.zip"))
            .unwrap();
        let p = unpack(verified, v).expect("pacote");
        assert!(p.exe.starts_with(b"MZ"));
        let mut bad = data;
        let n = bad.len() / 2;
        bad[n] ^= 1;
        assert!(verify(&bad, &sig, &PUBLIC_KEY).is_err());
        eprintln!("exe {} bytes, {} módulos", p.exe.len(), p.modules.len());
    }
}
