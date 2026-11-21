//! Motor de coleta: três threads.
//!
//! - rápida: CPU, sensores do PawnIO, memória, discos, rede, GPU e o consumo do próprio wintop,
//!   no intervalo principal (padrão 1 s, ajustável em passos de 100 ms). A 1ª coleta é de
//!   aquecimento (contadores de taxa) e não é publicada;
//! - lenta: leituras caras (PCIe do NVML, temperatura dos discos), a cada `SLOW_FACTOR`
//!   intervalos e nunca abaixo de `SLOW_FLOOR`;
//! - RTSS: pipe de frametime + Shared Memory (250 ms com jogo, 1 s sem).
//!
//! Cada thread cria os próprios coletores (as consultas PDH ficam na thread que as usa); o NVML
//! é um só, dividido entre a rápida e a lenta. Cada thread publica o último resultado com um
//! número de sequência; quem exibe só copia o que mudou. A parada é imediata: as esperas
//! acordam pela `Condvar` (e pelo evento de parada, no pipe).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::panic::{AssertUnwindSafe, catch_unwind};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

use crate::collect::{FastCollectors, RtssCollector, SlowCollectors, rtss};
use crate::model::{Fast, Fps, Slow};

pub const DEFAULT_INTERVAL_MS: u64 = 1000;
pub const MIN_INTERVAL_MS: u64 = 100;
pub const MAX_INTERVAL_MS: u64 = 10_000;
pub const STEP_MS: u64 = 100;
const RTSS_GAME_PERIOD: Duration = Duration::from_millis(250);
const RTSS_IDLE_PERIOD: Duration = Duration::from_secs(1);
/// A thread lenta roda a cada 2 intervalos, nunca abaixo de 2 s (PCIe custa ~46 ms por GPU;
/// a 100 ms de intervalo ela rodaria 5×/s).
const SLOW_FACTOR: u32 = 2;
const SLOW_FLOOR: Duration = Duration::from_secs(2);
/// Espera entre a coleta de aquecimento e a 1ª publicada.
const WARMUP: Duration = Duration::from_millis(300);

