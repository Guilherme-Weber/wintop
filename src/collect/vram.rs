//! VRAM por categoria (como o gputaskman): quanto da memória dedicada da GPU está com processos
//! de compute, apps, o compositor (DWM), navegadores, o sistema, o kernel/driver, e o livre.
//!
//! - Por processo: PDH `\GPU Process Memory(*)\Local Usage` (instâncias
//!   `pid_<PID>_luid_<LUID>_phys_N`); o NVML não tem VRAM por processo no WDDM.
//! - Nomes pelo snapshot do Toolhelp (não abre nenhum processo: nada de handle no jogo, regra do
//!   anticheat).
//! - Sem categoria "compute": no WDDM o NVML lista os mesmos processos como gráficos e compute
//!   (medido: 29 e 29), e descobrir o CUDA exigiria ler os módulos de cada processo (proibido no
//!   jogo). O jogo do RTSS é separado como "game" pela interface, que sabe o PID dele.
//! - "kernel": a VRAM dedicada do adaptador menos a soma dos processos (driver, superfícies sem
//!   dono). "free": total menos a dedicada usada.

use std::collections::HashMap;

use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

use crate::model::{Metric, VramBreakdown, VramKind, VramProc, fail};
use crate::pdh::{self, Query, Slot};
use crate::win::{from_wide, own};

const BROWSERS: &[&str] = &[
    "firefox.exe",
    "chrome.exe",
    "msedge.exe",
    "msedgewebview2.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
];
const SYSTEM: &[&str] = &[
    "system",
    "csrss.exe",
    "winlogon.exe",
    "wininit.exe",
    "services.exe",
    "lsass.exe",
    "svchost.exe",
    "fontdrvhost.exe",
    "smss.exe",
    "registry",
];

/// Categoria de um processo pelo nome.
pub(super) fn classify(name: &str, pid: u32) -> VramKind {
    let n = name.to_ascii_lowercase();
    if n == "dwm.exe" {
        VramKind::Compositor
    } else if BROWSERS.contains(&n.as_str()) {
        VramKind::Browser
    } else if SYSTEM.contains(&n.as_str()) || pid <= 4 {
        VramKind::System
    } else {
        VramKind::App
    }
}

/// `pid_1234_luid_..._phys_0` → (1234, "luid_..._phys_0").
fn parse_instance(name: &str) -> Option<(u32, &str)> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, luid) = rest.split_once('_')?;
    Some((pid.parse().ok()?, luid))
}

/// PID → nome do executável, pelo snapshot do Toolhelp.
pub(super) fn process_names() -> HashMap<u32, String> {
    let mut out = HashMap::new();
    // SAFETY: snapshot de processos (não abre nenhum processo); o handle passa a ter dono.
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return out;
    };
    // SAFETY: CreateToolhelp32Snapshot teve sucesso; handle válido e sem outro dono.
    let snap = unsafe { own(snap) };
    let h =
        windows::Win32::Foundation::HANDLE(std::os::windows::io::AsRawHandle::as_raw_handle(&snap));
    let mut e = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: estrutura local com dwSize preenchido.
    let mut ok = unsafe { Process32FirstW(h, &raw mut e) }.is_ok();
    while ok {
        out.insert(e.th32ProcessID, from_wide(&e.szExeFile));
        // SAFETY: idem.
        ok = unsafe { Process32NextW(h, &raw mut e) }.is_ok();
    }
    out
}

/// Monta o detalhamento a partir das leituras (separado para teste).
pub(super) fn breakdown(
    per_process: &[(u32, u64)],
    adapter_used: u64,
    total: u64,
    names: &HashMap<u32, String>,
) -> VramBreakdown {
    let mut processes: Vec<VramProc> = per_process
        .iter()
        .filter(|(_, b)| *b > 0)
        .map(|&(pid, local)| {
            let name = names
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| format!("pid {pid}"));
            let kind = classify(&name, pid);
            VramProc {
                pid,
                name,
                local,
                kind,
            }
        })
        .collect();
    processes.sort_by_key(|p| std::cmp::Reverse(p.local));
    let sum = |k: VramKind| -> u64 {
        processes
            .iter()
            .filter(|p| p.kind == k)
            .map(|p| p.local)
            .sum()
    };
    let by_process: u64 = processes.iter().map(|p| p.local).sum();
    let mut segments: Vec<(VramKind, u64)> = [
        VramKind::App,
        VramKind::Compositor,
        VramKind::Browser,
        VramKind::System,
    ]
    .into_iter()
    .map(|k| (k, sum(k)))
    .collect();
    segments.push((VramKind::Kernel, adapter_used.saturating_sub(by_process)));
    segments.push((
        VramKind::Free,
        total.saturating_sub(adapter_used.max(by_process)),
    ));
    VramBreakdown {
        total,
        used: adapter_used.max(by_process),
        segments,
        processes,
    }
}

/// VRAM de um processo em bytes: local, não local (spill para a RAM) e confirmada.
pub(super) type ProcMem = (u64, u64, u64);

