//! Frames do jogo por ETW (a lógica do PresentMon, enxuta), numa sessão em tempo real própria:
//!
//! - DXGI `Present_Start` (Microsoft-Windows-DXGI, evento 42): todo frame apresentado pelo jogo,
//!   inclusive os gerados pelo frame gen (o Streamline apresenta cada um). Frametime = intervalo
//!   entre `Present_Start` da mesma swap chain.
//! - `PCLStatsTraceLoggingProvider` (Reflex/PC Latency, TraceLogging, registrado pelo jogo):
//!   `PCLStatsEvent` com `Marker` = 0 (`SIMULATION_START`) uma vez por frame **real**;
//!   `PCLStatsEventV3` com `Marker` = 22 (`NUM_PRESENTS_IN_BATCH`) e `Value` = frames
//!   apresentados por frame real (o multiplicador do frame gen).
//!
//! Os dois providers são ligados com filtro de PID (só o jogo) e os eventos são lidos sem o TDH:
//! o payload do DXGI pelo layout do manifesto e os do TraceLogging pelos metadados que vêm no
//! próprio evento (`EVENT_HEADER_EXT_TYPE_EVENT_SCHEMA_TL`). Ligar o PCL faz o jogo emitir
//! também o "ping" de latência (mensagem ou tecla F13-F15 para a própria janela, a cada
//! 100-300 ms), como com o overlay da NVIDIA ou o FrameView.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{ERROR_SUCCESS, FILETIME, WIN32_ERROR};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, ENABLE_TRACE_PARAMETERS,
    ENABLE_TRACE_PARAMETERS_VERSION_2, EVENT_CONTROL_CODE_DISABLE_PROVIDER,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_FILTER_DESCRIPTOR, EVENT_FILTER_TYPE_PID,
    EVENT_HEADER_EXT_TYPE_EVENT_SCHEMA_TL, EVENT_HEADER_FLAG_32_BIT_HEADER, EVENT_RECORD,
    EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, EnableTraceEx2, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESS_TRACE_MODE_REAL_TIME, ProcessTrace, StartTraceW,
    WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::{GUID, PCWSTR, PWSTR};

use crate::win::wide;

/// Microsoft-Windows-DXGI.
pub const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);
/// DXGI `Present_Start`.
const DXGI_PRESENT_START: u16 = 42;
/// `DXGI_PRESENT_TEST`: só testa a oclusão, sem apresentar.
const DXGI_PRESENT_TEST: u32 = 0x1;
/// `PCLStatsTraceLoggingProvider` (`pclstats.h` do Streamline da NVIDIA).
pub const PCL: GUID = GUID::from_u128(0x0d216f06_82a6_4d49_bc4f_8f38ae56efab);
/// `PCLSTATS_SIMULATION_START`: um por frame real.
const MARKER_SIM_START: u64 = 0;
/// `PCLSTATS_NUM_PRESENTS_IN_BATCH`: `Value` = frames apresentados por frame real.
const MARKER_PRESENTS_IN_BATCH: u64 = 22;
/// `FlushTimer` em milissegundos (evntrace.h, Windows 8+): sem isso o mínimo é 1 s, e os frames
/// chegariam em blocos de até 1 s.
const EVENT_TRACE_USE_MS_FLUSH_TIMER: u32 = 0x10;
/// Entrega dos buffers a cada 100 ms.
const FLUSH_MS: u32 = 100;
/// Eventos guardados entre duas leituras do coletor (250 ms; 64 mil = 256 mil eventos/s):
/// acima disso os novos são descartados (contados), sem crescer a memória.
const MAX_INBOX: usize = 65_536;

/// O que um evento do jogo diz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Frame apresentado (real ou gerado), na swap chain dada.
    Present { swapchain: u64 },
    /// Início da simulação de um frame real (Reflex).
    SimStart,
    /// Frames apresentados por frame real (frame gen ativo quando > 1).
    Multiplier(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub pid: u32,
    /// Carimbo de tempo do evento (QPC: `ClientContext` = 1).
    pub qpc: i64,
    pub kind: Kind,
}

#[derive(Default)]
struct Inbox {
    events: Vec<Event>,
    dropped: u64,
}

