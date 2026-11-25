//! Consulta PDH (contadores de desempenho).
//!
//! Sempre `PdhAddEnglishCounterW`: o Windows da máquina tem interface en-US e formato pt-BR, e
//! os nomes localizados mudam com o idioma. Contadores de taxa só têm valor a partir da segunda
//! coleta (a primeira devolve "dado inválido").

use std::cell::RefCell;

#[cfg(test)]
use windows::Win32::System::Performance::PdhGetFormattedCounterValue;
use windows::Win32::System::Performance::{
    PDH_CALC_NEGATIVE_DENOMINATOR, PDH_CALC_NEGATIVE_TIMEBASE, PDH_CALC_NEGATIVE_VALUE,
    PDH_CSTATUS_INVALID_DATA, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_NO_COUNTER,
    PDH_CSTATUS_NO_INSTANCE, PDH_CSTATUS_NO_OBJECT, PDH_CSTATUS_VALID_DATA, PDH_FMT,
    PDH_FMT_COUNTERVALUE, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_INVALID_DATA, PDH_MORE_DATA, PDH_NO_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::core::{PCWSTR, PWSTR};

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
        PDH_CALC_NEGATIVE_DENOMINATOR => "PDH negative denominator".into(),
        PDH_CALC_NEGATIVE_TIMEBASE => "PDH negative timebase".into(),
        PDH_CALC_NEGATIVE_VALUE => "PDH negative value".into(),
        c => format!("PDH 0x{c:08X}"),
    }
}

/// Erro tipado do PDH: preserva o código numérico de status (quando veio da API) para distinguir
/// falhas transitórias de cálculo diferencial sem comparar strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdhError {
    pub code: Option<u32>,
    pub message: String,
}

impl PdhError {
    pub fn from_code(code: u32) -> Self {
        Self {
            code: Some(code),
            message: status_text(code),
        }
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self {
            code: None,
            message: msg.into(),
        }
    }

    /// Erro transitório de cálculo diferencial do PDH (ajuste de relógio/NTP, delta negativo ou
    /// denominador zerado numa amostra isolada de `% Processor Time`, por exemplo).
    pub fn is_transient_calc(&self) -> bool {
        matches!(
            self.code,
            Some(
                PDH_CALC_NEGATIVE_DENOMINATOR
                    | PDH_CALC_NEGATIVE_TIMEBASE
                    | PDH_CALC_NEGATIVE_VALUE
            )
        )
    }

    fn with_path(mut self, path: &str) -> Self {
        self.message = format!("{} ({path})", self.message);
        self
    }
}

impl std::fmt::Display for PdhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<PdhError> for String {
    fn from(e: PdhError) -> Self {
        e.message
    }
}

fn check(code: u32) -> Result<(), PdhError> {
    if code == 0 {
        Ok(())
    } else {
        Err(PdhError::from_code(code))
    }
}

