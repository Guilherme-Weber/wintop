//! Motor de coleta: duas threads.
//!
//! - rápida: CPU, memória, discos (PDH), rede e GPU (NVML), no intervalo principal (padrão 1 s,
//!   ajustável em passos de 100 ms);
//! - lenta: leituras caras (PCIe do NVML; temperaturas de disco a partir da Etapa 2), a cada
//!   `SLOW_FACTOR` intervalos.
//!
//! Cada thread cria os próprios coletores (consultas PDH e NVML ficam na thread que as usa) e
//! publica o último resultado. A parada é imediata: as threads esperam numa `Condvar`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::collect::{FastCollectors, SlowCollectors};
use crate::model::{Fast, Slow};

pub const DEFAULT_INTERVAL_MS: u64 = 1000;
pub const MIN_INTERVAL_MS: u64 = 100;
pub const MAX_INTERVAL_MS: u64 = 10_000;
pub const STEP_MS: u64 = 100;
/// A thread lenta roda a cada 2 intervalos.
const SLOW_FACTOR: u32 = 2;

#[derive(Default)]
struct State {
    fast: Option<Fast>,
    slow: Option<Slow>,
    /// Incrementado a cada publicação (quem exibe sabe se há dado novo).
    generation: u64,
    /// Incrementado só pela thread rápida.
    fast_seq: u64,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    interval_ms: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Um pânico com o lock seguro não invalida os dados (só snapshots): segue em frente.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.load(Ordering::Relaxed))
    }

    /// Espera até `deadline` ou até a parada. `true` = parar.
    fn wait_until(&self, deadline: Instant) -> bool {
        let mut st = self.lock();
        loop {
            if st.stop {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self
                .wake
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Engine {
    pub fn start(interval_ms: u64) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            interval_ms: AtomicU64::new(clamp_interval(interval_ms)),
        });
        let fast = {
            let sh = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("coleta-rapida".into())
                .spawn(move || {
                    let mut c = FastCollectors::new();
                    let mut next = Instant::now();
                    loop {
                        let snap = c.collect();
                        {
                            let mut st = sh.lock();
                            st.fast = Some(snap);
                            st.fast_seq += 1;
                            st.generation += 1;
                        }
                        sh.wake.notify_all();
                        next = next_tick(next, sh.interval(), Instant::now());
                        if sh.wait_until(next) {
                            break;
                        }
                    }
                })
                .expect("criar a thread de coleta rápida")
        };
        let slow = {
            let sh = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("coleta-lenta".into())
                .spawn(move || {
                    let mut c = SlowCollectors::new();
                    let mut next = Instant::now();
                    loop {
                        let snap = c.collect();
                        {
                            let mut st = sh.lock();
                            st.slow = Some(snap);
                            st.generation += 1;
                        }
                        sh.wake.notify_all();
                        next = next_tick(next, sh.interval() * SLOW_FACTOR, Instant::now());
                        if sh.wait_until(next) {
                            break;
                        }
                    }
                })
                .expect("criar a thread de coleta lenta")
        };
        Self {
            shared,
            threads: vec![fast, slow],
        }
    }

    /// Últimos resultados e o número da coleta rápida (para saber se ela mudou).
    pub fn latest(&self) -> (Option<Fast>, Option<Slow>, u64) {
        let st = self.shared.lock();
        (st.fast.clone(), st.slow.clone(), st.fast_seq)
    }

    /// Espera uma geração maior que `seen` (ou o tempo acabar). Devolve a geração atual.
    pub fn wait_newer(&self, seen: u64, timeout: Duration) -> u64 {
        let st = self.shared.lock();
        let (st, _) = self
            .shared
            .wake
            .wait_timeout_while(st, timeout, |s| s.generation <= seen && !s.stop)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.generation
    }

    pub fn interval_ms(&self) -> u64 {
        self.shared.interval_ms.load(Ordering::Relaxed)
    }

    /// Ajusta o intervalo (teclas `+`/`-`); vale a partir da próxima espera.
    #[expect(dead_code, reason = "teclas + e - na TUI (Etapa 5)")]
    pub fn set_interval_ms(&self, ms: u64) -> u64 {
        let ms = clamp_interval(ms);
        self.shared.interval_ms.store(ms, Ordering::Relaxed);
        ms
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.wake.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Intervalo dentro dos limites, em passos de 100 ms.
pub fn clamp_interval(ms: u64) -> u64 {
    (ms / STEP_MS * STEP_MS).clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
}

/// Próximo instante de coleta: cadência fixa a partir de `prev`; se a coleta atrasou mais de
/// um intervalo, recomeça a partir de agora (sem rajada para "recuperar" o atraso).
fn next_tick(prev: Instant, interval: Duration, now: Instant) -> Instant {
    let next = prev + interval;
    if next <= now { now + interval } else { next }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervalo_em_passos() {
        assert_eq!(clamp_interval(0), 100);
        assert_eq!(clamp_interval(1050), 1000);
        assert_eq!(clamp_interval(99_999), 10_000);
    }

    #[test]
    fn cadencia_sem_rajada() {
        let t0 = Instant::now();
        let i = Duration::from_secs(1);
        assert_eq!(next_tick(t0, i, t0 + Duration::from_millis(10)), t0 + i);
        let late = t0 + Duration::from_millis(2500);
        assert_eq!(next_tick(t0, i, late), late + i);
    }
}
