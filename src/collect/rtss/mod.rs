//! FPS pelo RTSS e ETW (thread própria): Shared Memory RTSSSharedMemoryV2 (shm.rs), pipe de
//! frametime (pipe.rs) e agregação de frames/lows/frame gen (ETW + pipe + Shared Memory).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};

mod pipe;
mod shm;

use pipe::{
    INSTANCES, Instance, MAX_FRAMES, MAX_KNOWN, MAX_SIGNALS_PER_PUMP, Pending, create_pipe,
};
pub use shm::hooked_now;
use shm::{ACTIVE_MS, App, SHM_NAME, api_name, is_ignored, read_apps, select_game, start_rtss};

use crate::collect::etw;
use crate::model::{Fps, FrameGen, FrameSource, Game, Metric, Missing, fail, neutral};
use crate::shm::SharedView;

/// Janela em que um jogo exato (pipe) continua exato sem frames: acima de `ACTIVE_MS` (que usa
/// o relógio do RTSS, de 15,6 ms), para um travamento de ~2 s não piscar como "approx.".
const EXACT_GRACE: Duration = Duration::from_millis(ACTIVE_MS as u64 + 500);
/// Pausa sem frames a partir da qual o FPS passa a contar o tempo parado.
const FREEZE_MIN_US: u64 = 50_000;
/// Período mais longo entre amostras da thread do RTSS (sem jogo).
const RTSS_PERIOD_MAX: Duration = Duration::from_secs(1);
/// Janela dos lows.
const WINDOW: Duration = Duration::from_secs(60);
/// Recalcular os lows no máximo uma vez por segundo (ordenar ~9 mil frames).
const LOWS_EVERY: Duration = Duration::from_secs(1);
/// GPU ocupada nesta fração do frame (ou mais) = limitado pela GPU.
const GPU_BOUND: f64 = 0.9;
/// Depois de iniciar o RTSS, quanto esperar a Shared Memory antes de chamar de falha.
const RTSS_START_GRACE: Duration = Duration::from_secs(15);
/// Sem Shared Memory depois de iniciar: nova tentativa a cada minuto.
const RTSS_START_RETRY: Duration = Duration::from_secs(60);
/// Atraso da entrega do ETW (flush de 100 ms + consumidor): as contas dos frames por ETW usam
/// "agora" menos isto, para o último segundo não parecer vazio nem um congelamento.
const ETW_LAG: Duration = Duration::from_millis(250);
/// Swap chains acompanhadas por jogo (a principal é a com mais frames no último segundo).
const MAX_CHAINS: usize = 4;
/// Sem jogo escolhido, o ETW continua no último por até isto (o histórico dele não se perde
/// num congelamento ou numa troca rápida de foco).
const ETW_HOLD: Duration = Duration::from_secs(10);
/// Intervalo maior que isto entre dois Present não vira frametime (jogo minimizado).
const MAX_FRAME_US: u64 = 60_000_000;
/// Marcadores de frame real guardados (2 s a 2000 FPS reais).
const MAX_REAL: usize = 4096;
/// Multiplicador visto há no máximo isto vale como frame gen ativo.
const MULT_FRESH: Duration = Duration::from_secs(2);
/// Sem multiplicador informado: apresentados/reais a partir disto = frame gen.
const FG_RATIO: f64 = 1.6;

/// (1% low, 0,1% low) em FPS: média dos frametimes do 1% (0,1%) mais lento. Exige 100
/// amostras; com menos de 1000, o 0,1% usa o pior frame.
fn lows(samples_us: impl Iterator<Item = u32>) -> Option<(f64, f64)> {
    let mut s: Vec<u32> = samples_us.filter(|&u| u > 0).collect();
    if s.len() < 100 {
        return None;
    }
    s.sort_unstable_by(|a, b| b.cmp(a));
    let avg_fps = |k: usize| {
        let k = k.clamp(1, s.len());
        let avg = s[..k].iter().map(|&u| u64::from(u)).sum::<u64>() as f64 / k as f64;
        (avg > 0.0).then(|| 1e6 / avg)
    };
    Some((avg_fps(s.len() / 100)?, avg_fps(s.len() / 1000)?))
}

/// Frames de um app (pipe, por índice da Shared Memory, ou amostras aproximadas, por PID).
#[derive(Default)]
struct Ring {
    /// PID dono do índice (o índice volta a ser usado quando o app fecha).
    pid: Option<u32>,
    frames: VecDeque<(Instant, u32)>,
}

impl Ring {
    fn push(&mut self, at: Instant, us: u32) {
        self.push_capped(at, us, MAX_FRAMES);
    }

    /// O pipe aceita escrita de processos sem elevação: um cliente inundando não cresce a
    /// memória sem limite (descarta os mais antigos).
    fn push_capped(&mut self, at: Instant, us: u32, cap: usize) {
        while self.frames.len() >= cap.max(1) {
            self.frames.pop_front();
        }
        self.frames.push_back((at, us));
    }

    fn prune(&mut self, now: Instant) {
        while self
            .frames
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > WINDOW)
        {
            self.frames.pop_front();
        }
        // Depois de um pico (menu sem limite de FPS), devolve a memória.
        if self.frames.capacity() > 4 * self.frames.len().max(1024) {
            self.frames.shrink_to(self.frames.len() * 2);
        }
    }

    /// Há frames nos últimos `within`.
    fn recent(&self, now: Instant, within: Duration) -> bool {
        self.frames
            .back()
            .is_some_and(|(t, _)| now.duration_since(*t) <= within)
    }

    /// Frametimes do segundo que termina em `now` (frames depois de `now` ficam de fora: pelo
    /// ETW, `now` é "agora" menos o atraso da entrega).
    fn last_second(&self, now: Instant) -> impl Iterator<Item = u32> + '_ {
        self.frames
            .iter()
            .rev()
            .skip_while(move |(t, _)| *t > now)
            .take_while(move |(t, _)| now.duration_since(*t) <= Duration::from_secs(1))
            .map(|(_, us)| *us)
    }
}

/// Frames de uma swap chain do jogo pelo ETW.
#[derive(Default)]
struct Chain {
    last_qpc: Option<i64>,
    ring: Ring,
    /// Pior frametime desde a última publicação.
    peak: Option<u32>,
}

/// Frames do jogo pelo ETW (só do PID com os providers ligados).
#[derive(Default)]
struct EtwFrames {
    pid: Option<u32>,
    /// Desde quando os providers estão ligados para `pid`: antes de 1 s completo de eventos, a
    /// contagem de frames reais ainda não vale.
    since: Option<Instant>,
    chains: HashMap<u64, Chain>,
    /// Início de cada frame real (Reflex `SIMULATION_START`).
    real: VecDeque<Instant>,
    /// Último multiplicador informado (1 = frame gen desligado).
    mult: Option<(Instant, u32)>,
    scratch: Vec<etw::Event>,
}

impl EtwFrames {
    fn retarget(&mut self, pid: Option<u32>, now: Instant) {
        if self.pid != pid {
            self.pid = pid;
            self.since = pid.map(|_| now);
            self.chains.clear();
            self.real.clear();
            self.mult = None;
        }
    }

