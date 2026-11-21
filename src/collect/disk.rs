//! Discos físicos: inventário (modelo, barramento, SSD/HDD, capacidade, membro de pool) e
//! atividade pelo PDH (`\PhysicalDisk(*)`). As temperaturas entram na Etapa 2 (thread lenta).
//!
//! Offsets das estruturas conferidos com o compilador MSVC (winioctl.h) na Etapa 0.

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_QUERY_PROPERTY,
};
use windows::Win32::System::Power::GetDevicePowerState;
use windows::core::{BOOL, PCWSTR};

use crate::model::{Bus, Disk, Metric, fail, neutral};
use crate::pdh::{self, Query, Slot};
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
        let path = wide(&format!(r"\\.\PhysicalDrive{n}"));
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

/// Instância do `\PhysicalDisk(*)`: `"7 C:"` → (7, ["C:"]); `"1"` → (1, []); `_Total` → None.
fn parse_instance(name: &str) -> Option<(u32, Vec<String>)> {
    let mut it = name.split_whitespace();
    let n = it.next()?.parse().ok()?;
    Some((n, it.map(str::to_ascii_uppercase).collect()))
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
        };
        inv.read_details(&meta);
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
            .and_then(|b| {
                b.get(..8)
                    .and_then(|s| s.try_into().ok())
                    .map(u64::from_le_bytes)
                    .ok_or_else(|| "response too short".into())
            })
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
}

/// Valores de um contador por disco: (número, letras, valor).
type PerDisk = Result<Vec<(u32, Vec<String>, f64)>, String>;

pub struct DiskCollector {
    query: Result<Query, String>,
    idle: Slot,
    read: Slot,
    write: Slot,
    response: Slot,
    disks: Vec<Inventory>,
    system_drive: Option<char>,
    pagefile_drives: Vec<char>,
}

impl DiskCollector {
    pub fn new() -> Self {
        let query = Query::open();
        let add = |c: &str| match &query {
            Ok(q) => q.add(&format!(r"\PhysicalDisk(*)\{c}")),
            Err(e) => Err(e.clone()),
        };
        let idle = add("% Idle Time");
        let read = add("Disk Read Bytes/sec");
        let write = add("Disk Write Bytes/sec");
        let response = add("Avg. Disk sec/Transfer");
        let pagefile_drives = win::reg_multi_sz(PAGEFILES_KEY, "ExistingPageFiles")
            .unwrap_or_default()
            .iter()
            .filter_map(|p| win::drive_letter(p))
            .collect();
        Self {
            query,
            idle,
            read,
            write,
            response,
            disks: (0..64).filter_map(Inventory::discover).collect(),
            system_drive: win::windows_drive(),
            pagefile_drives,
        }
    }

    pub fn read(&mut self) -> Metric<Vec<Disk>> {
        if self.disks.is_empty() {
            return fail("no physical disk could be opened");
        }
        for d in &mut self.disks {
            d.retry_if_asleep();
        }
        let sampled = match &self.query {
            Ok(q) => q.collect(),
            Err(e) => Err(e.clone()),
        };
        let arr = |slot: &Slot| -> PerDisk {
            sampled.clone()?;
            Ok(pdh::read_array(slot)?
                .into_iter()
                .filter_map(|(name, v)| parse_instance(&name).map(|(n, l)| (n, l, v)))
                .collect())
        };
        let idle = arr(&self.idle);
        let read = arr(&self.read);
        let write = arr(&self.write);
        let response = arr(&self.response);
        let pick = |a: &PerDisk, n: u32| -> Metric<f64> {
            match a {
                Err(e) => fail(e.clone()),
                Ok(v) => v
                    .iter()
                    .find(|(m, ..)| *m == n)
                    .map_or_else(|| fail("no PDH instance"), |x| Ok(x.2)),
            }
        };
        // Letras: de qualquer contador que tenha respondido (o nome da instância é igual).
        let names: Vec<(u32, Vec<String>)> = [&idle, &read, &write, &response]
            .into_iter()
            .find_map(|a| a.as_ref().ok())
            .map_or_else(Vec::new, |v| {
                v.iter().map(|(n, l, _)| (*n, l.clone())).collect()
            });

        let has = |letters: &[String], c: char| {
            letters
                .iter()
                .any(|l| l.starts_with(c) && l.len() == 2 && l.ends_with(':'))
        };
        let out = self
            .disks
            .iter()
            .map(|inv| {
                let letters = names
                    .iter()
                    .find(|(n, _)| *n == inv.number)
                    .map_or_else(Vec::new, |(_, l)| l.clone());
                Disk {
                    number: inv.number,
                    model: inv.model.clone(),
                    bus: inv.bus.clone(),
                    ssd: inv.ssd.clone(),
                    capacity: inv.capacity.clone(),
                    pool_member: inv.pool_member.clone(),
                    system: self.system_drive.is_some_and(|c| has(&letters, c)),
                    pagefile: self.pagefile_drives.iter().any(|&c| has(&letters, c)),
                    letters,
                    active: pick(&idle, inv.number).map(|v| (100.0 - v).clamp(0.0, 100.0)),
                    read: pick(&read, inv.number),
                    write: pick(&write, inv.number),
                    response: pick(&response, inv.number),
                }
            })
            .collect();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instancia_do_physicaldisk() {
        assert_eq!(parse_instance("7 c:"), Some((7, vec!["C:".to_string()])));
        assert_eq!(
            parse_instance("2 d: e:"),
            Some((2, vec!["D:".to_string(), "E:".to_string()]))
        );
        assert_eq!(parse_instance("1"), Some((1, vec![])));
        assert_eq!(parse_instance("_Total"), None);
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
}