/// Decodifica uma string UTF-16 terminada em zero para `out`, reaproveitando a capacidade do
/// buffer (zero alocações por instância nas iterações de arrays do PDH).
///
/// # Safety
/// `p` deve ser nulo ou apontar para uma string UTF-16 válida terminada em zero dentro do buffer
/// preenchido pelo PDH.
unsafe fn decode_pcwstr_into(p: PWSTR, out: &mut String) {
    out.clear();
    if p.is_null() {
        return;
    }
    // SAFETY: pré-condição desta função; `as_wide` lê até o primeiro 0.
    let wide = unsafe { p.as_wide() };
    for ch in char::decode_utf16(wide.iter().copied()) {
        out.push(ch.unwrap_or(char::REPLACEMENT_CHARACTER));
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
    /// Buffer das leituras de arrays, reaproveitado entre as coletas (cresce até o maior
    /// tamanho pedido; sem alocar a cada leitura).
    buf: RefCell<Vec<u64>>,
    /// Buffer de texto reaproveitado para decodificar `szName` sem alocar `String` por item.
    name_buf: RefCell<String>,
}

// SAFETY: os handles do PDH podem ser usados de outra thread, desde que não simultaneamente; a
// consulta não é `Sync`, então só uma thread a usa por vez.
unsafe impl Send for Query {}

/// Contador: identidade da `Query` e índice dentro dela.
#[derive(Clone, Copy, Debug)]
pub struct Counter(u64, usize);

static NEXT_QUERY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Query {
    pub fn open() -> Result<Self, PdhError> {
        let mut h = PDH_HQUERY::default();
        // SAFETY: fonte nula = dados ao vivo; `h` recebe o handle.
        check(unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &raw mut h) })?;
        Ok(Self {
            id: NEXT_QUERY.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            handle: h,
            counters: RefCell::new(Vec::new()),
            buf: RefCell::new(Vec::new()),
            name_buf: RefCell::new(String::new()),
        })
    }

    /// Adiciona um contador pelo caminho em inglês (ex.: `\Processor Information(*)\% Processor Time`).
    pub fn add(&self, path: &str) -> Result<Counter, PdhError> {
        let w = wide(path);
        let mut c = PDH_HCOUNTER::default();
        // SAFETY: consulta aberta; caminho terminado em zero; `c` recebe o handle.
        check(unsafe { PdhAddEnglishCounterW(self.handle, PCWSTR(w.as_ptr()), 0, &raw mut c) })
            .map_err(|e| e.with_path(path))?;
        let mut list = self.counters.borrow_mut();
        list.push(c);
        Ok(Counter(self.id, list.len() - 1))
    }

    fn counter(&self, c: Counter) -> Result<PDH_HCOUNTER, PdhError> {
        if c.0 != self.id {
            return Err(PdhError::other("PDH counter from another query"));
        }
        self.counters
            .borrow()
            .get(c.1)
            .copied()
            .ok_or_else(|| PdhError::other("PDH counter index out of range"))
    }

    pub fn collect(&self) -> Result<(), PdhError> {
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
    #[cfg(test)]
    pub fn value(&self, c: Counter) -> Result<f64, PdhError> {
        let h = self.counter(c)?;
        let mut v = PDH_FMT_COUNTERVALUE::default();
        // SAFETY: contador de uma consulta aberta; `v` local.
        check(unsafe { PdhGetFormattedCounterValue(h, FMT, None, &raw mut v) })?;
        valid(&v)
    }

    /// Valores de um contador com curinga (`(*)`): `(instância, valor)`, na ordem do PDH.
    /// Instâncias sem dado válido nesta coleta ficam de fora.
    #[cfg(test)]
    pub fn array(&self, c: Counter) -> Result<Vec<(String, f64)>, PdhError> {
        let mut out = Vec::new();
        self.for_each_array(c, |name, v| out.push((name.to_string(), v)))?;
        Ok(out)
    }

    /// Percorre as instâncias válidas de um contador com curinga (`(*)`) sem alocar `String`
    /// por item (reaproveita um buffer UTF-8 interno).
    pub fn for_each_array(&self, c: Counter, f: impl FnMut(&str, f64)) -> Result<(), PdhError> {
        self.for_each_array_with(c, false, f)
    }

    /// Como `for_each_array`, mas instâncias sem dado válido nesta coleta chegam com `f64::NAN`
    /// (preserva o alinhamento posicional de todas as instâncias).
    pub fn for_each_array_all(&self, c: Counter, f: impl FnMut(&str, f64)) -> Result<(), PdhError> {
        self.for_each_array_with(c, true, f)
    }

    fn for_each_array_with(
        &self,
        c: Counter,
        keep_invalid: bool,
        mut f: impl FnMut(&str, f64),
    ) -> Result<(), PdhError> {
        let h = self.counter(c)?;
        let mut size = 0u32;
        let mut count = 0u32;
        // SAFETY: primeira chamada só mede o buffer.
        let r =
            unsafe { PdhGetFormattedCounterArrayW(h, FMT, &raw mut size, &raw mut count, None) };
        if r != PDH_MORE_DATA {
            check(r)?;
            return Ok(());
        }
        // Buffer alinhado a 8 (os itens têm ponteiro e f64); os nomes ficam no mesmo buffer.
        let mut buf = self.buf.borrow_mut();
        let words = (size as usize).div_ceil(8);
        if buf.len() < words {
            buf.resize(words, 0);
        }
        let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        // O PDH escreve até `size` bytes: o que há além no buffer reaproveitado não é lido.
        // SAFETY: buffer de `size` bytes, alinhado.
        check(unsafe {
            PdhGetFormattedCounterArrayW(h, FMT, &raw mut size, &raw mut count, Some(items))
        })?;
        let fits = words * 8 / size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let n = (count as usize).min(fits);
        let mut name = self.name_buf.borrow_mut();
        let mut any_valid = false;
        let mut first_err = None;
        for i in 0..n {
            // SAFETY: `i < count` itens escritos pelo PDH, dentro do buffer (`fits`).
            let item = unsafe { &*items.add(i) };
            match valid(&item.FmtValue) {
                Ok(v) => {
                    any_valid = true;
                    // SAFETY: `szName` aponta para UTF-16 terminado em zero dentro de `buf`.
                    unsafe { decode_pcwstr_into(item.szName, &mut name) };
                    f(&name, v);
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                    if keep_invalid {
                        // SAFETY: `szName` aponta para UTF-16 terminado em zero dentro de `buf`.
                        unsafe { decode_pcwstr_into(item.szName, &mut name) };
                        f(&name, f64::NAN);
                    }
                }
            }
        }
        // Instâncias existem, mas nenhuma com dado válido (ex.: reset do driver): o status real,
        // não uma lista vazia que pareceria "nenhuma instância".
        match first_err {
            Some(e) if !any_valid => Err(e),
            _ => Ok(()),
        }
    }
}

