//! Log de diagnóstico opcional (`debug = true` no `config.toml`).
//!
//! Desligado por padrão (zero custo: só uma leitura atômica). Quando ligado, registra em
//! `wintop-debug.log` (na pasta do `config.toml`: `%APPDATA%\wintop\` ou na pasta portátil)
//! todas as falhas:
//! - transições de estado das métricas (`OK/neutral -> FAIL`, mudança de causa e recuperação);
//! - falhas transitórias ou suprimidas (recusas do SMART dentro da tolerância, NaN no PDH da
//!   CPU, pânicos isolados por `catch_unwind`, erros silenciosos do atualizador, avisos do
//!   config).
//!
//! Toda escrita passa pelo `userfile` (o wintop roda elevado e a pasta é do usuário: sem seguir
//! links, sem hardlinks, caminho final conferido) e rotaciona o arquivo ao atingir `MAX_LOG_BYTES`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::model::{Fast, Fps, Metric, Missing, Slow};

pub const LOG_FILE: &str = "wintop-debug.log";
const LOG_BACKUP: &str = "wintop-debug.log.1";
/// Teto por arquivo (2 MB; mantém no máximo o atual + `.1` = ~4 MB).
const MAX_LOG_BYTES: u64 = 2 << 20;
/// Mesma mensagem direta em `debug::log` não repete dentro desta janela.
const LOG_COOLDOWN: Duration = Duration::from_secs(30);
/// Teto de linhas diretas por minuto (evita tempestade de I/O se um driver oscilar).
const MAX_DIRECT_PER_MIN: u32 = 120;
const MAX_RECENT_KEYS: usize = 256;

static ENABLED: AtomicBool = AtomicBool::new(false);

struct DirectLimit {
    /// `(source, message)` → `(último registro, repetições suprimidas desde então)`.
    recent: HashMap<(String, String), (Instant, u32)>,
    window_start: Instant,
    window_count: u32,
    window_suppressed: u32,
}

impl Default for DirectLimit {
    fn default() -> Self {
        Self {
            recent: HashMap::new(),
            window_start: Instant::now(),
            window_count: 0,
            window_suppressed: 0,
        }
    }
}

