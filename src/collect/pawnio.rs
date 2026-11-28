//! Sensores da CPU pelo PawnIO, com os módulos oficiais do PawnIO.Modules (LGPL-2.1, arquivos
//! separados em `modules/`):
//!
//! - AMD (`AMDFamily17`): Tctl, temperatura por CCD e potência do pacote. Registradores conforme
//!   o AMD PPR (mesma lógica do LibreHardwareMonitor), validados no Zen 3 (5700X3D) na Etapa 0.
//! - Intel (`IntelMSR`): temperatura do pacote (`IA32_PACKAGE_THERM_STATUS`) e de cada núcleo
//!   (`IA32_THERM_STATUS`, lido com a thread presa no núcleo) a partir do TjMax
//!   (`MSR_TEMPERATURE_TARGET`), e potência do pacote pelo RAPL (Intel SDM vol. 4).
//!
//! `PawnIOLib.dll` por caminho absoluto (`InstallLocation` do registro); o módulo vem de
//! `<pasta do exe>\modules\` (o `build.rs` copia para lá na compilação).

use std::ffi::CStr;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::time::Instant;

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, FreeLibrary, HANDLE, HMODULE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentThread, MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex,
    SYNCHRONIZATION_ACCESS_RIGHTS, SYNCHRONIZATION_SYNCHRONIZE, SetThreadGroupAffinity,
    WaitForSingleObject,
};
use windows::core::{HRESULT, PCSTR, PCWSTR, w};

use crate::model::{CpuSensors, Metric, fail, neutral};
use crate::win::{SecurityDescriptor, own, wide};

type FnOpen = unsafe extern "system" fn(*mut HANDLE) -> HRESULT;
type FnLoad = unsafe extern "system" fn(HANDLE, *const u8, usize) -> HRESULT;
type FnExec = unsafe extern "system" fn(
    HANDLE,
    PCSTR,
    *const u64,
    usize,
    *mut u64,
    usize,
    *mut usize,
) -> HRESULT;
type FnClose = unsafe extern "system" fn(HANDLE) -> HRESULT;

const SMN_THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const MSR_PWR_UNIT: u64 = 0xC001_0299;
const MSR_PKG_ENERGY_STAT: u64 = 0xC001_029B;
const AMD_MODULE: &str = "AMDFamily17.bin";

const INTEL_MODULE: &str = "IntelMSR.bin";
const IA32_THERM_STATUS: u64 = 0x19C;
const MSR_TEMPERATURE_TARGET: u64 = 0x1A2;
const IA32_PACKAGE_THERM_STATUS: u64 = 0x1B1;
const MSR_RAPL_POWER_UNIT: u64 = 0x606;
const MSR_PKG_ENERGY_STATUS: u64 = 0x611;
/// TjMax quando o `MSR_TEMPERATURE_TARGET` não informa (mesmo padrão do LibreHardwareMonitor).
const TJMAX_DEFAULT: f64 = 100.0;
/// Espera máxima pelo mutex `Access_PCI` (outro monitor lendo o SMN).
const PCI_WAIT_MS: u32 = 100;

/// O que foi validado por família/modelo. Só entra o que foi conferido numa CPU real.
#[derive(Debug, PartialEq, Eq)]
struct CpuSupport {
    /// Endereço SMN do CCD1 (os demais em +4); `None` = sensores de CCD não validados.
    ccd_base: Option<u32>,
    ccds: u32,
}

fn cpu_support(family: u32, model: u32) -> Option<CpuSupport> {
    match (family, model) {
        // Zen 3 Vermeer (Ryzen 5000 desktop): validado no 5700X3D (19h, modelo 21h).
        (0x19, 0x20..=0x2F) => Some(CpuSupport {
            ccd_base: Some(0x0005_9954),
            ccds: 2,
        }),
        // Tctl e potência pelo mesmo caminho em 17h-1Ah; CCD muda entre gerações.
        (0x17..=0x1A, _) => Some(CpuSupport {
            ccd_base: None,
            ccds: 0,
        }),
        _ => None,
    }
}

/// Tctl em °C: bits 31:21 em passos de 0,125 °C; com o bit 19 a escala começa em -49 °C.
fn tctl_from_raw(raw: u32) -> f64 {
    let t = f64::from(raw >> 21) * 0.125;
    if raw & (1 << 19) != 0 { t - 49.0 } else { t }
}

/// CCD: bit 11 = válido; bits 10:0 em passos de 0,125 °C a partir de -49 °C.
fn ccd_from_raw(raw: u32) -> Option<f64> {
    (raw & (1 << 11) != 0).then(|| f64::from(raw & 0x7FF) * 0.125 - 49.0)
}

/// TjMax em °C: bits 23:16 do `MSR_TEMPERATURE_TARGET` (zero = não informado).
fn tjmax_from_raw(raw: u64) -> f64 {
    match (raw >> 16) & 0xFF {
        0 => TJMAX_DEFAULT,
        t => t as f64,
    }
}

