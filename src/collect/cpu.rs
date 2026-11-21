//! CPU: nome, topologia, frequência, uso total e por processador lógico, parcela de kernel.

use std::time::Duration;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Power::{
    CallNtPowerInformation, PROCESSOR_POWER_INFORMATION, ProcessorInformation,
};
use windows::Win32::System::ProcessStatus::PERFORMANCE_INFORMATION;
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetTickCount64, RelationAll,
};
use windows::Win32::System::Threading::{
    GetSystemTimes, IsProcessorFeaturePresent, PF_VIRT_FIRMWARE_ENABLED,
};

use crate::model::{Counts, Cpu, Metric, Topology, fail};
use crate::pdh::{self, Query, Slot};
use crate::win::filetime;

pub struct CpuCollector {
    query: Result<Query, String>,
    per: Slot,
    per_kernel: Slot,
    perf: Slot,
    name: Metric<String>,
    topology: Metric<Topology>,
    base_mhz: Metric<u32>,
    virtualization: Metric<bool>,
    /// Último (idle, kernel, user) de `GetSystemTimes`.
    times: Option<(u64, u64, u64)>,
}

impl CpuCollector {
    pub fn new() -> Self {
        let query = Query::open();
        let add = |p: &str| match &query {
            Ok(q) => q.add(p),
            Err(e) => Err(e.clone()),
        };
        let per = add(r"\Processor Information(*)\% Processor Time");
        let per_kernel = add(r"\Processor Information(*)\% Privileged Time");
        let perf = add(r"\Processor Information(_Total)\% Processor Performance");
        let topology = topology();
        // Sem topologia, o número de lógicos vem do sistema (com 1, o buffer do
        // CallNtPowerInformation seria pequeno demais e a frequência cairia junto).
        let logical = topology.as_ref().map_or_else(
            |_| std::thread::available_parallelism().map_or(64, |n| n.get() as u32),
            |t| t.logical,
        );
        let base_mhz = base_mhz(logical);
        // SAFETY: sem pré-condições.
        let virt = unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) }.as_bool();
        Self {
            query,
            per,
            per_kernel,
            perf,
            name: cpu_name().map_or_else(|| fail("CPUID has no brand string leaf"), Ok),
            topology,
            base_mhz,
            virtualization: Ok(virt),
            times: None,
        }
    }

    /// `PdhCollectQueryData` desta consulta (chamado antes de `read`).
    pub fn sample(&mut self) -> Result<(), String> {
        match &self.query {
            Ok(q) => q.collect(),
            Err(e) => Err(e.clone()),
        }
    }

    pub fn read(&mut self, perf_info: Result<&PERFORMANCE_INFORMATION, &String>) -> Cpu {
        let sampled = self.sample();
        let pdh_val = |slot: &Slot| -> Metric<f64> {
            if let Err(e) = &sampled {
                return fail(e.clone());
            }
            pdh::read(&self.query, slot).or_else(fail)
        };
        // Um contador só: a instância `_Total` vem junto com as dos lógicos.
        let array = match &sampled {
            Err(e) => fail(e.clone()),
            Ok(()) => pdh::read_array(&self.query, &self.per).or_else(fail),
        };
        let usage = array.as_ref().map_err(Clone::clone).and_then(|a| {
            a.iter()
                .find(|(n, _)| n == "_Total")
                .map(|(_, v)| v.clamp(0.0, 100.0))
                .map_or_else(|| fail("no _Total instance"), Ok)
        });
        // Antes de `per_logical` (a variável esconderia a função de mesmo nome).
        let kernel_per_logical = match &sampled {
            Err(e) => fail(e.clone()),
            Ok(()) => pdh::read_array(&self.query, &self.per_kernel)
                .or_else(fail)
                .and_then(|a| per_logical(&a).map_or_else(|| fail("no instances"), Ok)),
        };
        let per_logical =
            array.and_then(|a| per_logical(&a).map_or_else(|| fail("no instances"), Ok));
        let kernel_per_logical = match (&per_logical, kernel_per_logical) {
            (Ok(u), Ok(k)) if u.len() != k.len() => fail("kernel instances differ from usage"),
            (_, k) => k,
        };
        let freq_mhz = match (pdh_val(&self.perf), &self.base_mhz) {
            (Ok(p), Ok(b)) => Ok(p / 100.0 * f64::from(*b)),
            (Err(e), _) => Err(e),
            (_, Err(e)) => Err(e.clone()),
        };
        Cpu {
            name: self.name.clone(),
            topology: self.topology.clone(),
            base_mhz: self.base_mhz.clone(),
            virtualization: self.virtualization.clone(),
            usage,
            per_logical,
            kernel_per_logical,
            kernel: self.kernel_share(),
            freq_mhz,
            counts: perf_info.map_or_else(
                |e| fail(e.clone()),
                |p| {
                    Ok(Counts {
                        processes: p.ProcessCount,
                        threads: p.ThreadCount,
                        handles: p.HandleCount,
                    })
                },
            ),
            // SAFETY: sem pré-condições.
            uptime: Duration::from_millis(unsafe { GetTickCount64() }),
        }
    }

    /// Parcela de kernel desde a última chamada (o KernelTime inclui o IdleTime).
    fn kernel_share(&mut self) -> Metric<f64> {
        let (mut i, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: três FILETIME locais.
        let r = unsafe { GetSystemTimes(Some(&raw mut i), Some(&raw mut k), Some(&raw mut u)) };
        if let Err(e) = r {
            return fail(format!("GetSystemTimes: {e}"));
        }
        let now = (filetime(i), filetime(k), filetime(u));
        let prev = self.times.replace(now);
        let Some(prev) = prev else {
            return fail("waiting for a second sample");
        };
        kernel_share(prev, now).map_or_else(|| fail("no time elapsed since the last sample"), Ok)
    }
}

