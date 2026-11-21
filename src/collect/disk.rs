//! Discos físicos: inventário (modelo, barramento, SSD/HDD, capacidade, membro de pool) e
//! atividade pelos contadores do driver (`IOCTL_DISK_PERFORMANCE`, a mesma fonte do PDH
//! `\PhysicalDisk(*)`, sem os ~54 MB de memória privada que o provedor do PDH reserva). Letras
//! das unidades pelas extensões de cada volume. As temperaturas entram na Etapa 2 (thread lenta).
//!
//! Offsets das estruturas conferidos com o compilador MSVC (winioctl.h) na Etapa 0.

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    GetLogicalDrives, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_DISK_PERFORMANCE,
    IOCTL_STORAGE_QUERY_PROPERTY,
};
use windows::Win32::System::Power::GetDevicePowerState;
use windows::core::{BOOL, PCWSTR};

use crate::model::{Bus, Disk, Metric, fail, neutral};
use crate::win::{self, own, wide};

const PROP_DEVICE: u32 = 0; // StorageDeviceProperty
const PROP_SEEK_PENALTY: u32 = 7; // StorageDeviceSeekPenaltyProperty

// STORAGE_DEVICE_DESCRIPTOR
const D_VENDOR_OFF: usize = 12;
const D_PRODUCT_OFF: usize = 16;
const D_BUS: usize = 28;
// DRIVE_LAYOUT_INFORMATION_EX / PARTITION_INFORMATION_EX
const L_STYLE: usize = 0;
const L_COUNT: usize = 4;
const L_ENTRIES: usize = 48;
const P_SIZE: usize = 144;
const P_GPT_TYPE: usize = 32;
const STYLE_GPT: u32 = 1;
/// GUID E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D (Storage Spaces protective), em bytes.
const SPACES_PROTECTIVE: [u8; 16] = [
    0x8F, 0xAF, 0x5C, 0xE7, 0x80, 0xF6, 0xEE, 0x4C, 0xAF, 0xA3, 0xB0, 0x01, 0xE5, 0x6E, 0xFC, 0x2D,
];
const PAGEFILES_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management";

/// Nova tentativa de ler capacidade e layout de um HDD que estava em repouso.
const RETRY_ASLEEP: Duration = Duration::from_secs(60);

struct Handle(OwnedHandle);

impl Handle {
    fn open(n: u32, access: u32) -> Option<Self> {
        Self::open_path(&format!(r"\\.\PhysicalDrive{n}"), access)
    }

    fn open_path(path: &str, access: u32) -> Option<Self> {
        let path = wide(path);
        // SAFETY: caminho terminado em zero; o handle passa a ter dono.
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .ok()
        // SAFETY: CreateFileW teve sucesso; handle válido e sem outro dono.
        .map(|h| Self(unsafe { own(h) }))
    }

    fn raw(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }

    fn ioctl(&self, code: u32, input: Option<&[u8]>, out_len: usize) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; out_len];
        let mut ret = 0u32;
        // SAFETY: buffers locais com os tamanhos informados; chamada síncrona.
        unsafe {
            DeviceIoControl(
                self.raw(),
                code,
                input.map(|i| i.as_ptr().cast()),
                input.map_or(0, |i| i.len() as u32),
                Some(out.as_mut_ptr().cast()),
                out.len() as u32,
                Some(&raw mut ret),
                None,
            )
        }
        .map_err(|e| format!("IOCTL 0x{:X}: error 0x{:X}", code, e.code().0 & 0xFFFF))?;
        out.truncate(ret as usize);
        Ok(out)
    }

    /// STORAGE_PROPERTY_QUERY { PropertyId, QueryType = PropertyStandardQuery, ... }.
    fn property(&self, id: u32) -> Result<Vec<u8>, String> {
        let mut q = [0u8; 12];
        q[..4].copy_from_slice(&id.to_le_bytes());
        self.ioctl(IOCTL_STORAGE_QUERY_PROPERTY, Some(&q), 1024)
    }

    /// `Some(true)` acordado, `Some(false)` em repouso, `None` sem resposta.
    fn awake(&self) -> Option<bool> {
        let mut on = BOOL(1);
        // SAFETY: handle válido e ponteiro para BOOL.
        unsafe { GetDevicePowerState(self.raw(), &raw mut on) }
            .as_bool()
            .then(|| on.as_bool())
    }
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn cstr_at(b: &[u8], off: usize) -> String {
    if off == 0 || off >= b.len() {
        return String::new();
    }
    let end = b[off..]
        .iter()
        .position(|&c| c == 0)
        .map_or(b.len(), |p| off + p);
    String::from_utf8_lossy(&b[off..end]).trim().to_string()
}