/// Temperatura de `IA32_THERM_STATUS`/`IA32_PACKAGE_THERM_STATUS`: bits 22:16 = graus abaixo
/// do TjMax. No do núcleo, o bit 31 diz se a leitura é válida (o do pacote não tem esse bit).
fn intel_temp(raw: u64, tjmax: f64, needs_valid: bool) -> Option<f64> {
    (!needs_valid || raw & (1 << 31) != 0).then_some(tjmax - ((raw >> 16) & 0x7F) as f64)
}

/// Joules por contagem do RAPL e da AMD: bits 12:8 da unidade de energia (1 / 2^ESU).
fn joules_per_count(unit: u64) -> f64 {
    1.0 / f64::from(1u32 << ((unit >> 8) & 0x1F))
}

/// Sensores térmicos do CPUID 6 (EAX bit 0 = DTS por núcleo, bit 6 = sensor do pacote).
fn intel_thermal_features() -> (bool, bool) {
    use std::arch::x86_64::__cpuid;
    if __cpuid(0).eax < 6 {
        return (false, false);
    }
    let eax = __cpuid(6).eax;
    (eax & 1 != 0, eax & (1 << 6) != 0)
}

/// Um lógico para prender a thread: grupo e máscara de um único bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pin {
    group: u16,
    mask: u64,
}

/// Núcleos e pacotes físicos (um lógico de cada) pelos registros
/// `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX`: `GroupCount` u16 em +30 e `GROUP_AFFINITY` (máscara
/// u64, grupo u16) de 16 bytes a partir de +32. O primeiro bit da primeira máscara representa o
/// núcleo (os irmãos SMT leem o mesmo sensor).
fn parse_pins(b: &[u8]) -> (Vec<Pin>, Vec<Pin>) {
    const CORE: u32 = 0;
    const PACKAGE: u32 = 3;
    let (mut cores, mut packages) = (Vec::new(), Vec::new());
    let u32_at = |o: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            b.get(o..o.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    let mut off = 0usize;
    while let (Some(rel), Some(size)) = (u32_at(off), u32_at(off.saturating_add(4))) {
        let size = size as usize;
        let Some(end) = off.checked_add(size).filter(|&e| size >= 8 && e <= b.len()) else {
            break;
        };
        let rec = &b[off..end];
        if rel == CORE || rel == PACKAGE {
            let first = || -> Option<Pin> {
                let groups = u16::from_le_bytes(rec.get(30..32)?.try_into().ok()?);
                if groups == 0 {
                    return None;
                }
                let mask = u64::from_le_bytes(rec.get(32..40)?.try_into().ok()?);
                let group = u16::from_le_bytes(rec.get(40..42)?.try_into().ok()?);
                (mask != 0).then(|| Pin {
                    group,
                    mask: 1 << mask.trailing_zeros(),
                })
            };
            if let Some(p) = first() {
                if rel == CORE {
                    cores.push(p);
                } else {
                    packages.push(p);
                }
            }
        }
        off = end;
    }
    (cores, packages)
}

/// Núcleos e pacotes pela topologia do Windows (buffer alinhado em 8).
fn processor_pins() -> Result<(Vec<Pin>, Vec<Pin>), String> {
    use windows::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationAll,
    };
    let mut len = 0u32;
    // SAFETY: primeira chamada só mede o buffer.
    let _ = unsafe { GetLogicalProcessorInformationEx(RelationAll, None, &raw mut len) };
    if len == 0 {
        return Err("GetLogicalProcessorInformationEx returned no data".into());
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: buffer alinhado de pelo menos `len` bytes.
    unsafe {
        GetLogicalProcessorInformationEx(RelationAll, Some(buf.as_mut_ptr().cast()), &raw mut len)
    }
    .map_err(|e| format!("GetLogicalProcessorInformationEx: {e}"))?;
    // SAFETY: Vec<u64> visto como bytes, dentro do tamanho alocado.
    let bytes = unsafe {
        std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), (len as usize).min(buf.len() * 8))
    };
    let (cores, packages) = parse_pins(bytes);
    if cores.is_empty() || packages.is_empty() {
        return Err("processor topology has no cores".into());
    }
    Ok((cores, packages))
}

/// Afinidade da thread trocada para um lógico; a original volta no Drop. O MSR é lido no
/// processador em que a chamada ao driver roda: cada núcleo exige a thread nele.
struct Pinned {
    previous: GROUP_AFFINITY,
}

impl Pinned {
    fn to(p: Pin) -> Result<Self, String> {
        let ga = GROUP_AFFINITY {
            Mask: p.mask as usize,
            Group: p.group,
            Reserved: [0; 3],
        };
        let mut previous = GROUP_AFFINITY::default();
        // SAFETY: pseudo-handle da própria thread; estruturas locais.
        unsafe {
            SetThreadGroupAffinity(GetCurrentThread(), &raw const ga, Some(&raw mut previous))
        }
        .ok()
        .map_err(|e| format!("thread affinity (group {}): {e}", p.group))?;
        Ok(Self { previous })
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        // SAFETY: devolve a afinidade lida na troca.
        unsafe {
            let _ = SetThreadGroupAffinity(GetCurrentThread(), &raw const self.previous, None);
        }
    }
}

