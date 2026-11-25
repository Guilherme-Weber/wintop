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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::panic::{AssertUnwindSafe, catch_unwind};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

use crate::collect::{FastCollectors, RtssCollector, SlowCollectors, rtss};
use crate::model::{Fast, Fps, Slow, View};

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
    fast: Option<Arc<Fast>>,
    slow: Option<Arc<Slow>>,
    fps: Option<Arc<Fps>>,
    /// Números de publicação de cada thread (quem exibe sabe o que mudou).
    fast_seq: u64,
    slow_seq: u64,
    fps_seq: u64,
    /// A última publicação do RTSS ainda não foi lida pela interface.
    fps_unread: bool,
    stop: bool,
    /// Abas visíveis com coleta própria.
    view: View,
    /// Uma aba acabou de entrar em vista: a thread lenta coleta já, sem esperar o período.
    kick: bool,
    /// Lista nova de ignorados do RTSS (o config mudou), aplicada pela thread do RTSS.
    rtss_ignored: Option<Vec<String>>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    interval_ms: AtomicU64,
    /// Cópia de `State::stop` legível sem o lock (laços longos dos coletores).
    stopping: AtomicBool,
    /// PID do jogo em foco (0 = nenhum), publicado pela thread do RTSS para a thread lenta
    /// medir a RAM dele sem acoplar à UI.
    game_pid: AtomicU32,
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
    /// `allow_kick`: com `true`, um chute (`State::kick`, aba aberta) encerra a espera na hora.
    fn wait_tick(
        &self,
        base: Instant,
        factor: u32,
        floor: Duration,
        allow_kick: bool,
    ) -> Option<Instant> {
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
            if allow_kick && std::mem::take(&mut st.kick) {
                return Some(Instant::now());
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

    /// Publica um snapshot de FPS/RTSS, coalescendo `peak_ms` com a publicação anterior se a UI
    /// ainda não a leu (tela ocupada > 250 ms).
    fn publish_fps(&self, mut snap: Fps) {
        self.game_pid
            .store(snap.game.as_ref().map_or(0, |g| g.pid), Ordering::Relaxed);
        let mut st = self.lock();
        if st.fps_unread
            && let (Some(Ok(prev)), Ok(cur)) = (st.fps.as_ref().map(|f| &f.game), &mut snap.game)
            && prev.pid == cur.pid
            && let Some(p) = prev.peak_ms
        {
            cur.peak_ms = Some(cur.peak_ms.map_or(p, |c| c.max(p)));
        }
        st.fps = Some(Arc::new(snap));
        st.fps_seq += 1;
        st.fps_unread = true;
    }

    fn updates(&self, seen: &mut Seen) -> Updates {
        let mut st = self.lock();
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
            st.fps_unread = false;
        }
        u
    }
}

/// Últimas publicações já vistas por quem exibe.
#[derive(Default)]
pub struct Seen {
    fast: u64,
    slow: u64,
    fps: u64,
}

/// Dados novos desde a última consulta (compartilhados por `Arc`: sem clonar snapshots sob lock).
#[derive(Default)]
pub struct Updates {
    pub fast: Option<Arc<Fast>>,
    pub slow: Option<Arc<Slow>>,
    pub fps: Option<Arc<Fps>>,
}

pub struct Engine {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    stop_event: Arc<OwnedHandle>,
}

impl Engine {
    /// Com a lista de ignorados padrão do RTSS (testes).
    #[cfg(test)]
    pub fn start(interval_ms: u64) -> Self {
        Self::start_with(
            interval_ms,
            crate::config::DEFAULT_RTSS_IGNORED
                .iter()
                .map(ToString::to_string)
                .collect(),
        )
    }

