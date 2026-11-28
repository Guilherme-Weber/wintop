//! Memória: uso, compressão (RAM ocupada e tamanho original estimado), confirmada (+ pico desde
//! o boot), em cache (Standby + Modified), pools, arquivo de paginação, paginação real em disco,
//! hardware SMBIOS, reservada para hardware e páginas defeituosas.
//!
//! 100% via APIs nativas Win32/NT (`GlobalMemoryStatusEx`, `GetPerformanceInfo`,
//! `NtQuerySystemInformation` classes 2, 18, 80 e 109, SMBIOS): zero consultas PDH.

use std::time::{Duration, Instant};

use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS};
use windows::Win32::System::ProcessStatus::PERFORMANCE_INFORMATION;
use windows::Win32::System::SystemInformation::{
    FIRMWARE_TABLE_PROVIDER, GetPhysicallyInstalledSystemMemory, GetSystemFirmwareTable,
    GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO,
};

use crate::model::{CompressedMemory, Memory, MemoryHardware, Metric, fail, neutral};

/// Classes da `NtQuerySystemInformation` usadas diretamente.
const SYSTEM_PERFORMANCE_INFORMATION: i32 = 2;
const SYSTEM_PAGEFILE_INFORMATION: i32 = 18;
const SYSTEM_MEMORY_LIST_INFORMATION: i32 = 80;
const SYSTEM_STORE_INFORMATION: i32 = 109;
/// Subclasse `SmMemCompressionInfoRequest` dentro de `SystemStoreInformation`.
const STORE_COMPRESSION_INFO: u32 = 22;
const STORE_COMPRESSION_VERSION: u32 = 3;

/// Offsets de `PageReadCount` e `DirtyPagesWriteCount` (`u32`) em `SYSTEM_PERFORMANCE_INFORMATION`
/// (mesmos contadores por trás de `\Memory\Pages Input/sec` e `\Memory\Pages Output/sec`).
const PERF_PAGE_READ_COUNT: usize = 80;
const PERF_DIRTY_PAGES_WRITE_COUNT: usize = 96;
const PERF_MIN_LEN: usize = PERF_DIRTY_PAGES_WRITE_COUNT + 4;

/// Offsets de `SYSTEM_PAGEFILE_INFORMATION` (lista encadeada por `NextEntryOffset`, valores em
/// páginas).
const PF_NEXT_OFFSET: usize = 0;
const PF_TOTAL_SIZE: usize = 4;
const PF_TOTAL_IN_USE: usize = 8;
const PF_MIN_ENTRY: usize = 16;
const PF_MAX_BUF: usize = 1 << 20;

const STATUS_INVALID_INFO_CLASS: u32 = 0xC000_0003;
const STATUS_INFO_LENGTH_MISMATCH: u32 = 0xC000_0004;
const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
const STATUS_REVISION_MISMATCH: u32 = 0xC000_0059;
const STATUS_PRIVILEGE_NOT_HELD: u32 = 0xC000_0061;
const STATUS_NOT_SUPPORTED: u32 = 0xC000_00BB;
/// Retornado por `SystemStoreInformation(22)` no Windows Server e quando `MemoryCompression`
/// está desabilitada via `Disable-MMAgent -MemoryCompression`.
const STATUS_QUOTA_NOT_ENABLED: u32 = 0xC000_01A9;
const STATUS_NOT_FOUND: u32 = 0xC000_0225;
const STATUS_COMPRESSION_DISABLED: u32 = 0xC000_0426;

/// Assinatura `'RSMB'` (Raw SMBIOS) para `GetSystemFirmwareTable`.
const RSMB: FIRMWARE_TABLE_PROVIDER = FIRMWARE_TABLE_PROVIDER(u32::from_be_bytes(*b"RSMB"));

#[repr(C)]
struct SmMemCompressionInfoRequest {
    version: u32,
    compression_pid: u32,
    working_set_size: u64,
    total_data_compressed: u64,
    total_compressed_size: u64,
    total_unique_data_compressed: u64,
}