/// (fabricante, família, modelo) pelo CPUID.
fn cpu_id() -> (String, u32, u32) {
    use std::arch::x86_64::__cpuid;
    let r0 = __cpuid(0);
    let vendor: Vec<u8> = [r0.ebx, r0.edx, r0.ecx]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let r = __cpuid(1);
    let base = (r.eax >> 8) & 0xF;
    let family = if base == 0xF {
        base + ((r.eax >> 20) & 0xFF)
    } else {
        base
    };
    let model = if base == 0x6 || base == 0xF {
        ((r.eax >> 4) & 0xF) | ((r.eax >> 12) & 0xF0)
    } else {
        (r.eax >> 4) & 0xF
    };
    (String::from_utf8_lossy(&vendor).into_owned(), family, model)
}

/// Handle do executor do PawnIO; fecha com `pawnio_close` no Drop.
struct Executor {
    handle: HANDLE,
    close: FnClose,
}

impl Drop for Executor {
    fn drop(&mut self) {
        // SAFETY: handle obtido com `pawnio_open` e fechado uma única vez.
        unsafe {
            let _ = (self.close)(self.handle);
        }
    }
}

// SAFETY: o executor só é usado pela thread que possui o coletor (sem acesso simultâneo).
unsafe impl Send for Executor {}

struct PawnIo {
    exec_fn: FnExec,
    /// `Global\Access_PCI` (só para o SMN da AMD).
    pci_mutex: Option<OwnedHandle>,
    executor: Executor,
    /// Por último: a DLL só é liberada depois do `pawnio_close` do executor.
    _lib: Library,
}

/// `PawnIOLib.dll` carregada; `FreeLibrary` no Drop (também nos caminhos de erro de `open`).
struct Library(HMODULE);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: módulo carregado por LoadLibraryExW; nenhum ponteiro de função dele é usado
        // depois daqui (o executor, declarado depois na função e antes na struct, já fechou).
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

// SAFETY: o módulo pode ser liberado de qualquer thread.
unsafe impl Send for Library {}

/// Guarda do mutex `Access_PCI`: libera no Drop.
struct PciLock<'a>(&'a OwnedHandle);

impl Drop for PciLock<'_> {
    fn drop(&mut self) {
        // SAFETY: o mutex foi adquirido por esta thread em `PawnIo::lock`.
        unsafe {
            let _ = ReleaseMutex(HANDLE(self.0.as_raw_handle()));
        }
    }
}