    /// Eventos novos da sessão. `now_qpc` e `freq`: o QPC lido junto com `now`.
    fn ingest(&mut self, events: &mut Vec<etw::Event>, now_qpc: i64, freq: i64, now: Instant) {
        let age_us = |q: i64| etw::ticks_us(now_qpc.saturating_sub(q), freq);
        let Some(pid) = self.pid else {
            events.clear();
            return;
        };
        for ev in events.drain(..) {
            if ev.pid != pid {
                continue;
            }
            let at = now
                .checked_sub(Duration::from_micros(age_us(ev.qpc)))
                .unwrap_or(now);
            // Fila em ordem de tempo (prune, recent e last_second dependem disso).
            let after = |last: Option<Instant>| last.map_or(at, |l| at.max(l));
            match ev.kind {
                etw::Kind::Present { swapchain } => {
                    if !self.chains.contains_key(&swapchain) && self.chains.len() >= MAX_CHAINS {
                        let oldest = self
                            .chains
                            .iter()
                            .min_by_key(|(_, c)| c.ring.frames.back().map(|f| f.0))
                            .map(|(k, _)| *k);
                        if let Some(k) = oldest {
                            self.chains.remove(&k);
                        }
                    }
                    let c = self.chains.entry(swapchain).or_default();
                    match c.last_qpc {
                        // Fora de ordem (buffers de CPUs diferentes): ignorado.
                        Some(last) if ev.qpc <= last => {}
                        Some(last) => {
                            let us = etw::ticks_us(ev.qpc.saturating_sub(last), freq);
                            if us > 0 && us <= MAX_FRAME_US {
                                let us = us as u32;
                                let at = after(c.ring.frames.back().map(|f| f.0));
                                c.ring.push(at, us);
                                c.peak = Some(c.peak.map_or(us, |p| p.max(us)));
                            }
                            c.last_qpc = Some(ev.qpc);
                        }
                        None => c.last_qpc = Some(ev.qpc),
                    }
                }
                etw::Kind::SimStart => {
                    if self.real.len() >= MAX_REAL {
                        self.real.pop_front();
                    }
                    let at = after(self.real.back().copied());
                    self.real.push_back(at);
                }
                etw::Kind::Multiplier(v) => self.mult = Some((at, v)),
            }
        }
        while self
            .real
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > Duration::from_secs(2))
        {
            self.real.pop_front();
        }
        for c in self.chains.values_mut() {
            c.ring.prune(now);
        }
    }

    /// Swap chain principal: com frames recentes (até `t`), a de mais frames no último segundo.
    /// Só conta a que já tem frame em `t` ou antes (os primeiros frames do ETW chegam depois do
    /// corte: até lá vale a fonte anterior, sem uma amostra "sem frames").
    fn dominant(&self, t: Instant) -> Option<u64> {
        self.chains
            .iter()
            .filter(|(_, c)| c.ring.frames.front().is_some_and(|f| f.0 <= t))
            .filter(|(_, c)| c.ring.recent(t, EXACT_GRACE))
            .max_by_key(|(_, c)| {
                (
                    c.ring.last_second(t).count(),
                    c.ring.frames.back().map(|f| f.0),
                )
            })
            .map(|(k, _)| *k)
    }

    /// Frame gen no segundo que termina em `t`: `presented` = frames apresentados nessa mesma
    /// janela. Ativo com multiplicador >= 2 recente; com 1 informado, desligado; sem
    /// multiplicador, apresentados/reais >= `FG_RATIO`. Só com a janela inteira coberta desde
    /// a troca de jogo (no aquecimento a contagem de reais sairia baixa).
    fn framegen(&self, t: Instant, now: Instant, presented: f64) -> Option<FrameGen> {
        let start = t.checked_sub(Duration::from_secs(1))?;
        if self.since.is_none_or(|s| s > start) {
            return None;
        }
        let real = self.real.iter().filter(|a| **a > start && **a <= t).count();
        let mult = self
            .mult
            .filter(|(a, _)| now.saturating_duration_since(*a) <= MULT_FRESH)
            .map(|m| m.1);
        let active = match mult {
            Some(m) => m >= 2,
            None => real >= 5 && presented / real as f64 >= FG_RATIO,
        };
        let effective =
            (real >= 5 && presented > 0.0).then(|| (presented / real as f64).clamp(1.0, 16.0));
        (real > 0 && active).then_some(FrameGen {
            real_fps: real as f64,
            multiplier: mult,
            effective,
        })
    }
}

// --- Coletor ---

/// `Fps` com a mesma falha em tudo (pânico ou início impossível da thread).
pub fn failed(cause: &str) -> Fps {
    let m = Missing::Fail(cause.to_string());
    Fps {
        rtss: Err(m.clone()),
        pipe: Err(m.clone()),
        etw: Err(m.clone()),
        game: Err(m),
        hooked: Vec::new(),
    }
}

/// Lows em cache: (instante, PID, fonte, valor).
type LowsCache = Option<(Instant, u32, FrameSource, Metric<(f64, f64)>)>;

/// Estado puro e testável da agregação de frames (ETW > pipe > Shared Memory), detecção de
/// congelamento, lows e frame gen, sem dependência de handles do Windows nem de `Instant::now()`.
struct FrameTracker {
    /// Frames do pipe por índice da Shared Memory.
    exact: HashMap<u32, Ring>,
    /// Índices dos apps (fora dos ignorados) na última amostra: os que o pipe guarda inteiros.
    known: HashSet<u32>,
    /// Índices de apps ignorados (navegadores, overlays): os frames deles são descartados.
    ignored: HashSet<u32>,
    /// Executáveis (minúsculos) que nunca viram o jogo (`config.toml`, `[rtss] ignored`).
    ignored_names: Vec<String>,
    /// Amostras aproximadas da Shared Memory por PID.
    approx: HashMap<u32, Ring>,
    frames: EtwFrames,
    /// Último jogo escolhido e quando: o ETW continua nele por `ETW_HOLD` sem jogo escolhido
    /// (um congelamento tira o jogo da lista de ativos do RTSS por um instante).
    last_game: Option<(u32, Instant)>,
    lows: LowsCache,
    /// PID e momento da publicação anterior do jogo (para o pior frame desde então).
    published: Option<(u32, Instant)>,
}

impl FrameTracker {
    fn new(ignored_names: Vec<String>) -> Self {
        Self {
            exact: HashMap::new(),
            known: HashSet::new(),
            ignored: HashSet::new(),
            ignored_names,
            approx: HashMap::new(),
            frames: EtwFrames::default(),
            last_game: None,
            lows: None,
            published: None,
        }
    }

    fn reset(&mut self, now: Instant) {
        self.exact.clear();
        self.approx.clear();
        self.lows = None;
        self.last_game = None;
        self.frames.retarget(None, now);
        // Sem Shared Memory válida, nenhum índice é "conhecido" (o teto vale para todos).
        self.known.clear();
        self.ignored.clear();
        self.published = None;
    }

