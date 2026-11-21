//! Processos (aba Processos e colunas de RAM/CPU da aba GPU): uma chamada de
//! `NtQuerySystemInformation(SystemProcessInformation)`, sem abrir nenhum processo (nada de
//! handle no jogo, regra do anticheat) e sem elevação.
//!
//! - CPU % do sistema inteiro (como o Task Manager e o consumo do próprio wintop): Δ(kernel +
//!   user do processo) ÷ Δ(kernel + user do sistema, `GetSystemTimes`).
//! - Memória = working set privado (a coluna "Memory" do Task Manager); "commit" = bytes
//!   privados confirmados (`PagefileUsage`).
//! - I/O = bytes lidos e gravados por segundo (todo I/O do processo: disco, rede e dispositivos,
//!   como as colunas "I/O read/write bytes" do Task Manager).
//! - Um PID reaproveitado é reconhecido pelo instante de criação: não herda o tempo do antigo.
//!
//! O buffer é lido com limites (offsets do layout x64 de `SYSTEM_PROCESS_INFORMATION`, o mesmo
//! do `windows::Win32::System::WindowsProgramming`, onde parte dos campos é "Reserved").

use std::collections::HashMap;
use std::time::Instant;

use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
use windows::Win32::Foundation::{FILETIME, STATUS_INFO_LENGTH_MISMATCH};
use windows::Win32::System::Threading::GetSystemTimes;

use crate::model::{Metric, Proc, fail};
use crate::win::filetime;

// Offsets em `SYSTEM_PROCESS_INFORMATION` (x64).
const NEXT: usize = 0x00;
const THREADS: usize = 0x04;
const WS_PRIVATE: usize = 0x08;
const CREATE_TIME: usize = 0x20;
const USER_TIME: usize = 0x28;
const KERNEL_TIME: usize = 0x30;
const NAME_LEN: usize = 0x38;
const NAME_PTR: usize = 0x40;
const PID: usize = 0x50;
const PARENT: usize = 0x58;
const HANDLES: usize = 0x60;
const PAGEFILE: usize = 0xB8;
const READ_BYTES: usize = 0xE8;
const WRITE_BYTES: usize = 0xF0;
/// Tamanho da parte fixa (as threads vêm depois).
const ENTRY: usize = 0x100;
/// Teto do buffer (milhares de processos com centenas de threads cabem em poucos MB).
const MAX_BUF: usize = 64 << 20;

/// Um processo cru, antes das taxas.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Raw {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
    pub create: u64,
    /// Kernel + user, 100 ns.
    pub time: u64,
    pub threads: u32,
    pub handles: u32,
    pub mem: u64,
    pub commit: u64,
    pub read: u64,
    pub write: u64,
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// Percorre as entradas. `base` = endereço do buffer na memória: o nome de cada processo é um
/// ponteiro para dentro do mesmo buffer, aceito só se cair dentro dele.
pub(super) fn parse(b: &[u8], base: usize) -> Result<Vec<Raw>, String> {
    let oob = || "process list read out of bounds".to_string();
    let mut out = Vec::new();
    let mut off = 0usize;
    loop {
        let e = b.get(off..off + ENTRY).ok_or_else(oob)?;
        let pid = u64_at(e, PID).ok_or_else(oob)? as u32;
        let len = usize::from(u16::from_le_bytes([e[NAME_LEN], e[NAME_LEN + 1]]));
        let ptr = u64_at(e, NAME_PTR).ok_or_else(oob)? as usize;
        let name = if pid == 0 {
            "Idle".to_string()
        } else if len == 0 || ptr == 0 {
            "System".to_string()
        } else {
            let start = ptr.checked_sub(base).ok_or_else(oob)?;
            let end = start.checked_add(len).ok_or_else(oob)?;
            let raw = b.get(start..end).ok_or_else(oob)?;
            let wide: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            String::from_utf16_lossy(&wide)
        };
        out.push(Raw {
            pid,
            parent: u64_at(e, PARENT).ok_or_else(oob)? as u32,
            name,
            create: u64_at(e, CREATE_TIME).ok_or_else(oob)?,
            time: u64_at(e, USER_TIME)
                .ok_or_else(oob)?
                .wrapping_add(u64_at(e, KERNEL_TIME).ok_or_else(oob)?),
            threads: u32_at(e, THREADS).ok_or_else(oob)?,
            handles: u32_at(e, HANDLES).ok_or_else(oob)?,
            mem: u64_at(e, WS_PRIVATE).ok_or_else(oob)?,
            commit: u64_at(e, PAGEFILE).ok_or_else(oob)?,
            read: u64_at(e, READ_BYTES).ok_or_else(oob)?,
            write: u64_at(e, WRITE_BYTES).ok_or_else(oob)?,
        });
        let next = u32_at(e, NEXT).ok_or_else(oob)? as usize;
        if next == 0 {
            return Ok(out);
        }
        if next < ENTRY {
            return Err(format!("process entry of {next} bytes is too small"));
        }
        off = off.checked_add(next).ok_or_else(oob)?;
    }
}