    /// `rtss_ignored`: executáveis que nunca viram o jogo (`config.toml`).
    pub fn start_with(interval_ms: u64, rtss_ignored: Vec<String>) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            interval_ms: AtomicU64::new(clamp_interval(interval_ms)),
            stopping: AtomicBool::new(false),
            game_pid: AtomicU32::new(0),
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
                    let r = catch_unwind(AssertUnwindSafe(|| {
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
                            crate::debug::log_fast(&snap);
                            {
                                let mut st = sh.lock();
                                st.fast = Some(Arc::new(snap));
                                st.fast_seq += 1;
                            }
                            match sh.wait_tick(next, 1, Duration::ZERO, false) {
                                Some(t) => next = t,
                                None => break,
                            }
                        }
                    }));
                    // Pânico fora dos coletores: a falha aparece (a tela não congela no último dado).
                    if r.is_err() {
                        crate::debug::log("panic", "fast collection thread panicked");
                        let mut st = sh.lock();
                        st.fast = Some(Arc::new(Fast::failed("fast collection thread panicked")));
                        st.fast_seq += 1;
                    }
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
                .spawn(move || rtss_thread(&sh, HANDLE(stop.as_raw_handle()), rtss_ignored))
                .expect("failed to create the RTSS thread")
        };
        let slow = {
            let sh = Arc::clone(&shared);
            let nvml_slow = nvml;
            std::thread::Builder::new()
                .name("coleta-lenta".into())
                .spawn(move || {
                    let r = catch_unwind(AssertUnwindSafe(|| {
                        let mut c = SlowCollectors::new(nvml_slow);
                        let mut next = Instant::now();
                        loop {
                            if sh.stopping.load(Ordering::Relaxed) {
                                return;
                            }
                            let view = sh.lock().view;
                            let game_pid = sh.game_pid.load(Ordering::Relaxed);
                            let Some(snap) = c.collect(&sh.stopping, view, game_pid) else {
                                return;
                            };
                            crate::debug::log_slow(&snap);
                            {
                                let mut st = sh.lock();
                                st.slow = Some(Arc::new(snap));
                                st.slow_seq += 1;
                            }
                            match sh.wait_tick(next, SLOW_FACTOR, SLOW_FLOOR, true) {
                                Some(t) => next = t,
                                None => break,
                            }
                        }
                    }));
                    // Pânico fora dos coletores: a falha aparece (a tela não congela no último dado).
                    if r.is_err() {
                        crate::debug::log("panic", "slow collection thread panicked");
                        let mut st = sh.lock();
                        st.slow = Some(Arc::new(Slow::failed("slow collection thread panicked")));
                        st.slow_seq += 1;
                    }
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
    pub fn latest(&self) -> (Option<Arc<Fast>>, Option<Arc<Slow>>, u64) {
        let st = self.shared.lock();
        (st.fast.clone(), st.slow.clone(), st.fast_seq)
    }

    /// O que mudou desde `seen` (e atualiza `seen`). Só incrementa refcount (`Arc::clone`).
    pub fn updates(&self, seen: &mut Seen) -> Updates {
        self.shared.updates(seen)
    }

    /// Último estado do RTSS e do jogo.
    #[cfg(test)]
    pub fn fps(&self) -> Option<Arc<Fps>> {
        self.shared.lock().fps.clone()
    }

    pub fn interval_ms(&self) -> u64 {
        self.shared.interval_ms.load(Ordering::Relaxed)
    }

    /// Abas visíveis. Uma aba que entra em vista acorda a thread lenta na hora; uma que sai
    /// para a coleta dela no ciclo seguinte.
    pub fn set_view(&self, v: View) {
        // O chute é gravado com o lock: a thread lenta lê o chute com o lock e só o solta ao
        // começar a esperar, então o aviso não se perde entre a leitura e a espera.
        let kicked = {
            let mut st = self.shared.lock();
            let old = st.view;
            st.view = v;
            let kicked = (v.procs && !old.procs) || (v.gpu && !old.gpu);
            if kicked {
                st.kick = true;
            }
            kicked
        };
        if kicked {
            self.shared.wake.notify_all();
        }
    }

    /// Ajusta o intervalo (teclas `+`/`-`); vale a partir da próxima espera.
    /// Troca a lista de ignorados do RTSS (config editado com o wintop aberto); vale a partir
    /// da próxima amostra.
    pub fn set_rtss_ignored(&self, list: Vec<String>) {
        self.shared.lock().rtss_ignored = Some(list);
    }

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
fn rtss_thread(sh: &Shared, stop: HANDLE, ignored: Vec<String>) {
    let publish = |snap: Fps| {
        crate::debug::log_fps(&snap);
        sh.publish_fps(snap);
    };
    let Ok(mut c) = catch_unwind(AssertUnwindSafe(|| {
        RtssCollector::new(true, true).with_ignored(ignored)
    })) else {
        crate::debug::log("panic", "RTSS collector panicked on startup");
        publish(rtss::failed("RTSS collector panicked on startup"));
        return;
    };
    let mut last: Option<Fps> = None;
    loop {
        let pending = {
            let mut st = sh.lock();
            if st.stop {
                return;
            }
            st.rtss_ignored.take()
        };
        if let Some(list) = pending {
            c.set_ignored(list);
        }
        let Ok(snap) = catch_unwind(AssertUnwindSafe(|| c.sample())) else {
            crate::debug::log("panic", "RTSS collector panicked in sample");
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
                crate::debug::log("panic", "RTSS collector panicked in pump");
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

    fn shared(interval_ms: u64) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            interval_ms: AtomicU64::new(interval_ms),
            stopping: AtomicBool::new(false),
            game_pid: AtomicU32::new(0),
        })
    }

    /// Espera da thread lenta (período longo) num thread de teste; devolve quanto levou.
    fn waiting(sh: &Arc<Shared>) -> JoinHandle<(bool, Duration)> {
        let s = Arc::clone(sh);
        std::thread::spawn(move || {
            let t = Instant::now();
            let r = s.wait_tick(Instant::now(), SLOW_FACTOR, SLOW_FLOOR, true);
            (r.is_some(), t.elapsed())
        })
    }

    #[test]
    fn chute_acorda_a_thread_lenta() {
        let sh = shared(10_000); // período de 20 s
        let t = waiting(&sh);
        std::thread::sleep(Duration::from_millis(50));
        sh.lock().kick = true;
        sh.wake.notify_all();
        let (ok, took) = t.join().expect("thread");
        assert!(ok && took < Duration::from_millis(500), "{took:?}");
    }

    #[test]
    fn coalesce_peak_ms_enquanto_nao_lido_e_reseta_apos_leitura() {
        use crate::model::{FrameSource, Game, neutral};
        let make = |pid: u32, peak_ms: Option<f64>| Fps {
            rtss: Ok("2.18".into()),
            pipe: Ok((1, 1)),
            etw: Ok("ETW"),
            game: Ok(Game {
                pid,
                name: "game.exe".into(),
                api: "D3D12",
                resolution: Ok((1920, 1080)),
                source: FrameSource::Etw,
                fps: Ok(120.0),
                framegen: None,
                frametime: Ok((8.3, 7.0, 12.0)),
                lows: neutral("waiting"),
                gpu_time: neutral("waiting"),
                bound: neutral("waiting"),
                peak_ms,
            }),
            hooked: vec![pid],
        };
        let sh = shared(1000);
        let mut seen = Seen::default();
        // Duas publicações antes da UI ler: o maior peak_ms (42.0) sobrevive.
        sh.publish_fps(make(100, Some(42.0)));
        sh.publish_fps(make(100, Some(15.0)));
        let u = sh.updates(&mut seen);
        let g = u.fps.as_ref().unwrap().game.as_ref().unwrap();
        assert_eq!(g.peak_ms, Some(42.0));
        // Sem nova publicação: updates devolve None.
        assert!(sh.updates(&mut seen).fps.is_none());
        // Depois da leitura, um novo peak menor não herda o 42.0 antigo.
        sh.publish_fps(make(100, Some(9.5)));
        let u = sh.updates(&mut seen);
        assert_eq!(
            u.fps.as_ref().unwrap().game.as_ref().unwrap().peak_ms,
            Some(9.5)
        );
        // Troca de PID não mistura peak_ms do jogo anterior.
        sh.publish_fps(make(100, Some(80.0)));
        sh.publish_fps(make(200, Some(11.0)));
        let u = sh.updates(&mut seen);
        assert_eq!(
            u.fps.as_ref().unwrap().game.as_ref().unwrap().peak_ms,
            Some(11.0)
        );
    }

    #[test]
    fn intervalo_menor_vale_na_hora_e_parada_interrompe() {
        let sh = shared(10_000);
        let t = waiting(&sh);
        std::thread::sleep(Duration::from_millis(50));
        {
            let _st = sh.lock();
            sh.interval_ms.store(100, Ordering::Relaxed);
        }
        sh.wake.notify_all();
        // Novo período = piso de 2 s a partir da base (não os 20 s de antes).
        let (ok, took) = t.join().expect("thread");
        assert!(ok && took < Duration::from_millis(2600), "{took:?}");
        let t = waiting(&sh);
        std::thread::sleep(Duration::from_millis(50));
        sh.lock().stop = true;
        sh.wake.notify_all();
        let (ok, took) = t.join().expect("thread");
        assert!(!ok && took < Duration::from_millis(500), "{took:?}");
    }

    /// Orçamento: o motor inteiro (as três threads), alternando as abas com coleta própria,
    /// fica abaixo de 0,3% de CPU e de 45 MB de working set privado (o executável de teste é
    /// maior que o do wintop). Evita regressões silenciosas de consumo.
    #[test]
    #[ignore = "hardware: inicia todos os coletores por ~20 s (rodar sozinho)"]
    fn orcamento_de_consumo() {
        let e = Engine::start(DEFAULT_INTERVAL_MS);
        let views = [
            View::default(),
            View {
                procs: true,
                gpu: false,
            },
            View {
                procs: false,
                gpu: true,
            },
            View::default(),
        ];
        let mut cpu = Vec::new();
        let mut ws = 0u64;
        for v in views {
            e.set_view(v);
            std::thread::sleep(Duration::from_secs(5));
            if let (Some(f), _, _) = e.latest()
                && let Ok(o) = &f.own
            {
                if let Ok(c) = o.cpu {
                    cpu.push(c);
                }
                if let Ok(w) = o.private_ws {
                    ws = ws.max(w);
                }
            }
        }
        let avg = cpu.iter().sum::<f64>() / cpu.len().max(1) as f64;
        println!(
            "CPU {cpu:?} (média {avg:.3}%), WS privado máx. {} MB",
            ws >> 20
        );
        // Sem nenhuma medida o orçamento passaria com 0: o consumo tem de ter sido lido.
        assert!(!cpu.is_empty() && ws > 0, "consumo próprio não medido");
        assert!(avg < 0.3, "{avg}");
        assert!(ws < 45 << 20, "{} MB", ws >> 20);
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
