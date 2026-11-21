//! Sensores da CPU AMD pelo PawnIO + módulo `AMDFamily17` (LGPL-2.1, arquivo separado em
//! `modules/`): Tctl, temperatura por CCD e potência do pacote.
//!
//! Registradores conforme o AMD PPR (mesma lógica do LibreHardwareMonitor), validados no Zen 3
//! (5700X3D) na Etapa 0. `PawnIOLib.dll` por caminho absoluto (`InstallLocation` do registro);
//! o módulo vem de `<pasta do exe>\modules\` (o `build.rs` copia para lá na compilação).

use std::ffi::{CStr, c_void};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::time::Instant;

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, FreeLibrary, HANDLE, HLOCAL, HMODULE, LocalFree, WAIT_ABANDONED,
    WAIT_FAILED, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::Threading::{
    CreateMutexW, MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex, SYNCHRONIZATION_ACCESS_RIGHTS,
    SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows::core::{HRESULT, PCSTR, PCWSTR, w};

use crate::model::{CpuSensors, Metric, Missing, fail, neutral};
use crate::win::{own, wide};

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
const MODULE: &str = "AMDFamily17.bin";
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
    pci_mutex: OwnedHandle,
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
    fn open(module: &[u8]) -> Result<Self, String> {
        let dll = install_dir()?.join("PawnIOLib.dll");
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
            .map_err(|e| format!("pawnio_load(AMDFamily17): {e}"))?;
        Ok(Self {
            exec_fn,
            pci_mutex: open_pci_mutex()?,
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
        // SAFETY: handle de mutex válido.
        let w = unsafe { WaitForSingleObject(HANDLE(self.pci_mutex.as_raw_handle()), PCI_WAIT_MS) };
        if w == WAIT_OBJECT_0 || w == WAIT_ABANDONED {
            Ok(PciLock(&self.pci_mutex))
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

    fn smn(&self, addr: u32) -> Result<u32, String> {
        let _lock = self.lock()?;
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
    let sddl = wide("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x100001;;;AU)S:(ML;;NW;;;ME)");
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: SDDL terminado em zero; o descritor é liberado logo após o uso.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &raw mut sd,
            None,
        )
    }
    .map_err(|e| format!("Access_PCI mutex SDDL: {e}"))?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: atributos válidos durante a chamada; se o mutex já existir, só é aberto.
    let r = unsafe { CreateMutexW(Some(&raw const sa), false, w!("Global\\Access_PCI")) };
    // SAFETY: descritor alocado por ConvertString...; não é usado depois daqui.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
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

/// Pasta de instalação do PawnIO pelo registro. Sem fallback fixo: valor ausente, vazio ou
/// relativo é erro (processo elevado não carrega DLL de caminho incerto). Só `REG_SZ` (o que o
/// instalador grava): um `REG_EXPAND_SZ` seria expandido com o ambiente herdado do processo
/// sem elevação que abriu o wintop.
fn install_dir() -> Result<PathBuf, String> {
    let mut buf = [0u16; 520];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: buffer e tamanho em bytes consistentes; REG_EXPAND_SZ é expandido pela API.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\PawnIO"),
            w!("InstallLocation"),
            // NOEXPAND: um REG_EXPAND_SZ é recusado (ERROR_UNSUPPORTED_TYPE), não expandido.
            RRF_RT_REG_SZ | RRF_NOEXPAND,
            None,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            Some(&raw mut len),
        )
    }
    .ok()
    .map_err(|e| format!("PawnIO not installed (InstallLocation: {e})"))?;
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let dir = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
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
fn module_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe.parent().ok_or("executable has no folder")?;
    #[allow(unused_mut)]
    let mut candidates = vec![dir.join("modules").join(MODULE)];
    #[cfg(test)]
    candidates.push(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("modules")
            .join(MODULE),
    );
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| format!("{MODULE} not found in {}", dir.join("modules").display()))
}

enum State {
    /// CPU fora do módulo (estado neutro).
    Unsupported(String),
    Failed(String),
    Ready {
        pio: Box<PawnIo>,
        support: CpuSupport,
        joules_per_count: f64,
        /// (instante, contador de energia de 32 bits) da leitura anterior.
        last_energy: Option<(Instant, u32)>,
    },
}

pub struct PawnIoCollector {
    state: State,
}

impl PawnIoCollector {
    pub fn new() -> Self {
        let (vendor, family, model) = cpu_id();
        if vendor != "AuthenticAMD" {
            return Self {
                state: State::Unsupported("CPU not supported (not AMD)".into()),
            };
        }
        let Some(support) = cpu_support(family, model) else {
            return Self {
                state: State::Unsupported(format!(
                    "CPU not supported (family {family:#x} outside 17h-1Ah)"
                )),
            };
        };
        let state = match Self::open(support) {
            Ok(s) => s,
            Err(e) => State::Failed(e),
        };
        Self { state }
    }

    fn open(support: CpuSupport) -> Result<State, String> {
        let path = module_path()?;
        let module = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let pio = PawnIo::open(&module)?;
        let unit = pio.msr(MSR_PWR_UNIT)?;
        Ok(State::Ready {
            pio: Box::new(pio),
            support,
            joules_per_count: 1.0 / f64::from(1u32 << ((unit >> 8) & 0x1F)),
            last_energy: None,
        })
    }

    pub fn read(&mut self) -> Metric<CpuSensors> {
        let (pio, support, jpc, last_energy) = match &mut self.state {
            State::Unsupported(s) => return neutral(s.clone()),
            State::Failed(e) => return fail(e.clone()),
            State::Ready {
                pio,
                support,
                joules_per_count,
                last_energy,
            } => (pio, support, *joules_per_count, last_energy),
        };
        let tctl = pio
            .smn(SMN_THM_TCON_CUR_TMP)
            .map(tctl_from_raw)
            .or_else(fail);
        let ccds = match support.ccd_base {
            None => neutral("not validated on this CPU family"),
            Some(base) => {
                let mut list = Vec::new();
                let mut err = None;
                for c in 0..support.ccds {
                    match pio.smn(base + c * 4) {
                        Ok(raw) => {
                            if let Some(t) = ccd_from_raw(raw) {
                                list.push((c + 1, t));
                            }
                        }
                        Err(e) => err = Some(e),
                    }
                }
                match (list.is_empty(), err) {
                    (false, _) => Ok(list),
                    (true, Some(e)) => fail(e),
                    (true, None) => fail("no valid CCD sensor"),
                }
            }
        };
        let package_power = match pio.msr(MSR_PKG_ENERGY_STAT) {
            Err(e) => fail(e),
            Ok(raw) => {
                let now = (Instant::now(), raw as u32);
                match last_energy.replace(now) {
                    None => fail("waiting for a second sample"),
                    Some(prev) => watts(prev, now, jpc).map_or_else(
                        || {
                            Err(Missing::Fail(
                                "no time elapsed since the last sample".into(),
                            ))
                        },
                        Ok,
                    ),
                }
            }
        };
        Ok(CpuSensors {
            tctl,
            ccds,
            package_power,
        })
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