/// Estado dividido entre o callback do ETW (thread do consumidor) e o coletor.
#[derive(Default)]
struct Shared {
    /// PID acompanhado (0 = nenhum): eventos de outros processos são descartados no callback,
    /// além do filtro de PID do próprio ETW.
    target: AtomicU32,
    inbox: Mutex<Inbox>,
}

// --- Leitura dos eventos (sem TDH) ---

/// Tamanho fixo de um tipo de entrada do TraceLogging (`TlgIn*` = `TDH_INTYPE`), ou `None` para
/// os de tamanho variável (strings, binário, SID, struct), que encerram a leitura dos campos.
fn tl_size(in_type: u8, ptr64: bool) -> Option<usize> {
    Some(match in_type {
        3 | 4 => 1,                 // INT8, UINT8
        5 | 6 => 2,                 // INT16, UINT16
        7 | 8 | 11 | 13 | 20 => 4,  // INT32, UINT32, FLOAT, BOOL32, HEXINT32
        9 | 10 | 12 | 17 | 21 => 8, // INT64, UINT64, DOUBLE, FILETIME, HEXINT64
        15 | 18 => 16,              // GUID, SYSTEMTIME
        16 => {
            if ptr64 {
                8
            } else {
                4
            }
        }
        _ => return None,
    })
}

/// Lê um byte e os de continuação (bit 0x80) que o seguem; devolve o primeiro.
fn chained(m: &[u8], pos: &mut usize, max: usize) -> Option<u8> {
    let first = *m.get(*pos)?;
    *pos += 1;
    let mut b = first;
    let mut n = 1;
    while b & 0x80 != 0 {
        if n >= max {
            return None;
        }
        b = *m.get(*pos)?;
        *pos += 1;
        n += 1;
    }
    Some(first)
}

fn cstr<'a>(m: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let rest = m.get(*pos..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    *pos += end + 1;
    Some(&rest[..end])
}

/// Campo de tamanho fixo de um evento TraceLogging: (nome, deslocamento no payload, tamanho).
type Field<'a> = (&'a [u8], usize, usize);

/// Evento TraceLogging: nome e os campos de topo de tamanho fixo `(nome, deslocamento, tamanho)`
/// no payload, até o primeiro de tamanho variável (ou array).
///
/// Metadados: `UINT16 TotalSize; UINT8 Extension[] (continua com 0x80); char Name[];` e, por
/// campo, `char Name[]; UINT8 InType (0x80: segue OutType); [UINT8 OutType (0x80: segue a tag,
/// 1-4 bytes)]`. `InType & 0x60` = array.
fn tl_fields(meta: &[u8], ptr64: bool) -> Option<(&[u8], Vec<Field<'_>>)> {
    let total = usize::from(u16::from_le_bytes(meta.get(..2)?.try_into().ok()?));
    let meta = meta.get(..total.min(meta.len()))?;
    let mut pos = 2;
    chained(meta, &mut pos, 8)?;
    let name = cstr(meta, &mut pos)?;
    let mut fields = Vec::new();
    let mut off = 0usize;
    while pos < meta.len() && fields.len() < 16 {
        let fname = cstr(meta, &mut pos)?;
        let in_type = *meta.get(pos)?;
        pos += 1;
        if in_type & 0x80 != 0 {
            let out = *meta.get(pos)?;
            pos += 1;
            if out & 0x80 != 0 {
                chained(meta, &mut pos, 4)?;
            }
        }
        if in_type & 0x60 != 0 {
            break;
        }
        let Some(size) = tl_size(in_type & 0x1F, ptr64) else {
            break;
        };
        fields.push((fname, off, size));
        off = off.checked_add(size)?;
    }
    Some((name, fields))
}