impl PawnIo {
    /// `name`: nome do módulo para as mensagens; `pci`: abrir o mutex `Access_PCI` (SMN).
    fn open(module: &[u8], name: &str, pci: bool) -> Result<Self, String> {
        let dll = install_dir()?.join("PawnIOLib.dll");
        // Carregada no processo elevado: só de uma pasta que ninguém sem elevação altera (um
        // PawnIO instalado numa pasta do usuário viraria código do usuário rodando elevado).
        crate::install::trusted_file(&dll)?;
        let dll_w = wide(&dll.to_string_lossy());
        // SAFETY: caminho absoluto terminado em zero; busca restrita à pasta da DLL e System32.
        // `Library` libera a DLL quando `PawnIo` (ou um erro abaixo) a solta.
        let lib = unsafe {
            LoadLibraryExW(
                PCWSTR(dll_w.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
        .map_err(|e| format!("PawnIOLib.dll at {}: {e}", dll.display()))?;
        let library = Library(lib);
        let sym = |name: &CStr| {
            // SAFETY: `lib` é válido; o nome é uma string C estática.
            unsafe { GetProcAddress(lib, PCSTR(name.as_ptr().cast())) }
                .ok_or_else(|| format!("PawnIOLib.dll has no {name:?}"))
        };
        type Raw = unsafe extern "system" fn() -> isize;
        // SAFETY: assinaturas conforme PawnIOLib.h (HRESULT STDAPICALLTYPE).
        let (open, load, exec_fn, close) = unsafe {
            (
                std::mem::transmute::<Raw, FnOpen>(sym(c"pawnio_open")?),
                std::mem::transmute::<Raw, FnLoad>(sym(c"pawnio_load")?),
                std::mem::transmute::<Raw, FnExec>(sym(c"pawnio_execute")?),
                std::mem::transmute::<Raw, FnClose>(sym(c"pawnio_close")?),
            )
        };
        let mut handle = HANDLE::default();
        // SAFETY: ponteiro válido para HANDLE.
        unsafe { open(&raw mut handle) }
            .ok()
            .map_err(|e| format!("pawnio_open (driver loaded? elevated?): {e}"))?;
        let executor = Executor { handle, close };
        // SAFETY: `module` vive durante a chamada; o driver copia o blob.
        unsafe { load(executor.handle, module.as_ptr(), module.len()) }
            .ok()
            .map_err(|e| format!("pawnio_load({name}): {e}"))?;
        Ok(Self {
            exec_fn,
            pci_mutex: if pci { Some(open_pci_mutex()?) } else { None },
            executor,
            _lib: library,
        })
    }

    fn call(&self, name: &CStr, arg: u64) -> Result<u64, String> {
        let input = [arg];
        let mut out = [0u64; 1];
        let mut written = 0usize;
        // SAFETY: buffers locais com os tamanhos informados.
        let hr = unsafe {
            (self.exec_fn)(
                self.executor.handle,
                PCSTR(name.as_ptr().cast()),
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
                &raw mut written,
            )
        };
        if hr.is_err() {
            return Err(format!("{name:?}({arg:#x}): {hr}"));
        }
        // Sucesso sem valor escrito não pode virar "0" silencioso.
        if written < 1 {
            return Err(format!("{name:?}({arg:#x}): no value returned"));
        }
        Ok(out[0])
    }

    /// Mutex global que os monitores (HWiNFO, LHM, ...) usam para não intercalar os acessos de
    /// índice/dado do SMN.
    fn lock(&self) -> Result<PciLock<'_>, String> {
        let mutex = self
            .pci_mutex
            .as_ref()
            .ok_or("Access_PCI mutex not opened")?;
        // SAFETY: handle de mutex válido.
        let w = unsafe { WaitForSingleObject(HANDLE(mutex.as_raw_handle()), PCI_WAIT_MS) };
        if w == WAIT_OBJECT_0 || w == WAIT_ABANDONED {
            Ok(PciLock(mutex))
        } else if w == WAIT_FAILED {
            Err(format!(
                "Access_PCI: {}",
                windows::core::Error::from_thread()
            ))
        } else {
            Err(format!(
                "Access_PCI held by another monitor ({PCI_WAIT_MS} ms)"
            ))
        }
    }

    /// Leitura SMN com `Access_PCI` já seguro por quem chama (`lock`).
    fn smn_locked(&self, addr: u32) -> Result<u32, String> {
        self.call(c"ioctl_read_smn", u64::from(addr))
            .map(|v| v as u32)
    }

    fn msr(&self, msr: u64) -> Result<u64, String> {
        self.call(c"ioctl_read_msr", msr)
    }
}

/// Abre ou cria `Global\Access_PCI`. Se o wintop for o primeiro a criá-lo, a DACL deixa
/// monitores sem elevação esperar e liberar o mutex (SYNCHRONIZE | MUTEX_MODIFY_STATE para
/// usuários autenticados).
fn open_pci_mutex() -> Result<OwnedHandle, String> {
    // S:(ML;;NW;;;ME): sem o rótulo, o objeto criado por um processo elevado fica com
    // integridade alta e a escrita (MUTEX_MODIFY_STATE) é negada a quem roda em média.
    let sd = SecurityDescriptor::from_sddl(
        "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x100001;;;AU)S:(ML;;NW;;;ME)",
    )
    .map_err(|e| format!("Access_PCI mutex {e}"))?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.raw().0,
        bInheritHandle: false.into(),
    };
    // SAFETY: atributos válidos enquanto `sd` vive; se o mutex já existir, só é aberto.
    let r = unsafe { CreateMutexW(Some(&raw const sa), false, w!("Global\\Access_PCI")) };
    drop(sd);
    let h = match r {
        Ok(h) => h,
        // Já criado por um serviço (SYSTEM) com DACL restrita: o CreateMutexW pede acesso
        // total; basta esperar e liberar.
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            // SAFETY: nome estático.
            unsafe {
                OpenMutexW(
                    SYNCHRONIZATION_ACCESS_RIGHTS(
                        SYNCHRONIZATION_SYNCHRONIZE.0 | MUTEX_MODIFY_STATE.0,
                    ),
                    false,
                    w!("Global\\Access_PCI"),
                )
            }
            .map_err(|e| format!("Access_PCI mutex: {e}"))?
        }
        Err(e) => return Err(format!("Access_PCI mutex: {e}")),
    };
    // SAFETY: handle válido e sem outro dono.
    Ok(unsafe { own(h) })
}

/// `PawnIOLib.dll` da instalação do PawnIO (para a auditoria do `--check`).
pub fn dll_path() -> Option<PathBuf> {
    install_dir().ok().map(|d| d.join("PawnIOLib.dll"))
}

/// Pasta de instalação do PawnIO pelo registro. Sem fallback fixo: valor ausente, vazio ou
/// relativo é erro (processo elevado não carrega DLL de caminho incerto). Só `REG_SZ` (o que o
/// instalador grava): um `REG_EXPAND_SZ` seria recusado por `win::reg_sz` (o ambiente é herdado
/// de quem abriu o wintop).
fn install_dir() -> Result<PathBuf, String> {
    let raw = crate::win::reg_sz(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO",
        "InstallLocation",
    )
    .ok_or_else(|| "PawnIO not installed (InstallLocation missing)".to_string())?;
    let dir = PathBuf::from(raw);
    if !crate::win::is_local_absolute(&dir) {
        return Err(format!(
            "invalid PawnIO InstallLocation: {:?}",
            dir.display()
        ));
    }
    Ok(dir)
}