    /// Atualiza índices conhecidos/ignorados, poda anéis, escolhe o jogo em foco e devolve
    /// `(índice_do_jogo_em_apps, pid_desejado_para_etw)`.
    fn sync_apps(
        &mut self,
        apps: &[App],
        fg: Option<u32>,
        now: Instant,
    ) -> (Option<usize>, Option<u32>) {
        // Só os índices de apps acompanhados (os ignorados, como navegadores, não guardam 60 s
        // de frames).
        let (ign, ok): (Vec<&App>, Vec<&App>) = apps
            .iter()
            .partition(|a| is_ignored(&self.ignored_names, &a.name));
        // Janela completa (60 s) só para os MAX_KNOWN apps 3D com frames mais recentes: a
        // tabela do RTSS guarda apps fechados e, com uma Shared Memory falsa, até 256 entradas
        // (256 × 60 mil frames). Um jogo travado continua entre os mais recentes.
        let mut active: Vec<&App> = ok.iter().copied().filter(|a| a.api != 0).collect();
        // O app em primeiro plano sempre entra (pausado, ele envelhece mais que os outros).
        active.sort_by_key(|a| (Some(a.pid) != fg, a.age_ms.unwrap_or(u32::MAX)));
        active.truncate(MAX_KNOWN);
        self.known = active.iter().map(|a| a.index).collect();
        self.ignored = ign.iter().map(|a| a.index).collect();
        // Índice reaproveitado por outro app: os frames antigos não valem. Ring sem dono
        // (frames que chegaram antes desta amostra) fica com o app atual do índice.
        for a in &active {
            let ring = self.exact.entry(a.index).or_default();
            if ring.pid.is_some_and(|p| p != a.pid) {
                ring.frames.clear();
            }
            ring.pid = Some(a.pid);
        }
        self.exact.retain(|i, _| self.known.contains(i));
        self.approx
            .retain(|pid, _| apps.iter().any(|a| a.pid == *pid));
        for r in self.exact.values_mut().chain(self.approx.values_mut()) {
            r.prune(now);
        }
        let chosen = select_game(apps, fg, &self.ignored_names);
        let chosen_idx = chosen.and_then(|g| apps.iter().position(|a| std::ptr::eq(a, g)));
        if let Some(g) = chosen {
            self.last_game = Some((g.pid, now));
        } else {
            // Só um app 3D ainda acompanhado (fora dos ignorados); expirado ou sumido, esquece.
            self.last_game = self.last_game.filter(|(pid, t)| {
                now.duration_since(*t) < ETW_HOLD && ok.iter().any(|a| a.pid == *pid && a.api != 0)
            });
        }
        (chosen_idx, self.last_game.map(|(pid, _)| pid))
    }

    fn build_game(&mut self, g: &App, now: Instant) -> Game {
        // Exato enquanto o pipe trouxe frames na janela de atividade (um travamento de 1-2 s
        // não vira "approx." nem perde os lows exatos).
        let exact_recent = self
            .exact
            .get(&g.index)
            .is_some_and(|r| r.recent(now, EXACT_GRACE));
        // Por ETW, "agora" é o último instante que já chegou (a entrega atrasa ~100-200 ms).
        let t_etw = now.checked_sub(ETW_LAG).unwrap_or(now);
        let chain = if self.frames.pid == Some(g.pid) {
            self.frames.dominant(t_etw)
        } else {
            None
        };
        let source = match (chain, exact_recent) {
            (Some(_), _) => FrameSource::Etw,
            (None, true) => FrameSource::Pipe,
            (None, false) => FrameSource::Approx,
        };
        let approx = source == FrameSource::Approx;
        if approx && g.frametime_us > 0 {
            self.approx
                .entry(g.pid)
                .or_default()
                .push(now, g.frametime_us);
        }
        let etw_peak = chain
            .and_then(|k| self.frames.chains.get_mut(&k))
            .and_then(|c| c.peak.take());
        let (ring, t) = match source {
            FrameSource::Etw => (
                chain
                    .and_then(|k| self.frames.chains.get(&k))
                    .map(|c| &c.ring),
                t_etw,
            ),
            FrameSource::Pipe => (self.exact.get(&g.index), now),
            FrameSource::Approx => (self.approx.get(&g.pid), now),
        };
        let second: Vec<u32> =
            ring.map_or_else(Vec::new, |r| r.last_second(t).filter(|&u| u > 0).collect());
        let (fps, frametime) = if approx {
            // FPS do período do RTSS; frametime: o instantâneo e os extremos das amostras.
            let cur = f64::from(g.frametime_us) / 1000.0;
            let (lo, hi) = min_max_ms(&second).unwrap_or((cur, cur));
            (positive(g.fps), positive(cur).map(|c| (c, lo, hi)))
        } else if second.is_empty() {
            let m = Missing::Neutral("no frames in the last second".into());
            (Err(m.clone()), Err(m))
        } else {
            let sum: u64 = second.iter().map(|&u| u64::from(u)).sum();
            let n = second.len() as f64;
            let (lo, hi) = min_max_ms(&second).unwrap_or((0.0, 0.0));
            let avg = sum as f64 / n / 1000.0;
            // Congelamento em andamento: o tempo desde o último frame conta no FPS (senão um
            // travamento de 900 ms mostraria o FPS do frame anterior a ele).
            let gap = ring.and_then(|r| r.frames.back()).map_or(0, |(at, _)| {
                t.saturating_duration_since(*at).as_micros() as u64
            });
            let stalled = gap > FREEZE_MIN_US && gap as f64 > 2.0 * avg * 1000.0;
            let span = sum + if stalled { gap } else { 0 };
            (
                positive(1e6 * n / span as f64),
                positive(avg).map(|a| (a, lo, hi)),
            )
        };
        let lows = cached_lows(&mut self.lows, g.pid, source, now, ring);
        // Reais e apresentados na mesma janela (a do ETW, que chega atrasada).
        let framegen = if self.frames.pid == Some(g.pid) {
            // No modo aproximado a fila tem uma amostra por leitura, não um frame: o FPS dela.
            let presented = if approx {
                fps.as_ref().ok().copied().unwrap_or(0.0)
            } else {
                ring.map_or(0, |r| r.last_second(t_etw).count()) as f64
            };
            self.frames.framegen(t_etw, now, presented)
        } else {
            None
        };
        let gpu_time = match g.gpu {
            Some((active, frame)) if active > 0 && frame > 0 => {
                Ok((f64::from(active) / 1000.0, f64::from(frame) / 1000.0))
            }
            Some(_) => neutral("not available in this game"),
            None => neutral("needs RTSS shared memory 2.21+"),
        };
        let bound = match (&gpu_time, &frametime) {
            // Os dois tempos da mesma linha do RTSS (o frametime médio do pipe tem 1 s de
            // inércia e inverteria o veredito numa troca de cena).
            (Ok((active, frame)), Ok(_)) if *frame > 0.0 => Ok(if active / frame >= GPU_BOUND {
                "GPU"
            } else {
                "CPU"
            }),
            // O estado de quem falta (neutro num travamento curto) passa adiante.
            (Err(e), _) | (_, Err(e)) => Err(e.clone()),
            _ => fail("no frametime"),
        };
        Game {
            pid: g.pid,
            name: g.name.clone(),
            api: api_name(g.api),
            resolution: match g.res {
                Some((x, y)) if x > 0 && y > 0 => Ok((x, y)),
                Some(_) => fail("resolution not reported by RTSS"),
                None => neutral("needs RTSS shared memory 2.20+"),
            },
            source,
            fps,
            framegen,
            frametime,
            lows,
            gpu_time,
            bound,
            peak_ms: {
                // Só o que chegou desde a publicação anterior deste jogo (outro jogo ou a 1ª
                // publicação: no máximo o último período, não 60 s de histórico).
                let floor = now.checked_sub(RTSS_PERIOD_MAX).unwrap_or(now);
                let since = match self.published.replace((g.pid, now)) {
                    Some((pid, t)) if pid == g.pid => Some(t.max(floor)),
                    _ => Some(floor),
                };
                // Pelo ETW os frames chegam atrasados: o pior é o que entrou desde a anterior.
                if source == FrameSource::Etw {
                    etw_peak.map(|us| f64::from(us) / 1000.0)
                } else {
                    ring.and_then(|r| {
                        r.frames
                            .iter()
                            .rev()
                            .take_while(|(at, _)| since.is_none_or(|s| *at > s))
                            .map(|(_, us)| *us)
                            .max()
                    })
                    .map(|us| f64::from(us) / 1000.0)
                }
            },
        }
    }