/// Inteiro (até 8 bytes, little-endian, sem sinal) do campo `name`.
fn tl_int(fields: &[Field<'_>], data: &[u8], name: &[u8]) -> Option<u64> {
    let &(_, off, size) = fields.iter().find(|f| f.0 == name)?;
    if size > 8 {
        return None;
    }
    let raw = data.get(off..off.checked_add(size)?)?;
    let mut b = [0u8; 8];
    b[..size].copy_from_slice(raw);
    Some(u64::from_le_bytes(b))
}

/// Classifica um evento já filtrado pelo PID: DXGI `Present_Start` pelo layout do manifesto
/// (primeiro campo = ponteiro da swap chain), PCL pelos metadados do TraceLogging.
fn classify(provider: &GUID, id: u16, ptr64: bool, data: &[u8], tl: Option<&[u8]>) -> Option<Kind> {
    if *provider == DXGI {
        if id != DXGI_PRESENT_START {
            return None;
        }
        let (swapchain, rest) = if ptr64 {
            (u64::from_le_bytes(data.get(..8)?.try_into().ok()?), 8)
        } else {
            (
                u64::from(u32::from_le_bytes(data.get(..4)?.try_into().ok()?)),
                4,
            )
        };
        // `Flags` depois do ponteiro: `DXGI_PRESENT_TEST` (0x1) não apresenta nada (teste de
        // oclusão), como no PresentMon.
        let flags = u32::from_le_bytes(data.get(rest..rest + 4)?.try_into().ok()?);
        if flags & DXGI_PRESENT_TEST != 0 {
            return None;
        }
        return Some(Kind::Present { swapchain });
    }
    if *provider != PCL {
        return None;
    }
    let (name, fields) = tl_fields(tl?, ptr64)?;
    let marker = tl_int(&fields, data, b"Marker")?;
    match (name, marker) {
        (b"PCLStatsEvent" | b"PCLStatsEventV2", MARKER_SIM_START) => Some(Kind::SimStart),
        (b"PCLStatsEventV3", MARKER_PRESENTS_IN_BATCH) => {
            // `Value` é Int32: negativo ou absurdo não vira multiplicador.
            let v = tl_int(&fields, data, b"Value")? as u32 as i32;
            (1..=16).contains(&v).then_some(Kind::Multiplier(v as u32))
        }
        _ => None,
    }
}

unsafe extern "system" fn on_event(r: *mut EVENT_RECORD) {
    // SAFETY: o ETW passa um registro válido durante o callback.
    let r = unsafe { &*r };
    if r.UserContext.is_null() {
        return;
    }
    // SAFETY: `UserContext` = o `Shared` do `Arc` que a thread do consumidor segura até o
    // `ProcessTrace` voltar.
    let shared = unsafe { &*r.UserContext.cast::<Shared>() };
    let h = &r.EventHeader;
    let target = shared.target.load(Ordering::Relaxed);
    if target == 0 || h.ProcessId != target {
        return;
    }
    let data: &[u8] = if r.UserData.is_null() || r.UserDataLength == 0 {
        &[]
    } else {
        // SAFETY: payload do evento com o tamanho informado, válido durante o callback.
        unsafe {
            std::slice::from_raw_parts(r.UserData.cast::<u8>(), usize::from(r.UserDataLength))
        }
    };
    let mut tl = None;
    if !r.ExtendedData.is_null() {
        for i in 0..usize::from(r.ExtendedDataCount.min(64)) {
            // SAFETY: `ExtendedDataCount` itens válidos durante o callback.
            let e = unsafe { &*r.ExtendedData.add(i) };
            if u32::from(e.ExtType) == EVENT_HEADER_EXT_TYPE_EVENT_SCHEMA_TL
                && e.DataPtr != 0
                && e.DataSize > 0
            {
                // SAFETY: item de dados estendidos com o tamanho informado.
                tl = Some(unsafe {
                    std::slice::from_raw_parts(e.DataPtr as *const u8, usize::from(e.DataSize))
                });
                break;
            }
        }
    }
    let ptr64 = u32::from(h.Flags) & EVENT_HEADER_FLAG_32_BIT_HEADER == 0;
    let Some(kind) = classify(&h.ProviderId, h.EventDescriptor.Id, ptr64, data, tl) else {
        return;
    };
    let mut inbox = shared.inbox.lock().unwrap_or_else(PoisonError::into_inner);
    if inbox.events.len() >= MAX_INBOX {
        inbox.dropped += 1;
    } else {
        inbox.events.push(Event {
            pid: h.ProcessId,
            qpc: h.TimeStamp,
            kind,
        });
    }
}

// --- Sessão ---

/// Buffer de `EVENT_TRACE_PROPERTIES` com espaço para o nome da sessão.
fn props_buf(name: &[u16]) -> Vec<u64> {
    let size = size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2 + 64;
    let mut buf = vec![0u64; size.div_ceil(8)];
    let p = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
    // SAFETY: buffer alinhado e maior que a estrutura.
    unsafe {
        (*p).Wnode.BufferSize = (buf.len() * 8) as u32;
        (*p).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    }
    buf
}

fn stop_by_name(name: &[u16]) -> WIN32_ERROR {
    let mut buf = props_buf(name);
    // SAFETY: propriedades num buffer próprio; nome terminado em zero.
    unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(name.as_ptr()),
            buf.as_mut_ptr().cast(),
            EVENT_TRACE_CONTROL_STOP,
        )
    }
}

