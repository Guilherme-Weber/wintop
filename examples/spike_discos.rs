//! Spike da Etapa 0: inventário e temperatura de todos os discos físicos.
//!
//! - modelo, barramento, SSD/HDD e capacidade: `IOCTL_STORAGE_QUERY_PROPERTY`,
//!   `IOCTL_DISK_GET_LENGTH_INFO`;
//! - temperatura NVMe: `StorageDeviceTemperatureProperty`;
//! - temperatura SATA: SMART via `IOCTL_STORAGE_PREDICT_FAILURE` (atributo 194, senão 190);
//! - repouso: `GetDevicePowerState` (para não acordar HDD parado).
//!
//! Exige elevação. `cargo run --example spike_discos`

#[path = "common/mod.rs"]
mod common;

use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_PREDICT_FAILURE, IOCTL_STORAGE_QUERY_PROPERTY,
};
use windows::Win32::System::Power::GetDevicePowerState;
use windows::core::{BOOL, PCWSTR};

const PROP_DEVICE: u32 = 0; // StorageDeviceProperty
const PROP_SEEK_PENALTY: u32 = 7; // StorageDeviceSeekPenaltyProperty
const PROP_TEMPERATURE: u32 = 52; // StorageDeviceTemperatureProperty

struct Disk(HANDLE);

impl Disk {
    fn open(n: u32, access: u32) -> Option<Self> {
        let path = common::wide(&format!(r"\\.\PhysicalDrive{n}"));
        // SAFETY: caminho terminado em zero; o handle é fechado em Drop.
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
        .map(Self)
    }

    fn ioctl(&self, code: u32, input: Option<&[u8]>, out_len: usize) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; out_len];
        let mut ret = 0u32;
        // SAFETY: buffers locais com os tamanhos informados; chamada síncrona.
        unsafe {
            DeviceIoControl(
                self.0,
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
}

impl Drop for Disk {
    fn drop(&mut self) {
        // SAFETY: handle obtido em `open`.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
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

/// Temperatura SMART: tabela de 30 atributos de 12 bytes após 2 bytes de revisão.
fn smart_temp(vendor: &[u8]) -> Option<(u8, u8)> {
    let attr = |want: u8| {
        (0..30).find_map(|i| {
            let b = 2 + i * 12;
            let a = vendor.get(b..b + 12)?;
            (a[0] == want).then_some((want, a[5]))
        })
    };
    attr(194).or_else(|| attr(190))
}

fn main() {
    println!("Elevado: {}\n", common::is_elevated());
    for n in 0..32 {
        let Some(meta) = Disk::open(n, 0) else {
            continue;
        };
        let desc = meta.property(PROP_DEVICE).unwrap_or_default();
        let bus = u32_at(&desc, 28).unwrap_or(0);
        let model = cstr_at(&desc, u32_at(&desc, 16).unwrap_or(0) as usize);
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
        // O tamanho exige um handle com leitura (com acesso 0 o IOCTL falha).
        let size_gb = Disk::open(n, GENERIC_READ.0)
            .and_then(|r| r.ioctl(IOCTL_DISK_GET_LENGTH_INFO, None, 8).ok())
            .and_then(|b| Some(i64::from_le_bytes(b.get(..8)?.try_into().ok()?)))
            .map_or(0.0, |v| v as f64 / 1e9);

        let mut awake = BOOL(1);
        // SAFETY: handle válido e ponteiro para BOOL.
        let power_ok = unsafe { GetDevicePowerState(meta.0, &raw mut awake) }.as_bool();
        let power = if !power_ok {
            "?"
        } else if awake.as_bool() {
            "ativo"
        } else {
            "repouso"
        };

        let t = Instant::now();
        let native = meta.property(PROP_TEMPERATURE).ok().and_then(|b| {
            // STORAGE_TEMPERATURE_DATA_DESCRIPTOR: InfoCount @12, TemperatureInfo[0].Temperature @26.
            let count = u16::from_le_bytes(b.get(12..14)?.try_into().ok()?);
            if count == 0 {
                return None;
            }
            Some(i16::from_le_bytes(b.get(26..28)?.try_into().ok()?))
        });
        let native_us = t.elapsed().as_micros();

        let t = Instant::now();
        let smart = if native.is_some() {
            None
        } else {
            Disk::open(n, GENERIC_READ.0 | GENERIC_WRITE.0).and_then(|rw| {
                // STORAGE_PREDICT_FAILURE { ULONG PredictFailure; UCHAR VendorSpecific[512]; }
                let b = rw.ioctl(IOCTL_STORAGE_PREDICT_FAILURE, None, 516).ok()?;
                smart_temp(b.get(4..)?)
            })
        };
        let smart_us = t.elapsed().as_micros();

        let temp = match (native, smart) {
            (Some(c), _) => format!("{c} °C (IOCTL nativo, {native_us} µs)"),
            (None, Some((id, c))) => format!("{c} °C (SMART {id}, {smart_us} µs)"),
            _ if bus == 16 => "sem sensor (disco virtual: usar os membros)".into(),
            _ => "FALHA".into(),
        };
        println!(
            "Disk {n}: {model:<28} {:<6} {kind:<3} {size_gb:>8.1} GB  {power:<7}  temp {temp}",
            bus_name(bus)
        );
    }
}
