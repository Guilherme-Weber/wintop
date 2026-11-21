//! Consumo do próprio wintop: CPU (% do sistema inteiro, como a aba Processos do Task Manager) e
//! memória (working set privado, a coluna "Memory" do Task Manager; e a memória confirmada).

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};

use crate::model::{Metric, OwnUsage, fail};
use crate::win::filetime;

pub struct OwnCollector {
    /// (tempo do processo, tempo total do sistema) da leitura anterior, em 100 ns.
    last: Option<(u64, u64)>,
}

/// CPU do processo em % do sistema: Δprocesso / Δ(kernel + user do sistema). O tempo do
/// sistema soma todos os processadores lógicos (e o ocioso, dentro do kernel).
fn share(prev: (u64, u64), now: (u64, u64)) -> Option<f64> {
    let dp = now.0.checked_sub(prev.0)?;
    let ds = now.1.checked_sub(prev.1)?;
    (ds > 0).then(|| (dp as f64 / ds as f64 * 100.0).clamp(0.0, 100.0))
}

impl OwnCollector {
    pub fn new() -> Self {
        Self { last: None }
    }

    fn times() -> Result<(u64, u64), String> {
        let (mut c, mut e, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: pseudo-handle do próprio processo; FILETIME locais.
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &raw mut c,
                &raw mut e,
                &raw mut k,
                &raw mut u,
            )
        }
        .map_err(|e| format!("GetProcessTimes: {e}"))?;
        let (mut si, mut sk, mut su) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: três FILETIME locais.
        unsafe { GetSystemTimes(Some(&raw mut si), Some(&raw mut sk), Some(&raw mut su)) }
            .map_err(|e| format!("GetSystemTimes: {e}"))?;
        Ok((filetime(k) + filetime(u), filetime(sk) + filetime(su)))
    }

    pub fn read(&mut self) -> Metric<OwnUsage> {
        let cpu = match Self::times() {
            Err(e) => fail(e),
            Ok(now) => match self.last.replace(now) {
                None => fail("waiting for a second sample"),
                Some(prev) => share(prev, now)
                    .map_or_else(|| fail("no time elapsed since the last sample"), Ok),
            },
        };
        let mut pm = PROCESS_MEMORY_COUNTERS_EX2 {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
            ..Default::default()
        };
        // SAFETY: estrutura local com cb preenchido; a API aceita a versão EX2 pelo tamanho.
        let mem = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                (&raw mut pm).cast::<PROCESS_MEMORY_COUNTERS>(),
                pm.cb,
            )
        }
        .map(|()| (pm.PrivateWorkingSetSize as u64, pm.PrivateUsage as u64))
        .map_err(|e| format!("GetProcessMemoryInfo: {e}"));
        Ok(OwnUsage {
            cpu,
            private_ws: mem.clone().map(|m| m.0).or_else(fail),
            commit: mem.map(|m| m.1).or_else(fail),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parcela_do_sistema() {
        // 1 s de CPU do processo em 16 lógicos × 1 s de relógio = 6,25%.
        assert_eq!(share((0, 0), (10_000_000, 160_000_000)), Some(6.25));
        assert_eq!(share((0, 5), (1, 5)), None);
        assert_eq!(share((5, 0), (1, 10)), None);
    }

    #[test]
    fn le_o_proprio_processo() {
        let mut c = OwnCollector::new();
        let _ = c.read();
        // Gasta um pouco de CPU entre as leituras.
        let mut x = 0u64;
        for i in 0..5_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        std::hint::black_box(x);
        let u = c.read().expect("consumo");
        assert!(u.private_ws.expect("memória") > 0);
        assert!(u.cpu.is_ok(), "{:?}", u.cpu);
    }
}