pub fn qpc_now() -> i64 {
    let mut q = 0i64;
    // SAFETY: saída local.
    let _ = unsafe { QueryPerformanceCounter(&raw mut q) };
    q
}

/// Sessão ETW em tempo real do wintop. Parada (e consumidor encerrado) no `Drop`.
pub struct Session {
    handle: CONTROLTRACE_HANDLE,
    name: Vec<u16>,
    shared: Arc<Shared>,
    consumer: Option<JoinHandle<()>>,
    /// PID com os providers ligados agora.
    target: Option<u32>,
    freq: i64,
}

impl Session {
    /// Inicia a sessão `wintop-fps-s<sessão do Windows>` (uma instância do wintop por sessão:
    /// uma sessão ETW que sobrou de uma execução interrompida é parada antes).
    pub fn start() -> Result<Self, String> {
        let sid = crate::instance::session_id().map_err(|e| match e {
            crate::instance::Error::Os(e) => format!("session id: {}", e.message()),
            crate::instance::Error::AlreadyRunning => "session id".to_string(),
        })?;
        Self::start_named(&format!("wintop-fps-s{sid}"))
    }

    fn start_named(session: &str) -> Result<Self, String> {
        let name = wide(session);
        let _ = stop_by_name(&name);
        let mut buf = props_buf(&name);
        let p = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        // SAFETY: estrutura dentro do buffer próprio.
        unsafe {
            (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*p).Wnode.ClientContext = 1; // carimbos em QPC
            (*p).LogFileMode = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_USE_MS_FLUSH_TIMER;
            (*p).FlushTimer = FLUSH_MS;
            // Poucos eventos (um ou dois por frame do jogo): buffers pequenos.
            (*p).BufferSize = 16;
            (*p).MinimumBuffers = 4;
            (*p).MaximumBuffers = 32;
        }
        let mut handle = CONTROLTRACE_HANDLE::default();
        // SAFETY: propriedades com espaço para o nome; nome terminado em zero.
        let e = unsafe { StartTraceW(&raw mut handle, PCWSTR(name.as_ptr()), p) };
        if e != ERROR_SUCCESS {
            return Err(format!(
                "StartTrace: {}",
                windows::core::Error::from_hresult(e.to_hresult()).message()
            ));
        }
        let mut freq = 0i64;
        // SAFETY: saída local.
        let _ = unsafe { QueryPerformanceFrequency(&raw mut freq) };
        let shared = Arc::new(Shared::default());
        let ctx = Arc::clone(&shared);
        let (tx, rx) = std::sync::mpsc::channel();
        let consumer_name = name.clone();
        let consumer = std::thread::Builder::new()
            .name("etw".into())
            .spawn(move || {
                let mut lname = consumer_name;
                let mut log = EVENT_TRACE_LOGFILEW {
                    LoggerName: PWSTR(lname.as_mut_ptr()),
                    Context: Arc::as_ptr(&ctx).cast_mut().cast(),
                    ..Default::default()
                };
                log.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME
                    | PROCESS_TRACE_MODE_EVENT_RECORD
                    // Carimbos em QPC cru (sem isso o ETW converte para FILETIME).
                    | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
                log.Anonymous2.EventRecordCallback = Some(on_event);
                // SAFETY: estrutura local preenchida; nome e contexto vivos até o fim da thread.
                let h = unsafe { OpenTraceW(&raw mut log) };
                let ok = h.Value != u64::MAX;
                let _ = tx.send(ok);
                if ok {
                    // SAFETY: handle aberto acima; volta quando a sessão para.
                    let _ = unsafe {
                        ProcessTrace(&[h], None::<*const FILETIME>, None::<*const FILETIME>)
                    };
                    // SAFETY: handle aberto acima.
                    let _ = unsafe { CloseTrace(h) };
                }
                drop(ctx);
            })
            .map_err(|e| format!("ETW consumer thread: {e}"));
        let mut s = Self {
            handle,
            name,
            shared,
            consumer: None,
            target: None,
            freq: freq.max(1),
        };
        s.consumer = Some(consumer?);
        if !rx.recv().unwrap_or(false) {
            return Err("OpenTrace failed".into());
        }
        Ok(s)
    }

