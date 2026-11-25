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
use crate::pdh::{self, PdhError, Query, Slot};
use crate::win::filetime;

/// Quantas coletas seguidas toleram um erro transitório de cálculo do PDH (`0x800007D6..D8`,
/// ex.: passo de relógio/NTP ou delta negativo numa amostra isolada) repetindo a última leitura
/// válida antes de exibir falha real.
const TRANSIENT_LIMIT: u8 = 3;

pub struct CpuCollector {
    query: Result<Query, PdhError>,
    per: Slot,
    per_kernel: Slot,
    perf: Slot,
    name: Metric<String>,
    topology: Metric<Topology>,
    base_mhz: Metric<u32>,
    virtualization: Metric<bool>,
    /// Último (idle, kernel, user) de `GetSystemTimes`.
    times: Option<(u64, u64, u64)>,
    /// Últimos valores válidos por lógico (uso, kernel, desempenho): um lógico sem dado válido
    /// numa coleta repete o anterior, sem sumir da lista (sumir desalinharia as três listas e
    /// recriaria a grade de gráficos).
    last: [Vec<f64>; 3],
    /// Últimos valores válidos de `_Total` (uso e desempenho) para absorver falhas transitórias
    /// de cálculo do PDH (`0x800007D6..D8`) por até `TRANSIENT_LIMIT` coletas seguidas.
    last_total: [Option<f64>; 2],
    /// Coletas seguidas com erro transitório de cálculo do PDH em cada contador (`per`,
    /// `per_kernel`, `perf`).
    transient_streak: [u8; 3],
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
        // Todas as instâncias: o `_Total` dá a média e cada lógico o próprio clock.
        let perf = add(r"\Processor Information(*)\% Processor Performance");
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
            last: Default::default(),
            last_total: [None, None],
            transient_streak: [0; 3],
        }
    }

    /// `PdhCollectQueryData` desta consulta (chamado antes de `read`).
    pub fn sample(&mut self) -> Result<(), PdhError> {
        match &self.query {
            Ok(q) => q.collect(),
            Err(e) => Err(e.clone()),
        }
    }

    pub fn read(&mut self, perf_info: Result<&PERFORMANCE_INFORMATION, &String>) -> Cpu {
        let sampled = self.sample();
        // Um contador só: a instância `_Total` vem junto com as dos lógicos.
        let array = match &sampled {
            Err(e) => Err(e.clone()),
            Ok(()) => read_cpu_array(&self.query, &self.per),
        };
        let (usage, per_logical) = resolve_cpu_parsed(
            "usage",
            array,
            100.0,
            &mut self.last[0],
            &mut self.last_total[0],
            &mut self.transient_streak[0],
        );
        // Antes de `per_logical` já está resolvido; agora o kernel por lógico.
        let kernel_array = match &sampled {
            Err(e) => Err(e.clone()),
            Ok(()) => read_cpu_array(&self.query, &self.per_kernel),
        };
        let (_, kernel_per_logical) = resolve_cpu_parsed(
            "kernel",
            kernel_array,
            100.0,
            &mut self.last[1],
            &mut None,
            &mut self.transient_streak[1],
        );
        let kernel_per_logical = match (&per_logical, kernel_per_logical) {
            (Ok(u), Ok(k)) if u.len() != k.len() => fail("kernel instances differ from usage"),
            (_, k) => k,
        };
        // Clock efetivo = % Processor Performance × base (como o Task Manager); passa de 100%
        // com o turbo.
        let perf_array = match &sampled {
            Err(e) => Err(e.clone()),
            Ok(()) => read_cpu_array(&self.query, &self.perf),
        };
        let (perf_total, perf_per_logical) = resolve_cpu_parsed(
            "perf",
            perf_array,
            PERF_MAX,
            &mut self.last[2],
            &mut self.last_total[1],
            &mut self.transient_streak[2],
        );
        let to_mhz = |p: f64| -> Metric<f64> {
            self.base_mhz
                .as_ref()
                .map(|b| p / 100.0 * f64::from(*b))
                .map_err(Clone::clone)
        };
        let freq_mhz = perf_total.and_then(to_mhz);
        let freq_per_logical: Metric<Vec<f64>> =
            perf_per_logical.and_then(|v| v.into_iter().map(to_mhz).collect());
        let freq_per_logical = match (&per_logical, freq_per_logical) {
            (Ok(u), Ok(f)) if u.len() != f.len() => fail("clock instances differ from usage"),
            (_, f) => f,
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
            freq_per_logical,
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
    let total = dk.checked_add(du)?;
    (total > 0).then(|| (dk.saturating_sub(di) as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
}

/// Instâncias já decodificadas de um contador `\Processor Information(*)`: `(_Total, lógicos)`.
type CpuArray = (Option<f64>, Vec<((u32, u32), f64)>);

fn parse_logical(name: &str) -> Option<(u32, u32)> {
    let (g, n) = name.split_once(',')?;
    Some((g.parse().ok()?, n.parse().ok()?))
}

/// Lê um contador `\Processor Information(*)` sem alocar `String` por instância.
fn read_cpu_array(q: &Result<Query, PdhError>, slot: &Slot) -> Result<CpuArray, PdhError> {
    let mut total = None;
    let mut logical = Vec::new();
    pdh::for_each_array_all(q, slot, |name, val| {
        if name == "_Total" {
            total.get_or_insert(val);
        } else if let Some(key) = parse_logical(name) {
            logical.push((key, val));
        }
    })?;
    Ok((total, logical))
}

/// Extrai `(_Total, por_lógico)` de uma leitura de contador por processador do PDH.
///
/// Em erros transitórios de cálculo diferencial do PDH (`0x800007D6..D8`, ex.: ajuste de relógio
/// ou delta negativo numa amostra isolada), repete a última leitura válida por até
/// `TRANSIENT_LIMIT` coletas seguidas (registrando em `debug.log`) antes de expor a falha. Se só
/// `_Total` vier `NaN` numa coleta com lógicos válidos, calcula a média dos lógicos.
fn resolve_cpu_parsed(
    name: &str,
    raw: Result<CpuArray, PdhError>,
    max: f64,
    last_per: &mut Vec<f64>,
    last_total: &mut Option<f64>,
    streak: &mut u8,
) -> (Metric<f64>, Metric<Vec<f64>>) {
    match raw {
        Ok((total_raw, logical)) => {
            *streak = 0;
            let per =
                per_logical_parsed(logical, max, last_per).map_or_else(|| fail("no instances"), Ok);
            let total = match total_raw {
                Some(v) if v.is_finite() => {
                    let clamped = v.clamp(0.0, max);
                    *last_total = Some(clamped);
                    Ok(clamped)
                }
                // `_Total` veio com NaN mas os lógicos são válidos: usa a média deles (ou último).
                Some(_) => {
                    let fallback = per
                        .as_ref()
                        .ok()
                        .filter(|v| !v.is_empty())
                        .map(|v| (v.iter().sum::<f64>() / v.len() as f64).clamp(0.0, max))
                        .or(*last_total);
                    if let Some(v) = fallback {
                        crate::debug::log(
                            "cpu",
                            &format!("PDH returned NaN for {name} _Total; using {v:.1}"),
                        );
                        *last_total = Some(v);
                        Ok(v)
                    } else {
                        fail("no _Total instance")
                    }
                }
                None => fail("no _Total instance"),
            };
            (total, per)
        }
        Err(e) => {
            if e.is_transient_calc() && !last_per.is_empty() && *streak < TRANSIENT_LIMIT {
                *streak += 1;
                crate::debug::log(
                    "cpu",
                    &format!(
                        "PDH transient error ({e}) on {name}; kept last sample ({}/{TRANSIENT_LIMIT})",
                        *streak
                    ),
                );
                let total = last_total.map_or_else(|| fail(e.message.clone()), Ok);
                return (total, Ok(last_per.clone()));
            }
            (fail(e.message.clone()), fail(e.message))
        }
    }
}

#[cfg(test)]
fn resolve_cpu_array(
    name: &str,
    raw: Result<Vec<(String, f64)>, PdhError>,
    max: f64,
    last_per: &mut Vec<f64>,
    last_total: &mut Option<f64>,
    streak: &mut u8,
) -> (Metric<f64>, Metric<Vec<f64>>) {
    let parsed = raw.map(|items| {
        let mut total = None;
        let mut logical = Vec::new();
        for (n, v) in items {
            if n == "_Total" {
                total.get_or_insert(v);
            } else if let Some(key) = parse_logical(&n) {
                logical.push((key, v));
            }
        }
        (total, logical)
    });
    resolve_cpu_parsed(name, parsed, max, last_per, last_total, streak)
}

/// Uso por processador lógico, ordenado por (grupo, número); ignora as instâncias `_Total`.
#[cfg(test)]
fn per_logical(items: &[(String, f64)], last: &mut Vec<f64>) -> Option<Vec<f64>> {
    let logical: Vec<((u32, u32), f64)> = items
        .iter()
        .filter_map(|(n, v)| Some((parse_logical(n)?, *v)))
        .collect();
    per_logical_parsed(logical, 100.0, last)
}

/// Teto do `% Processor Performance` (turbo passa de 100%; acima disto é contador corrompido).
const PERF_MAX: f64 = 1000.0;

/// Valores por lógico em 0..=`max`, ordenados por (grupo, número). Lógico sem dado válido
/// (NaN) repete o valor de `last` na mesma posição (ou 0); `last` guarda o resultado.
fn per_logical_parsed(
    mut v: Vec<((u32, u32), f64)>,
    max: f64,
    last: &mut Vec<f64>,
) -> Option<Vec<f64>> {
    if v.is_empty() {
        return None;
    }
    v.sort_by_key(|(k, _)| *k);
    // Outra quantidade de lógicos (CPU ligada a quente): o histórico não vale mais.
    if last.len() != v.len() {
        last.clear();
    }
    let out: Vec<f64> = v
        .into_iter()
        .enumerate()
        .map(|(i, (_, x))| {
            if x.is_nan() {
                let prev = last.get(i).copied().unwrap_or(0.0);
                crate::debug::log(
                    "cpu",
                    &format!("PDH returned NaN for logical {i}; kept {prev:.1}"),
                );
                prev
            } else {
                x.clamp(0.0, max)
            }
        })
        .collect();
    last.clone_from(&out);
    Some(out)
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
        if size < 8 || off.checked_add(size).is_none_or(|end| end > b.len()) {
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

    /// Carga conhecida por lógico: K threads presas (afinidade) nos lógicos 0..K ocupadas o
    /// tempo todo aparecem nesses lógicos, na ordem do coletor, e no total (~K/N).
    #[test]
    #[ignore = "mede carga real: rodar sozinho (--test-threads=1)"]
    fn carga_presa_nos_logicos() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
        const K: usize = 4;
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..K)
            .map(|k| {
                let s = Arc::clone(&stop);
                std::thread::spawn(move || {
                    // SAFETY: pseudo-handle da própria thread; máscara de um lógico do grupo 0.
                    unsafe { SetThreadAffinityMask(GetCurrentThread(), 1 << k) };
                    let mut x = 0u64;
                    while !s.load(Ordering::Relaxed) {
                        x = std::hint::black_box(x.wrapping_add(1));
                    }
                })
            })
            .collect();
        let mut c = CpuCollector::new();
        let pi = super::super::perf_info();
        let _ = c.read(pi.as_ref());
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let pi = super::super::perf_info();
        let cpu = c.read(pi.as_ref());
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().expect("thread");
        }
        let per = cpu.per_logical.expect("por lógico");
        let n = per.len() as f64;
        let total = cpu.usage.expect("total");
        println!(
            "lógicos 0..{K}: {:?}; total {total:.1}% (mín. {:.1}%)",
            &per[..K],
            100.0 * K as f64 / n
        );
        assert!(per[..K].iter().all(|v| *v > 85.0), "{per:?}");
        assert!(total >= 100.0 * K as f64 / n * 0.9, "{total}");
    }

    #[test]
    fn ordem_por_grupo_e_numero() {
        let items = vec![
            ("0,10".to_string(), 5.0),
            ("_Total".to_string(), 50.0),
            ("0,2".to_string(), 120.0),
            ("1,0".to_string(), 7.0),
            ("0,_Total".to_string(), 3.0),
        ];
        let mut last = Vec::new();
        assert_eq!(per_logical(&items, &mut last), Some(vec![100.0, 5.0, 7.0]));
        assert_eq!(per_logical(&[], &mut last), None);
    }

    /// Um lógico sem dado válido numa coleta (o PDH devolve NaN para ele) continua na lista, na
    /// mesma posição, com o último valor: a lista não encolhe (o que desalinhava uso, kernel e
    /// clock e recriava a grade de gráficos).
    #[test]
    fn logico_sem_dado_mantem_a_posicao() {
        let items = |v: [f64; 3]| -> Vec<(String, f64)> {
            ["0,0", "0,1", "0,2"]
                .iter()
                .zip(v)
                .map(|(n, x)| (n.to_string(), x))
                .collect()
        };
        let mut last = Vec::new();
        assert_eq!(
            per_logical(&items([10.0, 20.0, 30.0]), &mut last),
            Some(vec![10.0, 20.0, 30.0])
        );
        assert_eq!(
            per_logical(&items([11.0, f64::NAN, 31.0]), &mut last),
            Some(vec![11.0, 20.0, 31.0])
        );
        assert_eq!(
            per_logical(&items([12.0, 22.0, 32.0]), &mut last),
            Some(vec![12.0, 22.0, 32.0])
        );
        // Sem valor anterior (1ª coleta): 0.
        let mut fresh = Vec::new();
        assert_eq!(
            per_logical(&items([f64::NAN, 5.0, 6.0]), &mut fresh),
            Some(vec![0.0, 5.0, 6.0])
        );
    }

    #[test]
    fn erro_transitorio_do_pdh_mantem_ultima_leitura_ate_o_limite() {
        let mut last_per = Vec::new();
        let mut last_total = None;
        let mut streak = 0u8;
        let ok_items = || {
            Ok(vec![
                ("_Total".to_string(), 25.0),
                ("0,0".to_string(), 20.0),
                ("0,1".to_string(), 30.0),
            ])
        };
        let (u, p) = resolve_cpu_array(
            "usage",
            ok_items(),
            100.0,
            &mut last_per,
            &mut last_total,
            &mut streak,
        );
        assert_eq!(u, Ok(25.0));
        assert_eq!(p, Ok(vec![20.0, 30.0]));
        assert_eq!(streak, 0);

        // Até TRANSIENT_LIMIT falhas transitórias seguidas (0x800007D6..D8) repetem a última leitura.
        let transient =
            PdhError::from_code(windows::Win32::System::Performance::PDH_CALC_NEGATIVE_DENOMINATOR);
        for expected in 1..=TRANSIENT_LIMIT {
            let (u, p) = resolve_cpu_array(
                "usage",
                Err(transient.clone()),
                100.0,
                &mut last_per,
                &mut last_total,
                &mut streak,
            );
            assert_eq!(u, Ok(25.0));
            assert_eq!(p, Ok(vec![20.0, 30.0]));
            assert_eq!(streak, expected);
        }
        // Acima do limite, a falha aparece normalmente.
        let (u, p) = resolve_cpu_array(
            "usage",
            Err(transient),
            100.0,
            &mut last_per,
            &mut last_total,
            &mut streak,
        );
        assert!(u.is_err());
        assert!(p.is_err());

        // Leitura boa zera a contagem; erro não-transitório aparece imediatamente.
        let _ = resolve_cpu_array(
            "usage",
            ok_items(),
            100.0,
            &mut last_per,
            &mut last_total,
            &mut streak,
        );
        assert_eq!(streak, 0);
        let (u, p) = resolve_cpu_array(
            "usage",
            Err(PdhError::from_code(
                windows::Win32::System::Performance::PDH_CSTATUS_NO_OBJECT,
            )),
            100.0,
            &mut last_per,
            &mut last_total,
            &mut streak,
        );
        assert!(u.is_err());
        assert!(p.is_err());

        // Só `_Total` em NaN usa a média dos lógicos válidos.
        let (u, p) = resolve_cpu_array(
            "usage",
            Ok(vec![
                ("_Total".to_string(), f64::NAN),
                ("0,0".to_string(), 10.0),
                ("0,1".to_string(), 50.0),
            ]),
            100.0,
            &mut last_per,
            &mut last_total,
            &mut streak,
        );
        assert_eq!(u, Ok(30.0));
        assert_eq!(p, Ok(vec![10.0, 50.0]));
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

    /// Tempos do sistema e instâncias do PDH quaisquer: participação do kernel e uso por lógico
    /// sempre em 0..=100 (NaN do PDH vira 0; contadores que estouram, "sem amostra").
    #[test]
    fn fuzz_kernel_e_uso_por_logico() {
        let t = |v: [u64; 6]| -> Vec<u8> { v.iter().flat_map(|x| x.to_le_bytes()).collect() };
        let seed = t([100, 500, 300, 150, 900, 700]);
        crate::fuzz::run("kernel", &[seed], 48, |_, b| {
            let mut v = [0u64; 6];
            for (i, c) in b.as_chunks::<8>().0.iter().take(6).enumerate() {
                v[i] = u64::from_le_bytes(*c);
            }
            if let Some(k) = kernel_share((v[0], v[1], v[2]), (v[3], v[4], v[5])) {
                assert!((0.0..=100.0).contains(&k));
            }
        });
        crate::fuzz::run_text("uso_por_logico", &["0,15", "1,63", "_Total"], 64, |name| {
            for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 42.0] {
                let items = [(name.to_string(), v), ("0,0".to_string(), v)];
                let u = per_logical(&items, &mut Vec::new()).expect("0,0 é válido");
                assert!(u.iter().all(|x| (0.0..=100.0).contains(x)));
            }
        });
    }

    #[test]
    fn fuzz_topologia() {
        let mut b = record(3, 48, |_| {});
        b.extend(record(0, 48, |r| {
            r[30..32].copy_from_slice(&1u16.to_le_bytes());
            r[32..40].copy_from_slice(&0xFFu64.to_le_bytes());
        }));
        b.extend(record(2, 48, |r| r[8] = 2));
        crate::fuzz::run("topologia", &[b], 4096, |_, b| {
            if let Some(t) = parse_topology(b) {
                // Cada núcleo ocupa pelo menos um registro de 8 bytes.
                assert!(t.cores > 0 && t.cores as usize <= b.len() / 8);
            }
        });
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
