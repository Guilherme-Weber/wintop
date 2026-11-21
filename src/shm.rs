//! Visão somente leitura de uma Shared Memory nomeada (RTSS), com limites pela `VirtualQuery`.

use std::os::windows::io::{AsRawHandle, OwnedHandle};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    FILE_MAP_READ, MEM_COMMIT, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
};
use windows::core::PCWSTR;

use crate::win::{own, wide};

/// Visão somente leitura de uma Shared Memory nomeada, com o tamanho real da região.
///
/// A memória é escrita por outro processo enquanto lemos: toda leitura é `volatile`, com limites
/// conferidos contra o tamanho da região. Campos de 32/64 bits alinhados são lidos numa única
/// carga (atômica no x86-64: nunca metade do valor antigo e metade do novo); só offsets
/// desalinhados caem na leitura byte a byte. Campos diferentes (ex.: `dwTime0`, `dwTime1`,
/// `dwFrames`) podem vir de instantes diferentes; quem lê valida a coerência.
pub struct SharedView {
    /// `None` só na visão de teste sobre um buffer local (nada a desmapear).
    _handle: Option<OwnedHandle>,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
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
            _handle: Some(handle),
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
        // Controles (C0 e os C1 que o Latin-1 mapeia de 0x80-0x9F) viram '?': nada de
        // sequências de terminal vindas de outro processo.
        Some(
            b[..end]
                .iter()
                .map(|&c| char::from(c))
                .map(|c| if c.is_control() { '?' } else { c })
                .collect(),
        )
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
        if self._handle.is_none() {
            return;
        }
        // SAFETY: a view foi obtida em `open` e não é usada depois daqui (o handle fecha sozinho).
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

#[cfg(test)]
impl SharedView {
    /// Visão sobre um buffer local (testes dos parsers). O buffer é vazado: vive até o fim do
    /// processo de teste, como uma região mapeada.
    pub fn from_bytes(b: &[u8]) -> Self {
        let words: &'static mut [u64] =
            Box::leak(vec![0u64; b.len().div_ceil(8)].into_boxed_slice());
        // SAFETY: o buffer tem pelo menos `b.len()` bytes e é só nosso.
        unsafe {
            std::ptr::copy_nonoverlapping(b.as_ptr(), words.as_mut_ptr().cast::<u8>(), b.len());
        }
        Self {
            _handle: None,
            view: MEMORY_MAPPED_VIEW_ADDRESS {
                Value: words.as_mut_ptr().cast(),
            },
            len: b.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leituras_nas_bordas() {
        let mut b = vec![0u8; 16];
        b[1..5].copy_from_slice(&0xA1B2_C3D4u32.to_le_bytes());
        b[8..12].copy_from_slice(b"ab\x9b\x07");
        let v = SharedView::from_bytes(&b);
        // Desalinhado: lido byte a byte, mesmo valor.
        assert_eq!(v.u32_at(1), Some(0xA1B2_C3D4));
        // Fim exato e além do fim.
        assert!(v.u32_at(12).is_some());
        assert_eq!(v.u32_at(13), None);
        assert_eq!(v.u32_at(usize::MAX), None);
        // Entrada de array com estouro na multiplicação e na soma.
        assert_eq!(v.entry(0, usize::MAX, 2), None);
        assert_eq!(v.entry(usize::MAX, 1, 1), None);
        assert_eq!(v.entry(0, 1, 8), Some(8));
        assert_eq!(v.entry(0, 2, 8), None);
        // String sem terminador e com controles C0/C1: nada de sequência no terminal.
        assert_eq!(v.cstr(8, 4).as_deref(), Some("ab??"));
        assert_eq!(v.cstr(14, 4), None);
    }
}