    fn enable(&self, guid: &GUID, pid: Option<u32>) -> WIN32_ERROR {
        let pids = [pid.unwrap_or(0)];
        let mut filter = EVENT_FILTER_DESCRIPTOR {
            Ptr: pids.as_ptr() as u64,
            Size: 4,
            Type: EVENT_FILTER_TYPE_PID,
        };
        let params = ENABLE_TRACE_PARAMETERS {
            Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
            EnableFilterDesc: &raw mut filter,
            FilterDescCount: 1,
            ..Default::default()
        };
        let code = if pid.is_some() {
            EVENT_CONTROL_CODE_ENABLE_PROVIDER
        } else {
            EVENT_CONTROL_CODE_DISABLE_PROVIDER
        };
        // SAFETY: sessão aberta; parâmetros e filtro vivos durante a chamada.
        unsafe {
            EnableTraceEx2(
                self.handle,
                guid,
                code.0,
                5, // TRACE_LEVEL_VERBOSE (os eventos do PCL não têm nível: 5)
                0,
                0,
                0,
                pid.map(|_| &raw const params),
            )
        }
    }

    /// Liga os providers só para `pid` (ou desliga tudo com `None`). Troca de jogo = desliga e
    /// liga de novo com o filtro novo. Eventos já recebidos de outro PID são descartados.
    pub fn target(&mut self, pid: Option<u32>) -> Result<(), String> {
        if self.target == pid {
            return Ok(());
        }
        if self.target.is_some() {
            let _ = self.enable(&DXGI, None);
            let _ = self.enable(&PCL, None);
        }
        self.shared
            .target
            .store(pid.unwrap_or(0), Ordering::Relaxed);
        self.target = pid;
        if pid.is_none() {
            return Ok(());
        }
        let e = self.enable(&DXGI, pid);
        if e == ERROR_SUCCESS {
            // O PCL só existe em jogos com Reflex: falha ao ligar não é erro da fonte. Ligado só
            // com o DXGI (senão ficaria ligado sem alvo, com o jogo emitindo o ping à toa).
            let _ = self.enable(&PCL, pid);
            Ok(())
        } else {
            self.target = None;
            self.shared.target.store(0, Ordering::Relaxed);
            Err(format!(
                "enable DXGI provider: {}",
                windows::core::Error::from_hresult(e.to_hresult()).message()
            ))
        }
    }

    /// Move os eventos recebidos para `out`; devolve quantos foram descartados (caixa cheia).
    pub fn drain(&self, out: &mut Vec<Event>) -> u64 {
        let mut inbox = self
            .shared
            .inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        out.append(&mut inbox.events);
        // Depois de um pico, devolve a memória.
        if inbox.events.capacity() > 4096 {
            inbox.events.shrink_to(1024);
        }
        std::mem::take(&mut inbox.dropped)
    }

    /// Frequência do QPC (ticks por segundo).
    pub fn freq(&self) -> i64 {
        self.freq
    }

    /// Microssegundos de `qpc` até agora (0 se o carimbo for do "futuro").
    #[cfg(test)]
    pub fn age_us(&self, qpc: i64) -> u64 {
        ticks_us(qpc_now().saturating_sub(qpc), self.freq)
    }
}