#[derive(Default)]
struct State {
    /// Última falha registrada por chave de métrica (deduplicação: só registra mudanças e
    /// recuperações, sem repetir a mesma linha a cada segundo).
    active: HashMap<String, String>,
    /// Proteção contra tempestade de eventos em `debug::log(...)`.
    direct: DirectLimit,
    /// Pasta alternativa só para testes unitários (sem encostar no AppData real).
    #[cfg(test)]
    test_dir: Option<(PathBuf, String)>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// Serializa apenas o append no arquivo (fora do mutex de `State`), evitando contention nas
/// coletas quando o modo debug grava em disco e impedindo violação de compartilhamento entre
/// threads gravando ao mesmo tempo.
fn io_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

struct WriteTarget {
    #[cfg(test)]
    dir: Option<(PathBuf, String)>,
}

fn write_target(st: &State) -> WriteTarget {
    #[cfg(test)]
    {
        WriteTarget {
            dir: st.test_dir.clone(),
        }
    }
    #[cfg(not(test))]
    {
        let _ = st;
        WriteTarget {}
    }
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Caminho do arquivo de log (`%APPDATA%\wintop\wintop-debug.log` ou ao lado do exe portátil).
pub fn log_path() -> Option<PathBuf> {
    crate::config::default_path().and_then(|p| p.parent().map(|d| d.join(LOG_FILE)))
}

/// Liga ou desliga o modo debug (na partida ou na recarga ao vivo do `config.toml`).
pub fn set_enabled(on: bool) {
    let prev = ENABLED.swap(on, Ordering::SeqCst);
    if on == prev {
        return;
    }
    let target = {
        let mut st = state().lock().unwrap_or_else(PoisonError::into_inner);
        st.active.clear();
        st.direct = DirectLimit::default();
        write_target(&st)
    };
    if on {
        let where_ = log_path().map_or_else(|| LOG_FILE.to_string(), |p| p.display().to_string());
        write_line(
            &target,
            "info",
            &format!(
                "debug logging enabled (wintop {}, file: {where_})",
                env!("CARGO_PKG_VERSION")
            ),
        );
    } else {
        write_line(&target, "info", "debug logging disabled");
    }
}

/// Registra um evento avulso (falha transitória, aviso, pânico) se o modo debug estiver ligado,
/// com deduplicação por `(source, message)` (30 s) e teto por minuto para evitar tempestade de I/O.
pub fn log(source: &str, message: &str) {
    if !is_enabled() {
        return;
    }
    let (target, payload) = {
        let mut st = state().lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let mut prefix = String::new();
        if now.duration_since(st.direct.window_start) >= Duration::from_secs(60) {
            if st.direct.window_suppressed > 0 {
                prefix = format!(
                    "[{}] [info] suppressed {} excess debug log events in the last minute\r\n",
                    timestamp(),
                    st.direct.window_suppressed
                );
            }
            st.direct.window_start = now;
            st.direct.window_count = 0;
            st.direct.window_suppressed = 0;
            st.direct
                .recent
                .retain(|_, (at, _)| now.duration_since(*at) < LOG_COOLDOWN * 2);
        }
        let key = (source.to_string(), message.to_string());
        let repeated_note = if let Some((at, sup)) = st.direct.recent.get_mut(&key) {
            if now.duration_since(*at) < LOG_COOLDOWN {
                *sup = sup.saturating_add(1);
                return;
            }
            let prev = std::mem::take(sup);
            *at = now;
            (prev > 0).then(|| format!(" (repeated {prev}x while suppressed)"))
        } else {
            if st.direct.recent.len() >= MAX_RECENT_KEYS {
                st.direct
                    .recent
                    .retain(|_, (at, _)| now.duration_since(*at) < LOG_COOLDOWN);
            }
            if st.direct.recent.len() < MAX_RECENT_KEYS {
                st.direct.recent.insert(key, (now, 0));
            }
            None
        };
        if st.direct.window_count >= MAX_DIRECT_PER_MIN {
            st.direct.window_suppressed = st.direct.window_suppressed.saturating_add(1);
            return;
        }
        st.direct.window_count += 1;
        let suffix = repeated_note.as_deref().unwrap_or("");
        prefix.push_str(&format!(
            "[{}] [{}] {}{suffix}\r\n",
            timestamp(),
            clean(source),
            clean(message)
        ));
        (write_target(&st), prefix)
    };
    write_raw(&target, payload.as_bytes());
}

fn timestamp() -> String {
    // SAFETY: sem pré-condições; devolve a estrutura por valor.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

fn clean(s: &str) -> String {
    let one_line = s.replace(['\r', '\n'], " ");
    crate::win::printable(&one_line)
}

fn write_line(target: &WriteTarget, source: &str, message: &str) {
    let line = format!(
        "[{}] [{}] {}\r\n",
        timestamp(),
        clean(source),
        clean(message)
    );
    write_raw(target, line.as_bytes());
}

fn write_raw(target: &WriteTarget, bytes: &[u8]) {
    let _io = io_lock().lock().unwrap_or_else(PoisonError::into_inner);
    #[cfg(test)]
    if let Some((base, name)) = &target.dir {
        if let Ok(Some(dir)) = crate::userfile::UserDir::open(base, name, true) {
            let _ = dir.append_rotating(LOG_FILE, LOG_BACKUP, MAX_LOG_BYTES, bytes);
        }
        return;
    }
    let _ = target;
    if let Ok(Some(dir)) = crate::config::open_home_dir(true) {
        let _ = dir.append_rotating(LOG_FILE, LOG_BACKUP, MAX_LOG_BYTES, bytes);
    }
}

/// Coleta linhas a gravar numa única passagem sobre um snapshot (evita abrir o arquivo para cada
/// campo individual).
struct Batch<'a> {
    active: &'a mut HashMap<String, String>,
    buf: String,
    ts: String,
}

impl<'a> Batch<'a> {
    fn new(active: &'a mut HashMap<String, String>) -> Self {
        Self {
            active,
            buf: String::new(),
            ts: timestamp(),
        }
    }

    fn metric<T>(&mut self, key: &str, m: &Metric<T>) {
        self.status(key, m.as_ref().err());
    }

    fn status(&mut self, key: &str, err: Option<&Missing>) {
        match err {
            Some(Missing::Fail(cause)) => {
                if self.active.get(key).is_none_or(|old| old != cause) {
                    self.active.insert(key.to_string(), cause.clone());
                    self.buf.push_str(&format!(
                        "[{}] [error] {}: {}\r\n",
                        self.ts,
                        clean(key),
                        clean(cause)
                    ));
                }
            }
            None | Some(Missing::Neutral(_)) => {
                if let Some(old) = self.active.remove(key) {
                    self.buf.push_str(&format!(
                        "[{}] [recovered] {} (was: {})\r\n",
                        self.ts,
                        clean(key),
                        clean(&old)
                    ));
                }
            }
        }
    }
}

/// Registra falhas novas, alteradas ou recuperadas de uma coleta rápida.
pub fn log_fast(f: &Fast) {
    if !is_enabled() {
        return;
    }
    let (target, out) = {
        let mut st = state().lock().unwrap_or_else(PoisonError::into_inner);
        let mut b = Batch::new(&mut st.active);
        b.metric("cpu", &f.cpu);
        if let Ok(c) = &f.cpu {
            c.visit_metrics(|name, e| b.status(&format!("cpu.{name}"), e));
        }
        b.metric("cpu_sensors", &f.cpu_sensors);
        if let Ok(s) = &f.cpu_sensors {
            s.visit_metrics(|name, e| b.status(&format!("cpu_sensors.{name}"), e));
        }
        b.metric("memory", &f.memory);
        if let Ok(m) = &f.memory {
            m.visit_metrics(|name, e| b.status(&format!("memory.{name}"), e));
        }
        b.metric("disks", &f.disks);
        if let Ok(list) = &f.disks {
            for d in list {
                let p = format!("disk.{}", d.number);
                d.visit_metrics(|name, e| b.status(&format!("{p}.{name}"), e));
            }
        }
        b.metric("nics", &f.nics);
        if let Ok(list) = &f.nics {
            for n in list {
                let p = format!("nic.{}", n.alias);
                n.visit_metrics(|name, e| b.status(&format!("{p}.{name}"), e));
            }
        }
        b.metric("gpus", &f.gpus);
        if let Ok(list) = &f.gpus {
            for g in list {
                let p = format!("gpu.{}", g.index);
                g.visit_metrics(|name, e| b.status(&format!("{p}.{name}"), e));
            }
        }
        b.metric("own", &f.own);
        if let Ok(o) = &f.own {
            o.visit_metrics(|name, e| b.status(&format!("own.{name}"), e));
        }
        let out = b.buf;
        (write_target(&st), out)
    };
    if !out.is_empty() {
        write_raw(&target, out.as_bytes());
    }
}

/// Registra falhas novas, alteradas ou recuperadas de uma coleta lenta.
pub fn log_slow(s: &Slow) {
    if !is_enabled() {
        return;
    }
    let (target, out) = {
        let mut st = state().lock().unwrap_or_else(PoisonError::into_inner);
        let mut b = Batch::new(&mut st.active);
        b.metric("slow.gpus", &s.gpus);
        if let Ok(list) = &s.gpus {
            for g in list {
                let p = format!("gpu.{}", g.index);
                g.visit_metrics(|name, e| b.status(&format!("{p}.{name}"), e));
            }
        }
        b.metric("slow.disk_temps", &s.disk_temps);
        if let Ok(list) = &s.disk_temps {
            for t in list {
                let p = format!("disk.{}", t.number);
                t.visit_metrics(|name, e| b.status(&format!("{p}.{name}"), e));
            }
        }
        if let Some(procs) = &s.procs {
            b.metric("slow.procs", procs);
        }
        let out = b.buf;
        (write_target(&st), out)
    };
    if !out.is_empty() {
        write_raw(&target, out.as_bytes());
    }
}

/// Registra falhas novas, alteradas ou recuperadas do RTSS / ETW / jogo.
pub fn log_fps(f: &Fps) {
    if !is_enabled() {
        return;
    }
    let (target, out) = {
        let mut st = state().lock().unwrap_or_else(PoisonError::into_inner);
        let mut b = Batch::new(&mut st.active);
        b.metric("rtss", &f.rtss);
        b.metric("rtss.pipe", &f.pipe);
        b.metric("rtss.etw", &f.etw);
        b.metric("rtss.game", &f.game);
        if let Ok(g) = &f.game {
            g.visit_metrics(|name, e| b.status(&format!("game.{name}"), e));
        }
        let out = b.buf;
        (write_target(&st), out)
    };
    if !out.is_empty() {
        write_raw(&target, out.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DiskTemp, fail, neutral};

    /// Restaura o estado global de debug e remove a pasta de teste no `Drop`.
    struct DebugGuard(PathBuf);
    impl Drop for DebugGuard {
        fn drop(&mut self) {
            ENABLED.store(false, Ordering::SeqCst);
            let mut st = state().lock().unwrap_or_else(PoisonError::into_inner);
            st.test_dir = None;
            st.active.clear();
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn registra_transicoes_e_recuperacoes_sem_repetir() {
        let base = crate::config::roaming().expect("Roaming");
        let dir_name = format!("wintop_test_{}_debug", std::process::id());
        let dir_path = base.join(&dir_name);
        let _ = std::fs::remove_dir_all(&dir_path);
        let _guard = DebugGuard(dir_path.clone());

        {
            let mut st = state().lock().unwrap_or_else(PoisonError::into_inner);
            st.test_dir = Some((base, dir_name));
            st.active.clear();
        }
        ENABLED.store(false, Ordering::SeqCst);

        // Desligado: nada é gravado.
        log("test", "nao deve aparecer");
        assert!(!dir_path.join(LOG_FILE).exists());

        set_enabled(true);
        // Mesma mensagem direta repetida dentro do cooldown: só grava uma vez.
        log("smart", "disk 0 failed\nwith newline");
        log("smart", "disk 0 failed\nwith newline");

        let slow_err = Slow {
            gpus: Ok(vec![]),
            disk_temps: Ok(vec![DiskTemp {
                number: 0,
                model: Some("Netac".into()),
                celsius: fail("SMART: IOCTL 0x2D1100: error 0x1F"),
                source: "",
                health: neutral("not reported"),
            }]),
            procs: None,
            game_ram: None,
            took: std::time::Duration::ZERO,
        };
        // Mesma falha repetida: grava só uma vez.
        log_slow(&slow_err);
        log_slow(&slow_err);

        let slow_ok = Slow {
            disk_temps: Ok(vec![DiskTemp {
                number: 0,
                model: Some("Netac".into()),
                celsius: Ok(38),
                source: "SMART 194",
                health: Ok(false),
            }]),
            ..slow_err
        };
        log_slow(&slow_ok);

        set_enabled(false);

        let text = std::fs::read_to_string(dir_path.join(LOG_FILE)).expect("log gravado");

        assert!(!text.contains("nao deve aparecer"), "{text}");
        assert!(text.contains("[info] debug logging enabled"), "{text}");
        assert_eq!(
            text.matches("[smart] disk 0 failed with newline").count(),
            1,
            "não repete evento direto igual dentro do cooldown:\n{text}"
        );
        assert_eq!(
            text.matches("[error] disk.0.temp: SMART: IOCTL 0x2D1100: error 0x1F")
                .count(),
            1,
            "não repete a mesma falha:\n{text}"
        );
        assert!(
            text.contains("[recovered] disk.0.temp (was: SMART: IOCTL 0x2D1100: error 0x1F)"),
            "{text}"
        );
        assert!(text.contains("[info] debug logging disabled"), "{text}");
    }
}