/// Fabricante + modelo do `STORAGE_DEVICE_DESCRIPTOR` (sem repetir o fabricante).
fn model_name(desc: &[u8]) -> Option<String> {
    let vendor = cstr_at(desc, u32_at(desc, D_VENDOR_OFF)? as usize);
    let product = cstr_at(desc, u32_at(desc, D_PRODUCT_OFF)? as usize);
    let name = if vendor.is_empty() || product.starts_with(&vendor) {
        product
    } else {
        format!("{vendor} {product}")
    };
    (!name.is_empty()).then_some(name)
}

/// `true` se o layout GPT tem uma partição "Storage Spaces protective".
fn is_pool_member(layout: &[u8]) -> bool {
    if u32_at(layout, L_STYLE) != Some(STYLE_GPT) {
        return false;
    }
    let count = u32_at(layout, L_COUNT).unwrap_or(0) as usize;
    (0..count).any(|i| {
        let t = L_ENTRIES + i * P_SIZE + P_GPT_TYPE;
        layout.get(t..t + 16) == Some(&SPACES_PROTECTIVE[..])
    })
}

// DISK_PERFORMANCE (winioctl.h): tempos e QueryTime em unidades de 100 ns.
const P_BYTES_READ: usize = 0;
const P_BYTES_WRITTEN: usize = 8;
const P_READ_TIME: usize = 16;
const P_WRITE_TIME: usize = 24;
const P_IDLE_TIME: usize = 32;
const P_READ_COUNT: usize = 40;
const P_WRITE_COUNT: usize = 44;
const P_QUERY_TIME: usize = 56;
const PERF_SIZE: usize = 88;
// VOLUME_DISK_EXTENTS: contagem u32, extensões a partir do byte 8 (DISK_EXTENT de 24 bytes,
// DiskNumber u32 no início).
const X_COUNT: usize = 0;
const X_ENTRIES: usize = 8;
const X_SIZE: usize = 24;
const MAX_EXTENTS: usize = 32;
// GetDriveTypeW
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;

/// Redescoberta de discos (hot-plug) e das letras das unidades.
const REDISCOVER: Duration = Duration::from_secs(30);

