//! Spike da Etapa 0: inventário e temperatura de todos os discos físicos.
//!
//! - fabricante, modelo, barramento, SSD/HDD e capacidade: `IOCTL_STORAGE_QUERY_PROPERTY`,
//!   `IOCTL_DISK_GET_LENGTH_INFO`;
//! - membros de Storage Spaces: partição GPT do tipo "Storage Spaces protective"
//!   (`IOCTL_DISK_GET_DRIVE_LAYOUT_EX`), sem WMI;
//! - repouso: `GetDevicePowerState`, consultado ANTES de qualquer leitura de temperatura (um
//!   disco em repouso não é consultado, para não acordá-lo);
//! - temperatura NVMe: `StorageDeviceTemperatureProperty`; SATA: SMART via
//!   `IOCTL_STORAGE_PREDICT_FAILURE` (atributo 194, senão 190);
//! - Storage Space: máximo dos membros.
//!
//! Offsets das estruturas conferidos com o compilador MSVC (winioctl.h).
//! Exige elevação. `cargo run --example spike_discos`

#[path = "common/mod.rs"]
mod common;

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::time::Instant;

use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_PREDICT_FAILURE,
    IOCTL_STORAGE_QUERY_PROPERTY,
};
use windows::Win32::System::Power::GetDevicePowerState;
use windows::core::{BOOL, PCWSTR};

const PROP_DEVICE: u32 = 0; // StorageDeviceProperty
const PROP_SEEK_PENALTY: u32 = 7; // StorageDeviceSeekPenaltyProperty
const PROP_TEMPERATURE: u32 = 52; // StorageDeviceTemperatureProperty
const BUS_SPACES: u32 = 16;
const TEMP_NOT_REPORTED: i16 = i16::MIN; // STORAGE_TEMPERATURE_VALUE_NOT_REPORTED

// STORAGE_DEVICE_DESCRIPTOR
const D_VENDOR_OFF: usize = 12;
const D_PRODUCT_OFF: usize = 16;
const D_BUS: usize = 28;
// STORAGE_TEMPERATURE_DATA_DESCRIPTOR / STORAGE_TEMPERATURE_INFO
const T_INFO_COUNT: usize = 12;
const T_INFO: usize = 24;
const T_INFO_SIZE: usize = 16;
const T_TEMP: usize = 2;
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

struct Disk(OwnedHandle);

impl Disk {
    fn open(n: u32, access: u32) -> Option<Self> {
        let path = common::wide(&format!(r"\\.\PhysicalDrive{n}"));
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
        .map(|h| Self(common::own(h)))
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
        .map_err(|e| format!("{:#x}", e.code().0 & 0xFFFF))?;
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

fn bus_name(bus: u32) -> &'static str {
    match bus {
        7 => "USB",
        8 => "RAID",
        10 => "SAS",
        11 => "SATA",
        14 => "Virtual",
        16 => "Spaces",
        17 => "NVMe",
        _ => "outro",
    }
}

/// Primeira temperatura reportada em STORAGE_TEMPERATURE_DATA_DESCRIPTOR (índice 0 = composta
/// no NVMe), ignorando o sentinela "não reportado".
fn native_temp(b: &[u8]) -> Option<i16> {
    let count = usize::from(u16::from_le_bytes(
        b.get(T_INFO_COUNT..T_INFO_COUNT + 2)?.try_into().ok()?,
    ));
    (0..count).find_map(|i| {
        let off = T_INFO + i * T_INFO_SIZE + T_TEMP;
        let t = i16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?);
        (t != TEMP_NOT_REPORTED).then_some(t)
    })
}