/// Caminho do módulo: `<pasta do exe>\modules\` (o `build.rs` copia para `target\<perfil>`).
/// Só nos testes (o executável de teste fica na pasta de build do Cargo) vale também a pasta
/// `modules\` do repositório; o binário distribuído nunca procura fora da própria pasta.
fn module_path(module: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe.parent().ok_or("executable has no folder")?;
    let primary = dir.join("modules").join(module);
    if primary.is_file() {
        return Ok(primary);
    }
    #[cfg(test)]
    {
        let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("modules")
            .join(module);
        if dev.is_file() {
            return Ok(dev);
        }
    }
    Err(format!(
        "{module} not found in {} (reinstall wintop from the full zip)",
        dir.join("modules").display()
    ))
}

/// Teto para os módulos (os reais têm ~10 KB).
const MAX_MODULE_BYTES: u64 = 16 << 20;

/// Lê um módulo do PawnIO pelo handle sem seguir reparse points, hardlinks, pipes ou UNC, com
/// teto de tamanho (seguro mesmo em pasta portátil gravável pelo usuário).
fn read_module_safe(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType, OPEN_EXISTING, SECURITY_ANONYMOUS,
        SECURITY_SQOS_PRESENT,
    };

    if !crate::win::is_local_absolute(path) {
        return Err(format!("{} is not a local path (refused)", path.display()));
    }
    let w = crate::win::wide_os(path.as_os_str());
    // SAFETY: caminho terminado em zero; flags impedem seguir reparse points ou impersonação.
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
            None,
        )
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: CreateFileW teve sucesso; handle válido e sem outro dono.
    let owned = unsafe { own(h) };
    let raw = HANDLE(owned.as_raw_handle());
    // SAFETY: handle válido.
    if unsafe { GetFileType(raw) } != FILE_TYPE_DISK {
        return Err(format!("{} is not a disk file (refused)", path.display()));
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: handle válido; estrutura local.
    unsafe { GetFileInformationByHandle(raw, &raw mut info) }
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(format!("{} is a link (refused)", path.display()));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        return Err(format!("{} is a directory (refused)", path.display()));
    }
    if info.nNumberOfLinks > 1 {
        return Err(format!("{} is a hardlink (refused)", path.display()));
    }
    let mut buf = Vec::new();
    let n = std::fs::File::from(owned)
        .take(MAX_MODULE_BYTES.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if n as u64 > MAX_MODULE_BYTES {
        return Err(format!(
            "{} is larger than {} MB",
            path.display(),
            MAX_MODULE_BYTES >> 20
        ));
    }
    Ok(buf)
}

/// Núcleos e pacotes da Intel, com o TjMax de cada um (lido na abertura).
struct IntelTopo {
    /// (lógico do núcleo, TjMax).
    cores: Vec<(Pin, f64)>,
    /// (lógico do pacote, TjMax).
    packages: Vec<(Pin, f64)>,
    /// `IA32_PACKAGE_THERM_STATUS` existe (CPUID 6, bit 6).
    package_sensor: bool,
}

enum Kind {
    Amd(CpuSupport),
    Intel(IntelTopo),
}

enum State {
    /// CPU fora dos módulos (estado neutro).
    Unsupported(String),
    Failed(String),
    Ready {
        pio: Box<PawnIo>,
        kind: Kind,
        /// Joules por contagem do contador de energia; `Err` = sem contador (causa).
        joules_per_count: Result<f64, String>,
        /// (instante, contador de energia de 32 bits) da leitura anterior, por pacote.
        last_energy: Vec<Option<(Instant, u32)>>,
    },
}

pub struct PawnIoCollector {
    state: State,
}

impl PawnIoCollector {
    pub fn new() -> Self {
        let (vendor, family, model) = cpu_id();
        let state = match vendor.as_str() {
            "AuthenticAMD" => match cpu_support(family, model) {
                Some(support) => Self::open_amd(support).unwrap_or_else(State::Failed),
                None => State::Unsupported(format!(
                    "CPU not supported (family {family:#x} outside 17h-1Ah)"
                )),
            },
            "GenuineIntel" => match intel_thermal_features() {
                (false, _) => State::Unsupported("CPU has no digital thermal sensor".into()),
                (true, package_sensor) => {
                    Self::open_intel(package_sensor).unwrap_or_else(State::Failed)
                }
            },
            other => State::Unsupported(format!("CPU not supported ({})", other.trim())),
        };
        Self { state }
    }

