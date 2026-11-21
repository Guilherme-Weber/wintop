//! Spike da Etapa 0: temperatura (Tctl e CCD) e potência do pacote do Ryzen pelo PawnIO.
//!
//! Exige elevação e o PawnIO instalado. Carrega `modules/AMDFamily17.bin` (PawnIO.Modules,
//! LGPL-2.1, fonte em `modules/source/`) e, se o Afterburner estiver aberto, compara a
//! temperatura com a dele.
//!
//! `cargo run --example spike_pawnio [amostras]`

#[path = "common/mod.rs"]
mod common;

use std::ffi::{CStr, c_void};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    HANDLE, HLOCAL, HMODULE, LocalFree, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::{HRESULT, PCSTR, PCWSTR, w};

type FnVersion = unsafe extern "system" fn(*mut u32) -> HRESULT;
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

// Registradores (AMD PPR; mesma lógica do LibreHardwareMonitor).
const SMN_THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const MSR_PWR_UNIT: u64 = 0xC001_0299;
const MSR_PKG_ENERGY_STAT: u64 = 0xC001_029B;

/// O que foi validado por família/modelo. Só entra na tabela o que foi conferido numa CPU real;
/// o resto aparece como "não validado" em vez de arriscar um offset errado.
struct CpuSupport {
    /// Endereço SMN do CCD1 (os demais em +4); `None` = sensores de CCD não validados.
    ccd_base: Option<u32>,
    /// Quantos CCDs consultar.
    ccds: u32,
}

fn cpu_support(family: u32, model: u32) -> Option<CpuSupport> {
    match (family, model) {
        // Zen 3 Vermeer (Ryzen 5000 desktop): validado no 5700X3D (19h, modelo 21h).
        (0x19, 0x20..=0x2F) => Some(CpuSupport {
            ccd_base: Some(0x0005_9954),
            ccds: 2,
        }),
        // Demais famílias suportadas pelo módulo (17h-1Ah): Tctl e potência pelo mesmo
        // caminho, CCD ainda não validado (o offset muda entre gerações).
        (0x17..=0x1A, _) => Some(CpuSupport {
            ccd_base: None,
            ccds: 0,
        }),
        _ => None,
    }
}

/// Handle do executor do PawnIO; fecha com `pawnio_close` no Drop (inclusive em erro).
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