/// Ticks do QPC em µs (0 para negativos).
pub fn ticks_us(ticks: i64, freq: i64) -> u64 {
    if ticks <= 0 || freq <= 0 {
        return 0;
    }
    (i128::from(ticks) * 1_000_000 / i128::from(freq)).min(i128::from(u64::MAX)) as u64
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.enable(&DXGI, None);
        let _ = self.enable(&PCL, None);
        let mut buf = props_buf(&self.name);
        // SAFETY: sessão iniciada por este processo; propriedades num buffer próprio.
        let _ = unsafe {
            ControlTraceW(
                self.handle,
                PCWSTR::null(),
                buf.as_mut_ptr().cast(),
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        if let Some(c) = self.consumer.take() {
            let _ = c.join();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Metadados TraceLogging de um evento com campos `(nome, InType)`.
    pub(crate) fn tl_meta(name: &str, fields: &[(&str, u8)]) -> Vec<u8> {
        let mut m = vec![0, 0, 0]; // tamanho (abaixo) + 1 byte de extensão
        m.extend_from_slice(name.as_bytes());
        m.push(0);
        for (f, t) in fields {
            m.extend_from_slice(f.as_bytes());
            m.push(0);
            m.push(*t);
        }
        let n = m.len() as u16;
        m[..2].copy_from_slice(&n.to_le_bytes());
        m
    }

    pub(crate) fn pcl_v1(marker: u32, frame: u64) -> (Vec<u8>, Vec<u8>) {
        let meta = tl_meta("PCLStatsEvent", &[("Marker", 8), ("FrameID", 10)]);
        let mut d = marker.to_le_bytes().to_vec();
        d.extend_from_slice(&frame.to_le_bytes());
        (meta, d)
    }

    pub(crate) fn pcl_v3(marker: u32, frame: u64, value: i32) -> (Vec<u8>, Vec<u8>) {
        let meta = tl_meta(
            "PCLStatsEventV3",
            &[("Marker", 8), ("FrameID", 10), ("Value", 7)],
        );
        let mut d = marker.to_le_bytes().to_vec();
        d.extend_from_slice(&frame.to_le_bytes());
        d.extend_from_slice(&value.to_le_bytes());
        (meta, d)
    }

    #[test]
    fn present_do_dxgi() {
        // Ponteiro da swap chain (8 bytes) + Flags 0; em 32 bits, ponteiro de 4 + Flags 0.
        let mut d = 0x1234_5678_9ABC_u64.to_le_bytes().to_vec();
        d.extend(0u32.to_le_bytes());
        let mut d32 = 0x5678_9ABC_u32.to_le_bytes().to_vec();
        d32.extend(0u32.to_le_bytes());
        assert_eq!(
            classify(&DXGI, 42, true, &d, None),
            Some(Kind::Present {
                swapchain: 0x1234_5678_9ABC
            })
        );
        assert_eq!(
            classify(&DXGI, 42, false, &d32, None),
            Some(Kind::Present {
                swapchain: 0x5678_9ABC
            })
        );
        assert_eq!(classify(&DXGI, 43, true, &d, None), None);
        assert_eq!(classify(&DXGI, 42, true, &d[..3], None), None);
        assert_eq!(classify(&DXGI, 42, true, &d[..8], None), None);
        // DXGI_PRESENT_TEST: não é frame.
        d[8] = 1;
        assert_eq!(classify(&DXGI, 42, true, &d, None), None);
    }

    #[test]
    fn marcadores_do_pcl() {
        let (m, d) = pcl_v1(0, 77);
        assert_eq!(classify(&PCL, 0, true, &d, Some(&m)), Some(Kind::SimStart));
        let (m, d) = pcl_v1(5, 77);
        assert_eq!(classify(&PCL, 0, true, &d, Some(&m)), None);
        let (m, d) = pcl_v3(22, 77, 4);
        assert_eq!(
            classify(&PCL, 0, true, &d, Some(&m)),
            Some(Kind::Multiplier(4))
        );
        let (m, d) = pcl_v3(22, 77, -1);
        assert_eq!(classify(&PCL, 0, true, &d, Some(&m)), None);
        // Outro provider com o mesmo evento: ignorado.
        let (m, d) = pcl_v1(0, 1);
        assert_eq!(classify(&DXGI, 0, true, &d, Some(&m)), None);
        // Sem metadados ou payload curto.
        assert_eq!(classify(&PCL, 0, true, &d, None), None);
        assert_eq!(classify(&PCL, 0, true, &d[..2], Some(&m)), None);
    }

    #[test]
    fn metadados_com_outtype_tag_e_array() {
        // Marker com OutType (0x80) e tag (OutType 0x80 + 2 bytes); depois um array: fim.
        let mut m = vec![0, 0, 0x81, 0x01];
        m.extend_from_slice(b"Ev\0Marker\0");
        m.extend_from_slice(&[8 | 0x80, 0x80 | 1, 0x85, 0x01]);
        m.extend_from_slice(b"Arr\0");
        m.push(8 | 0x20);
        m.extend_from_slice(b"Depois\0");
        m.push(8);
        let n = m.len() as u16;
        m[..2].copy_from_slice(&n.to_le_bytes());
        let (name, f) = tl_fields(&m, true).expect("metadados");
        assert_eq!(name, b"Ev");
        assert_eq!(f, vec![(&b"Marker"[..], 0, 4)]);
    }

    #[test]
    fn tempo_em_microssegundos() {
        assert_eq!(ticks_us(10_000_000, 10_000_000), 1_000_000);
        assert_eq!(ticks_us(-5, 10_000_000), 0);
        assert_eq!(ticks_us(i64::MAX, 1), u64::MAX);
    }

    /// Os eventos vêm de processos quaisquer (o filtro é por PID, e o payload é do jogo):
    /// qualquer conteúdo vira `None` ou um `Kind` coerente, sem pânico.
    #[test]
    fn fuzz_eventos_etw() {
        let seeds: Vec<Vec<u8>> = [pcl_v1(0, 9), pcl_v3(22, 9, 4), pcl_v1(3, 1)]
            .into_iter()
            .map(|(m, d)| {
                // [len(meta) u16][meta][data]
                let mut b = (m.len() as u16).to_le_bytes().to_vec();
                b.extend(m);
                b.extend(d);
                b
            })
            .collect();
        crate::fuzz::run("eventos_etw", &seeds, 512, |r, b| {
            let n = b
                .get(..2)
                .map_or(0, |x| usize::from(u16::from_le_bytes([x[0], x[1]])));
            let rest = b.get(2..).unwrap_or(&[]);
            let (meta, data) = rest.split_at(n.min(rest.len()));
            let ptr64 = r.chance(2);
            match classify(&PCL, 0, ptr64, data, Some(meta)) {
                None | Some(Kind::SimStart) => {}
                Some(Kind::Multiplier(v)) => assert!((1..=16).contains(&v)),
                Some(Kind::Present { .. }) => panic!("present fora do DXGI"),
            }
            let _ = classify(&DXGI, 42, ptr64, data, None);
            if let Some((_, f)) = tl_fields(meta, ptr64) {
                assert!(f.len() <= 16);
                for (_, off, size) in f {
                    assert!(size <= 16 && off <= 16 * 16);
                }
            }
        });
    }

    /// Sessão real com um processo D3D11 (o `fps_probe`): frames por DXGI, entrega em ~100 ms.
    /// Terminal elevado; `cargo build --release --example fps_probe` antes.
    #[test]
    #[ignore = "precisa de terminal elevado e do fps_probe compilado"]
    fn sessao_real_com_fps_probe() {
        let Some(probe) = crate::collect::probe_tests::probe_exe() else {
            return;
        };
        let mut child = std::process::Command::new(&probe)
            .args(["--fps", "100", "--seconds", "8"])
            .spawn()
            .expect("fps_probe");
        let mut s = Session::start_named("wintop-fps-teste").expect("sessão (elevado?)");
        s.target(Some(child.id())).expect("target");
        std::thread::sleep(std::time::Duration::from_secs(2));
        let mut ev = Vec::new();
        s.drain(&mut ev);
        ev.clear();
        let t = std::time::Instant::now();
        let mut ages = Vec::new();
        while t.elapsed() < std::time::Duration::from_secs(3) {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let n0 = ev.len();
            s.drain(&mut ev);
            if let Some(last) = ev[n0..].last() {
                ages.push(s.age_us(last.qpc));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let presents = ev
            .iter()
            .filter(|e| matches!(e.kind, Kind::Present { .. }))
            .count();
        ages.sort_unstable();
        let med = ages.get(ages.len() / 2).copied().unwrap_or(u64::MAX);
        eprintln!("presents em 3 s: {presents}; idade mediana na entrega: {med} µs");
        assert!((240..=330).contains(&presents), "{presents}");
        assert!(med < 400_000, "entrega lenta: {med} µs");
    }
}