#[repr(C)]
struct SystemStoreInformation {
    version: u32,
    store_information_class: u32,
    data: *mut SmMemCompressionInfoRequest,
    length: u32,
}

#[repr(C)]
struct SystemMemoryListInformation {
    zero_page_count: usize,
    free_page_count: usize,
    modified_page_count: usize,
    modified_no_write_page_count: usize,
    bad_page_count: usize,
    page_count_by_priority: [usize; 8],
    repurposed_pages_by_priority: [usize; 8],
    modified_page_count_page_file: usize,
}

pub struct MemoryCollector {
    /// Tamanho de página do sistema (tipicamente 4096 bytes), lido uma vez na abertura.
    page_size: u64,
    /// Buffer alinhado reaproveitado para `SystemPagefileInformation`.
    pagefile_buf: Vec<u64>,
    /// Amostra anterior `(instante, PageReadCount, DirtyPagesWriteCount)` para a taxa de paginação.
    last_paging: Option<(Instant, u32, u32)>,
    /// Estáticos por boot: lidos uma única vez na abertura.
    hardware: Metric<MemoryHardware>,
    installed_bytes: Metric<u64>,
}

impl MemoryCollector {
    pub fn new() -> Self {
        let mut si = SYSTEM_INFO::default();
        // SAFETY: estrutura local preenchida pelo kernel.
        unsafe { GetSystemInfo(&raw mut si) };
        let page_size = u64::from(si.dwPageSize).max(4096);
        Self {
            page_size,
            pagefile_buf: Vec::new(),
            last_paging: None,
            hardware: read_hardware(),
            installed_bytes: read_installed_ram(),
        }
    }

    pub fn read(&mut self, perf_info: Result<&PERFORMANCE_INFORMATION, &String>) -> Memory {
        let mut ms = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        // SAFETY: estrutura local com dwLength preenchido.
        let status = unsafe { GlobalMemoryStatusEx(&raw mut ms) }
            .map(|()| (ms.ullTotalPhys, ms.ullAvailPhys))
            .map_err(|e| format!("GlobalMemoryStatusEx: {e}"));

        let page_size = perf_info.map_or(self.page_size, |p| (p.PageSize as u64).max(1));
        let mem_list = read_memory_list(page_size);
        let standby = mem_list.as_ref().map(|m| m.0).map_err(Clone::clone);
        let modified = mem_list.as_ref().map(|m| m.1).map_err(Clone::clone);
        let cached = mem_list
            .as_ref()
            .map(|m| m.0.saturating_add(m.1))
            .map_err(Clone::clone);
        let bad_pages = mem_list.map(|m| m.2);

        let pages = |f: fn(&PERFORMANCE_INFORMATION) -> usize| -> Metric<u64> {
            match perf_info {
                Ok(p) => Ok((f(p) as u64).saturating_mul(p.PageSize as u64)),
                Err(e) => fail(e.clone()),
            }
        };
        let pagefile_usage = read_pagefile(&mut self.pagefile_buf, page_size);
        let paging = read_paging(&mut self.last_paging);
        let total = status.clone().map(|s| s.0).or_else(fail);
        let hardware_reserved = self
            .installed_bytes
            .clone()
            .and_then(|inst| Ok(inst.saturating_sub(total.clone()?)));

        Memory {
            total,
            used: status.map(|(t, a)| t.saturating_sub(a)).or_else(fail),
            commit: pages(|p| p.CommitTotal).and_then(|c| Ok((c, pages(|p| p.CommitLimit)?))),
            commit_peak: pages(|p| p.CommitPeak),
            compressed: read_compressed(),
            cached,
            standby,
            modified,
            paged_pool: pages(|p| p.KernelPaged),
            nonpaged_pool: pages(|p| p.KernelNonpaged),
            pagefile_usage,
            paging,
            hardware: self.hardware.clone(),
            hardware_reserved,
            bad_pages,
        }
    }
}