    fn open_amd(support: CpuSupport) -> Result<State, String> {
        let module = read_module_safe(&module_path(AMD_MODULE)?)?;
        let pio = PawnIo::open(&module, "AMDFamily17", true)?;
        let unit = pio.msr(MSR_PWR_UNIT)?;
        Ok(State::Ready {
            pio: Box::new(pio),
            kind: Kind::Amd(support),
            joules_per_count: Ok(joules_per_count(unit)),
            last_energy: vec![None],
        })
    }

    fn open_intel(package_sensor: bool) -> Result<State, String> {
        let (cores, packages) = processor_pins()?;
        let module = read_module_safe(&module_path(INTEL_MODULE)?)?;
        let pio = PawnIo::open(&module, "IntelMSR", false)?;
        // TjMax de cada núcleo e pacote, com a thread nele (pode variar entre soquetes).
        let tjmax = |p: Pin| -> Result<(Pin, f64), String> {
            let _pin = Pinned::to(p)?;
            let t = pio
                .msr(MSR_TEMPERATURE_TARGET)
                .map_or(TJMAX_DEFAULT, tjmax_from_raw);
            Ok((p, t))
        };
        let cores = cores
            .into_iter()
            .map(tjmax)
            .collect::<Result<Vec<_>, _>>()?;
        let packages = packages
            .into_iter()
            .map(tjmax)
            .collect::<Result<Vec<_>, _>>()?;
        // Sem RAPL (anterior ao Sandy Bridge) o MSR não existe: a potência mostra a causa.
        let jpc = pio
            .msr(MSR_RAPL_POWER_UNIT)
            .map(joules_per_count)
            .map_err(|e| format!("RAPL not available: {e}"));
        let n = packages.len();
        Ok(State::Ready {
            pio: Box::new(pio),
            kind: Kind::Intel(IntelTopo {
                cores,
                packages,
                package_sensor,
            }),
            joules_per_count: jpc,
            last_energy: vec![None; n],
        })
    }

    pub fn read(&mut self) -> Metric<CpuSensors> {
        match &mut self.state {
            State::Unsupported(s) => neutral(s.clone()),
            State::Failed(e) => fail(e.clone()),
            State::Ready {
                pio,
                kind: Kind::Amd(support),
                joules_per_count,
                last_energy,
            } => Ok(read_amd(pio, support, joules_per_count, last_energy)),
            State::Ready {
                pio,
                kind: Kind::Intel(topo),
                joules_per_count,
                last_energy,
            } => Ok(read_intel(pio, topo, joules_per_count, last_energy)),
        }
    }
}

fn read_amd(
    pio: &PawnIo,
    support: &CpuSupport,
    jpc: &Result<f64, String>,
    last_energy: &mut [Option<(Instant, u32)>],
) -> CpuSensors {
    // Um `Access_PCI` para todas as leituras SMN da coleta: menos disputa com outros
    // monitores, e Tctl e CCDs ficam coerentes (ou todos lidos, ou a mesma causa).
    let lock = pio.lock();
    let smn = |addr: u32| -> Result<u32, String> {
        lock.as_ref().map_err(Clone::clone)?;
        pio.smn_locked(addr)
    };
    let tctl = smn(SMN_THM_TCON_CUR_TMP).map(tctl_from_raw).or_else(fail);
    let ccds = match support.ccd_base {
        None => neutral("not validated on this CPU family"),
        Some(base) => {
            let mut list = Vec::new();
            let mut err = None;
            for c in 0..support.ccds {
                match smn(base + c * 4) {
                    Ok(raw) => {
                        if let Some(t) = ccd_from_raw(raw) {
                            list.push((format!("CCD{}", c + 1), t));
                        }
                    }
                    Err(e) => err = Some(format!("CCD{}: {e}", c + 1)),
                }
            }
            // Um CCD que falhou não some calado: a causa aparece no lugar da lista.
            match (list.is_empty(), err) {
                (_, Some(e)) => fail(e),
                (false, None) => Ok(list),
                (true, None) => fail("no valid CCD sensor"),
            }
        }
    };
    drop(lock);
    let energy = pio.msr(MSR_PKG_ENERGY_STAT);
    let package_power = match last_energy.first_mut() {
        Some(last) => power(jpc, last, energy),
        None => fail("no energy counter"),
    };
    CpuSensors::amd(tctl, ccds, package_power)
}