    fn clear_etw_peaks(&mut self) {
        for c in self.frames.chains.values_mut() {
            c.peak = None;
        }
    }
}

pub struct RtssCollector {
    view: Option<SharedView>,
    /// Se o RTSS deve ser iniciado quando ausente (modo normal; nunca no `--check`).
    start_rtss: bool,
    started_at: Option<Instant>,
    start_error: Option<String>,
    /// `Err(None)`: pipe desligado de propósito (`--check`); `Err(Some(e))`: falhou.
    pipe: Result<Vec<Instance>, Option<String>>,
    turn: usize,
    /// Sessão ETW: `Err(None)` = desligada de propósito (`--check`); `Err(Some(e))` = falhou.
    etw: Result<etw::Session, Option<String>>,
    /// Falha ao ligar os providers para o jogo atual.
    etw_error: Option<String>,
    tracker: FrameTracker,
}

impl RtssCollector {
    /// `pipe`: criar o servidor do pipe (só no modo normal: o `--check` não espera frames).
    pub fn new(start_rtss: bool, pipe: bool) -> Self {
        Self {
            view: None,
            start_rtss,
            started_at: None,
            start_error: None,
            pipe: if pipe {
                create_pipe().map_err(Some)
            } else {
                Err(None)
            },
            turn: 0,
            // Junto com o pipe: só no modo normal (o `--check` roda com a TUI aberta e não
            // pode parar a sessão dela).
            etw: if pipe {
                etw::Session::start().map_err(Some)
            } else {
                Err(None)
            },
            etw_error: None,
            tracker: FrameTracker::new(
                crate::config::DEFAULT_RTSS_IGNORED
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            ),
        }
    }

    /// Troca a lista de ignorados (do `config.toml`).
    pub fn with_ignored(mut self, list: Vec<String>) -> Self {
        self.tracker.ignored_names = list;
        self
    }

    /// Como `with_ignored`, com o coletor rodando: a próxima amostra refaz os índices
    /// conhecidos e ignorados com a lista nova.
    pub fn set_ignored(&mut self, list: Vec<String>) {
        self.tracker.ignored_names = list;
    }

    /// Atende o pipe até `deadline` (conclusões de conexão e leitura). Volta na hora se `stop`
    /// (evento manual) for sinalizado; devolve `true` nesse caso.
    pub fn pump(&mut self, deadline: Instant, stop: HANDLE) -> bool {
        let mut handled = 0u32;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let timeout = left.as_millis().min(u128::from(u32::MAX - 1)) as u32;
            let Ok(instances) = &mut self.pipe else {
                // SAFETY: evento válido (dono: o motor, que espera esta thread terminar).
                return unsafe { WaitForSingleObject(stop, timeout) } == WAIT_OBJECT_0;
            };
            if handled >= MAX_SIGNALS_PER_PUMP {
                // SAFETY: evento válido.
                return unsafe { WaitForSingleObject(stop, timeout) } == WAIT_OBJECT_0;
            }
            // O evento de parada vem primeiro (índice 0); depois os das instâncias armadas,
            // começando por uma diferente a cada volta (justiça entre clientes).
            let mut events = [HANDLE::default(); INSTANCES + 1];
            let mut armed = [0usize; INSTANCES];
            events[0] = stop;
            let mut n = 0;
            for i in 0..instances.len() {
                let k = (i + self.turn) % instances.len();
                if instances[k].pending != Pending::None {
                    armed[n] = k;
                    events[n + 1] = instances[k].event_handle();
                    n += 1;
                }
            }
            self.turn = self.turn.wrapping_add(1);
            // SAFETY: eventos válidos (no máximo 9).
            let r = unsafe { WaitForMultipleObjects(&events[..=n], false, timeout) };
            let idx = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            if idx == 0 {
                return true;
            }
            match armed[..n].get(idx - 1) {
                Some(&k) => {
                    instances[k].on_signal(
                        &mut self.tracker.exact,
                        &self.tracker.known,
                        &self.tracker.ignored,
                    );
                    handled += 1;
                }
                None => return false, // timeout ou erro
            }
        }
    }

    /// `None` com a Shared Memory aberta; senão o estado do RTSS (iniciando ou falha).
    fn open_view(&mut self) -> Option<Missing> {
        if self.view.is_none() {
            self.view = SharedView::open(SHM_NAME);
        }
        if self.view.is_some() {
            self.started_at = None;
            self.start_error = None;
            return None;
        }
        if !self.start_rtss {
            return Some(Missing::Fail("RTSS not running".into()));
        }
        // Nova tentativa de iniciar depois de `RTSS_START_RETRY`.
        if self
            .started_at
            .is_none_or(|t| t.elapsed() >= RTSS_START_RETRY)
        {
            self.started_at = Some(Instant::now());
            self.start_error = start_rtss().err();
        }
        if let Some(e) = &self.start_error {
            return Some(Missing::Fail(format!("RTSS not running: {e}")));
        }
        Some(
            if self
                .started_at
                .is_some_and(|t| t.elapsed() < RTSS_START_GRACE)
            {
                Missing::Neutral("starting RTSS".into())
            } else {
                Missing::Fail("RTSS not running (started, but no shared memory yet)".into())
            },
        )
    }

    /// Sem Shared Memory válida os índices do pipe não têm dono: descarta tudo.
    fn forget_frames(&mut self) {
        let now = Instant::now();
        self.tracker.reset(now);
        self.retarget(None, now);
    }

    /// Uma amostra: estado do RTSS e do jogo.
    pub fn sample(&mut self) -> Fps {
        if let Ok(list) = &mut self.pipe {
            for inst in list.iter_mut().filter(|i| i.pending == Pending::None) {
                inst.start_connect();
            }
        }
        let pipe = match &self.pipe {
            Ok(list) => Ok((list.len(), list.iter().filter(|i| i.connected).count())),
            Err(Some(e)) => fail(e.clone()),
            Err(None) => neutral("not started in --check"),
        };
        if let Some(missing) = self.open_view() {
            self.forget_frames();
            return Fps {
                rtss: Err(missing.clone()),
                pipe,
                etw: self.etw_status(),
                game: Err(missing),
                hooked: Vec::new(),
            };
        }
        let read = self.view.as_ref().map(read_apps);
        let (apps, fg, ver) = match read {
            Some(Ok(x)) => x,
            Some(Err(e)) => {
                // 0xDEAD ou lixo: desmapeia e reconecta na próxima amostra.
                self.view = None;
                self.forget_frames();
                return Fps {
                    rtss: fail(e.clone()),
                    pipe,
                    etw: self.etw_status(),
                    game: fail(e),
                    hooked: Vec::new(),
                };
            }
            None => unreachable!("view aberta em open_view"),
        };
        let rtss = Ok(format!("shared memory {}.{}", ver >> 16, ver & 0xFFFF));
        let now = Instant::now();
        let (chosen_idx, target_pid) = self.tracker.sync_apps(&apps, fg, now);
        self.retarget(target_pid, now);
        self.ingest();
        let game = match chosen_idx {
            None => neutral("no 3D game in focus"),
            Some(i) => Ok(self.tracker.build_game(&apps[i], now)),
        };
        // O pior frame é "desde a publicação anterior": o que sobrou não passa para a próxima.
        self.tracker.clear_etw_peaks();
        let hooked = apps.iter().map(|a| a.pid).collect();
        Fps {
            rtss,
            pipe,
            etw: self.etw_status(),
            game,
            hooked,
        }
    }

    /// Desliga o ETW (testes da reserva: pipe e Shared Memory).
    #[cfg(test)]
    pub(crate) fn disable_etw(&mut self) {
        self.retarget(None, Instant::now());
        self.etw = Err(None);
    }

    /// Liga os providers do ETW para o jogo (ou desliga sem jogo).
    fn retarget(&mut self, pid: Option<u32>, now: Instant) {
        // Os frames só valem para o PID com os providers ligados de fato (falha ao ligar = sem
        // alvo, e o aquecimento recomeça quando ligar).
        let on = match &mut self.etw {
            Ok(s) => {
                self.etw_error = s.target(pid).err();
                self.etw_error.is_none()
            }
            Err(_) => false,
        };
        self.tracker
            .frames
            .retarget(if on { pid } else { None }, now);
    }

    fn ingest(&mut self) {
        let Ok(s) = &self.etw else {
            return;
        };
        let mut events = std::mem::take(&mut self.tracker.frames.scratch);
        // Descartes (caixa cheia) só acontecem numa inundação: os frames que chegaram valem.
        let dropped = s.drain(&mut events);
        if dropped > 0 {
            crate::debug::log("etw", &format!("dropped {dropped} events (inbox full)"));
        }
        // Relógios lidos juntos: a idade de cada evento vira um `Instant` sem desvio.
        let (now, qpc) = (Instant::now(), etw::qpc_now());
        self.tracker.frames.ingest(&mut events, qpc, s.freq(), now);
        self.tracker.frames.scratch = events;
    }

    fn etw_status(&self) -> Metric<&'static str> {
        match (&self.etw, &self.etw_error) {
            (Ok(_), None) => Ok("DXGI + Reflex markers"),
            (Ok(_), Some(e)) | (Err(Some(e)), _) => fail(e.clone()),
            (Err(None), _) => neutral("not started in --check"),
        }
    }
}