/// Contadores brutos do `IOCTL_DISK_PERFORMANCE` (os mesmos de onde o PDH calcula
/// `\PhysicalDisk(*)`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PerfSample {
    bytes_read: u64,
    bytes_written: u64,
    read_time: u64,
    write_time: u64,
    idle_time: u64,
    reads: u32,
    writes: u32,
    query_time: u64,
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(off..off.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn parse_perf(b: &[u8]) -> Option<PerfSample> {
    Some(PerfSample {
        bytes_read: u64_at(b, P_BYTES_READ)?,
        bytes_written: u64_at(b, P_BYTES_WRITTEN)?,
        read_time: u64_at(b, P_READ_TIME)?,
        write_time: u64_at(b, P_WRITE_TIME)?,
        idle_time: u64_at(b, P_IDLE_TIME)?,
        reads: u32_at(b, P_READ_COUNT)?,
        writes: u32_at(b, P_WRITE_COUNT)?,
        query_time: u64_at(b, P_QUERY_TIME)?,
    })
}

/// Atividade entre duas amostras: (tempo ativo %, leitura B/s, escrita B/s, resposta média s).
/// Igual ao PDH: ativo = 100 - ocioso/decorrido; resposta = tempo das transferências / número
/// de transferências (0 sem transferências). `None` se algum contador voltou (disco reiniciado).
fn activity(prev: &PerfSample, now: &PerfSample) -> Option<(f64, f64, f64, f64)> {
    let dt = now.query_time.checked_sub(prev.query_time)?;
    if dt == 0 {
        return None;
    }
    let secs = dt as f64 / 1e7;
    let idle = now.idle_time.checked_sub(prev.idle_time)?;
    let read = now.bytes_read.checked_sub(prev.bytes_read)?;
    let written = now.bytes_written.checked_sub(prev.bytes_written)?;
    let busy =
        now.read_time.checked_sub(prev.read_time)? + now.write_time.checked_sub(prev.write_time)?;
    // Contagens de 32 bits: a volta é normal em uptime longo.
    let transfers = u64::from(now.reads.wrapping_sub(prev.reads))
        + u64::from(now.writes.wrapping_sub(prev.writes));
    let active = (100.0 - idle as f64 / dt as f64 * 100.0).clamp(0.0, 100.0);
    let response = if transfers == 0 {
        0.0
    } else {
        busy as f64 / 1e7 / transfers as f64
    };
    Some((active, read as f64 / secs, written as f64 / secs, response))
}

/// Discos de um `VOLUME_DISK_EXTENTS`.
fn parse_extents(b: &[u8]) -> Vec<u32> {
    let count = (u32_at(b, X_COUNT).unwrap_or(0) as usize).min(MAX_EXTENTS);
    let mut disks: Vec<u32> = (0..count)
        .filter_map(|i| u32_at(b, X_ENTRIES + i * X_SIZE))
        .collect();
    disks.sort_unstable();
    disks.dedup();
    disks
}

/// Letras das unidades locais (fixas e removíveis) por disco físico: `(disco, "C:")`.
/// Unidades de rede e de CD ficam de fora (abri-las pode travar ou não ter disco).
fn volume_letters() -> Vec<(u32, String)> {
    // SAFETY: sem pré-condições.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = char::from(b'A' + i);
        let root = wide(&format!(r"{letter}:\"));
        // SAFETY: caminho terminado em zero.
        let kind = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
        if kind != DRIVE_FIXED && kind != DRIVE_REMOVABLE {
            continue;
        }
        let Some(vol) = Handle::open_path(&format!(r"\\.\{letter}:"), 0) else {
            continue;
        };
        let Ok(b) = vol.ioctl(
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            None,
            X_ENTRIES + MAX_EXTENTS * X_SIZE,
        ) else {
            continue;
        };
        out.extend(
            parse_extents(&b)
                .into_iter()
                .map(|d| (d, format!("{letter}:"))),
        );
    }
    out
}

/// Inventário de um disco (lido na descoberta; capacidade e layout de novo se estava em repouso).
struct Inventory {
    number: u32,
    model: Metric<String>,
    bus: Metric<Bus>,
    ssd: Metric<bool>,
    capacity: Metric<u64>,
    pool_member: Metric<bool>,
    asleep_since: Option<Instant>,
    /// Handle de metadados mantido aberto; USB não (não impedir a remoção segura).
    handle: Option<Handle>,
    last: Option<PerfSample>,
    /// A última leitura de desempenho falhou (disco removido?).
    lost: bool,
}

impl Inventory {
    fn discover(n: u32) -> Option<Self> {
        let meta = Handle::open(n, 0)?;
        let desc = meta.property(PROP_DEVICE);
        let model = desc
            .as_deref()
            .map_err(Clone::clone)
            .and_then(|d| model_name(d).ok_or_else(|| "descriptor has no model".into()))
            .or_else(fail);
        let bus = desc
            .as_deref()
            .map_err(Clone::clone)
            .and_then(|d| u32_at(d, D_BUS).ok_or_else(|| "descriptor too short".into()))
            .map(Bus::from_raw)
            .or_else(fail);
        let ssd = match meta.property(PROP_SEEK_PENALTY) {
            Ok(b) if b.len() > 8 => Ok(b[8] == 0),
            Ok(_) => fail("response too short"),
            Err(e) => fail(e),
        };
        let mut inv = Self {
            number: n,
            model,
            bus,
            ssd,
            capacity: fail("not read"),
            pool_member: fail("not read"),
            asleep_since: None,
            handle: None,
            last: None,
            lost: false,
        };
        inv.read_details(&meta);
        if inv.bus != Ok(Bus::Usb) {
            inv.handle = Some(meta);
        }
        Some(inv)
    }

    /// Capacidade e layout exigem um handle com leitura. Um HDD em repouso não é aberto para
    /// leitura (para não acordá-lo): tenta de novo depois.
    fn read_details(&mut self, meta: &Handle) {
        let hdd = self.ssd == Ok(false);
        if hdd && meta.awake() == Some(false) {
            self.capacity = neutral("asleep");
            self.pool_member = neutral("asleep");
            self.asleep_since = Some(Instant::now());
            return;
        }
        self.asleep_since = None;
        let Some(reader) = Handle::open(self.number, GENERIC_READ.0) else {
            self.capacity = fail("no read access (not elevated?)");
            self.pool_member = fail("no read access (not elevated?)");
            return;
        };
        self.capacity = reader
            .ioctl(IOCTL_DISK_GET_LENGTH_INFO, None, 8)
            .and_then(|b| u64_at(&b, 0).ok_or_else(|| "response too short".into()))
            .or_else(fail);
        self.pool_member = reader
            .ioctl(
                IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
                None,
                L_ENTRIES + 128 * P_SIZE,
            )
            .map(|l| is_pool_member(&l))
            .or_else(fail);
    }

    fn retry_if_asleep(&mut self) {
        if self
            .asleep_since
            .is_some_and(|t| t.elapsed() >= RETRY_ASLEEP)
            && let Some(meta) = Handle::open(self.number, 0)
        {
            self.read_details(&meta);
        }
    }

    /// Atividade desde a última leitura. Não acorda o disco: os contadores são do driver.
    fn activity(&mut self) -> Metric<(f64, f64, f64, f64)> {
        let temp;
        let h = if let Some(h) = &self.handle {
            h
        } else {
            temp = Handle::open(self.number, 0);
            let Some(h) = &temp else {
                self.lost = true;
                return fail("disk not found");
            };
            h
        };
        let now = match h.ioctl(IOCTL_DISK_PERFORMANCE, None, PERF_SIZE) {
            Ok(b) => parse_perf(&b),
            Err(e) => {
                self.lost = true;
                return fail(e);
            }
        };
        self.lost = false;
        let Some(now) = now else {
            return fail("response too short");
        };
        match self.last.replace(now) {
            None => fail("waiting for a second sample"),
            Some(prev) => activity(&prev, &now)
                .map_or_else(|| fail("counters went backwards (disk reset)"), Ok),
        }
    }
}

pub struct DiskCollector {
    disks: Vec<Inventory>,
    letters: Vec<(u32, String)>,
    discovered: Instant,
    system_drive: Option<char>,
    pagefile_drives: Vec<char>,
}

impl DiskCollector {
    pub fn new() -> Self {
        let pagefile_drives = win::reg_multi_sz(PAGEFILES_KEY, "ExistingPageFiles")
            .unwrap_or_default()
            .iter()
            .filter_map(|p| win::drive_letter(p))
            .collect();
        Self {
            disks: (0..64).filter_map(Inventory::discover).collect(),
            letters: volume_letters(),
            discovered: Instant::now(),
            system_drive: win::windows_drive(),
            pagefile_drives,
        }
    }

    /// Discos novos entram, discos que sumiram saem; letras relidas.
    fn rediscover(&mut self) {
        self.discovered = Instant::now();
        // Disco perdido sai; se o número voltar a existir, é redescoberto do zero (handle e
        // metadados novos: pode ser outro disco).
        self.disks.retain(|d| !d.lost);
        for n in 0..64 {
            if !self.disks.iter().any(|d| d.number == n)
                && let Some(inv) = Inventory::discover(n)
            {
                self.disks.push(inv);
            }
        }
        self.disks.sort_by_key(|d| d.number);
        self.letters = volume_letters();
    }

    pub fn read(&mut self) -> Metric<Vec<Disk>> {
        if self.discovered.elapsed() >= REDISCOVER {
            self.rediscover();
        }
        if self.disks.is_empty() {
            return fail("no physical disk could be opened");
        }
        let has = |letters: &[String], c: char| {
            letters
                .iter()
                .any(|l| l.starts_with(c) && l.len() == 2 && l.ends_with(':'))
        };
        let mut out = Vec::with_capacity(self.disks.len());
        for inv in &mut self.disks {
            inv.retry_if_asleep();
            let act = inv.activity();
            let letters: Vec<String> = self
                .letters
                .iter()
                .filter(|(n, _)| *n == inv.number)
                .map(|(_, l)| l.clone())
                .collect();
            out.push(Disk {
                number: inv.number,
                model: inv.model.clone(),
                bus: inv.bus.clone(),
                ssd: inv.ssd.clone(),
                capacity: inv.capacity.clone(),
                pool_member: inv.pool_member.clone(),
                system: self.system_drive.is_some_and(|c| has(&letters, c)),
                pagefile: self.pagefile_drives.iter().any(|&c| has(&letters, c)),
                letters,
                active: act.clone().map(|a| a.0),
                read: act.clone().map(|a| a.1),
                write: act.clone().map(|a| a.2),
                response: act.map(|a| a.3),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perf(read: u64, idle: u64, reads: u32, time: u64, query: u64) -> Vec<u8> {
        let mut b = vec![0u8; PERF_SIZE];
        b[P_BYTES_READ..P_BYTES_READ + 8].copy_from_slice(&read.to_le_bytes());
        b[P_READ_TIME..P_READ_TIME + 8].copy_from_slice(&time.to_le_bytes());
        b[P_IDLE_TIME..P_IDLE_TIME + 8].copy_from_slice(&idle.to_le_bytes());
        b[P_READ_COUNT..P_READ_COUNT + 4].copy_from_slice(&reads.to_le_bytes());
        b[P_QUERY_TIME..P_QUERY_TIME + 8].copy_from_slice(&query.to_le_bytes());
        b
    }

    #[test]
    fn atividade_igual_ao_pdh() {
        // 1 s (1e7 × 100 ns), 0,75 s ocioso, 4 MB lidos em 8 leituras de 2 ms cada.
        let a = parse_perf(&perf(0, 0, u32::MAX - 3, 0, 0)).expect("amostra");
        let b = parse_perf(&perf(4 << 20, 7_500_000, 4, 160_000, 10_000_000)).expect("amostra");
        let (active, read, write, resp) = activity(&a, &b).expect("atividade");
        assert!((active - 25.0).abs() < 1e-9);
        assert!((read - f64::from(4u32 << 20)).abs() < 1e-6);
        assert!(write.abs() < 1e-9);
        assert!((resp - 0.002).abs() < 1e-12, "{resp}");
        // Contador voltou: sem atividade; mesmo instante: sem atividade.
        assert_eq!(activity(&b, &a), None);
        assert_eq!(activity(&b, &b), None);
        assert_eq!(parse_perf(&[0u8; 40]), None);
    }

    #[test]
    fn extensoes_do_volume() {
        let mut b = vec![0u8; X_ENTRIES + 3 * X_SIZE];
        b[X_COUNT..X_COUNT + 4].copy_from_slice(&3u32.to_le_bytes());
        for (i, d) in [5u32, 2, 5].iter().enumerate() {
            let o = X_ENTRIES + i * X_SIZE;
            b[o..o + 4].copy_from_slice(&d.to_le_bytes());
        }
        assert_eq!(parse_extents(&b), vec![2, 5]);
        b[X_COUNT..X_COUNT + 4].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(parse_extents(&b), vec![2, 5]);
    }

    #[test]
    fn membro_de_pool_pela_particao_gpt() {
        let mut l = vec![0u8; L_ENTRIES + 2 * P_SIZE];
        l[L_STYLE..L_STYLE + 4].copy_from_slice(&STYLE_GPT.to_le_bytes());
        l[L_COUNT..L_COUNT + 4].copy_from_slice(&2u32.to_le_bytes());
        assert!(!is_pool_member(&l));
        let t = L_ENTRIES + P_SIZE + P_GPT_TYPE;
        l[t..t + 16].copy_from_slice(&SPACES_PROTECTIVE);
        assert!(is_pool_member(&l));
    }

    #[test]
    fn modelo_sem_repetir_fabricante() {
        let mut d = vec![0u8; 64];
        d[D_VENDOR_OFF..D_VENDOR_OFF + 4].copy_from_slice(&40u32.to_le_bytes());
        d[D_PRODUCT_OFF..D_PRODUCT_OFF + 4].copy_from_slice(&48u32.to_le_bytes());
        d[40..44].copy_from_slice(b"WDC\0");
        d[48..56].copy_from_slice(b"WDC X1  ");
        assert_eq!(model_name(&d).as_deref(), Some("WDC X1"));
        d[48..56].copy_from_slice(b"X2\0\0\0\0\0\0");
        assert_eq!(model_name(&d).as_deref(), Some("WDC X2"));
    }

    /// Compara com o PDH na mesma janela. Rodar elevado, com carga no disco do sistema:
    /// `winsat disk -seq -read -drive c` em outro terminal.
    #[test]
    #[ignore = "hardware: compara com o PDH sob carga (terminal elevado)"]
    fn compara_com_pdh() {
        use crate::pdh::{Query, read_array};
        let c = win::windows_drive().expect("unidade do Windows");
        let n = volume_letters()
            .into_iter()
            .find(|(_, l)| l.starts_with(c))
            .expect("disco do sistema")
            .0;
        let qr = Query::open();
        let q = qr.as_ref().expect("PDH");
        let names = [
            "% Idle Time",
            "Disk Read Bytes/sec",
            "Disk Write Bytes/sec",
            "Avg. Disk sec/Transfer",
        ];
        let slots: Vec<_> = names
            .iter()
            .map(|c| q.add(&format!(r"\PhysicalDisk(*)\{c}")))
            .collect();
        let h = Handle::open(n, 0).expect("disco");
        let sample = || {
            parse_perf(
                &h.ioctl(IOCTL_DISK_PERFORMANCE, None, PERF_SIZE)
                    .expect("IOCTL"),
            )
            .expect("amostra")
        };
        for _ in 0..5 {
            let a = sample();
            q.collect().expect("coleta");
            std::thread::sleep(Duration::from_secs(1));
            let b = sample();
            q.collect().expect("coleta");
            let ours = activity(&a, &b).expect("atividade");
            let pdh: Vec<f64> = slots
                .iter()
                .map(|s| {
                    read_array(&qr, s)
                        .expect("PDH")
                        .into_iter()
                        .find(|(name, _)| name.split_whitespace().next() == Some(&n.to_string()))
                        .expect("instância")
                        .1
                })
                .collect();
            println!(
                "IOCTL: ativo {:6.2}% leit {:12.0} escr {:12.0} resp {:.6} s | PDH: ativo {:6.2}% leit {:12.0} escr {:12.0} resp {:.6} s",
                ours.0,
                ours.1,
                ours.2,
                ours.3,
                100.0 - pdh[0],
                pdh[1],
                pdh[2],
                pdh[3]
            );
        }
    }
}