struct PawnIo {
    exec_fn: FnExec,
    pci_mutex: OwnedHandle,
    executor: Executor,
}

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
        let dir = install_dir()?;
        let dll = dir.join("PawnIOLib.dll");
        let dll_w = common::wide(&dll.to_string_lossy());
        // SAFETY: caminho absoluto terminado em zero; busca restrita à pasta da DLL e System32.
        let lib: HMODULE = unsafe {
            LoadLibraryExW(
                PCWSTR(dll_w.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
        .map_err(|e| format!("PawnIOLib.dll em {}: {e}", dll.display()))?;

        let sym = |name: &CStr| {
            // SAFETY: `lib` é válido; o nome é uma string C estática.
            unsafe { GetProcAddress(lib, PCSTR(name.as_ptr().cast())) }
                .ok_or_else(|| format!("símbolo {name:?} ausente"))
        };
        type Raw = unsafe extern "system" fn() -> isize;
        // SAFETY: assinaturas conforme PawnIOLib.h (HRESULT STDAPICALLTYPE).
        let (version, open, load, exec_fn, close) = unsafe {
            (
                std::mem::transmute::<Raw, FnVersion>(sym(c"pawnio_version")?),
                std::mem::transmute::<Raw, FnOpen>(sym(c"pawnio_open")?),
                std::mem::transmute::<Raw, FnLoad>(sym(c"pawnio_load")?),
                std::mem::transmute::<Raw, FnExec>(sym(c"pawnio_execute")?),
                std::mem::transmute::<Raw, FnClose>(sym(c"pawnio_close")?),
            )
        };

        let mut ver = 0u32;
        // SAFETY: ponteiro válido para u32.
        unsafe { version(&raw mut ver) }
            .ok()
            .map_err(|e| format!("pawnio_version: {e}"))?;
        println!(
            "PawnIOLib {}.{}.{} ({})",
            ver >> 16,
            (ver >> 8) & 0xFF,
            ver & 0xFF,
            dll.display()
        );

        let mut handle = HANDLE::default();
        // SAFETY: ponteiro válido para HANDLE.
        unsafe { open(&raw mut handle) }
            .ok()
            .map_err(|e| format!("pawnio_open (driver carregado? elevado?): {e}"))?;
        // Dono do handle a partir daqui: qualquer `?` abaixo fecha o executor.
        let executor = Executor { handle, close };
        // SAFETY: `module` vive durante a chamada; o driver copia o blob.
        unsafe { load(executor.handle, module.as_ptr(), module.len()) }
            .ok()
            .map_err(|e| format!("pawnio_load(AMDFamily17): {e}"))?;

        let pci_mutex = open_pci_mutex()?;
        Ok(Self {
            exec_fn,
            pci_mutex,
            executor,
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
            return Err(format!("{name:?}({arg:#x}): nenhum valor devolvido"));
        }
        Ok(out[0])
    }

    /// Adquire o mutex global que os monitores (HWiNFO, LHM, ...) usam para não intercalar
    /// acessos de índice/dado do SMN (AMDFamily17.p: "acquire Access_PCI").
    fn lock(&self) -> Result<PciLock<'_>, String> {
        // SAFETY: handle de mutex válido.
        let w = unsafe { WaitForSingleObject(HANDLE(self.pci_mutex.as_raw_handle()), 100) };
        if w == WAIT_OBJECT_0 || w == WAIT_ABANDONED {
            Ok(PciLock(&self.pci_mutex))
        } else if w == WAIT_FAILED {
            Err(format!(
                "Access_PCI: {}",
                windows::core::Error::from_thread()
            ))
        } else {
            Err("Access_PCI ocupado por outro monitor (100 ms)".into())
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

/// Abre ou cria `Global\Access_PCI`. Se o wintop (elevado) for o primeiro a criá-lo, a DACL
/// explícita deixa monitores sem elevação esperar e liberar o mutex (SYNCHRONIZE e
/// MUTEX_MODIFY_STATE para usuários autenticados).
fn open_pci_mutex() -> Result<OwnedHandle, String> {
    let sddl = common::wide("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x100001;;;AU)");
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
    .map_err(|e| format!("SDDL do mutex: {e}"))?;
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
    // SAFETY: CreateMutexW teve sucesso; handle válido e sem outro dono.
    r.map(|h| unsafe { common::own(h) })
        .map_err(|e| format!("mutex Access_PCI: {e}"))
}

/// Pasta de instalação do PawnIO pelo registro. Sem fallback fixo: valor ausente, vazio ou
/// relativo é erro (um processo elevado não carrega DLL de caminho incerto).
fn install_dir() -> Result<PathBuf, String> {
    let mut buf = [0u16; 520];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: buffer e tamanho em bytes consistentes; REG_EXPAND_SZ é expandido pela API.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\PawnIO"),
            w!("InstallLocation"),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            Some(&raw mut len),
        )
    }
    .ok()
    .map_err(|e| format!("PawnIO não instalado (InstallLocation: {e})"))?;
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let dir = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
    if dir.as_os_str().is_empty() || !dir.is_absolute() {
        return Err(format!("InstallLocation inválido: {:?}", dir.display()));
    }
    Ok(dir)
}

/// Tctl em °C a partir de THM_TCON_CUR_TMP: bits 31:21 em passos de 0,125 °C; com o bit 19
/// (CUR_TEMP_RANGE_SEL) a escala começa em -49 °C.
fn tctl_from_raw(raw: u32) -> f64 {
    let t = f64::from(raw >> 21) * 0.125;
    if raw & (1 << 19) != 0 { t - 49.0 } else { t }
}

/// Temperatura de um CCD: bit 11 = válido; bits 10:0 em passos de 0,125 °C a partir de -49 °C.
fn ccd_from_raw(raw: u32) -> Option<f64> {
    (raw & (1 << 11) != 0).then(|| f64::from(raw & 0x7FF) * 0.125 - 49.0)
}

/// (vendor, família, modelo, nome) pelo CPUID, checando as folhas disponíveis.
fn cpu_id() -> (String, u32, u32, String) {
    use std::arch::x86_64::__cpuid;
    let r0 = __cpuid(0);
    let vendor: Vec<u8> = [r0.ebx, r0.edx, r0.ecx]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let vendor = String::from_utf8_lossy(&vendor).to_string();
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
    let mut brand = Vec::new();
    if __cpuid(0x8000_0000).eax >= 0x8000_0004 {
        for leaf in 0x8000_0002u32..=0x8000_0004 {
            let r = __cpuid(leaf);
            for v in [r.eax, r.ebx, r.ecx, r.edx] {
                brand.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    let brand = String::from_utf8_lossy(&brand)
        .trim_matches(['\0', ' '])
        .to_string();
    (vendor, family, model, brand)
}

fn main() {
    common::harden_dll_search();
    let samples: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let (vendor, family, model, brand) = cpu_id();
    println!("CPU: {brand} ({vendor}, família {family:#x}, modelo {model:#x})");
    println!("Elevado: {}", common::is_elevated());
    if vendor != "AuthenticAMD" {
        return eprintln!("FALHA: o módulo AMDFamily17 só atende processadores AMD");
    }
    let Some(support) = cpu_support(family, model) else {
        return eprintln!("FALHA: família {family:#x} fora do módulo AMDFamily17 (17h-1Ah)");
    };

    let module_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("modules/AMDFamily17.bin");
    let module = match std::fs::read(&module_path) {
        Ok(m) => m,
        Err(e) => return eprintln!("FALHA: {}: {e}", module_path.display()),
    };
    let t0 = Instant::now();
    let pio = match PawnIo::open(&module) {
        Ok(p) => p,
        Err(e) => return eprintln!("FALHA: {e}"),
    };
    println!(
        "Abrir + carregar módulo: {:.1} ms\n",
        t0.elapsed().as_secs_f64() * 1e3
    );

    let unit = match pio.msr(MSR_PWR_UNIT) {
        Ok(u) => u,
        Err(e) => return eprintln!("FALHA: {e}"),
    };
    let joules_per_count = 1.0 / f64::from(1u32 << ((unit >> 8) & 0x1F));
    println!("Unidade de energia: {joules_per_count:e} J/contagem (MSR_PWR_UNIT = {unit:#x})");

    let mut last_energy = pio.msr(MSR_PKG_ENERGY_STAT).map(|e| e as u32).ok();
    let mut last_t = Instant::now();
    for i in 0..samples {
        std::thread::sleep(Duration::from_secs(1));
        let t_read = Instant::now();
        let tctl = pio.smn(SMN_THM_TCON_CUR_TMP);
        let ccds = match support.ccd_base {
            None => "CCD: não validado nesta família".to_string(),
            Some(base) => {
                let list: Vec<String> = (0..support.ccds)
                    .filter_map(|c| {
                        let raw = pio.smn(base + c * 4).ok()?;
                        ccd_from_raw(raw).map(|t| format!("CCD{} {t:.1}", c + 1))
                    })
                    .collect();
                if list.is_empty() {
                    "CCD: nenhum válido".into()
                } else {
                    list.join(", ") + " °C"
                }
            }
        };
        let energy = pio.msr(MSR_PKG_ENERGY_STAT).map(|e| e as u32).ok();
        let cost_us = t_read.elapsed().as_micros();
        let now = Instant::now();
        let watts = match (last_energy, energy) {
            (Some(a), Some(b)) => {
                Some(f64::from(b.wrapping_sub(a)) * joules_per_count / (now - last_t).as_secs_f64())
            }
            _ => None,
        };
        last_energy = energy;
        last_t = now;
        let ab = common::afterburner(0x80);
        println!(
            "[{}] Tctl {} | {ccds} | pacote {} | Afterburner {} | custo da leitura {cost_us} µs",
            i + 1,
            tctl.map_or_else(
                |e| format!("FALHA ({e})"),
                |r| format!("{:.1} °C (raw {r:#010x})", tctl_from_raw(r))
            ),
            watts.map_or_else(|| "FALHA".into(), |w| format!("{w:.1} W")),
            ab.map_or_else(|| "fechado".into(), |t| format!("{t:.1} °C")),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tctl_sem_e_com_faixa_estendida() {
        // 50,0 °C sem o bit 19: 50 / 0,125 = 400 em 31:21.
        assert!((tctl_from_raw(400 << 21) - 50.0).abs() < 1e-9);
        // Mesmo valor bruto com o bit 19: 50 - 49 = 1 °C.
        assert!((tctl_from_raw((400 << 21) | (1 << 19)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn tctl_valor_real_do_spike() {
        // Leitura real do 5700X3D (spike de 2026-11-20): 0x673b0000 -> 54,125 °C.
        assert!((tctl_from_raw(0x673b_0000) - 54.125).abs() < 1e-9);
    }

    #[test]
    fn ccd_invalido_e_valido() {
        assert_eq!(ccd_from_raw(0), None);
        // (49 + 40) / 0,125 = 712 -> 40 °C.
        assert!((ccd_from_raw((1 << 11) | 712).unwrap() - 40.0).abs() < 1e-9);
    }

    #[test]
    fn tabela_de_familias() {
        assert!(cpu_support(0x19, 0x21).unwrap().ccd_base.is_some());
        assert!(cpu_support(0x19, 0x61).unwrap().ccd_base.is_none());
        assert!(cpu_support(0x17, 0x71).unwrap().ccd_base.is_none());
        assert!(cpu_support(0x16, 0x00).is_none());
    }
}