fn system_time() -> Result<u64, String> {
    let (mut i, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: três FILETIME locais.
    unsafe { GetSystemTimes(Some(&raw mut i), Some(&raw mut k), Some(&raw mut u)) }
        .map_err(|e| format!("GetSystemTimes: {e}"))?;
    Ok(filetime(k) + filetime(u))
}

/// Leitura anterior de um processo: (instante de criação, tempo de CPU, bytes lidos, gravados).
type Prev = (u64, u64, u64, u64);

pub(super) struct ProcCollector {
    /// Buffer reaproveitado (alinhado a 8: as entradas têm campos de 8 bytes).
    buf: Vec<u64>,
    prev: HashMap<u32, Prev>,
    /// Tempo do sistema e instante da leitura anterior.
    prev_sys: Option<(u64, Instant)>,
}

impl ProcCollector {
    pub(super) fn new() -> Self {
        Self {
            // Alocado na 1ª leitura (aba aberta) e liberado no `reset` (aba fechada).
            buf: Vec::new(),
            prev: HashMap::new(),
            prev_sys: None,
        }
    }

    /// Sem leitura anterior (a aba acabou de abrir): as taxas só existem a partir da 2ª.
    pub(super) fn primed(&self) -> bool {
        self.prev_sys.is_some()
    }

    /// Esquece as leituras (aba fechada: a próxima abertura começa do zero).
    pub(super) fn reset(&mut self) {
        self.prev = HashMap::new();
        self.prev_sys = None;
        self.buf = Vec::new();
    }

    fn query(&mut self) -> Result<Vec<Raw>, String> {
        if self.buf.is_empty() {
            self.buf = vec![0; (512 << 10) / 8];
        }
        loop {
            let bytes = self.buf.len() * 8;
            let mut need = 0u32;
            // SAFETY: buffer próprio de `bytes` bytes; `need` local.
            let st = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    self.buf.as_mut_ptr().cast(),
                    bytes as u32,
                    &raw mut need,
                )
            };
            if st == STATUS_INFO_LENGTH_MISMATCH {
                // Folga para processos que surgem entre as duas chamadas.
                let want = (need as usize).max(bytes) + (64 << 10);
                if want > MAX_BUF {
                    return Err(format!("process list needs {want} bytes"));
                }
                self.buf = vec![0; want.div_ceil(8)];
                continue;
            }
            if st.is_err() {
                return Err(format!("NtQuerySystemInformation: 0x{:08X}", st.0));
            }
            let len = (need as usize).min(bytes);
            // SAFETY: `len` <= tamanho do buffer; u64 -> u8 sem requisito de alinhamento.
            let b = unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), len) };
            return parse(b, self.buf.as_ptr() as usize);
        }
    }

    pub(super) fn read(&mut self) -> Metric<Vec<Proc>> {
        let raw = self.query().or_else(fail)?;
        let sys = system_time().or_else(fail)?;
        let now = Instant::now();
        let prev_sys = self.prev_sys.replace((sys, now));
        let mut next = HashMap::with_capacity(raw.len());
        let mut out = Vec::with_capacity(raw.len());
        for r in raw {
            next.insert(r.pid, (r.create, r.time, r.read, r.write));
            // O Idle (PID 0) não é processo: o tempo dele é o ocioso da máquina.
            if r.pid == 0 {
                continue;
            }
            let old = self.prev.get(&r.pid).filter(|p| p.0 == r.create);
            let (cpu, io_read, io_write) = match (old, prev_sys) {
                (Some(p), Some((s0, t0))) => {
                    let ds = sys.saturating_sub(s0);
                    let secs = now.duration_since(t0).as_secs_f64();
                    let cpu = if ds > 0 {
                        (r.time.saturating_sub(p.1) as f64 / ds as f64 * 100.0).clamp(0.0, 100.0)
                    } else {
                        0.0
                    };
                    let rate = |a: u64, b: u64| {
                        if secs > 0.0 {
                            a.saturating_sub(b) as f64 / secs
                        } else {
                            0.0
                        }
                    };
                    (cpu, rate(r.read, p.2), rate(r.write, p.3))
                }
                _ => (0.0, 0.0, 0.0),
            };
            out.push(Proc {
                pid: r.pid,
                parent: r.parent,
                name: r.name,
                cpu,
                mem: r.mem,
                commit: r.commit,
                threads: r.threads,
                handles: r.handles,
                io_read,
                io_write,
            });
        }
        self.prev = next;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monta uma entrada com o nome logo depois da parte fixa.
    fn entry(pid: u64, name: &str, next: u32, base: usize, at: usize) -> Vec<u8> {
        let mut e = vec![0u8; ENTRY];
        let w: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        e[NEXT..NEXT + 4].copy_from_slice(&next.to_le_bytes());
        e[THREADS..THREADS + 4].copy_from_slice(&7u32.to_le_bytes());
        e[NAME_LEN..NAME_LEN + 2].copy_from_slice(&(w.len() as u16).to_le_bytes());
        let ptr = if w.is_empty() { 0 } else { base + at + ENTRY };
        e[NAME_PTR..NAME_PTR + 8].copy_from_slice(&(ptr as u64).to_le_bytes());
        e[PID..PID + 8].copy_from_slice(&pid.to_le_bytes());
        e[WS_PRIVATE..WS_PRIVATE + 8].copy_from_slice(&4096u64.to_le_bytes());
        e[USER_TIME..USER_TIME + 8].copy_from_slice(&10u64.to_le_bytes());
        e[KERNEL_TIME..KERNEL_TIME + 8].copy_from_slice(&5u64.to_le_bytes());
        e.extend(w);
        e.resize(e.len().next_multiple_of(8), 0);
        e
    }

    #[test]
    fn percorre_as_entradas() {
        let base = 0x1000_0000;
        let a = entry(0, "", 0, base, 0);
        let mut b = a;
        let next = b.len() as u32;
        b[NEXT..NEXT + 4].copy_from_slice(&next.to_le_bytes());
        b.extend(entry(1234, "game.exe", 0, base, next as usize));
        let list = parse(&b, base).expect("lista");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "Idle");
        assert_eq!(list[1].pid, 1234);
        assert_eq!(list[1].name, "game.exe");
        assert_eq!(list[1].time, 15);
        assert_eq!(list[1].threads, 7);
        assert_eq!(list[1].mem, 4096);
    }

    #[test]
    fn ponteiro_fora_do_buffer_e_erro() {
        let base = 0x1000_0000;
        let mut e = entry(8, "x.exe", 0, base, 0);
        // Nome apontando para antes do buffer.
        e[NAME_PTR..NAME_PTR + 8].copy_from_slice(&0x10u64.to_le_bytes());
        assert!(parse(&e, base).is_err());
        // Entrada cortada.
        assert!(parse(&e[..ENTRY - 1], base).is_err());
        // Próxima entrada menor que a parte fixa (laço infinito ou leitura torta).
        let mut l = entry(8, "x.exe", 4, base, 0);
        l.extend(vec![0; ENTRY]);
        assert!(parse(&l, base).is_err());
    }

    /// Carga conhecida: uma thread ocupada o tempo todo = 1 lógico inteiro = 100/N % do
    /// sistema; e I/O conhecido: 64 MB gravados num arquivo temporário.
    #[test]
    #[ignore = "mede carga real: rodar sozinho (--test-threads=1)"]
    fn carga_conhecida() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let n = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f64;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let spin = std::thread::spawn(move || {
            let mut x = 0u64;
            while !s.load(Ordering::Relaxed) {
                x = std::hint::black_box(x.wrapping_add(1));
            }
        });
        let mut c = ProcCollector::new();
        c.read().expect("1ª leitura");
        let t0 = std::time::Instant::now();
        let path = std::env::temp_dir().join(format!("wintop_io_{}.bin", std::process::id()));
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).expect("arquivo temporário");
            let block = vec![7u8; 1 << 20];
            for _ in 0..64 {
                f.write_all(&block).expect("gravação");
            }
            f.sync_all().expect("sync");
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let list = c.read().expect("2ª leitura");
        let secs = t0.elapsed().as_secs_f64();
        stop.store(true, Ordering::Relaxed);
        spin.join().expect("thread");
        let _ = std::fs::remove_file(&path);
        let me = list
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("o próprio processo");
        let expected = 100.0 / n;
        let written = me.io_write * secs / f64::from(1 << 20);
        println!(
            "CPU {:.2}% (esperado ~{expected:.2}%), gravado ~{written:.1} MB",
            me.cpu
        );
        assert!(
            (me.cpu - expected).abs() < expected * 0.25,
            "{} × {expected}",
            me.cpu
        );
        assert!(written >= 60.0, "{written} MB");
    }

    #[test]
    fn lista_real_tem_o_proprio_processo() {
        let mut c = ProcCollector::new();
        assert!(!c.primed());
        c.read().expect("1ª leitura");
        assert!(c.primed());
        std::thread::sleep(std::time::Duration::from_millis(200));
        let list = c.read().expect("2ª leitura");
        let me = std::process::id();
        let p = list
            .iter()
            .find(|p| p.pid == me)
            .expect("o próprio processo");
        assert!(p.name.to_ascii_lowercase().contains("wintop"), "{}", p.name);
        assert!(p.threads >= 1 && p.mem > 0, "{p:?}");
        assert!(list.iter().any(|p| p.pid == 4), "System");
        assert!(list.iter().all(|p| p.pid != 0), "sem o Idle");
    }
}
