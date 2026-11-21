//! Consulta PDH (contadores de desempenho).
//!
//! Sempre `PdhAddEnglishCounterW`: o Windows da máquina tem interface en-US e formato pt-BR, e
//! os nomes localizados mudam com o idioma. Contadores de taxa só têm valor a partir da segunda
//! coleta (a primeira devolve "dado inválido").

use std::cell::RefCell;

use windows::Win32::System::Performance::{
    PDH_CSTATUS_INVALID_DATA, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_NO_COUNTER,
    PDH_CSTATUS_NO_INSTANCE, PDH_CSTATUS_NO_OBJECT, PDH_CSTATUS_VALID_DATA, PDH_FMT,
    PDH_FMT_COUNTERVALUE, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_INVALID_DATA, PDH_MORE_DATA, PDH_NO_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhGetFormattedCounterValue, PdhOpenQueryW,
};
use windows::core::PCWSTR;

use crate::win::wide;

/// `PDH_FMT_NOCAP100`: não limitar a 100 (o limite é aplicado por quem lê, quando faz sentido).
const FMT_NOCAP100: u32 = 0x8000;
const FMT: PDH_FMT = PDH_FMT(PDH_FMT_DOUBLE.0 | FMT_NOCAP100);

/// Texto de um código de status do PDH.
pub fn status_text(code: u32) -> String {
    match code {
        PDH_CSTATUS_NO_OBJECT => "PDH object not found".into(),
        PDH_CSTATUS_NO_COUNTER => "PDH counter not found".into(),
        PDH_CSTATUS_NO_INSTANCE => "PDH instance not found".into(),
        PDH_CSTATUS_INVALID_DATA | PDH_INVALID_DATA => "waiting for a second sample".into(),
        PDH_NO_DATA => "PDH returned no data".into(),
        c => format!("PDH 0x{c:08X}"),
    }
}

fn check(code: u32) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(status_text(code))
    }
}

/// Consulta PDH com os seus contadores. Os handles dos contadores ficam só aqui dentro (o
/// `PdhCloseQuery` do `Drop` os libera), então nenhum sobrevive à consulta. Não é `Sync`
/// (`RefCell`): cada thread de coleta tem as suas.
pub struct Query {
    /// Identidade única (um `Counter` de outra consulta é recusado, não lido no lugar errado).
    id: u64,
    handle: PDH_HQUERY,
    counters: RefCell<Vec<PDH_HCOUNTER>>,
}

// SAFETY: os handles do PDH podem ser usados de outra thread, desde que não simultaneamente; a
// consulta não é `Sync`, então só uma thread a usa por vez.
unsafe impl Send for Query {}

/// Contador: identidade da `Query` e índice dentro dela.
#[derive(Clone, Copy, Debug)]
pub struct Counter(u64, usize);

static NEXT_QUERY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Query {
    pub fn open() -> Result<Self, String> {
        let mut h = PDH_HQUERY::default();
        // SAFETY: fonte nula = dados ao vivo; `h` recebe o handle.
        check(unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &raw mut h) })?;
        Ok(Self {
            id: NEXT_QUERY.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            handle: h,
            counters: RefCell::new(Vec::new()),
        })
    }

    /// Adiciona um contador pelo caminho em inglês (ex.: `\Memory\Modified Page List Bytes`).
    pub fn add(&self, path: &str) -> Result<Counter, String> {
        let w = wide(path);
        let mut c = PDH_HCOUNTER::default();
        // SAFETY: consulta aberta; caminho terminado em zero; `c` recebe o handle.
        check(unsafe { PdhAddEnglishCounterW(self.handle, PCWSTR(w.as_ptr()), 0, &raw mut c) })
            .map_err(|e| format!("{e} ({path})"))?;
        let mut list = self.counters.borrow_mut();
        list.push(c);
        Ok(Counter(self.id, list.len() - 1))
    }

    fn counter(&self, c: Counter) -> Result<PDH_HCOUNTER, String> {
        if c.0 != self.id {
            return Err("PDH counter from another query".into());
        }
        self.counters
            .borrow()
            .get(c.1)
            .copied()
            .ok_or_else(|| "PDH counter index out of range".into())
    }

    pub fn collect(&self) -> Result<(), String> {
        // SAFETY: consulta aberta.
        check(unsafe { PdhCollectQueryData(self.handle) })
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: handle aberto em `open`; fecha também os contadores.
        unsafe { PdhCloseQuery(self.handle) };
    }
}