fn read_intel(
    pio: &PawnIo,
    topo: &IntelTopo,
    jpc: &Result<f64, String>,
    last_energy: &mut [Option<(Instant, u32)>],
) -> CpuSensors {
    let mut zones = Vec::with_capacity(topo.cores.len() + topo.packages.len());
    let mut err = None;
    for (i, &(pin, tjmax)) in topo.cores.iter().enumerate() {
        let raw = Pinned::to(pin).and_then(|_pin| pio.msr(IA32_THERM_STATUS));
        match raw {
            Ok(raw) => {
                if let Some(t) = intel_temp(raw, tjmax, true) {
                    zones.push((format!("Core {i}"), t));
                }
            }
            Err(e) => err = Some(format!("Core {i}: {e}")),
        }
    }
    let multi = topo.packages.len() > 1;
    let mut hottest_pkg: Option<f64> = None;
    let mut pkg_err = None;
    let mut watts_sum: Metric<f64> = Ok(0.0);
    for (p, (&(pin, tjmax), last)) in topo.packages.iter().zip(last_energy).enumerate() {
        let pinned = Pinned::to(pin);
        let read = |msr: u64| -> Result<u64, String> {
            pinned.as_ref().map_err(Clone::clone)?;
            pio.msr(msr)
        };
        if topo.package_sensor {
            match read(IA32_PACKAGE_THERM_STATUS) {
                Ok(raw) => {
                    let t = intel_temp(raw, tjmax, false).unwrap_or(tjmax);
                    hottest_pkg = Some(hottest_pkg.map_or(t, |h| h.max(t)));
                    if multi {
                        zones.push((format!("Package {p}"), t));
                    }
                }
                Err(e) => pkg_err = Some(format!("package {p}: {e}")),
            }
        }
        let w = power(jpc, last, read(MSR_PKG_ENERGY_STATUS));
        drop(pinned);
        watts_sum = match (watts_sum, w) {
            (Ok(a), Ok(b)) => Ok(a + b),
            (Err(e), _) | (_, Err(e)) => Err(e),
        };
    }
    // Leitura principal: o pacote mais quente; sem sensor de pacote, o núcleo mais quente.
    let main = match (pkg_err, hottest_pkg) {
        (Some(e), _) => fail(e),
        (None, Some(t)) => Ok(t),
        (None, None) => zones
            .iter()
            .map(|z| z.1)
            .reduce(f64::max)
            .map_or_else(|| fail("no valid temperature sensor"), Ok),
    };
    let zones = match (zones.is_empty(), err) {
        (_, Some(e)) => fail(e),
        (false, None) => Ok(zones),
        (true, None) => fail("no valid core sensor"),
    };
    let limit = topo
        .packages
        .iter()
        .chain(&topo.cores)
        .map(|p| p.1)
        .reduce(f64::max)
        .unwrap_or(TJMAX_DEFAULT);
    CpuSensors {
        label: "Package",
        zone: "Core",
        limit,
        tctl: main,
        ccds: zones,
        package_power: watts_sum,
    }
}

/// Potência pelo contador de energia de 32 bits (precisa de duas leituras).
fn power(
    jpc: &Result<f64, String>,
    last: &mut Option<(Instant, u32)>,
    energy: Result<u64, String>,
) -> Metric<f64> {
    let jpc = match jpc {
        Ok(j) => *j,
        Err(e) => return fail(e.clone()),
    };
    let raw = match energy {
        Ok(raw) => raw,
        Err(e) => return fail(e),
    };
    let now = (Instant::now(), raw as u32);
    match last.replace(now) {
        None => fail("waiting for a second sample"),
        Some(prev) => {
            watts(prev, now, jpc).map_or_else(|| fail("no time elapsed since the last sample"), Ok)
        }
    }
}

