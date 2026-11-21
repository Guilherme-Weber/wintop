//! Discos físicos: inventário (modelo, barramento, SSD/HDD, capacidade, membro de pool) e
//! atividade pelos contadores do driver (`IOCTL_DISK_PERFORMANCE`, a mesma fonte do PDH
//! `\PhysicalDisk(*)`, sem os ~54 MB de memória privada que o provedor do PDH reserva). Letras
//! das unidades pelas extensões de cada volume. As temperaturas entram na Etapa 2 (thread lenta).
//!
//! Offsets das estruturas conferidos com o compilador MSVC (winioctl.h) na Etapa 0.

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::time::{Duration, Instant};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA,
    SPDRP_FRIENDLYNAME, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceRegistryPropertyW,
};
use windows::Win32::Foundation::{GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    GetLogicalDrives, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    GUID_DEVINTERFACE_DISK, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_DISK_PERFORMANCE, IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
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

pub(super) struct Handle(OwnedHandle);

impl Handle {
    pub(super) fn open(n: u32, access: u32) -> Option<Self> {
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

    pub(super) fn ioctl(
        &self,
        code: u32,
        input: Option<&[u8]>,
        out_len: usize,
    ) -> Result<Vec<u8>, String> {
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
    pub(super) fn property(&self, id: u32) -> Result<Vec<u8>, String> {
        let mut q = [0u8; 12];
        q[..4].copy_from_slice(&id.to_le_bytes());
        self.ioctl(IOCTL_STORAGE_QUERY_PROPERTY, Some(&q), 1024)
    }

    /// `Some(true)` acordado, `Some(false)` em repouso, `None` sem resposta.
    pub(super) fn awake(&self) -> Option<bool> {
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
    // O número de partições não passa do que cabe no buffer (entrada corrompida não vira um
    // laço de bilhões de voltas).
    let fits = layout.len().saturating_sub(L_ENTRIES) / P_SIZE;
    let count = (u32_at(layout, L_COUNT).unwrap_or(0) as usize).min(fits);
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
    // Somas checadas: contadores corrompidos (ou que saltam) viram "sem amostra", não pânico.
    let busy = now
        .read_time
        .checked_sub(prev.read_time)?
        .checked_add(now.write_time.checked_sub(prev.write_time)?)?;
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
pub(super) struct Inventory {
    pub(super) number: u32,
    pub(super) model: Metric<String>,
    pub(super) bus: Metric<Bus>,
    pub(super) ssd: Metric<bool>,
    capacity: Metric<u64>,
    pub(super) pool_member: Metric<bool>,
    asleep_since: Option<Instant>,
    /// Handle de metadados mantido aberto; USB não (não impedir a remoção segura).
    pub(super) handle: Option<Handle>,
    last: Option<PerfSample>,
    /// A última leitura falhou (disco removido?).
    pub(super) lost: bool,
}

impl Inventory {
    /// `names`: nomes amigáveis do PnP (`friendly_names`), preferidos ao do descritor.
    fn discover(n: u32, names: &[(u32, String)]) -> Option<Self> {
        let meta = Handle::open(n, 0)?;
        let desc = meta.property(PROP_DEVICE);
        let model = match names.iter().find(|(m, _)| *m == n) {
            Some((_, name)) => Ok(name.clone()),
            None => desc
                .as_deref()
                .map_err(Clone::clone)
                .and_then(|d| model_name(d).ok_or_else(|| "descriptor has no model".into()))
                .or_else(fail),
        };
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

    /// Disco que estava em repouso e agora respondeu acordado: relê capacidade e layout com o
    /// handle já aberto (sem esperar o `RETRY_ASLEEP`).
    /// O disco ainda é o mesmo e está presente. Com handle guardado: ele ainda responde (se
    /// outro disco pegou o número depois de uma remoção, o número abre, mas o handle velho não).
    /// Sem handle guardado (USB): o número ainda abre.
    pub(super) fn alive(&self) -> bool {
        match &self.handle {
            Some(h) => h.property(PROP_DEVICE).is_ok(),
            None => Handle::open(self.number, 0).is_some(),
        }
    }

    pub(super) fn reread_if_was_asleep(&mut self, meta: &Handle) {
        if self.asleep_since.is_some() {
            self.read_details(meta);
        }
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
    /// Disco perdido não é reaberto: o mesmo número pode ser outro disco (redescoberta).
    fn activity(&mut self) -> Metric<(f64, f64, f64, f64)> {
        if self.lost {
            return fail("disk removed");
        }
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
                if !self.alive() {
                    self.lost = true;
                    self.handle = None;
                }
                return fail(e);
            }
        };
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

/// Todos os discos físicos presentes.
pub(super) fn discover_all() -> Vec<Inventory> {
    let mut disks = Vec::new();
    refresh(&mut disks);
    disks
}

/// Disco perdido sai; se o número voltar a existir, é redescoberto do zero (handle e
/// metadados novos: pode ser outro disco). Discos novos entram.
pub(super) fn refresh(disks: &mut Vec<Inventory>) {
    disks.retain(|d| !d.lost);
    let new: Vec<u32> = (0..64)
        .filter(|n| !disks.iter().any(|d| d.number == *n) && Handle::open(*n, 0).is_some())
        .collect();
    if new.is_empty() {
        return;
    }
    let names = friendly_names();
    disks.extend(
        new.into_iter()
            .filter_map(|n| Inventory::discover(n, &names)),
    );
    disks.sort_by_key(|d| d.number);
}

/// Nome amigável do PnP de cada disco (o que o Task Manager mostra): `(número, nome)`.
/// Pela interface `GUID_DEVINTERFACE_DISK` do SetupAPI; o número vem do
/// `IOCTL_STORAGE_GET_DEVICE_NUMBER` no caminho da interface.
fn friendly_names() -> Vec<(u32, String)> {
    let guid = GUID_DEVINTERFACE_DISK;
    // SAFETY: GUID local; a lista é destruída no fim da função.
    let Ok(set) = (unsafe {
        SetupDiGetClassDevsW(
            Some(&raw const guid),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    }) else {
        return Vec::new();
    };
    // Destrói a lista também em pânico (os coletores rodam sob catch_unwind).
    struct List(HDEVINFO);
    impl Drop for List {
        fn drop(&mut self) {
            // SAFETY: lista criada por SetupDiGetClassDevsW, destruída uma única vez.
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }
    let _list = List(set);
    let mut out = Vec::new();
    for i in 0.. {
        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: lista válida; erro (ERROR_NO_MORE_ITEMS) encerra.
        let r =
            unsafe { SetupDiEnumDeviceInterfaces(set, None, &raw const guid, i, &raw mut iface) };
        if r.is_err() {
            break;
        }
        if let Some(entry) = interface_name(set, &iface) {
            out.push(entry);
        }
    }
    out
}

fn interface_name(set: HDEVINFO, iface: &SP_DEVICE_INTERFACE_DATA) -> Option<(u32, String)> {
    // SP_DEVICE_INTERFACE_DETAIL_DATA_W: cbSize (8 no x64) + caminho UTF-16 a partir do byte 4.
    let mut buf = vec![0u64; 256];
    buf[0] = 8;
    let mut dev = SP_DEVINFO_DATA {
        cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    // SAFETY: buffer alinhado de 2 KB com cbSize preenchido; tamanho informado em bytes.
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set,
            iface,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 8) as u32,
            None,
            Some(&raw mut dev),
        )
    }
    .ok()?;
    // SAFETY: Vec<u64> visto como u16 a partir do byte 4, dentro do buffer (o caminho termina
    // em zero; `from_wide` para no zero ou no fim).
    let path_w =
        unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u16>().add(2), buf.len() * 4 - 2) };
    let path = win::from_wide(path_w);
    // STORAGE_DEVICE_NUMBER { DeviceType, DeviceNumber, PartitionNumber }.
    let number = Handle::open_path(&path, 0)?
        .ioctl(IOCTL_STORAGE_GET_DEVICE_NUMBER, None, 12)
        .ok()
        .and_then(|b| u32_at(&b, 4))?;
    let mut name = [0u8; 512];
    // SAFETY: estruturas válidas; buffer local.
    unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            &raw const dev,
            SPDRP_FRIENDLYNAME,
            None,
            Some(&mut name),
            None,
        )
    }
    .ok()?;
    let wide_name: Vec<u16> = name
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let name = win::from_wide(&wide_name).trim().to_string();
    (!name.is_empty()).then_some((number, name))
}

pub struct DiskCollector {
    disks: Vec<Inventory>,
    letters: Vec<(u32, String)>,
    /// Última redescoberta; `None` força uma na próxima coleta (disco perdido).
    discovered: Option<Instant>,
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
            disks: discover_all(),
            letters: volume_letters(),
            discovered: Some(Instant::now()),
            system_drive: win::windows_drive(),
            pagefile_drives,
        }
    }

    /// Discos novos entram, discos que sumiram saem; letras relidas.
    fn rediscover(&mut self) {
        self.discovered = Some(Instant::now());
        refresh(&mut self.disks);
        self.letters = volume_letters();
    }

    pub fn read(&mut self) -> Metric<Vec<Disk>> {
        if self.discovered.is_none_or(|t| t.elapsed() >= REDISCOVER) {
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
            if inv.lost {
                self.discovered = None;
            }
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

    #[test]
    fn fuzz_layout_descritor_e_extensoes() {
        let mut l = vec![0u8; L_ENTRIES + 2 * P_SIZE];
        l[L_STYLE..L_STYLE + 4].copy_from_slice(&STYLE_GPT.to_le_bytes());
        l[L_COUNT..L_COUNT + 4].copy_from_slice(&2u32.to_le_bytes());
        let t = L_ENTRIES + P_SIZE + P_GPT_TYPE;
        l[t..t + 16].copy_from_slice(&SPACES_PROTECTIVE);
        let mut d = vec![0u8; 64];
        d[D_VENDOR_OFF..D_VENDOR_OFF + 4].copy_from_slice(&40u32.to_le_bytes());
        d[D_PRODUCT_OFF..D_PRODUCT_OFF + 4].copy_from_slice(&48u32.to_le_bytes());
        d[40..43].copy_from_slice(b"WDC");
        d[48..54].copy_from_slice(b"WD80EF");
        let mut x = vec![0u8; X_ENTRIES + 3 * X_SIZE];
        x[X_COUNT..X_COUNT + 4].copy_from_slice(&3u32.to_le_bytes());
        for (i, n) in [5u32, 1, 5].iter().enumerate() {
            let o = X_ENTRIES + i * X_SIZE;
            x[o..o + 4].copy_from_slice(&n.to_le_bytes());
        }
        crate::fuzz::run("disco", &[l, d, x], 4096, |_, b| {
            let _ = is_pool_member(b);
            let _ = model_name(b);
            let disks = parse_extents(b);
            assert!(disks.len() <= MAX_EXTENTS);
            assert!(disks.windows(2).all(|w| w[0] < w[1]));
        });
    }

    /// Duas amostras quaisquer do `IOCTL_DISK_PERFORMANCE` (contadores que voltam, valores no
    /// limite do u64): a atividade sai finita e dentro das faixas, ou `None`.
    #[test]
    fn fuzz_atividade_entre_amostras() {
        let mut p = vec![0u8; PERF_SIZE];
        for (o, v) in [(P_BYTES_READ, 1u64 << 30), (P_IDLE_TIME, 9_000_000)] {
            p[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
        p[P_QUERY_TIME..P_QUERY_TIME + 8].copy_from_slice(&10_000_000u64.to_le_bytes());
        crate::fuzz::run("atividade", &[p.clone()], 2 * PERF_SIZE, |r, b| {
            let Some(a) = parse_perf(b) else { return };
            let mut c = p.clone();
            for _ in 0..=r.below(6) {
                crate::fuzz::mutate(r, &mut c);
            }
            c.resize(PERF_SIZE, 0);
            let c = parse_perf(&c).expect("amostra completa");
            for (prev, now) in [(&a, &c), (&c, &a)] {
                if let Some((active, read, write, resp)) = activity(prev, now) {
                    assert!((0.0..=100.0).contains(&active));
                    assert!(read.is_finite() && read >= 0.0);
                    assert!(write.is_finite() && write >= 0.0);
                    assert!(resp.is_finite() && resp >= 0.0);
                }
            }
        });
    }

    #[test]
    fn contagem_de_particoes_corrompida_nao_trava() {
        let mut l = vec![0u8; L_ENTRIES + P_SIZE];
        l[L_STYLE..L_STYLE + 4].copy_from_slice(&STYLE_GPT.to_le_bytes());
        l[L_COUNT..L_COUNT + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let t = std::time::Instant::now();
        assert!(!is_pool_member(&l));
        assert!(t.elapsed() < std::time::Duration::from_millis(50));
    }

    /// Carga conhecida: 256 MB gravados sem cache (`FILE_FLAG_NO_BUFFERING` +
    /// `WRITE_THROUGH`) na pasta temporária; o disco dela mostra pelo menos isso de escrita.
    #[test]
    #[ignore = "mede carga real de disco: rodar sozinho (--test-threads=1)"]
    fn escrita_conhecida_no_disco() {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        const MB: usize = 1 << 20;
        let dir = std::env::temp_dir();
        let letter =
            crate::win::drive_letter(&dir.to_string_lossy()).expect("letra da pasta temporária");
        let mut c = DiskCollector::new();
        c.read().expect("1ª leitura");
        let t0 = std::time::Instant::now();
        let path = dir.join(format!("wintop_disk_{}.bin", std::process::id()));
        {
            // Buffer alinhado a 4 KiB (exigência do NO_BUFFERING).
            let raw = vec![7u8; MB + 4096];
            let off = raw.as_ptr().align_offset(4096);
            let block = &raw[off..off + MB];
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(0x2000_0000 | 0x8000_0000) // NO_BUFFERING | WRITE_THROUGH
                .open(&path)
                .expect("arquivo sem cache");
            for _ in 0..256 {
                f.write_all(block).expect("gravação");
            }
        }
        let list = c.read().expect("2ª leitura");
        let secs = t0.elapsed().as_secs_f64();
        let _ = std::fs::remove_file(&path);
        let d = list
            .iter()
            .find(|d| d.letters.iter().any(|l| l.starts_with(letter)))
            .expect("disco da pasta temporária");
        let written = d.write.clone().expect("escrita") * secs / MB as f64;
        println!(
            "disco {} ({letter}:): ~{written:.0} MB gravados em {secs:.2} s",
            d.number
        );
        assert!(written >= 250.0, "{written} MB");
        assert!(d.active.clone().expect("atividade") > 0.0);
    }

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
