//! Memória: uso, confirmada, em cache, pools e arquivo de paginação.

use windows::Win32::System::ProcessStatus::PERFORMANCE_INFORMATION;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use crate::model::{Memory, Metric, fail};
use crate::pdh::{self, Query, Slot};

/// Contadores do "Em cache" do Gerenciador de Tarefas: Standby (três prioridades) + Modified.
const CACHE: [&str; 4] = [
    r"\Memory\Standby Cache Core Bytes",
    r"\Memory\Standby Cache Normal Priority Bytes",
    r"\Memory\Standby Cache Reserve Bytes",
    r"\Memory\Modified Page List Bytes",
];

pub struct MemoryCollector {
    query: Result<Query, String>,
    cache: Vec<Slot>,
    pagefile: Slot,
}

impl MemoryCollector {
    pub fn new() -> Self {
        let query = Query::open();
        let add = |p: &str| match &query {
            Ok(q) => q.add(p),
            Err(e) => Err(e.clone()),
        };
        let cache = CACHE.iter().map(|p| add(p)).collect();
        let pagefile = add(r"\Paging File(_Total)\% Usage");
        Self {
            query,
            cache,
            pagefile,
        }
    }

    pub fn read(&mut self, perf_info: Result<&PERFORMANCE_INFORMATION, &String>) -> Memory {
        let sampled = match &self.query {
            Ok(q) => q.collect(),
            Err(e) => Err(e.clone()),
        };
        let pdh_val = |slot: &Slot| -> Metric<f64> {
            match &sampled {
                Err(e) => fail(e.clone()),
                Ok(()) => pdh::read(&self.query, slot).or_else(fail),
            }
        };

        let mut ms = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: estrutura local com dwLength preenchido.
        let status = unsafe { GlobalMemoryStatusEx(&raw mut ms) }
            .map(|()| (ms.ullTotalPhys, ms.ullAvailPhys))
            .map_err(|e| format!("GlobalMemoryStatusEx: {e}"));

        // As três prioridades do standby e a lista modificada (a última do `CACHE`).
        let standby = self.cache[..3]
            .iter()
            .map(pdh_val)
            .sum::<Metric<f64>>()
            .map(|v| v as u64);
        let modified = pdh_val(&self.cache[3]).map(|v| v as u64);
        let cached = standby.clone().and_then(|s| Ok(s + modified.clone()?));
        let pages = |f: fn(&PERFORMANCE_INFORMATION) -> usize| -> Metric<u64> {
            match perf_info {
                Ok(p) => Ok((f(p) as u64).saturating_mul(p.PageSize as u64)),
                Err(e) => fail(e.clone()),
            }
        };
        let pf_total = match perf_info {
            Ok(p) => Ok((p.CommitLimit.saturating_sub(p.PhysicalTotal) as u64)
                .saturating_mul(p.PageSize as u64)),
            Err(e) => fail(e.clone()),
        };
        let pagefile_usage = pdh_val(&self.pagefile).and_then(|pct| {
            let total = pf_total?;
            let pct = pct.clamp(0.0, 100.0);
            let used = ((pct / 100.0) * total as f64).round() as u64;
            Ok((used.min(total), total))
        });
        Memory {
            total: status.clone().map(|s| s.0).or_else(fail),
            used: status.map(|(t, a)| t.saturating_sub(a)).or_else(fail),
            commit: pages(|p| p.CommitTotal).and_then(|c| Ok((c, pages(|p| p.CommitLimit)?))),
            cached,
            standby,
            modified,
            paged_pool: pages(|p| p.KernelPaged),
            nonpaged_pool: pages(|p| p.KernelNonpaged),
            pagefile_usage,
        }
    }
}