/// Estima o tamanho original (descomprimido) dos dados comprimidos atualmente residentes em RAM
/// usando a taxa de compressão global do store (`total_data / total_comp`), como o tooltip do
/// Gerenciador de Tarefas.
fn estimate_uncompressed(ws: u64, total_data: u64, total_comp: u64) -> u64 {
    if ws == 0 || total_comp == 0 || total_data == 0 {
        return ws;
    }
    let ratio = total_data as f64 / total_comp as f64;
    let est = (ws as f64 * ratio).round();
    if est.is_finite() && est > 0.0 {
        (est as u64).max(ws)
    } else {
        ws
    }
}

/// Lê a compressão de memória do Windows via `NtQuerySystemInformation(SystemStoreInformation)`.
fn read_compressed() -> Metric<CompressedMemory> {
    let mut req = SmMemCompressionInfoRequest {
        version: STORE_COMPRESSION_VERSION,
        compression_pid: 0,
        working_set_size: 0,
        total_data_compressed: 0,
        total_compressed_size: 0,
        total_unique_data_compressed: 0,
    };
    let mut ssi = SystemStoreInformation {
        version: 1,
        store_information_class: STORE_COMPRESSION_INFO,
        data: &raw mut req,
        length: size_of::<SmMemCompressionInfoRequest>() as u32,
    };
    let mut ret = 0u32;
    // SAFETY: buffers locais alinhados e válidos com tamanhos exatos.
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_INFORMATION_CLASS(SYSTEM_STORE_INFORMATION),
            (&raw mut ssi).cast(),
            size_of::<SystemStoreInformation>() as u32,
            &raw mut ret,
        )
    };
    if status.is_ok() {
        if req.compression_pid == 0 && req.working_set_size == 0 {
            return neutral("disabled");
        }
        return Ok(CompressedMemory {
            compressed: req.working_set_size,
            uncompressed: estimate_uncompressed(
                req.working_set_size,
                req.total_data_compressed,
                req.total_compressed_size,
            ),
        });
    }
    map_store_status(status.0 as u32)
}

fn map_store_status(code: u32) -> Metric<CompressedMemory> {
    match code {
        STATUS_QUOTA_NOT_ENABLED | STATUS_NOT_FOUND | STATUS_COMPRESSION_DISABLED => {
            neutral("disabled")
        }
        STATUS_NOT_SUPPORTED | STATUS_INVALID_INFO_CLASS | STATUS_REVISION_MISMATCH => {
            neutral("not supported by Windows")
        }
        STATUS_ACCESS_DENIED | STATUS_PRIVILEGE_NOT_HELD => neutral("needs elevation"),
        _ => fail(format!("SystemStoreInformation: 0x{code:08X}")),
    }
}

/// Lê `(standby_bytes, modified_bytes, bad_pages)` em uma única chamada NT
/// (`SystemMemoryListInformation = 80`).
///
/// `standby` = soma das 8 prioridades; `modified` = `ModifiedPageCount + ModifiedNoWritePageCount`
/// (exatamente igual aos contadores PDH `\Memory\Standby Cache *` e `Modified Page List Bytes`).
fn read_memory_list(page_size: u64) -> Metric<(u64, u64, u64)> {
    let mut info = SystemMemoryListInformation {
        zero_page_count: 0,
        free_page_count: 0,
        modified_page_count: 0,
        modified_no_write_page_count: 0,
        bad_page_count: 0,
        page_count_by_priority: [0; 8],
        repurposed_pages_by_priority: [0; 8],
        modified_page_count_page_file: 0,
    };
    let mut ret = 0u32;
    // SAFETY: estrutura local com o tamanho exato esperado pelo kernel.
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_INFORMATION_CLASS(SYSTEM_MEMORY_LIST_INFORMATION),
            (&raw mut info).cast(),
            size_of::<SystemMemoryListInformation>() as u32,
            &raw mut ret,
        )
    };
    if status.is_ok() {
        let standby_pages = info
            .page_count_by_priority
            .iter()
            .fold(0u64, |acc, &p| acc.saturating_add(p as u64));
        let modified_pages = (info.modified_page_count as u64)
            .saturating_add(info.modified_no_write_page_count as u64);
        return Ok((
            standby_pages.saturating_mul(page_size),
            modified_pages.saturating_mul(page_size),
            info.bad_page_count as u64,
        ));
    }
    let code = status.0 as u32;
    match code {
        STATUS_ACCESS_DENIED | STATUS_PRIVILEGE_NOT_HELD => neutral("needs elevation"),
        STATUS_NOT_SUPPORTED | STATUS_INVALID_INFO_CLASS => neutral("not supported by Windows"),
        _ => fail(format!("SystemMemoryListInformation: 0x{code:08X}")),
    }
}

