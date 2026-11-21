//! Utilitários compartilhados pelos spikes da Etapa 0 (não fazem parte do programa).
#![allow(dead_code)]

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, MEM_COMMIT, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;

/// Converte para UTF-16 terminado em zero.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Dono de um `HANDLE` do Win32: fecha no `Drop` (inclusive em caminhos de erro).
///
/// # Safety
/// `h` tem de ser um handle aberto, válido (nem nulo nem `INVALID_HANDLE_VALUE`) e sem outro
/// dono: depois desta chamada só o `OwnedHandle` o fecha.
pub unsafe fn own(h: HANDLE) -> OwnedHandle {
    // SAFETY: garantido por quem chama (ver acima).
    unsafe { OwnedHandle::from_raw_handle(h.0) }
}

/// Processo elevado: DLLs só de System32 (ou por caminho absoluto), nunca do diretório atual
/// nem da pasta do executável. Chamar no início de `main`.
pub fn harden_dll_search() {
    // SAFETY: sem pré-condições; afeta só as cargas seguintes deste processo.
    if let Err(e) = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } {
        eprintln!("aviso: SetDefaultDllDirectories: {e}");
    }
}

/// Pasta System32 pela API (a variável de ambiente `SystemRoot` é herdável e manipulável).
pub fn system_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    // SAFETY: buffer local com o tamanho informado.
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// `true` quando o processo atual roda elevado.
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: token recebe um handle válido em caso de sucesso e passa a ter dono logo abaixo.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }.is_err() {
        return false;
    }
    // SAFETY: OpenProcessToken teve sucesso; handle válido e sem outro dono.
    let _token = unsafe { own(token) };
    let mut elev = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: buffer do tamanho de TOKEN_ELEVATION.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elev).cast::<c_void>()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut len,
        )
    }
    .is_ok();
    ok && elev.TokenIsElevated != 0
}

/// Visão somente leitura de uma Shared Memory nomeada, com o tamanho real da região.
///
/// A memória é escrita por outro processo enquanto lemos: toda leitura é `volatile`, com limites
/// conferidos contra o tamanho da região. Campos de 32/64 bits alinhados são lidos numa única
/// carga (atômica no x86-64: nunca metade do valor antigo e metade do novo); só offsets
/// desalinhados caem na leitura byte a byte. Campos diferentes (ex.: `dwTime0`, `dwTime1`,
/// `dwFrames`) podem vir de instantes diferentes; quem lê valida a coerência.
pub struct SharedView {
    _handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    pub len: usize,
}