fn cached_lows(
    cache: &mut LowsCache,
    pid: u32,
    source: FrameSource,
    now: Instant,
    ring: Option<&Ring>,
) -> Metric<(f64, f64)> {
    if let Some((t, p, a, v)) = &*cache
        && *p == pid
        && *a == source
        && now.duration_since(*t) < LOWS_EVERY
    {
        return v.clone();
    }
    let v = ring
        .and_then(|r| lows(r.frames.iter().map(|(_, us)| *us)))
        .map_or_else(|| neutral("waiting for samples"), Ok);
    *cache = Some((now, pid, source, v.clone()));
    v
}

fn positive(v: f64) -> Metric<f64> {
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        neutral("no frames in the last second")
    }
}

fn min_max_ms(us: &[u32]) -> Option<(f64, f64)> {
    let lo = us.iter().min()?;
    let hi = us.iter().max()?;
    Some((f64::from(*lo) / 1000.0, f64::from(*hi) / 1000.0))
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::{AsRawHandle, OwnedHandle};

    use super::pipe::*;
    use super::shm::*;
    use super::*;

    fn app(index: u32, name: &str, api: u32, fps: f64, age: Option<u32>) -> App {
        App {
            index,
            pid: index + 100,
            name: name.into(),
            api,
            fps,
            age_ms: age,
            ..App::default()
        }
    }

    #[test]
    fn idade_sem_underflow() {
        assert_eq!(age_ms(1000, 0), None);
        assert_eq!(age_ms(1000, 900), Some(100));
        assert_eq!(age_ms(1000, 1001), Some(0));
        assert_eq!(age_ms(5, u32::MAX - 4), Some(10));
    }

    #[test]
    fn escolha_do_jogo() {
        let apps = vec![
            app(1, "spotifast.exe", 1, 100.0, Some(10)),
            app(2, "deadlock.exe", 6, 33.0, Some(40)),
            app(3, "outro.exe", 7, 120.0, Some(9000)),
            app(4, "semapi.exe", 0, 60.0, Some(10)),
        ];
        assert_eq!(
            select_game(&apps, Some(101), &defaults()).map(|a| a.index),
            Some(2)
        );
        assert_eq!(
            select_game(&apps, None, &defaults()).map(|a| a.index),
            Some(2)
        );
        assert_eq!(
            select_game(&apps[2..], None, &defaults()).map(|a| a.index),
            None
        );
    }

    #[test]
    fn lows_basicos() {
        assert_eq!(lows([16_000u32; 50].into_iter()), None);
        assert_eq!(lows([0u32; 200].into_iter()), None);
        let mut s = vec![10_000u32; 990];
        s.extend([50_000u32; 10]);
        let (l1, l01) = lows(s.into_iter()).expect("lows");
        assert!((l1 - 20.0).abs() < 1e-9);
        assert!((l01 - 20.0).abs() < 1e-9);
    }

    /// Shared Memory sintética com o layout do header (v2.21 por padrão).
    fn shm(ver: u32, esize: usize, apps: &[(u32, &[u8], u32)]) -> Vec<u8> {
        const ARR: usize = 0x100;
        let mut b = vec![0u8; ARR + esize * apps.len().max(1)];
        let put = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut b, 0, RTSS_SIGNATURE);
        put(&mut b, OFF_VERSION, ver);
        put(&mut b, OFF_APP_ENTRY_SIZE, esize as u32);
        put(&mut b, OFF_APP_ARR_OFFSET, ARR as u32);
        put(&mut b, OFF_APP_ARR_SIZE, apps.len() as u32);
        put(
            &mut b,
            OFF_LAST_FOREGROUND_PID,
            apps.first().map_or(0, |a| a.0),
        );
        for (i, (pid, name, flags)) in apps.iter().enumerate() {
            let e = ARR + i * esize;
            put(&mut b, e + E_PID, *pid);
            b[e + E_NAME..e + E_NAME + name.len()].copy_from_slice(name);
            put(&mut b, e + E_FLAGS, *flags);
            put(&mut b, e + E_FRAME_TIME, 6944);
            if esize >= E_RES_Y + 4 {
                put(&mut b, e + E_RES_X, 2560);
                put(&mut b, e + E_RES_Y, 1440);
            }
            if esize >= E_GPU_FRAME + 4 {
                put(&mut b, e + E_GPU_ACTIVE, 5200);
                put(&mut b, e + E_GPU_FRAME, 6800);
            }
        }
        b
    }

    const ENTRY_V221: usize = 12416;

    #[test]
    fn shared_memory_v2_21() {
        let b = shm(
            V2_21,
            ENTRY_V221,
            &[
                (4242, br"C:\Games\deadlock.exe", 0x6),
                (0, b"", 0), // entrada vazia: ignorada
                (1784, b"dwm.exe", 0x7),
            ],
        );
        let (apps, fg, ver) = read_apps(&SharedView::from_bytes(&b)).expect("leitura");
        assert_eq!(ver, V2_21);
        assert_eq!(fg, Some(4242));
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "deadlock.exe");
        assert_eq!(apps[0].index, 0);
        assert_eq!(
            apps[1].index, 2,
            "o índice é a posição no array (= dwApp do pipe)"
        );
        assert_eq!(apps[0].res, Some((2560, 1440)));
        assert_eq!(apps[0].gpu, Some((5200, 6800)));
        assert_eq!(apps[0].frametime_us, 6944);
    }

    /// A Shared Memory é escrita por outro processo (e pode ser criada por um processo sem
    /// elevação no lugar do RTSS): qualquer conteúdo vira erro ou apps coerentes.
    #[test]
    fn fuzz_shared_memory() {
        // Período de FPS preenchido (agora - 500 ms .. agora, 72 frames): o cálculo do FPS e da
        // idade é exercitado, não só o caso "sem período".
        // Relógio fixo (o mesmo na leitura): a semente reproduz a falha em qualquer máquina.
        const NOW: u32 = 10_000;
        let timed = |mut b: Vec<u8>, esize: usize, n: usize| {
            for i in 0..n {
                let e = 0x100 + i * esize;
                for (o, v) in [(E_TIME0, NOW - 500), (E_TIME1, NOW), (E_FRAMES, 72)] {
                    b[e + o..e + o + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            b
        };
        let seeds = [
            timed(
                shm(
                    V2_21,
                    ENTRY_V221,
                    &[(4242, br"C:\g\a.exe", 6), (7, b"b.exe", 7)],
                ),
                ENTRY_V221,
                2,
            ),
            timed(
                shm(0x0002_000F, E_FRAME_TIME + 4, &[(10, b"old.exe", 5)]),
                E_FRAME_TIME + 4,
                1,
            ),
            timed(
                shm(V2_20, E_RES_Y + 4, &[(11, b"mid.exe", 5), (0, b"", 0)]),
                E_RES_Y + 4,
                2,
            ),
            // Entradas pequenas e muitas (a maior parte dos bytes é cabeçalho e campos lidos).
            timed(
                shm(V2_21, E_FRAME_TIME + 4, &[(1u32, &b"x.exe"[..], 6u32); 8]),
                E_FRAME_TIME + 4,
                8,
            ),
        ];
        let ignored = defaults();
        crate::fuzz::run("shared_memory", &seeds, 32 * 1024, |r, b| {
            let mut b = b.to_vec();
            // Metade das entradas com a assinatura certa: as mutações chegam ao resto.
            if b.len() >= 4 && r.chance(2) {
                b[..4].copy_from_slice(&RTSS_SIGNATURE.to_le_bytes());
            }
            let Ok((apps, fg, ver)) = read_apps_at(&SharedView::from_bytes(&b), || NOW) else {
                return;
            };
            assert_eq!(ver >> 16, 2);
            assert!(ver >= V2_16 || fg.is_none());
            assert!(apps.len() <= MAX_APPS);
            for a in &apps {
                assert!(a.pid != 0 && (a.index as usize) < MAX_APPS);
                assert!(a.fps.is_finite() && a.fps >= 0.0);
                // O nome vem de um processo qualquer: nunca chega com controles ao terminal.
                assert!(!a.name.contains('\\') && !a.name.chars().any(char::is_control));
                assert!(ver >= V2_20 || a.res.is_none());
                assert!(ver >= V2_21 || a.gpu.is_none());
            }
            if let Some(g) = select_game(&apps, fg, &ignored) {
                assert!(g.api != 0 && g.age_ms.is_some_and(|m| m < ACTIVE_MS));
                assert!(!is_ignored(&ignored, &g.name));
            }
        });
    }

    /// O pipe aceita escrita de processos sem elevação: mensagens quaisquer, partidas em
    /// pedaços quaisquer, mantêm os tetos (filas desconhecidas, frames, sobra < 8 bytes).
    #[test]
    fn fuzz_pipe() {
        let rec = |app: u32, us: u32| {
            let mut v = app.to_le_bytes().to_vec();
            v.extend(us.to_le_bytes());
            v
        };
        let mut seed = rec(0, 6944);
        seed.extend(rec(3, 16_000));
        seed.extend(rec(200, 1));
        let known: HashSet<u32> = [0, 1].into();
        let ignored: HashSet<u32> = [1].into();
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        crate::fuzz::run("pipe", &[seed], 512, |r, b| {
            // Recomeça de vez em quando: as filas desconhecidas voltam a ser criadas (o ramo
            // do teto não fica sempre cheio) e a conferência abaixo não varre 90 mil frames.
            if r.chance(64) {
                rings.clear();
                carry.clear();
            }
            let complete = r.chance(2);
            feed(&mut rings, &known, &ignored, &mut carry, b, complete);
            assert!(carry.len() < 8);
            assert!(!complete || carry.is_empty());
            let unknown = rings.keys().filter(|i| !known.contains(i)).count();
            assert!(unknown <= MAX_UNKNOWN_RINGS);
            for (i, ring) in &rings {
                assert!((*i as usize) < MAX_APPS && !ignored.contains(i));
                let cap = if known.contains(i) {
                    MAX_FRAMES
                } else {
                    UNKNOWN_FRAMES
                };
                assert!(ring.frames.len() <= cap);
                assert!(ring.frames.iter().all(|&(_, us)| us > 0));
            }
        });
    }

    #[test]
    fn fuzz_lows() {
        // Até ~1100 amostras: passa pelas duas regras do 0,1% (abaixo e acima de 1000).
        crate::fuzz::run("lows", &[], 4400, |_, b| {
            let s = b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c));
            if let Some((l1, l01)) = lows(s) {
                assert!(l1.is_finite() && l01.is_finite() && l1 > 0.0 && l01 > 0.0);
                assert!(l01 <= l1 + 1e-9, "0,1% low acima do 1% low");
            }
        });
    }

    #[test]
    fn shared_memory_antiga_sem_campos_novos() {
        // v2.15: sem primeiro plano, resolução e tempo de GPU; entrada no tamanho antigo.
        let b = shm(0x0002_000F, E_FRAME_TIME + 4, &[(10, b"old.exe", 0x5)]);
        let (apps, fg, _) = read_apps(&SharedView::from_bytes(&b)).expect("leitura");
        assert_eq!(fg, None);
        assert_eq!(apps[0].res, None);
        assert_eq!(apps[0].gpu, None);
    }

    #[test]
    fn shared_memory_invalida_e_erro() {
        let ok = shm(V2_21, ENTRY_V221, &[(1, b"a.exe", 1)]);
        let read = |b: &[u8]| read_apps(&SharedView::from_bytes(b)).map(|_| ());
        // RTSS fechando.
        let mut dead = ok.clone();
        dead[..4].copy_from_slice(&RTSS_DEAD.to_le_bytes());
        assert_eq!(read(&dead), Err("RTSS is shutting down".into()));
        // Assinatura errada e versão 1.x.
        let mut sig = ok.clone();
        sig[..4].copy_from_slice(&0x1234u32.to_le_bytes());
        assert!(read(&sig).is_err());
        let mut v1 = ok.clone();
        v1[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&0x0001_0005u32.to_le_bytes());
        assert!(read(&v1).is_err());
        // Entrada menor que o mínimo.
        let mut small = ok.clone();
        small[OFF_APP_ENTRY_SIZE..OFF_APP_ENTRY_SIZE + 4].copy_from_slice(&16u32.to_le_bytes());
        assert!(read(&small).is_err());
        // Array além do fim da região (e offset que estoura na multiplicação).
        let mut far = ok.clone();
        far[OFF_APP_ARR_OFFSET..OFF_APP_ARR_OFFSET + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read(&far).is_err());
        let mut many = ok.clone();
        many[OFF_APP_ARR_SIZE..OFF_APP_ARR_SIZE + 4].copy_from_slice(&200u32.to_le_bytes());
        assert!(read(&many).is_err());
        // Header cortado.
        assert!(read(&ok[..10]).is_err());
    }

    #[test]
    fn nome_com_controles_nao_chega_ao_terminal() {
        let b = shm(V2_21, ENTRY_V221, &[(1, b"ev\x1b[2Jil\x9b.exe", 1)]);
        let (apps, _, _) = read_apps(&SharedView::from_bytes(&b)).expect("leitura");
        assert!(
            !apps[0].name.chars().any(char::is_control),
            "{:?}",
            apps[0].name
        );
    }

    /// Servidor do pipe real com um nome de teste e clientes falsos (sem RTSS nem jogo): os
    /// registros chegam inteiros, inclusive partidos em mensagens; a 9ª conexão é recusada
    /// (as 8 instâncias são todas nossas); o evento de parada interrompe a espera na hora.
    #[test]
    fn servidor_do_pipe_com_clientes_falsos() {
        use std::io::Write;
        use std::os::windows::io::FromRawHandle;
        let name = format!(r"\\.\pipe\wintop_test_{}", std::process::id());
        let mut c = RtssCollector::new(false, false);
        c.pipe = create_pipe_named(&name).map_err(Some);
        assert!(c.pipe.is_ok(), "{:?}", c.pipe.as_ref().err());
        // SAFETY: evento sem nome; o handle passa a ter dono na hora.
        let ev = unsafe {
            OwnedHandle::from_raw_handle(
                windows::Win32::System::Threading::CreateEventW(None, true, false, None)
                    .expect("evento")
                    .0,
            )
        };
        let stop = HANDLE(ev.as_raw_handle());
        let rec = |app: u32, us: u32| [app.to_le_bytes(), us.to_le_bytes()].concat();
        let open = || std::fs::OpenOptions::new().write(true).open(&name);
        let mut a = open().expect("cliente 1");
        let mut b = open().expect("cliente 2");
        // 100 frames do app 3 num cliente; 50 do app 5 em outro, numa mensagem só.
        for _ in 0..100 {
            a.write_all(&rec(3, 7000)).expect("escrita");
        }
        let many: Vec<u8> = (0..50).flat_map(|_| rec(5, 16_000)).collect();
        b.write_all(&many).expect("escrita");
        // 8 instâncias: com 2 em uso, mais 6 conectam e a 9ª é recusada.
        let extra: Vec<_> = (0..6).map(|_| open().expect("cliente extra")).collect();
        assert!(open().is_err(), "9ª conexão recusada");
        c.pump(Instant::now() + Duration::from_millis(300), stop);
        assert_eq!(c.tracker.exact.get(&3).map(|r| r.frames.len()), Some(100));
        assert_eq!(c.tracker.exact.get(&5).map(|r| r.frames.len()), Some(50));
        drop(extra);
        // Parada: o pump volta na hora mesmo com prazo longo.
        // SAFETY: evento válido.
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(stop);
        }
        let t = Instant::now();
        assert!(c.pump(Instant::now() + Duration::from_secs(10), stop));
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "{:?}",
            t.elapsed()
        );
    }

    /// Eventos sintéticos: `fps` Present por segundo (um real a cada `fg`, multiplicador
    /// `mult`) durante `secs`, terminando agora. QPC em µs (frequência 1 MHz).
    fn etw_frames(fps: u32, fg: u32, mult: Option<u32>, secs: u32) -> (EtwFrames, Instant) {
        let mut f = EtwFrames::default();
        let now = Instant::now();
        f.retarget(Some(7), now);
        f.since = now.checked_sub(Duration::from_secs(u64::from(secs) + 1));
        let n = fps * secs;
        let step = 1_000_000 / i64::from(fps);
        let now_qpc = 1_000_000_000i64;
        let mut ev = Vec::new();
        for i in 0..n {
            let qpc = now_qpc - i64::from(n - i) * step;
            let e = |kind| etw::Event { pid: 7, qpc, kind };
            ev.push(e(etw::Kind::Present { swapchain: 1 }));
            if i % fg == 0 {
                ev.push(e(etw::Kind::SimStart));
                if let Some(m) = mult {
                    ev.push(e(etw::Kind::Multiplier(m)));
                }
            }
        }
        // Outro PID e um evento fora de ordem: ignorados.
        ev.push(etw::Event {
            pid: 8,
            qpc: now_qpc,
            kind: etw::Kind::SimStart,
        });
        ev.push(etw::Event {
            pid: 7,
            qpc: now_qpc - 3 * step * i64::from(n),
            kind: etw::Kind::Present { swapchain: 1 },
        });
        f.ingest(&mut ev, now_qpc, 1_000_000, now);
        (f, now)
    }

    #[test]
    fn frame_gen_pela_contagem_na_mesma_janela() {
        let t = |now: Instant| now - ETW_LAG;
        let presented =
            |f: &EtwFrames, now: Instant| f.chains[&1].ring.last_second(t(now)).count() as f64;
        // 4x informado: real = 1/4 dos apresentados, razão efetiva ~4x.
        let (f, now) = etw_frames(200, 4, Some(4), 3);
        let p = presented(&f, now);
        let fg = f.framegen(t(now), now, p).expect("frame gen 4x");
        assert_eq!(fg.multiplier, Some(4));
        assert!((fg.real_fps - 50.0).abs() <= 1.0, "{fg:?}");
        assert_eq!(fg.ratio_text().as_deref(), Some("4x"));
        assert!((199.0..=201.0).contains(&p), "{p}");
        // Razão dinâmica (MFG dinâmico: multiplicador nominal 4, mas efetivo ~2.5x).
        let dyn_fg = crate::model::FrameGen {
            real_fps: 60.0,
            multiplier: Some(4),
            effective: Some(2.5),
        };
        assert_eq!(dyn_fg.ratio_text().as_deref(), Some("2.5x"));
        let near_int = crate::model::FrameGen {
            real_fps: 50.0,
            multiplier: Some(4),
            effective: Some(3.96),
        };
        assert_eq!(near_int.ratio_text().as_deref(), Some("4x"));
        // Fila do ETW em ordem de tempo, mesmo com o evento fora de ordem.
        let ring = &f.chains[&1].ring.frames;
        assert!(
            ring.iter()
                .zip(ring.iter().skip(1))
                .all(|(a, b)| a.0 <= b.0)
        );
        assert!(ring.iter().all(|&(_, us)| us == 5000));
        // Reflex sem frame gen, com 1 informado: nada.
        let (f, now) = etw_frames(100, 1, Some(1), 3);
        assert_eq!(f.framegen(t(now), now, presented(&f, now)), None);
        // Sem multiplicador: pela razão (2x sim; 1x não).
        let (f, now) = etw_frames(120, 2, None, 3);
        let fg = f.framegen(t(now), now, presented(&f, now));
        assert!(fg.is_some_and(|g| g.multiplier.is_none() && (g.real_fps - 60.0).abs() <= 1.0));
        let (f, now) = etw_frames(120, 1, None, 3);
        assert_eq!(f.framegen(t(now), now, presented(&f, now)), None);
        // Aquecimento (menos de 1 s completo desde a troca de jogo): ainda não diz nada, num
        // caso que daria frame gen sem a guarda.
        let (mut f, now) = etw_frames(200, 4, Some(4), 3);
        let p = presented(&f, now);
        assert!(f.framegen(t(now), now, p).is_some());
        f.since = Some(now - Duration::from_millis(500));
        assert_eq!(f.framegen(t(now), now, p), None);
        // Swap chain cujos frames ainda não alcançaram o corte do ETW não é a principal.
        let (f, now) = etw_frames(100, 1, None, 3);
        assert_eq!(f.dominant(t(now)), Some(1));
        let first = f.chains[&1]
            .ring
            .frames
            .front()
            .map(|x| x.0)
            .expect("frames");
        assert_eq!(f.dominant(first - Duration::from_millis(1)), None);
    }

    fn defaults() -> Vec<String> {
        crate::config::DEFAULT_RTSS_IGNORED
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn all_known() -> HashSet<u32> {
        (0..MAX_APPS as u32).collect()
    }

    #[test]
    fn indices_desconhecidos_tem_teto() {
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        let known: HashSet<u32> = [3].into_iter().collect();
        // Um cliente inunda 200 índices inventados com 10 mil frames cada.
        let mut data = Vec::new();
        for app in 0..200u32 {
            for _ in 0..50 {
                data.extend(app.to_le_bytes());
                data.extend(1000u32.to_le_bytes());
            }
        }
        for _ in 0..200 {
            feed(&mut rings, &known, &HashSet::new(), &mut carry, &data, true);
        }
        let unknown: Vec<_> = rings.keys().filter(|i| !known.contains(i)).collect();
        assert!(unknown.len() <= MAX_UNKNOWN_RINGS, "{}", unknown.len());
        assert!(rings.values().all(|r| r.frames.len() <= MAX_FRAMES));
        assert!(
            rings
                .iter()
                .filter(|(i, _)| !known.contains(i))
                .all(|(_, r)| r.frames.len() <= UNKNOWN_FRAMES)
        );
        // O índice conhecido guarda tudo.
        assert_eq!(rings[&3].frames.len(), 200 * 50);
    }

    #[test]
    fn registros_partidos_entre_mensagens() {
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        let mut rec = 7u32.to_le_bytes().to_vec();
        rec.extend(16_000u32.to_le_bytes());
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &rec[..5],
            false,
        ); // ERROR_MORE_DATA no meio
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &rec[5..],
            true,
        );
        let got: Vec<u32> = rings[&7].frames.iter().map(|(_, us)| *us).collect();
        assert_eq!(got, vec![16_000]);
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &rec[..3],
            true,
        ); // malformada: descartada
        assert!(carry.is_empty());
        // Índice fora da Shared Memory é ignorado.
        let mut bad = 9999u32.to_le_bytes().to_vec();
        bad.extend(1u32.to_le_bytes());
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &bad,
            true,
        );
        assert!(!rings.contains_key(&9999));
    }

    #[test]
    fn frametime_zero_ignorado() {
        let mut rings = HashMap::new();
        let mut carry = Vec::new();
        let mut rec = 3u32.to_le_bytes().to_vec();
        rec.extend(0u32.to_le_bytes());
        feed(
            &mut rings,
            &all_known(),
            &HashSet::new(),
            &mut carry,
            &rec,
            true,
        );
        assert!(!rings.contains_key(&3));
    }

    #[test]
    fn janela_e_ultimo_segundo() {
        let mut r = Ring::default();
        let t0 = Instant::now();
        r.push(t0, 10_000);
        r.push(t0 + Duration::from_secs(30), 20_000);
        r.push(t0 + Duration::from_secs(61), 30_000);
        let now = t0 + Duration::from_secs(61);
        r.prune(now);
        assert_eq!(r.frames.len(), 2); // o de t0 saiu da janela de 60 s
        assert_eq!(r.last_second(now).collect::<Vec<_>>(), vec![30_000]);
        // Teto de frames: um cliente inundando não cresce sem limite.
        let mut big = Ring::default();
        for i in 0..(MAX_FRAMES + 10) {
            big.push(now, i as u32 + 1);
        }
        assert_eq!(big.frames.len(), MAX_FRAMES);
        assert_eq!(big.frames.front().map(|f| f.1), Some(11));
        assert!(r.recent(now, Duration::from_secs(2)));
        assert!(!r.recent(now + Duration::from_secs(3), Duration::from_secs(2)));
    }

    #[test]
    fn frame_tracker_prioridade_congelamento_troca_e_peak() {
        let mut tr = FrameTracker::new(defaults());
        let t0 = Instant::now();
        let mut g = app(0, "game.exe", 6, 100.0, Some(10));
        g.frametime_us = 10_000;
        let apps = vec![g.clone()];

        // 1) Sem pipe nem ETW -> Approx.
        let (idx, target) = tr.sync_apps(&apps, Some(g.pid), t0);
        assert_eq!((idx, target), (Some(0), Some(g.pid)));
        let out = tr.build_game(&apps[0], t0);
        assert_eq!(out.source, FrameSource::Approx);
        assert_eq!(out.fps, Ok(100.0));

        // 2) Chegam frames no pipe (100 FPS = 10 ms) -> assume Pipe como fonte.
        let t1 = t0 + Duration::from_secs(2);
        {
            let ring = tr.exact.entry(0).or_default();
            ring.pid = Some(g.pid);
            for i in 0..100 {
                let at = t1 - Duration::from_millis(1000 - i * 10);
                ring.push(at, if i == 90 { 35_000 } else { 10_000 });
            }
        }
        let (idx, _) = tr.sync_apps(&apps, Some(g.pid), t1);
        assert_eq!(idx, Some(0));
        let out = tr.build_game(&apps[0], t1);
        assert_eq!(out.source, FrameSource::Pipe);
        assert_eq!(out.peak_ms, Some(35.0));

        // Congelamento de 500 ms (gap > 2x avg) reduz o FPS em vez de congelar no antigo.
        let t_stall = t1 + Duration::from_millis(500);
        let stalled = tr.build_game(&apps[0], t_stall);
        assert_eq!(stalled.source, FrameSource::Pipe);
        assert!(stalled.fps.as_ref().unwrap() < &80.0, "{:?}", stalled.fps);

        // 1,5 s sem frames: dentro de EXACT_GRACE continua Pipe, mas reporta "no frames".
        let t_freeze = t1 + Duration::from_millis(1500);
        let frozen = tr.build_game(&apps[0], t_freeze);
        assert_eq!(frozen.source, FrameSource::Pipe);
        assert_eq!(frozen.fps, neutral("no frames in the last second"));

        // 3) ETW toma prioridade sobre Pipe e calcula MFG dinâmico.
        let t2 = t1 + Duration::from_secs(5);
        tr.frames.retarget(Some(g.pid), t2 - Duration::from_secs(3));
        let now_qpc = 10_000_000i64;
        let mut ev = Vec::new();
        for i in 0..300u32 {
            let qpc = now_qpc - i64::from(300 - i) * 5_000; // 200 FPS
            ev.push(etw::Event {
                pid: g.pid,
                qpc,
                kind: etw::Kind::Present { swapchain: 1 },
            });
            if i % 3 == 0 {
                ev.push(etw::Event {
                    pid: g.pid,
                    qpc,
                    kind: etw::Kind::SimStart,
                });
                ev.push(etw::Event {
                    pid: g.pid,
                    qpc,
                    kind: etw::Kind::Multiplier(3),
                });
            }
        }
        tr.frames.ingest(&mut ev, now_qpc, 1_000_000, t2);
        let out = tr.build_game(&apps[0], t2);
        assert_eq!(out.source, FrameSource::Etw);
        let fg = out.framegen.expect("MFG 3x");
        assert_eq!(fg.ratio_text().as_deref(), Some("3x"));

        // 4) Sem jogo ativo por < ETW_HOLD: mantém target ETW do último jogo.
        let mut idle_app = g.clone();
        idle_app.age_ms = Some(ACTIVE_MS + 500);
        let (idx, hold_target) = tr.sync_apps(&[idle_app], None, t2 + Duration::from_secs(2));
        assert_eq!(idx, None);
        assert_eq!(hold_target, Some(g.pid));
        // Após ETW_HOLD: solta o alvo ETW.
        let (_, expired_target) = tr.sync_apps(&[], None, t2 + ETW_HOLD + Duration::from_secs(1));
        assert_eq!(expired_target, None);
    }
}