/// Lê `(usado, total)` em bytes de todos os arquivos de paginação ativos via
/// `NtQuerySystemInformation(SystemPagefileInformation = 18)`.
fn read_pagefile(buf: &mut Vec<u64>, page_size: u64) -> Metric<(u64, u64)> {
    if buf.len() < 64 {
        buf.resize(64, 0);
    }
    for _ in 0..4 {
        let mut ret = 0u32;
        let cap_bytes = buf.len() * 8;
        // SAFETY: buffer próprio, contíguo e alinhado a 8 bytes com tamanho informado em bytes.
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_INFORMATION_CLASS(SYSTEM_PAGEFILE_INFORMATION),
                buf.as_mut_ptr().cast(),
                cap_bytes as u32,
                &raw mut ret,
            )
        };
        if status.is_ok() {
            let len = (ret as usize).min(cap_bytes);
            // SAFETY: `buf` é uma fatia válida de `cap_bytes` bytes já inicializados.
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len) };
            return parse_pagefiles(bytes, page_size);
        }
        let code = status.0 as u32;
        if code == STATUS_INFO_LENGTH_MISMATCH {
            let needed = (ret as usize).max(cap_bytes.saturating_mul(2));
            if needed > PF_MAX_BUF {
                return fail("SystemPagefileInformation: buffer too large");
            }
            buf.resize(needed.div_ceil(8), 0);
            continue;
        }
        return match code {
            STATUS_ACCESS_DENIED | STATUS_PRIVILEGE_NOT_HELD => neutral("needs elevation"),
            STATUS_NOT_SUPPORTED | STATUS_INVALID_INFO_CLASS => neutral("not supported by Windows"),
            _ => fail(format!("SystemPagefileInformation: 0x{code:08X}")),
        };
    }
    fail("SystemPagefileInformation: buffer mismatch")
}

