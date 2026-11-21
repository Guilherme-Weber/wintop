//! Utilitários compartilhados pelos spikes da Etapa 0 (não fazem parte do programa).
#![allow(dead_code)]

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;

/// Converte para UTF-16 terminado em zero.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `true` quando o processo atual roda elevado.
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: token recebe um handle válido em caso de sucesso e é fechado logo abaixo.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elev).cast::<c_void>()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

/// Visão somente leitura de uma Shared Memory nomeada, com o tamanho real da região.
pub struct SharedView {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    pub len: usize,
}

impl SharedView {
    pub fn open(name: &str) -> Option<Self> {
        let w = wide(name);
        // SAFETY: nome terminado em zero; handle e view são liberados em Drop.
        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(w.as_ptr())).ok()?;
            let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                let _ = CloseHandle(handle);
                return None;
            }
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            VirtualQuery(
                Some(view.Value),
                &raw mut mbi,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            );
            Some(Self {
                handle,
                view,
                len: mbi.RegionSize,
            })
        }
    }

    /// Lê um `u32` no deslocamento dado, ou `None` fora dos limites da view.
    pub fn u32_at(&self, off: usize) -> Option<u32> {
        (off.checked_add(4)? <= self.len).then(|| {
            // SAFETY: limites conferidos contra o tamanho da região mapeada.
            unsafe {
                self.view
                    .Value
                    .cast::<u8>()
                    .add(off)
                    .cast::<u32>()
                    .read_unaligned()
            }
        })
    }

    pub fn u64_at(&self, off: usize) -> Option<u64> {
        (off.checked_add(8)? <= self.len).then(|| {
            // SAFETY: limites conferidos contra o tamanho da região mapeada.
            unsafe {
                self.view
                    .Value
                    .cast::<u8>()
                    .add(off)
                    .cast::<u64>()
                    .read_unaligned()
            }
        })
    }

    pub fn f32_at(&self, off: usize) -> Option<f32> {
        self.u32_at(off).map(f32::from_bits)
    }

    /// Cópia dos bytes `[off, off+len)`, ou `None` fora dos limites.
    pub fn bytes(&self, off: usize, len: usize) -> Option<Vec<u8>> {
        (off.checked_add(len)? <= self.len).then(|| {
            let mut v = vec![0u8; len];
            // SAFETY: limites conferidos; destino tem `len` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.view.Value.cast::<u8>().add(off),
                    v.as_mut_ptr(),
                    len,
                );
            }
            v
        })
    }

    /// String C (CP-1252 tratado como Latin-1) de até `max` bytes.
    pub fn cstr(&self, off: usize, max: usize) -> Option<String> {
        let b = self.bytes(off, max)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        Some(b[..end].iter().map(|&c| char::from(c)).collect())
    }
}

impl Drop for SharedView {
    fn drop(&mut self) {
        // SAFETY: view e handle foram obtidos em `open` e não são usados depois daqui.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Valor de uma fonte do Afterburner (MAHM) pelo `dwSrcId`, só para comparação nos spikes.
pub fn afterburner(src_id: u32) -> Option<f32> {
    const MAHM: u32 = 0x4D41_484D; // 'MAHM' como constante multi-caractere
    let v = SharedView::open("MAHMSharedMemory")?;
    if v.u32_at(0)? != MAHM {
        return None;
    }
    let header = v.u32_at(8)? as usize;
    let count = v.u32_at(12)? as usize;
    let size = v.u32_at(16)? as usize;
    (0..count).find_map(|i| {
        let e = header + i * size;
        // MAHM_SHARED_MEMORY_ENTRY: 5 strings MAX_PATH, depois data(f32) @1300 e dwSrcId @1320.
        (v.u32_at(e + 1320)? == src_id)
            .then(|| v.f32_at(e + 1300))
            .flatten()
    })
}