/// Watts entre duas leituras do contador de energia de 32 bits (a volta é normal).
fn watts(prev: (Instant, u32), now: (Instant, u32), joules_per_count: f64) -> Option<f64> {
    let dt = now.0.checked_duration_since(prev.0)?.as_secs_f64();
    (dt > 0.0).then(|| f64::from(now.1.wrapping_sub(prev.1)) * joules_per_count / dt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn tctl_sem_e_com_faixa_estendida() {
        assert!((tctl_from_raw(400 << 21) - 50.0).abs() < 1e-9);
        assert!((tctl_from_raw((400 << 21) | (1 << 19)) - 1.0).abs() < 1e-9);
        // Leitura real do 5700X3D (spike de 2026-11-20): 0x673b0000 -> 54,125 °C.
        assert!((tctl_from_raw(0x673b_0000) - 54.125).abs() < 1e-9);
    }

    #[test]
    fn ccd_invalido_e_valido() {
        assert_eq!(ccd_from_raw(0), None);
        assert!((ccd_from_raw((1 << 11) | 712).expect("válido") - 40.0).abs() < 1e-9);
    }

    #[test]
    fn intel_tjmax_e_leituras() {
        // TjMax 100 °C (bits 23:16 = 0x64); zero = padrão de 100.
        assert!((tjmax_from_raw(0x0064_0000) - 100.0).abs() < 1e-9);
        assert!((tjmax_from_raw(0x0069_0000) - 105.0).abs() < 1e-9);
        assert!((tjmax_from_raw(0) - TJMAX_DEFAULT).abs() < 1e-9);
        // Núcleo: 48 graus abaixo do TjMax com o bit 31 (válido).
        let core = (1u64 << 31) | (48 << 16);
        assert!((intel_temp(core, 100.0, true).expect("válido") - 52.0).abs() < 1e-9);
        assert_eq!(intel_temp(48 << 16, 100.0, true), None);
        // Pacote: sem o bit de validade.
        assert!((intel_temp(48 << 16, 100.0, false).expect("pacote") - 52.0).abs() < 1e-9);
        // RAPL: ESU = 14 (Haswell) -> 1/16384 J.
        assert!((joules_per_count(0x000A_0E03) - 1.0 / 16384.0).abs() < 1e-12);
    }

    /// Registro `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` de núcleo/pacote com uma máscara.
    fn pin_record(rel: u32, mask: u64, group: u16) -> Vec<u8> {
        let mut r = vec![0u8; 48];
        r[..4].copy_from_slice(&rel.to_le_bytes());
        r[4..8].copy_from_slice(&48u32.to_le_bytes());
        r[30..32].copy_from_slice(&1u16.to_le_bytes());
        r[32..40].copy_from_slice(&mask.to_le_bytes());
        r[40..42].copy_from_slice(&group.to_le_bytes());
        r
    }

    #[test]
    fn nucleos_e_pacotes_da_topologia() {
        let mut b = pin_record(3, 0xFF, 0);
        b.extend(pin_record(0, 0b1100, 0));
        b.extend(pin_record(2, 0, 0)); // cache: ignorado
        b.extend(pin_record(0, 1 << 40, 1));
        let (cores, packages) = parse_pins(&b);
        assert_eq!(
            cores,
            vec![
                Pin {
                    group: 0,
                    mask: 0b100
                },
                Pin {
                    group: 1,
                    mask: 1 << 40
                }
            ]
        );
        assert_eq!(packages, vec![Pin { group: 0, mask: 1 }]);
        // Registro truncado ou com tamanho zero não trava nem lê fora.
        let (c, p) = parse_pins(&b[..50]);
        assert_eq!((c.len(), p.len()), (0, 1));
        let mut zero = pin_record(0, 1, 0);
        zero[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_pins(&zero), (vec![], vec![]));
    }

    #[test]
    fn fuzz_topologia_intel() {
        let mut seed = pin_record(3, 0xFF, 0);
        seed.extend(pin_record(0, 0b11, 0));
        crate::fuzz::run("intel_pins", &[seed], 256, |_, b| {
            let (c, p) = parse_pins(b);
            assert!(c.iter().chain(&p).all(|x| x.mask.count_ones() == 1));
        });
    }

    /// O MSR por núcleo (Intel) é lido no processador onde a thread roda: com a afinidade
    /// trocada, a thread está mesmo no núcleo pedido, e a afinidade original volta no Drop.
    #[test]
    fn prende_a_thread_em_cada_nucleo() {
        use windows::Win32::System::Threading::{
            GetCurrentProcessorNumber, GetThreadGroupAffinity,
        };
        let (cores, packages) = processor_pins().expect("topologia");
        assert!(!packages.is_empty());
        let affinity = || {
            let mut ga = GROUP_AFFINITY::default();
            // SAFETY: pseudo-handle da própria thread; estrutura local.
            unsafe { GetThreadGroupAffinity(GetCurrentThread(), &raw mut ga) }
                .ok()
                .expect("afinidade");
            (ga.Group, ga.Mask)
        };
        let before = affinity();
        for p in cores {
            let _pin = Pinned::to(p).expect("trocar afinidade");
            // Número do lógico dentro do grupo da thread (o grupo é o da afinidade).
            // SAFETY: sem pré-condições.
            let n = unsafe { GetCurrentProcessorNumber() };
            assert_eq!((affinity().0, 1u64 << n), (p.group, p.mask));
        }
        assert_eq!(affinity(), before);
    }

    #[test]
    fn tabela_de_familias() {
        assert!(cpu_support(0x19, 0x21).expect("Zen 3").ccd_base.is_some());
        assert!(cpu_support(0x19, 0x61).expect("Zen 4").ccd_base.is_none());
        assert!(cpu_support(0x17, 0x71).expect("Zen 2").ccd_base.is_none());
        assert_eq!(cpu_support(0x16, 0x00), None);
    }

    #[test]
    fn potencia_com_volta_do_contador() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        // 100 contagens atravessando a volta de 32 bits, 0,5 J cada, em 2 s = 25 W.
        let w = watts((t0, u32::MAX - 49), (t1, 50), 0.5).expect("watts");
        assert!((w - 25.0).abs() < 1e-9, "{w}");
        assert_eq!(watts((t1, 0), (t0, 0), 0.5), None);
    }

    /// Rodar elevado, com o PawnIO instalado: `cargo test pawnio_real -- --ignored`.
    #[test]
    #[ignore = "hardware: PawnIO instalado e terminal elevado"]
    fn pawnio_real() {
        let mut c = PawnIoCollector::new();
        let _ = c.read();
        std::thread::sleep(Duration::from_millis(500));
        let s = c.read().expect("sensores");
        let t = s.tctl.expect("Tctl");
        assert!((10.0..110.0).contains(&t), "{t}");
        let w = s.package_power.expect("potência");
        assert!((1.0..400.0).contains(&w), "{w}");
        println!("Tctl {t:.1} °C, CCD {:?}, {w:.1} W", s.ccds);
    }
}