fn valid(v: &PDH_FMT_COUNTERVALUE) -> Result<f64, PdhError> {
    if v.CStatus != PDH_CSTATUS_VALID_DATA && v.CStatus != PDH_CSTATUS_NEW_DATA {
        return Err(PdhError::from_code(v.CStatus));
    }
    // SAFETY: formato pedido = PDH_FMT_DOUBLE.
    let d = unsafe { v.Anonymous.doubleValue };
    if d.is_finite() {
        Ok(d)
    } else {
        Err(PdhError::other("PDH returned a non-finite value"))
    }
}

/// Contador opcional: guarda o erro de `add` para mostrar como causa.
pub type Slot = Result<Counter, PdhError>;

/// Lê um contador simples guardado em `Slot`.
#[cfg(test)]
pub fn read(q: &Result<Query, PdhError>, slot: &Slot) -> Result<f64, PdhError> {
    q.as_ref().map_err(Clone::clone)?.value(slot.clone()?)
}

/// Lê um contador com curinga guardado em `Slot`.
#[cfg(test)]
pub fn read_array(
    q: &Result<Query, PdhError>,
    slot: &Slot,
) -> Result<Vec<(String, f64)>, PdhError> {
    q.as_ref().map_err(Clone::clone)?.array(slot.clone()?)
}

/// Percorre as instâncias válidas de um contador com curinga guardado em `Slot` sem alocar
/// `String` por item.
pub fn for_each_array(
    q: &Result<Query, PdhError>,
    slot: &Slot,
    f: impl FnMut(&str, f64),
) -> Result<(), PdhError> {
    q.as_ref()
        .map_err(Clone::clone)?
        .for_each_array(slot.clone()?, f)
}

/// Como `for_each_array`, entregando `f64::NAN` para instâncias sem dado válido.
pub fn for_each_array_all(
    q: &Result<Query, PdhError>,
    slot: &Slot,
    f: impl FnMut(&str, f64),
) -> Result<(), PdhError> {
    q.as_ref()
        .map_err(Clone::clone)?
        .for_each_array_all(slot.clone()?, f)
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
        let mut saw_total = false;
        for_each_array(&q, &per, |name, _| {
            if name == "_Total" {
                saw_total = true;
            }
        })
        .expect("for_each_array");
        assert!(saw_total);
    }

    #[test]
    fn codigos_transitorios_de_calculo_do_pdh() {
        for code in [
            PDH_CALC_NEGATIVE_DENOMINATOR,
            PDH_CALC_NEGATIVE_TIMEBASE,
            PDH_CALC_NEGATIVE_VALUE,
        ] {
            let e = PdhError::from_code(code);
            assert!(e.is_transient_calc(), "{code:#X} -> {e}");
        }
        assert!(!PdhError::from_code(PDH_CSTATUS_NO_OBJECT).is_transient_calc());
        assert!(!PdhError::from_code(PDH_NO_DATA).is_transient_calc());
        assert!(!PdhError::other("PDH negative denominator").is_transient_calc());
    }
}