impl SharedView {
    pub fn open(name: &str) -> Option<Self> {
        let w = wide(name);
        // SAFETY: nome terminado em zero.
        let handle =
            unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(w.as_ptr())) }.ok()?;
        // SAFETY: OpenFileMappingW teve sucesso; handle válido e sem outro dono.
        let handle = unsafe { own(handle) };
        // SAFETY: handle válido de mapeamento; a view é liberada em Drop.
        let view = unsafe { MapViewOfFile(HANDLE(handle.as_raw_handle()), FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            return None;
        }
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: endereço da view recém-mapeada; estrutura local.
        let got = unsafe {
            VirtualQuery(
                Some(view.Value),
                &raw mut mbi,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        let mut me = Self {
            _handle: handle,
            view,
            len: 0,
        };
        if got == 0 || mbi.State != MEM_COMMIT {
            return None; // `me` desmapeia a view no Drop
        }
        me.len = mbi.RegionSize;
        Some(me)
    }

    /// Cópia volátil dos bytes `[off, off+N)`, ou `None` fora dos limites.
    fn read<const N: usize>(&self, off: usize) -> Option<[u8; N]> {
        if off.checked_add(N)? > self.len {
            return None;
        }
        let base = self.view.Value.cast::<u8>();
        let mut out = [0u8; N];
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: `off + i < len`, dentro da região mapeada e comprometida.
            *b = unsafe { base.add(off + i).read_volatile() };
        }
        Some(out)
    }

    /// Ponteiro para `[off, off+n)` se couber na região.
    fn at(&self, off: usize, n: usize) -> Option<*const u8> {
        (off.checked_add(n)? <= self.len).then(|| {
            // SAFETY: `off + n <= len`: o resultado está dentro da região mapeada.
            unsafe { self.view.Value.cast::<u8>().cast_const().add(off) }
        })
    }

    pub fn u32_at(&self, off: usize) -> Option<u32> {
        let p = self.at(off, 4)?;
        if p.cast::<u32>().is_aligned() {
            // SAFETY: dentro da região (at) e alinhado: uma carga volátil de 32 bits.
            Some(unsafe { p.cast::<u32>().read_volatile() })
        } else {
            self.read::<4>(off).map(u32::from_le_bytes)
        }
    }

    pub fn u64_at(&self, off: usize) -> Option<u64> {
        let p = self.at(off, 8)?;
        if p.cast::<u64>().is_aligned() {
            // SAFETY: dentro da região (at) e alinhado: uma carga volátil de 64 bits.
            Some(unsafe { p.cast::<u64>().read_volatile() })
        } else {
            self.read::<8>(off).map(u64::from_le_bytes)
        }
    }

    pub fn f32_at(&self, off: usize) -> Option<f32> {
        self.u32_at(off).map(f32::from_bits)
    }

    /// Cópia volátil dos bytes `[off, off+len)`, ou `None` fora dos limites.
    pub fn bytes(&self, off: usize, len: usize) -> Option<Vec<u8>> {
        if off.checked_add(len)? > self.len {
            return None;
        }
        let base = self.view.Value.cast::<u8>();
        // SAFETY: `off + len <= self.len`, dentro da região mapeada e comprometida.
        Some(
            (0..len)
                .map(|i| unsafe { base.add(off + i).read_volatile() })
                .collect(),
        )
    }

    /// String C (CP-1252 tratado como Latin-1) de até `max` bytes.
    pub fn cstr(&self, off: usize, max: usize) -> Option<String> {
        let b = self.bytes(off, max)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        Some(b[..end].iter().map(|&c| char::from(c)).collect())
    }

    /// Deslocamento da entrada `i` de um array `[base + i*size]`, só se a entrada inteira
    /// (`size` bytes) couber na região. Aritmética checada.
    pub fn entry(&self, base: usize, i: usize, size: usize) -> Option<usize> {
        let off = base.checked_add(i.checked_mul(size)?)?;
        (off.checked_add(size)? <= self.len).then_some(off)
    }
}

impl Drop for SharedView {
    fn drop(&mut self) {
        // SAFETY: a view foi obtida em `open` e não é usada depois daqui (o handle fecha sozinho).
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

/// Valor de uma fonte do Afterburner (MAHM) pelo `dwSrcId`, só para comparação nos spikes.
pub fn afterburner(src_id: u32) -> Option<f32> {
    const MAHM: u32 = 0x4D41_484D; // 'MAHM' como constante multi-caractere
    const ENTRY_MIN: usize = 1324; // MAHM_SHARED_MEMORY_ENTRY v2.0
    let v = SharedView::open("MAHMSharedMemory")?;
    if v.u32_at(0)? != MAHM {
        return None;
    }
    let header = v.u32_at(8)? as usize;
    let count = v.u32_at(12)? as usize;
    let size = v.u32_at(16)? as usize;
    if size < ENTRY_MIN {
        return None;
    }
    for i in 0..count {
        // Para na primeira entrada fora da região (cabeçalho corrompido não vira laço longo).
        let e = v.entry(header, i, size)?;
        // 5 strings MAX_PATH, depois data (f32) @1300 e dwSrcId @1320.
        if v.u32_at(e + 1320)? == src_id {
            return v.f32_at(e + 1300);
        }
    }
    None
}
