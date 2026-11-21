//! Spike da Etapa 0: temperatura (Tctl e CCD) e potência do pacote do Ryzen pelo PawnIO.
//!
//! Exige elevação e o PawnIO instalado. Carrega `modules/AMDFamily17.bin` (PawnIO.Modules,
//! LGPL-2.1) e, se o Afterburner estiver aberto, compara a temperatura com a dele.
//!
//! `cargo run --example spike_pawnio [amostras]`

#[path = "common/mod.rs"]
mod common;

use std::ffi::{CStr, c_void};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
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

// Registradores (AMD PPR família 17h-19h; mesma lógica do LibreHardwareMonitor).
const SMN_THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const SMN_CCD_TEMP_BASE: u32 = 0x0005_9954;
const MSR_PWR_UNIT: u64 = 0xC001_0299;
const MSR_PKG_ENERGY_STAT: u64 = 0xC001_029B;

struct PawnIo {
    handle: HANDLE,
    exec: FnExec,
    close: FnClose,
    pci_mutex: HANDLE,
}

impl PawnIo {
    fn open(module: &[u8]) -> Result<Self, String> {
        let dir = install_dir();
        let dll = dir.join("PawnIOLib.dll");
        let dll_w = common::wide(&dll.to_string_lossy());
        // SAFETY: caminho absoluto terminado em zero; busca restrita à pasta da DLL e System32
        // (sem diretório atual, contra DLL hijacking num processo elevado).
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
        let (version, open, load, exec, close) = unsafe {
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
        // SAFETY: `module` vive durante a chamada; o driver copia o blob.
        unsafe { load(handle, module.as_ptr(), module.len()) }
            .ok()
            .map_err(|e| format!("pawnio_load(AMDFamily17): {e}"))?;

        // Mutex global que os monitores (HWiNFO, LHM, ...) usam para não intercalar acessos
        // de índice/dado do SMN no barramento PCI (AMDFamily17.p: "acquire Access_PCI").
        // SAFETY: nome estático; o handle é fechado em Drop.
        let pci_mutex = unsafe { CreateMutexW(None, false, w!("Global\\Access_PCI")) }
            .map_err(|e| format!("mutex Access_PCI: {e}"))?;

        Ok(Self {
            handle,
            exec,
            close,
            pci_mutex,
        })
    }

    fn call(&self, name: &CStr, arg: u64) -> Result<u64, HRESULT> {
        let input = [arg];
        let mut out = [0u64; 1];
        let mut written = 0usize;
        // SAFETY: buffers locais com os tamanhos informados.
        let hr = unsafe {
            (self.exec)(
                self.handle,
                PCSTR(name.as_ptr().cast()),
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
                &raw mut written,
            )
        };
        if hr.is_ok() { Ok(out[0]) } else { Err(hr) }
    }

    /// Lê um registrador SMN segurando o mutex `Access_PCI`.
    fn smn(&self, addr: u32) -> Result<u32, String> {
        // SAFETY: handle de mutex válido.
        let w = unsafe { WaitForSingleObject(self.pci_mutex, 100) };
        if w != WAIT_OBJECT_0 && w != WAIT_ABANDONED {
            return Err("Access_PCI ocupado".into());
        }
        let r = self.call(c"ioctl_read_smn", u64::from(addr));
        // SAFETY: o mutex foi adquirido acima.
        unsafe {
            let _ = ReleaseMutex(self.pci_mutex);
        }
        r.map(|v| v as u32)
            .map_err(|e| format!("ioctl_read_smn({addr:#x}): {e}"))
    }

    fn msr(&self, msr: u64) -> Result<u64, String> {
        self.call(c"ioctl_read_msr", msr)
            .map_err(|e| format!("ioctl_read_msr({msr:#x}): {e}"))
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        // SAFETY: handles obtidos em `open`.
        unsafe {
            let _ = (self.close)(self.handle);
            let _ = CloseHandle(self.pci_mutex);
        }
    }
}

fn install_dir() -> PathBuf {
    let mut buf = [0u16; 260];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: buffer e tamanho em bytes consistentes.
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\PawnIO"),
            w!("InstallLocation"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            Some(&raw mut len),
        )
    }
    .is_ok();
    if ok {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        PathBuf::from(String::from_utf16_lossy(&buf[..n]))
    } else {
        PathBuf::from(r"C:\Program Files\PawnIO")
    }
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

fn cpu_id() -> (u32, u32, String) {
    use std::arch::x86_64::__cpuid;
    let r = __cpuid(1);
    let base = (r.eax >> 8) & 0xF;
    let family = if base == 0xF {
        base + ((r.eax >> 20) & 0xFF)
    } else {
        base
    };
    let model = ((r.eax >> 4) & 0xF) | ((r.eax >> 12) & 0xF0);
    let mut brand = Vec::new();
    for leaf in 0x8000_0002u32..=0x8000_0004 {
        let r = __cpuid(leaf);
        for v in [r.eax, r.ebx, r.ecx, r.edx] {
            brand.extend_from_slice(&v.to_le_bytes());
        }
    }
    let brand = String::from_utf8_lossy(&brand)
        .trim_matches(['\0', ' '])
        .to_string();
    (family, model, brand)
}

fn main() {
    let samples: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let (family, model, brand) = cpu_id();
    println!("CPU: {brand} (família {family:#x}, modelo {model:#x})");
    println!("Elevado: {}", common::is_elevated());

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
        let ccds: Vec<String> = (0..8)
            .filter_map(|c| {
                let raw = pio.smn(SMN_CCD_TEMP_BASE + c * 4).ok()?;
                ccd_from_raw(raw).map(|t| format!("CCD{} {t:.1}", c + 1))
            })
            .collect();
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
            "[{}] Tctl {} | {} | pacote {} | Afterburner {} | custo da leitura {cost_us} µs",
            i + 1,
            tctl.map_or_else(
                |e| format!("FALHA ({e})"),
                |r| format!("{:.1} °C (raw {r:#010x})", tctl_from_raw(r))
            ),
            if ccds.is_empty() {
                "CCD: nenhum válido".into()
            } else {
                ccds.join(", ") + " °C"
            },
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
    fn ccd_invalido_e_valido() {
        assert_eq!(ccd_from_raw(0), None);
        // (49 + 40) / 0,125 = 712 -> 40 °C.
        assert!((ccd_from_raw((1 << 11) | 712).unwrap() - 40.0).abs() < 1e-9);
    }
}