/// Percorre a lista encadeada de `SYSTEM_PAGEFILE_INFORMATION` e soma `(usado, total)` em bytes.
fn parse_pagefiles(buf: &[u8], page_size: u64) -> Metric<(u64, u64)> {
    if buf.is_empty() {
        return neutral("no page file");
    }
    let u32_at = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            buf.get(off..off.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    let mut off = 0usize;
    let mut total_pages = 0u64;
    let mut used_pages = 0u64;
    for _ in 0..64 {
        if off.checked_add(PF_MIN_ENTRY).is_none_or(|e| e > buf.len()) {
            break;
        }
        let Some(next) = u32_at(off + PF_NEXT_OFFSET) else {
            break;
        };
        let Some(total) = u32_at(off + PF_TOTAL_SIZE) else {
            break;
        };
        let Some(in_use) = u32_at(off + PF_TOTAL_IN_USE) else {
            break;
        };
        total_pages = total_pages.saturating_add(u64::from(total));
        used_pages = used_pages.saturating_add(u64::from(in_use));
        if next == 0 {
            break;
        }
        let next = next as usize;
        if next < PF_MIN_ENTRY {
            break;
        }
        let Some(next_off) = off.checked_add(next) else {
            break;
        };
        off = next_off;
    }
    if total_pages == 0 {
        return neutral("no page file");
    }
    let total = total_pages.saturating_mul(page_size);
    let used = used_pages.saturating_mul(page_size).min(total);
    Ok((used, total))
}

/// Lê `(pages_in/s, pages_out/s)` pelo delta de `PageReadCount` e `DirtyPagesWriteCount` de
/// `NtQuerySystemInformation(SystemPerformanceInformation = 2)`.
fn read_paging(last: &mut Option<(Instant, u32, u32)>) -> Metric<(f64, f64)> {
    let mut words = [0u64; 128];
    let mut ret = 0u32;
    // SAFETY: buffer local alinhado a 8 bytes na pilha com tamanho informado em bytes (1024).
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_INFORMATION_CLASS(SYSTEM_PERFORMANCE_INFORMATION),
            words.as_mut_ptr().cast(),
            size_of_val(&words) as u32,
            &raw mut ret,
        )
    };
    let now = Instant::now();
    if status.is_err() {
        let code = status.0 as u32;
        return match code {
            STATUS_ACCESS_DENIED | STATUS_PRIVILEGE_NOT_HELD => neutral("needs elevation"),
            STATUS_NOT_SUPPORTED | STATUS_INVALID_INFO_CLASS => neutral("not supported by Windows"),
            _ => fail(format!("SystemPerformanceInformation: 0x{code:08X}")),
        };
    }
    if (ret as usize) < PERF_MIN_LEN {
        return fail("SystemPerformanceInformation: response too short");
    }
    // SAFETY: `words` é um array local inicializado de 1024 bytes.
    let buf =
        unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), size_of_val(&words)) };
    let page_read = u32::from_le_bytes([
        buf[PERF_PAGE_READ_COUNT],
        buf[PERF_PAGE_READ_COUNT + 1],
        buf[PERF_PAGE_READ_COUNT + 2],
        buf[PERF_PAGE_READ_COUNT + 3],
    ]);
    let dirty_write = u32::from_le_bytes([
        buf[PERF_DIRTY_PAGES_WRITE_COUNT],
        buf[PERF_DIRTY_PAGES_WRITE_COUNT + 1],
        buf[PERF_DIRTY_PAGES_WRITE_COUNT + 2],
        buf[PERF_DIRTY_PAGES_WRITE_COUNT + 3],
    ]);
    let Some((prev_t, prev_read, prev_dirty)) = last.replace((now, page_read, dirty_write)) else {
        return neutral("waiting for a second sample");
    };
    paging_rate(
        (prev_read, prev_dirty),
        (page_read, dirty_write),
        now.saturating_duration_since(prev_t),
    )
    .map_or_else(|| neutral("waiting for a second sample"), Ok)
}

/// Taxa de páginas lidas/gravadas por segundo entre duas amostras (`u32` com volta em uptime longo).
fn paging_rate(prev: (u32, u32), now: (u32, u32), dt: Duration) -> Option<(f64, f64)> {
    let secs = dt.as_secs_f64();
    if secs <= 0.0 {
        return None;
    }
    let pin = f64::from(now.0.wrapping_sub(prev.0)) / secs;
    let pout = f64::from(now.1.wrapping_sub(prev.1)) / secs;
    Some((pin, pout))
}

/// RAM física instalada pelo SMBIOS (`GetPhysicallyInstalledSystemMemory`), em bytes.
fn read_installed_ram() -> Metric<u64> {
    let mut kb = 0u64;
    // SAFETY: ponteiro local válido.
    match unsafe { GetPhysicallyInstalledSystemMemory(&raw mut kb) } {
        Ok(()) if kb > 0 => Ok(kb.saturating_mul(1024)),
        Ok(()) => neutral("not reported by firmware"),
        Err(e) if e.code().0 as u32 == 0x8007_000D => neutral("not reported by firmware"),
        Err(e) => fail(format!("GetPhysicallyInstalledSystemMemory: {e}")),
    }
}