/// Temperatura SMART: tabela de 30 atributos de 12 bytes após 2 bytes de revisão.
fn smart_temp(vendor: &[u8]) -> Option<(u8, u8)> {
    let attr = |want: u8| {
        (0..30).find_map(|i| {
            let b = 2 + i * 12;
            let a = vendor.get(b..b + 12)?;
            (a[0] == want && a[5] > 0).then_some((want, a[5]))
        })
    };
    attr(194).or_else(|| attr(190))
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

struct Info {
    n: u32,
    name: String,
    bus: u32,
    kind: &'static str,
    size_gib: f64,
    member: bool,
    awake: Option<bool>,
    temp: Option<(i16, String)>,
    temp_err: String,
}

fn main() {
    common::harden_dll_search();
    println!("Elevado: {}\n", common::is_elevated());
    let mut disks = Vec::new();
    for n in 0..64 {
        let Some(meta) = Disk::open(n, 0) else {
            continue;
        };
        let desc = meta.property(PROP_DEVICE).unwrap_or_default();
        let bus = u32_at(&desc, D_BUS).unwrap_or(0);
        let vendor = cstr_at(&desc, u32_at(&desc, D_VENDOR_OFF).unwrap_or(0) as usize);
        let product = cstr_at(&desc, u32_at(&desc, D_PRODUCT_OFF).unwrap_or(0) as usize);
        let name = if vendor.is_empty() || product.starts_with(&vendor) {
            product
        } else {
            format!("{vendor} {product}")
        };
        let kind = match meta.property(PROP_SEEK_PENALTY) {
            Ok(b) if b.len() > 8 => {
                if b[8] != 0 {
                    "HDD"
                } else {
                    "SSD"
                }
            }
            _ => "?",
        };
        // Tamanho e layout exigem um handle com leitura (com acesso 0 o IOCTL falha).
        let reader = Disk::open(n, GENERIC_READ.0);
        let size_gib = reader
            .as_ref()
            .and_then(|r| r.ioctl(IOCTL_DISK_GET_LENGTH_INFO, None, 8).ok())
            .and_then(|b| Some(i64::from_le_bytes(b.get(..8)?.try_into().ok()?)))
            .map_or(0.0, |v| v as f64 / f64::from(1u32 << 30));
        let member = reader
            .as_ref()
            .and_then(|r| {
                r.ioctl(IOCTL_DISK_GET_DRIVE_LAYOUT_EX, None, 48 + 128 * P_SIZE)
                    .ok()
            })
            .is_some_and(|l| is_pool_member(&l));

        // Repouso primeiro: em repouso (HDD) não há leitura de temperatura.
        let awake = meta.awake();
        let mut info = Info {
            n,
            name,
            bus,
            kind,
            size_gib,
            member,
            awake,
            temp: None,
            temp_err: String::new(),
        };
        if bus == BUS_SPACES {
            // disco virtual: temperatura agregada depois
        } else if awake == Some(false) && kind == "HDD" {
            info.temp_err = "em repouso (não consultado)".into();
        } else {
            let t = Instant::now();
            let native = meta
                .property(PROP_TEMPERATURE)
                .ok()
                .as_deref()
                .and_then(native_temp);
            if let Some(c) = native {
                info.temp = Some((c, format!("IOCTL nativo, {} µs", t.elapsed().as_micros())));
            } else {
                let t = Instant::now();
                let smart = Disk::open(n, GENERIC_READ.0 | GENERIC_WRITE.0).and_then(|rw| {
                    // STORAGE_PREDICT_FAILURE { ULONG PredictFailure; UCHAR VendorSpecific[512]; }
                    let b = rw.ioctl(IOCTL_STORAGE_PREDICT_FAILURE, None, 516).ok()?;
                    smart_temp(b.get(4..)?)
                });
                match smart {
                    Some((id, c)) => {
                        info.temp = Some((
                            i16::from(c),
                            format!("SMART {id}, {} µs", t.elapsed().as_micros()),
                        ));
                    }
                    None => info.temp_err = "FALHA".into(),
                }
            }
        }
        disks.push(info);
    }

    // Storage Space: máximo dos membros (com um pool; vários pools exigem o ID do pool).
    let pools = disks.iter().filter(|d| d.bus == BUS_SPACES).count();
    let member_max = disks
        .iter()
        .filter(|d| d.member)
        .filter_map(|d| d.temp.as_ref().map(|t| t.0))
        .max();
    let members: Vec<u32> = disks.iter().filter(|d| d.member).map(|d| d.n).collect();

    for d in &disks {
        let power = match d.awake {
            Some(true) => "ativo",
            Some(false) => "repouso",
            None => "?",
        };
        let temp = match (&d.temp, d.bus == BUS_SPACES) {
            (Some((c, how)), _) => format!("{c} °C ({how})"),
            (None, true) if pools == 1 => match member_max {
                Some(c) => format!("{c} °C (máximo dos membros {members:?})"),
                None => "FALHA (membros sem temperatura)".into(),
            },
            (None, true) => "vários pools: associação por pool pendente".into(),
            (None, false) => d.temp_err.clone(),
        };
        println!(
            "Disk {}: {:<30} {:<6} {:<3} {:>7.1} GB  {power:<7} {:<13} temp {temp}",
            d.n,
            d.name,
            bus_name(d.bus),
            d.kind,
            d.size_gib,
            if d.member { "membro de pool" } else { "" },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperatura_nativa_ignora_nao_reportado() {
        let mut b = vec![0u8; T_INFO + 2 * T_INFO_SIZE];
        b[T_INFO_COUNT..T_INFO_COUNT + 2].copy_from_slice(&2u16.to_le_bytes());
        b[T_INFO + T_TEMP..T_INFO + T_TEMP + 2].copy_from_slice(&TEMP_NOT_REPORTED.to_le_bytes());
        let second = T_INFO + T_INFO_SIZE + T_TEMP;
        b[second..second + 2].copy_from_slice(&41i16.to_le_bytes());
        assert_eq!(native_temp(&b), Some(41));
        b[T_INFO_COUNT..T_INFO_COUNT + 2].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(native_temp(&b), None);
    }

    #[test]
    fn smart_prefere_194() {
        let mut v = vec![0u8; 2 + 30 * 12];
        v[2] = 190;
        v[2 + 5] = 30;
        v[2 + 12] = 194;
        v[2 + 12 + 5] = 35;
        assert_eq!(smart_temp(&v), Some((194, 35)));
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
}