/// Coletor (thread lenta): uma consulta PDH com o uso por processo e o do adaptador. A não
/// local e a confirmada só são lidas com a aba GPU visível (mesmo objeto PDH da local: a
/// coleta custa o mesmo).
pub(super) struct VramCollector {
    query: Result<Query, String>,
    local: Slot,
    non_local: Slot,
    committed: Slot,
    adapter: Slot,
}

impl VramCollector {
    pub(super) fn new() -> Self {
        let query = Query::open();
        let add = |p: &str| match &query {
            Ok(q) => q.add(p),
            Err(e) => Err(e.clone()),
        };
        let local = add(r"\GPU Process Memory(*)\Local Usage");
        let non_local = add(r"\GPU Process Memory(*)\Non Local Usage");
        let committed = add(r"\GPU Process Memory(*)\Total Committed");
        let adapter = add(r"\GPU Adapter Memory(*)\Dedicated Usage");
        Self {
            query,
            local,
            non_local,
            committed,
            adapter,
        }
    }

    /// Soma por PID de um contador de processo, só das instâncias do adaptador.
    fn per_process(&self, slot: &Slot, luid: &str) -> Result<HashMap<u32, u64>, String> {
        let rows = pdh::read_array(&self.query, slot)?;
        let mut out: HashMap<u32, u64> = HashMap::new();
        for (name, v) in &rows {
            if let Some((pid, rest)) = parse_instance(name)
                && rest.to_ascii_lowercase().starts_with(luid)
            {
                *out.entry(pid).or_default() += *v as u64;
            }
        }
        Ok(out)
    }

    /// VRAM local, não local e confirmada por PID (aba GPU), da coleta feita pelo `read`.
    pub(super) fn per_process_detail(&self, luid: &str) -> Result<HashMap<u32, ProcMem>, String> {
        let mut out: HashMap<u32, ProcMem> = HashMap::new();
        for (pid, v) in self.per_process(&self.local, luid)? {
            out.entry(pid).or_default().0 = v;
        }
        for (pid, v) in self.per_process(&self.non_local, luid)? {
            out.entry(pid).or_default().1 = v;
        }
        for (pid, v) in self.per_process(&self.committed, luid)? {
            out.entry(pid).or_default().2 = v;
        }
        Ok(out)
    }

    /// `luid`: prefixo `luid_0x..._0x..._phys_` do adaptador; `total`: VRAM total (NVML).
    pub(super) fn read(&mut self, luid: &str, total: u64) -> Metric<VramBreakdown> {
        if let Ok(q) = &self.query {
            q.collect().or_else(fail)?;
        }
        let per_process = self.per_process(&self.local, luid).or_else(fail)?;
        let adapter = pdh::read_array(&self.query, &self.adapter).or_else(fail)?;
        let mine: Vec<u64> = adapter
            .iter()
            .filter(|(n, _)| n.to_ascii_lowercase().starts_with(luid))
            .map(|(_, v)| *v as u64)
            .collect();
        if mine.is_empty() {
            return fail("adapter has no PDH instance");
        }
        let adapter_used = mine.iter().sum();
        let list: Vec<(u32, u64)> = per_process.into_iter().collect();
        Ok(breakdown(&list, adapter_used, total, &process_names()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instancia_por_processo() {
        assert_eq!(
            parse_instance("pid_14300_luid_0x00000000_0x0000dd01_phys_0"),
            Some((14300, "luid_0x00000000_0x0000dd01_phys_0"))
        );
        assert_eq!(parse_instance("luid_0x0_phys_0"), None);
    }

    #[test]
    fn categorias() {
        assert_eq!(classify("dwm.exe", 1784), VramKind::Compositor);
        assert_eq!(classify("Firefox.exe", 900), VramKind::Browser);
        assert_eq!(classify("deadlock.exe", 27576), VramKind::App);
        assert_eq!(classify("Code.exe", 2244), VramKind::App);
        assert_eq!(classify("csrss.exe", 1236), VramKind::System);
        // PIDs 0 e 4 (Idle e System) são do sistema mesmo sem nome conhecido.
        assert_eq!(classify("pid 4", 4), VramKind::System);
    }

    #[test]
    fn soma_por_categoria_kernel_e_livre() {
        let names: HashMap<u32, String> = [(1, "dwm.exe"), (2, "firefox.exe"), (3, "game.exe")]
            .into_iter()
            .map(|(p, n)| (p + 100, n.to_string()))
            .collect();
        let rows = [(101, 800), (102, 600), (103, 5000), (104, 0)];
        let b = breakdown(&rows, 7000, 16_000, &names);
        let get = |k| b.segments.iter().find(|(x, _)| *x == k).map(|(_, v)| *v);
        assert_eq!(get(VramKind::App), Some(5000));
        assert_eq!(get(VramKind::Compositor), Some(800));
        assert_eq!(get(VramKind::Browser), Some(600));
        assert_eq!(get(VramKind::Kernel), Some(600)); // 7000 - 6400
        assert_eq!(get(VramKind::Free), Some(9000));
        assert_eq!(b.processes.first().map(|p| p.pid), Some(103));
        assert_eq!(b.processes.len(), 3); // o de 0 bytes fica de fora
    }
}