/// `(kernel - idle) / (kernel + user)` entre duas leituras `(idle, kernel, user)`, em %.
fn kernel_share(prev: (u64, u64, u64), now: (u64, u64, u64)) -> Option<f64> {
    let di = now.0.checked_sub(prev.0)?;
    let dk = now.1.checked_sub(prev.1)?;
    let du = now.2.checked_sub(prev.2)?;
    let total = dk + du;
    (total > 0).then(|| (dk.saturating_sub(di) as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
}

/// Uso por processador lógico, ordenado por (grupo, número); ignora as instâncias `_Total`.
fn per_logical(items: &[(String, f64)]) -> Option<Vec<f64>> {
    let mut v: Vec<((u32, u32), f64)> = items
        .iter()
        .filter_map(|(name, val)| {
            let (g, n) = name.split_once(',')?;
            Some(((g.parse().ok()?, n.parse().ok()?), val.clamp(0.0, 100.0)))
        })
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by_key(|(k, _)| *k);
    Some(v.into_iter().map(|(_, x)| x).collect())
}

/// Nome do processador pelo CPUID (folhas 0x80000002-4).
fn cpu_name() -> Option<String> {
    use std::arch::x86_64::__cpuid;
    // CPUID existe em todo x86-64; a folha estendida máxima é conferida antes.
    let max = __cpuid(0x8000_0000).eax;
    if max < 0x8000_0004 {
        return None;
    }
    let mut bytes = Vec::with_capacity(48);
    for leaf in 0x8000_0002u32..=0x8000_0004 {
        let r = __cpuid(leaf);
        for reg in [r.eax, r.ebx, r.ecx, r.edx] {
            bytes.extend_from_slice(&reg.to_le_bytes());
        }
    }
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Frequência base (MHz) pelo `CallNtPowerInformation(ProcessorInformation)` (`MaxMhz`).
fn base_mhz(logical: u32) -> Metric<u32> {
    let n = logical.max(1) as usize;
    let mut buf = vec![PROCESSOR_POWER_INFORMATION::default(); n];
    let bytes = (n * size_of::<PROCESSOR_POWER_INFORMATION>()) as u32;
    // SAFETY: buffer local de `n` estruturas com o tamanho em bytes informado.
    let st = unsafe {
        CallNtPowerInformation(
            ProcessorInformation,
            None,
            0,
            Some(buf.as_mut_ptr().cast()),
            bytes,
        )
    };
    if st.is_err() {
        return fail(format!("CallNtPowerInformation: 0x{:08X}", st.0));
    }
    buf.iter()
        .map(|p| p.MaxMhz)
        .max()
        .filter(|&m| m > 0)
        .map_or_else(|| fail("CallNtPowerInformation returned no MaxMhz"), Ok)
}

fn topology() -> Metric<Topology> {
    let mut len = 0u32;
    // SAFETY: primeira chamada só mede o buffer (falha com ERROR_INSUFFICIENT_BUFFER).
    let _ = unsafe { GetLogicalProcessorInformationEx(RelationAll, None, &raw mut len) };
    if len == 0 {
        return fail("GetLogicalProcessorInformationEx returned no data");
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: buffer alinhado de `len` bytes.
    if let Err(e) = unsafe {
        GetLogicalProcessorInformationEx(RelationAll, Some(buf.as_mut_ptr().cast()), &raw mut len)
    } {
        return fail(format!("GetLogicalProcessorInformationEx: {e}"));
    }
    // SAFETY: Vec<u64> visto como bytes, dentro do tamanho alocado.
    let bytes = unsafe {
        std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), (len as usize).min(buf.len() * 8))
    };
    parse_topology(bytes).map_or_else(|| fail("topology has no cores"), Ok)
}

/// Percorre os registros `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` (cabeçalho: relação u32,
/// tamanho u32; união no byte 8). Núcleo: `GroupCount` u16 em +30, `GroupMask[]` em +32
/// (`GROUP_AFFINITY` de 16 bytes, máscara u64). Cache: nível u8 em +8, tamanho u32 em +12.
pub(crate) fn parse_topology(b: &[u8]) -> Option<Topology> {
    const CORE: u32 = 0;
    const CACHE: u32 = 2;
    const PACKAGE: u32 = 3;
    let u32_at = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?));
    let mut t = Topology::default();
    let mut off = 0usize;
    while off + 8 <= b.len() {
        let rel = u32_at(off)?;
        let size = u32_at(off + 4)? as usize;
        if size < 8 || off + size > b.len() {
            break;
        }
        let rec = &b[off..off + size];
        match rel {
            CORE => {
                t.cores += 1;
                let groups = usize::from(u16::from_le_bytes(rec.get(30..32)?.try_into().ok()?));
                for g in 0..groups {
                    let m = 32 + g * 16;
                    let mask = u64::from_le_bytes(rec.get(m..m + 8)?.try_into().ok()?);
                    t.logical += mask.count_ones();
                }
            }
            PACKAGE => t.sockets += 1,
            CACHE => {
                let level = *rec.get(8)?;
                let bytes = u64::from(u32::from_le_bytes(rec.get(12..16)?.try_into().ok()?));
                match level {
                    1 => t.l1 += bytes,
                    2 => t.l2 += bytes,
                    3 => t.l3 += bytes,
                    _ => {}
                }
            }
            _ => {}
        }
        off += size;
    }
    (t.cores > 0).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordem_por_grupo_e_numero() {
        let items = vec![
            ("0,10".to_string(), 5.0),
            ("_Total".to_string(), 50.0),
            ("0,2".to_string(), 120.0),
            ("1,0".to_string(), 7.0),
            ("0,_Total".to_string(), 3.0),
        ];
        assert_eq!(per_logical(&items), Some(vec![100.0, 5.0, 7.0]));
        assert_eq!(per_logical(&[]), None);
    }

    #[test]
    fn parcela_de_kernel() {
        // 100 de kernel (60 ociosos) + 100 de usuário: (100 - 60) / 200 = 20%.
        assert_eq!(kernel_share((0, 0, 0), (60, 100, 100)), Some(20.0));
        assert_eq!(kernel_share((0, 0, 0), (0, 0, 0)), None);
        assert_eq!(kernel_share((5, 0, 0), (0, 0, 0)), None);
    }

    fn record(rel: u32, size: usize, fill: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let mut r = vec![0u8; size];
        r[..4].copy_from_slice(&rel.to_le_bytes());
        r[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        fill(&mut r);
        r
    }

    #[test]
    fn topologia_sintetica() {
        let mut b = Vec::new();
        b.extend(record(3, 48, |_| {}));
        for _ in 0..2 {
            b.extend(record(0, 48, |r| {
                r[30..32].copy_from_slice(&1u16.to_le_bytes());
                r[32..40].copy_from_slice(&0b11u64.to_le_bytes());
            }));
        }
        b.extend(record(2, 48, |r| {
            r[8] = 3;
            r[12..16].copy_from_slice(&(32u32 << 20).to_le_bytes());
        }));
        let t = parse_topology(&b).expect("topologia");
        assert_eq!(
            t,
            Topology {
                sockets: 1,
                cores: 2,
                logical: 4,
                l1: 0,
                l2: 0,
                l3: 32 << 20
            }
        );
    }

    #[test]
    fn topologia_real() {
        let t = topology().expect("topologia desta máquina");
        assert!(
            t.cores >= 1 && t.logical >= t.cores && t.sockets >= 1,
            "{t:?}"
        );
    }
}