impl Query {
    /// Valor de um contador de instância única.
    pub fn value(&self, c: Counter) -> Result<f64, String> {
        let h = self.counter(c)?;
        let mut v = PDH_FMT_COUNTERVALUE::default();
        // SAFETY: contador de uma consulta aberta; `v` local.
        check(unsafe { PdhGetFormattedCounterValue(h, FMT, None, &raw mut v) })?;
        valid(&v)
    }

    /// Valores de um contador com curinga (`(*)`): `(instância, valor)`, na ordem do PDH.
    /// Instâncias sem dado válido nesta coleta ficam de fora.
    pub fn array(&self, c: Counter) -> Result<Vec<(String, f64)>, String> {
        let h = self.counter(c)?;
        let mut size = 0u32;
        let mut count = 0u32;
        // SAFETY: primeira chamada só mede o buffer.
        let r =
            unsafe { PdhGetFormattedCounterArrayW(h, FMT, &raw mut size, &raw mut count, None) };
        if r != PDH_MORE_DATA {
            check(r)?;
            return Ok(Vec::new());
        }
        // Buffer alinhado a 8 (os itens têm ponteiro e f64); os nomes ficam no mesmo buffer.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        // SAFETY: buffer de `size` bytes, alinhado.
        check(unsafe {
            PdhGetFormattedCounterArrayW(h, FMT, &raw mut size, &raw mut count, Some(items))
        })?;
        let fits = buf.len() * 8 / size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let n = (count as usize).min(fits);
        let mut out = Vec::with_capacity(n);
        let mut first_err = None;
        for i in 0..n {
            // SAFETY: `i < count` itens escritos pelo PDH, dentro do buffer (`fits`).
            let item = unsafe { &*items.add(i) };
            // SAFETY: o PDH aponta o nome para uma string terminada em zero dentro do buffer.
            let name = unsafe { item.szName.to_string() }.unwrap_or_default();
            match valid(&item.FmtValue) {
                Ok(v) => out.push((name, v)),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        // Instâncias existem, mas nenhuma com dado válido (ex.: reset do driver): o status real,
        // não uma lista vazia que pareceria "nenhuma instância".
        match first_err {
            Some(e) if out.is_empty() => Err(e),
            _ => Ok(out),
        }
    }
}

fn valid(v: &PDH_FMT_COUNTERVALUE) -> Result<f64, String> {
    if v.CStatus != PDH_CSTATUS_VALID_DATA && v.CStatus != PDH_CSTATUS_NEW_DATA {
        return Err(status_text(v.CStatus));
    }
    // SAFETY: formato pedido = PDH_FMT_DOUBLE.
    let d = unsafe { v.Anonymous.doubleValue };
    if d.is_finite() {
        Ok(d)
    } else {
        Err("PDH returned a non-finite value".into())
    }
}

/// Contador opcional: guarda o erro de `add` para mostrar como causa.
pub type Slot = Result<Counter, String>;

/// Lê um contador simples guardado em `Slot`.
pub fn read(q: &Result<Query, String>, slot: &Slot) -> Result<f64, String> {
    q.as_ref().map_err(Clone::clone)?.value(slot.clone()?)
}

/// Lê um contador com curinga guardado em `Slot`.
pub fn read_array(q: &Result<Query, String>, slot: &Slot) -> Result<Vec<(String, f64)>, String> {
    q.as_ref().map_err(Clone::clone)?.array(slot.clone()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contador_ingles_e_curinga() {
        let q = Query::open();
        let qq = q.as_ref().expect("abrir consulta");
        let total = qq.add(r"\Processor Information(_Total)\% Processor Time");
        let per = qq.add(r"\Processor Information(*)\% Processor Time");
        let bad = qq.add(r"\Objeto Inexistente\Contador");
        assert!(bad.is_err());
        qq.collect().expect("1ª coleta");
        std::thread::sleep(std::time::Duration::from_millis(200));
        qq.collect().expect("2ª coleta");
        let v = read(&q, &total).expect("valor total");
        assert!(v >= 0.0, "{v}");
        // Contador de outra consulta: erro, nunca um handle inválido.
        let other = Query::open().expect("outra consulta");
        assert!(other.value(total.expect("contador")).is_err());
        let a = read_array(&q, &per).expect("valores por processador");
        assert!(a.iter().any(|(n, _)| n == "_Total"), "{a:?}");
    }
}