/// Lê a tabela SMBIOS bruta (`RSMB`) e extrai o hardware de RAM (Type 17).
fn read_hardware() -> Metric<MemoryHardware> {
    // SAFETY: consulta de tamanho (buffer vazio) sem efeitos colaterais.
    let len = unsafe { GetSystemFirmwareTable(RSMB, 0, None) } as usize;
    if len == 0 {
        return neutral("not reported by firmware");
    }
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` tem o tamanho pedido pelo firmware.
    let got = unsafe { GetSystemFirmwareTable(RSMB, 0, Some(&mut buf)) } as usize;
    if got == 0 {
        return neutral("not reported by firmware");
    }
    buf.truncate(got);
    parse_smbios_memory(&buf)
}

/// Decodifica estruturas SMBIOS Type 17 (Memory Device) a partir de um buffer `RawSMBIOSData`.
fn parse_smbios_memory(raw: &[u8]) -> Metric<MemoryHardware> {
    // Cabeçalho `RawSMBIOSData`: 4 bytes de versão/revisão + 4 bytes de `Length`.
    if raw.len() < 8 {
        return neutral("not reported by firmware");
    }
    let data_len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let end = raw.len().min(8usize.saturating_add(data_len));
    let mut pos = 8usize;
    let mut slots_total = 0u32;
    let mut slots_used = 0u32;
    let mut max_speed = 0u32;
    let mut cfg_speed = 0u32;
    let mut kind: &'static str = "RAM";
    let mut form_factor: Option<&'static str> = None;

    while pos + 4 <= end {
        let typ = raw[pos];
        let hlen = usize::from(raw[pos + 1]);
        if hlen < 4 || pos + hlen > end {
            break;
        }
        let t = &raw[pos..pos + hlen];
        if typ == 17 && hlen >= 0x10 {
            slots_total += 1;
            let size_raw = u16::from_le_bytes([t[0x0C], t[0x0D]]);
            let populated = if size_raw == 0x7FFF {
                hlen >= 0x20 && u32::from_le_bytes([t[0x1C], t[0x1D], t[0x1E], t[0x1F]]) > 0
            } else {
                size_raw != 0 && size_raw != 0xFFFF
            };
            if populated {
                slots_used += 1;
                if let Some(ff) = smbios_form_factor(t[0x0E]) {
                    form_factor = Some(ff);
                }
                if hlen > 0x12
                    && let Some(k) = smbios_mem_type(t[0x12])
                {
                    kind = k;
                }
                if hlen >= 0x17 {
                    let mut spd = u32::from(u16::from_le_bytes([t[0x15], t[0x16]]));
                    if spd == 0xFFFF && hlen >= 0x58 {
                        spd = u32::from_le_bytes([t[0x54], t[0x55], t[0x56], t[0x57]]);
                    }
                    if spd != 0 && spd != 0xFFFF {
                        max_speed = max_speed.max(spd);
                    }
                }
                if hlen >= 0x22 {
                    let mut cspd = u32::from(u16::from_le_bytes([t[0x20], t[0x21]]));
                    if cspd == 0xFFFF && hlen >= 0x5C {
                        cspd = u32::from_le_bytes([t[0x58], t[0x59], t[0x5A], t[0x5B]]);
                    }
                    if cspd != 0 && cspd != 0xFFFF {
                        cfg_speed = cfg_speed.max(cspd);
                    }
                }
            }
        }
        // Avança sobre a área de strings terminada em `\0\0`.
        pos += hlen;
        while pos + 1 < end && !(raw[pos] == 0 && raw[pos + 1] == 0) {
            pos += 1;
        }
        if pos + 2 > end {
            break;
        }
        pos += 2;
        // Type 127 = End-of-Table.
        if typ == 127 {
            break;
        }
    }

    if slots_used == 0 {
        return neutral("not reported by firmware");
    }
    let speed_mt = if cfg_speed > 0 {
        Some(cfg_speed)
    } else if max_speed > 0 {
        Some(max_speed)
    } else {
        None
    };
    Ok(MemoryHardware {
        kind,
        speed_mt,
        slots_used,
        slots_total,
        form_factor,
    })
}

fn smbios_mem_type(code: u8) -> Option<&'static str> {
    Some(match code {
        0x12 => "DDR",
        0x13 => "DDR2",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        _ => return None,
    })
}

fn smbios_form_factor(code: u8) -> Option<&'static str> {
    Some(match code {
        0x09 => "DIMM",
        0x0C => "SODIMM",
        0x0D => "SRIMM",
        0x0F => "FB-DIMM",
        0x10 => "Die",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type17_entry(size_mb: u16, form: u8, mem_type: u8, speed: u16, cfg_speed: u16) -> Vec<u8> {
        let mut e = vec![0u8; 0x22];
        e[0] = 17;
        e[1] = 0x22;
        e[0x0C..0x0E].copy_from_slice(&size_mb.to_le_bytes());
        e[0x0E] = form;
        e[0x12] = mem_type;
        e[0x15..0x17].copy_from_slice(&speed.to_le_bytes());
        e[0x20..0x22].copy_from_slice(&cfg_speed.to_le_bytes());
        e.extend_from_slice(&[0, 0]);
        e
    }

    fn pagefile_entry(next: u32, total_pages: u32, in_use_pages: u32) -> Vec<u8> {
        let mut e = vec![0u8; 32];
        e[PF_NEXT_OFFSET..PF_NEXT_OFFSET + 4].copy_from_slice(&next.to_le_bytes());
        e[PF_TOTAL_SIZE..PF_TOTAL_SIZE + 4].copy_from_slice(&total_pages.to_le_bytes());
        e[PF_TOTAL_IN_USE..PF_TOTAL_IN_USE + 4].copy_from_slice(&in_use_pages.to_le_bytes());
        e
    }

    #[test]
    fn estimativa_descomprimida_respeita_limites() {
        assert_eq!(estimate_uncompressed(0, 6000, 2000), 0);
        assert_eq!(estimate_uncompressed(1000, 0, 2000), 1000);
        assert_eq!(estimate_uncompressed(1000, 3000, 0), 1000);
        // Razão 3:1 -> 1800 MB em RAM representam ~5400 MB descomprimidos.
        assert_eq!(estimate_uncompressed(1800, 6000, 2000), 5400);
        // Dados incompressíveis nunca retornam menos que o tamanho em RAM.
        assert_eq!(estimate_uncompressed(1000, 900, 1000), 1000);
    }

    #[test]
    fn decodifica_pagefiles_e_tolera_lixo() {
        let mut buf = pagefile_entry(32, 1000, 250);
        buf.extend(pagefile_entry(0, 500, 100));
        assert_eq!(parse_pagefiles(&buf, 4096), Ok((350 * 4096, 1500 * 4096)));

        // Sem pagefile -> neutro.
        assert!(matches!(
            parse_pagefiles(&[], 4096),
            Err(crate::model::Missing::Neutral(_))
        ));
        // `next` menor que o mínimo não entra em laço infinito.
        let bad = pagefile_entry(4, 100, 10);
        assert_eq!(parse_pagefiles(&bad, 4096), Ok((10 * 4096, 100 * 4096)));
        // `in_use > total` é limitado ao total.
        let clamped = pagefile_entry(0, 100, 200);
        assert_eq!(
            parse_pagefiles(&clamped, 4096),
            Ok((100 * 4096, 100 * 4096))
        );
    }

    #[test]
    fn taxa_de_paginacao_com_volta_u32() {
        assert_eq!(
            paging_rate((100, 20), (200, 70), Duration::from_secs(2)),
            Some((50.0, 25.0))
        );
        // Volta do u32 em uptime longo.
        assert_eq!(
            paging_rate((u32::MAX - 9, 0), (10, 0), Duration::from_secs(1)),
            Some((20.0, 0.0))
        );
        assert_eq!(paging_rate((0, 0), (10, 10), Duration::ZERO), None);
    }

    #[test]
    fn decodifica_smbios_type17_e_tolera_lixo() {
        let mut body = Vec::new();
        body.extend(type17_entry(8192, 0x09, 0x1A, 3200, 3600));
        body.extend(type17_entry(8192, 0x09, 0x1A, 3200, 3600));
        // Slot vazio (size = 0) conta no total mas não nos ocupados.
        body.extend(type17_entry(0, 0x09, 0x02, 0, 0));
        body.extend(type17_entry(0, 0x09, 0x02, 0, 0));
        let mut raw = vec![0, 3, 2, 0];
        raw.extend_from_slice(&(body.len() as u32).to_le_bytes());
        raw.extend(body);

        let hw = parse_smbios_memory(&raw).expect("SMBIOS válido");
        assert_eq!(
            hw,
            MemoryHardware {
                kind: "DDR4",
                speed_mt: Some(3600),
                slots_used: 2,
                slots_total: 4,
                form_factor: Some("DIMM"),
            }
        );

        // Buffer truncado ou vazio não entra em pânico.
        assert!(parse_smbios_memory(&[]).is_err());
        assert!(parse_smbios_memory(&raw[..12]).is_err());
    }

    #[test]
    fn fuzz_smbios_e_pagefiles() {
        let mut smbios_body = type17_entry(8192, 0x09, 0x1A, 3200, 3600);
        smbios_body.extend(type17_entry(0x7FFF, 0x0C, 0x22, 0xFFFF, 0xFFFF));
        let mut smbios_seed = vec![0, 3, 2, 0];
        smbios_seed.extend_from_slice(&(smbios_body.len() as u32).to_le_bytes());
        smbios_seed.extend(smbios_body);

        let mut pf_seed = pagefile_entry(32, 1000, 250);
        pf_seed.extend(pagefile_entry(0, 500, 100));

        crate::fuzz::run(
            "smbios_e_pagefiles",
            &[smbios_seed, pf_seed],
            4096,
            |_, b| {
                if let Ok(hw) = parse_smbios_memory(b) {
                    assert!(hw.slots_used > 0 && hw.slots_used <= hw.slots_total);
                }
                if let Ok((used, total)) = parse_pagefiles(b, 4096) {
                    assert!(used <= total);
                }
            },
        );
    }

    #[test]
    fn leituras_reais_nt_de_memoria_nao_falham() {
        let mut c = MemoryCollector::new();
        let pi = super::super::perf_info();
        let m1 = c.read(pi.as_ref());
        std::thread::sleep(Duration::from_millis(50));
        let pi = super::super::perf_info();
        let m2 = c.read(pi.as_ref());

        assert!(m2.total.is_ok(), "{:?}", m2.total);
        assert!(m2.standby.is_ok(), "{:?}", m2.standby);
        assert!(m2.modified.is_ok(), "{:?}", m2.modified);
        assert!(m2.cached.is_ok(), "{:?}", m2.cached);
        assert!(
            m2.pagefile_usage.is_ok()
                || matches!(m2.pagefile_usage, Err(crate::model::Missing::Neutral(_))),
            "{:?}",
            m2.pagefile_usage
        );
        // 1ª amostra de paging é neutra (aquecimento); 2ª já traz taxa finita >= 0.
        assert!(
            matches!(m1.paging, Err(crate::model::Missing::Neutral(_))),
            "{:?}",
            m1.paging
        );
        let (pin, pout) = m2.paging.expect("2ª amostra de paginação");
        assert!(pin.is_finite() && pin >= 0.0 && pout.is_finite() && pout >= 0.0);
        assert!(
            m2.compressed.is_ok()
                || matches!(m2.compressed, Err(crate::model::Missing::Neutral(_))),
            "{:?}",
            m2.compressed
        );
        assert!(
            m2.hardware.is_ok() || matches!(m2.hardware, Err(crate::model::Missing::Neutral(_))),
            "{:?}",
            m2.hardware
        );
        assert!(
            m2.bad_pages.is_ok() || matches!(m2.bad_pages, Err(crate::model::Missing::Neutral(_))),
            "{:?}",
            m2.bad_pages
        );
    }

    #[test]
    fn status_de_compressao_desativada_vira_neutro() {
        use crate::model::Missing;
        for code in [
            STATUS_QUOTA_NOT_ENABLED,
            STATUS_NOT_FOUND,
            STATUS_COMPRESSION_DISABLED,
        ] {
            assert_eq!(map_store_status(code), neutral("disabled"));
        }
        assert_eq!(
            map_store_status(STATUS_NOT_SUPPORTED),
            neutral("not supported by Windows")
        );
        assert_eq!(
            map_store_status(STATUS_ACCESS_DENIED),
            neutral("needs elevation")
        );
        assert!(matches!(
            map_store_status(0xC000_0001),
            Err(Missing::Fail(_))
        ));
    }
}