#[derive(Default)]
struct State {
    fast: Option<Fast>,
    slow: Option<Slow>,
    fps: Option<Fps>,
    /// Números de publicação de cada thread (quem exibe sabe o que mudou).
    fast_seq: u64,
    slow_seq: u64,
    fps_seq: u64,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    interval_ms: AtomicU64,
    /// Cópia de `State::stop` legível sem o lock (laços longos dos coletores).
    stopping: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Um pânico com o lock seguro não invalida os dados (só snapshots): segue em frente.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Espera `d` ou a parada. `true` = parar.
    fn sleep(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
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

    /// Espera a próxima coleta: `base` é o instante agendado da anterior e o período é
    /// `intervalo × factor`. Um intervalo novo (teclas `+`/`-`) vale na hora: o prazo é refeito a
    /// partir de `base` (se já passou, coleta agora). Devolve a nova base, ou `None` = parar.
    fn wait_tick(&self, base: Instant, factor: u32, floor: Duration) -> Option<Instant> {
        let period = |ms: u64| (Duration::from_millis(ms) * factor).max(floor);
        let mut st = self.lock();
        // Lido com o lock: `set_interval_ms` grava com o lock, então nenhuma mudança se perde
        // entre esta leitura e a espera.
        let mut seen = self.interval_ms.load(Ordering::Relaxed);
        let mut target = next_tick(base, period(seen), Instant::now());
        loop {
            if st.stop {
                return None;
            }
            let ms = self.interval_ms.load(Ordering::Relaxed);
            let now = Instant::now();
            if ms != seen {
                seen = ms;
                target = (base + period(ms)).max(now);
            }
            if now >= target {
                return Some(target);
            }
            st = self
                .wake
                .wait_timeout(st, target - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// Últimas publicações já vistas por quem exibe.
#[derive(Default)]
pub struct Seen {
    fast: u64,
    slow: u64,
    fps: u64,
}

/// Dados novos desde a última consulta.
#[derive(Default)]
pub struct Updates {
    pub fast: Option<Fast>,
    pub slow: Option<Slow>,
    pub fps: Option<Fps>,
}

pub struct Engine {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    stop_event: Arc<OwnedHandle>,
}

impl Engine {
    pub fn start(interval_ms: u64) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            interval_ms: AtomicU64::new(clamp_interval(interval_ms)),
            stopping: AtomicBool::new(false),
        });
        // Um NVML para as duas threads (iniciado aqui, antes delas: sem nvmlInit simultâneo).
        let nvml = crate::collect::shared_nvml();
        let fast = {
            let sh = Arc::clone(&shared);
            let nvml_fast = nvml.clone();
            std::thread::Builder::new()
                .name("coleta-rapida".into())
                .spawn(move || {
                    // Cada coletor já roda sob catch_unwind; este cobre o resto da thread.
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        let mut c = FastCollectors::new(nvml_fast);
                        if sh.stopping.load(Ordering::Relaxed) {
                            return;
                        }
                        // Aquecimento: os contadores de taxa só têm valor na 2ª leitura. A 1ª
                        // coleta não é publicada (a tela não abre cheia de "waiting...").
                        let _ = c.collect();
                        if sh.sleep(WARMUP) {
                            return;
                        }
                        let mut next = Instant::now();
                        loop {
                            let snap = c.collect();
                            {
                                let mut st = sh.lock();
                                st.fast = Some(snap);
                                st.fast_seq += 1;
                            }
                            match sh.wait_tick(next, 1, Duration::ZERO) {
                                Some(t) => next = t,
                                None => break,
                            }
                        }
                    }));
                })
                .expect("failed to create the fast collection thread")
        };
        // Evento manual de parada da thread do RTSS: acorda a espera do pipe na hora.
        // SAFETY: evento sem nome; o handle passa a ter dono logo abaixo.
        let stop_event = Arc::new(unsafe {
            OwnedHandle::from_raw_handle(
                CreateEventW(None, true, false, None)
                    .expect("failed to create the stop event")
                    .0,
            )
        });
        let rtss = {
            let sh = Arc::clone(&shared);
            let stop = Arc::clone(&stop_event);
            std::thread::Builder::new()
                .name("rtss".into())
                .spawn(move || rtss_thread(&sh, HANDLE(stop.as_raw_handle())))
                .expect("failed to create the RTSS thread")
        };
        let slow = {
            let sh = Arc::clone(&shared);
            let nvml_slow = nvml;
            std::thread::Builder::new()
                .name("coleta-lenta".into())
                .spawn(move || {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        let mut c = SlowCollectors::new(nvml_slow);
                        let mut next = Instant::now();
                        loop {
                            if sh.stopping.load(Ordering::Relaxed) {
                                return;
                            }
                            let snap = c.collect(&sh.stopping);
                            {
                                let mut st = sh.lock();
                                st.slow = Some(snap);
                                st.slow_seq += 1;
                            }
                            match sh.wait_tick(next, SLOW_FACTOR, SLOW_FLOOR) {
                                Some(t) => next = t,
                                None => break,
                            }
                        }
                    }));
                })
                .expect("failed to create the slow collection thread")
        };
        Self {
            shared,
            threads: vec![fast, slow, rtss],
            stop_event,
        }
    }

    /// Últimos resultados e o número da coleta rápida (testes).
    #[cfg(test)]
    pub fn latest(&self) -> (Option<Fast>, Option<Slow>, u64) {
        let st = self.shared.lock();
        (st.fast.clone(), st.slow.clone(), st.fast_seq)
    }

    /// O que mudou desde `seen` (e atualiza `seen`). Só clona o que é novo.
    pub fn updates(&self, seen: &mut Seen) -> Updates {
        let st = self.shared.lock();
        let mut u = Updates::default();
        if st.fast_seq != seen.fast {
            seen.fast = st.fast_seq;
            u.fast.clone_from(&st.fast);
        }
        if st.slow_seq != seen.slow {
            seen.slow = st.slow_seq;
            u.slow.clone_from(&st.slow);
        }
        if st.fps_seq != seen.fps {
            seen.fps = st.fps_seq;
            u.fps.clone_from(&st.fps);
        }
        u
    }

    /// Último estado do RTSS e do jogo.
    #[cfg(test)]
    pub fn fps(&self) -> Option<Fps> {
        self.shared.lock().fps.clone()
    }

    pub fn interval_ms(&self) -> u64 {
        self.shared.interval_ms.load(Ordering::Relaxed)
    }

    /// Ajusta o intervalo (teclas `+`/`-`); vale a partir da próxima espera.
    pub fn set_interval_ms(&self, ms: u64) -> u64 {
        let ms = clamp_interval(ms);
        {
            let _st = self.shared.lock();
            self.shared.interval_ms.store(ms, Ordering::Relaxed);
        }
        // Acorda as threads para refazerem o prazo com o intervalo novo.
        self.shared.wake.notify_all();
        ms
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.stopping.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
        // SAFETY: evento válido enquanto o motor existe.
        unsafe {
            let _ = SetEvent(HANDLE(self.stop_event.as_raw_handle()));
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Thread do RTSS: amostra a cada 250 ms com jogo (1 s sem) e atende o pipe no meio. Um pânico
/// publica a falha (a interface não fica parada no último valor) e encerra a thread.
fn rtss_thread(sh: &Shared, stop: HANDLE) {
    let publish = |snap: Fps| {
        let mut st = sh.lock();
        st.fps = Some(snap);
        st.fps_seq += 1;
    };
    let Ok(mut c) = catch_unwind(|| RtssCollector::new(true, true)) else {
        publish(rtss::failed("RTSS collector panicked on startup"));
        return;
    };
    let mut last: Option<Fps> = None;
    loop {
        if sh.lock().stop {
            return;
        }
        let Ok(snap) = catch_unwind(AssertUnwindSafe(|| c.sample())) else {
            publish(rtss::failed("RTSS collector panicked"));
            return;
        };
        let period = if snap.game.is_ok() {
            RTSS_GAME_PERIOD
        } else {
            RTSS_IDLE_PERIOD
        };
        // Com jogo, toda amostra vai para o gráfico (FPS travado por VSync se repete e mesmo
        // assim é um ponto novo). Sem jogo o estado se repete a cada segundo: só o que mudou.
        if snap.game.is_ok() {
            last = None;
            publish(snap);
        } else if last.as_ref() != Some(&snap) {
            last = Some(snap.clone());
            publish(snap);
        }
        match catch_unwind(AssertUnwindSafe(|| c.pump(Instant::now() + period, stop))) {
            Ok(false) => {}
            Ok(true) => return,
            Err(_) => {
                publish(rtss::failed("RTSS collector panicked"));
                return;
            }
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

    /// Parada imediata: o `Drop` acorda as três threads (a do RTSS pelo evento de parada).
    #[test]
    #[ignore = "hardware: inicia todos os coletores (terminal elevado)"]
    fn parada_rapida() {
        let e = Engine::start(DEFAULT_INTERVAL_MS);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(e.fps().is_some(), "a thread do RTSS publicou");
        let t = Instant::now();
        drop(e);
        let took = t.elapsed();
        println!("parada em {took:?}");
        assert!(took < Duration::from_millis(300), "{took:?}");
    }

    /// O intervalo novo vale a partir da próxima espera: de 1 s para 300 ms, ~10 coletas em 3 s.
    #[test]
    #[ignore = "hardware: inicia todos os coletores (terminal elevado)"]
    fn intervalo_ajustavel() {
        let e = Engine::start(1000);
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(e.set_interval_ms(250), 200, "passos de 100 ms");
        assert_eq!(e.set_interval_ms(300), 300);
        // A troca acorda a espera antiga: a próxima coleta sai em <= 300 ms.
        let (_, _, before) = e.latest();
        std::thread::sleep(Duration::from_secs(3));
        let (_, _, after) = e.latest();
        let n = after - before;
        println!("{n} coletas rápidas em 3 s a 300 ms");
        assert!((8..=12).contains(&n), "{n}");
    }

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
