//! Aba Geral (v2, a partir do mockup da revisão visual, aprovado pelo usuário): barra de abas,
//! CPU (gráfico total + grade de gráficos por lógico) e GPU (engine mais usada, VRAM por
//! categoria) no topo; memória e rede embaixo à esquerda; discos e jogo (FPS + frametime)
//! embaixo à direita; rodapé com atalhos e o consumo do próprio wintop.
//!
//! Direção do usuário: uso em gráfico; RAM e VRAM em barra; temperaturas só como número;
//! bordas neutras estilo btop; mínimo/máximo da sessão (tecla `r` zera). Desenho puro a partir
//! do estado (snapshots do `insta`).

use std::collections::{HashMap, VecDeque};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use super::theme::th;
use super::widgets::{Graph, GraphColor, Meter, Segmented};
use crate::fmt::{bits, bytes, num};
use crate::i18n::tr;
use crate::model::{
    Disk, Fast, Fps, Gpu, GpuProc, Memory, Metric, Missing, Nic, Proc, Slow, View, VramBreakdown,
    VramKind,
};
use crate::win::printable;

pub(super) mod tabs;

pub use tabs::{Cursor, GpuSort, ProcSort};

/// Amostras guardadas por série (cobre ~500 colunas de braille).
const HISTORY: usize = 1000;
/// Estado de uma leitura de runtimes em andamento.
const READING: &str = "reading modules...";

fn is_reading<T>(r: &crate::model::Metric<T>) -> bool {
    matches!(r, Err(Missing::Neutral(m)) if m == READING)
}
/// Escala mínima da rede (100 Kbps): ruído de poucos bytes não parece tráfego.
const NET_FLOOR: f64 = 100_000.0;
/// Teto do frametime (como o Afterburner); o limite de alerta vem do config.
const FT_CEIL_MS: f64 = 50.0;
/// Degraus do teto do gráfico de FPS (sobe para o próximo quando o máximo passa).
const FPS_STEPS: [f64; 8] = [60.0, 120.0, 165.0, 240.0, 360.0, 500.0, 750.0, 1000.0];

/// Aba visível.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Overview,
    Processes,
    Gpu,
    Sensors,
}

/// Série com tamanho limitado.
#[derive(Default)]
pub struct Series(VecDeque<f64>);

impl Series {
    pub fn push(&mut self, v: f64) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        // NaN = coleta sem valor: lacuna no gráfico (um 0 falso pareceria uso zero).
        self.0
            .push_back(if v.is_finite() || v.is_nan() { v } else { 0.0 });
    }

    fn values(&mut self) -> &[f64] {
        self.0.make_contiguous()
    }

    /// As últimas `n` amostras divididas por `ceil`.
    fn over(&mut self, n: usize, ceil: f64) -> Vec<f64> {
        let v = self.values();
        let tail = &v[v.len().saturating_sub(n)..];
        tail.iter()
            .map(|x| if ceil > 0.0 { x / ceil } else { 0.0 })
            .collect()
    }

    /// Maior valor entre as últimas `n` amostras.
    fn max_of(&mut self, n: usize) -> f64 {
        let v = self.values();
        v[v.len().saturating_sub(n)..]
            .iter()
            .copied()
            .fold(0.0, f64::max)
    }
}

/// Mínimo e máximo da sessão (zerados com `r`).
#[derive(Clone, Copy, Default)]
pub struct MinMax(Option<(f64, f64)>);

impl MinMax {
    fn update(&mut self, v: f64) {
        if !v.is_finite() {
            return;
        }
        self.0 = Some(match self.0 {
            None => (v, v),
            Some((lo, hi)) => (lo.min(v), hi.max(v)),
        });
    }

    fn get(self) -> Option<(f64, f64)> {
        self.0
    }
}

/// Extremos da sessão de cada série exibida.
#[derive(Default)]
struct Session {
    cpu: MinMax,
    tctl: MinMax,
    cpu_power: MinMax,
    gpu: MinMax,
    gpu_temp: MinMax,
    gpu_power: MinMax,
    vram: MinMax,
    gfx_clock: MinMax,
    ram: MinMax,
    commit: MinMax,
    send: HashMap<String, MinMax>,
    recv: HashMap<String, MinMax>,
    fps: MinMax,
    ft: MinMax,
    /// Aba Sensores: extremos de cada sensor numérico, pela chave "grupo/nome".
    sensors: HashMap<String, MinMax>,
}

/// Estado da interface: os últimos dados, o histórico dos gráficos e os extremos da sessão.
#[derive(Default)]
pub struct State {
    pub fast: Option<Fast>,
    pub slow: Option<Slow>,
    pub fps: Option<Fps>,
    pub interval_ms: u64,
    pub tab: Tab,
    /// Membros do Storage Space recolhidos: `None` = automático (recolhe se não couberem).
    pub fold_pool: Option<bool>,
    /// Relógio da barra de abas (vem de fora: o desenho é puro).
    pub clock: String,
    cpu: Series,
    cores: Vec<Series>,
    /// Tempo de kernel (total e por lógico), alinhado com `cpu` e `cores`.
    cpu_kernel: Series,
    cores_kernel: Vec<Series>,
    /// Histórico de cada GPU (todas, para a troca não começar vazia).
    gpu: Vec<Series>,
    disks: HashMap<u32, Series>,
    send: HashMap<String, Series>,
    recv: HashMap<String, Series>,
    game_fps: Series,
    game_ft: Series,
    /// FPS mais recente e o pior frametime (ms) desde o último tick da UI.
    pending_fps: Option<f64>,
    pending_ft: Option<f64>,
    session: Session,
    /// Aba Processos: a última lista (só chega com a aba visível), cursor, ordem e filtro.
    pub procs: Option<Metric<Vec<Proc>>>,
    pub proc_cursor: Cursor,
    pub proc_sort: ProcSort,
    /// Inverte a ordem natural da coluna (tecla `i`).
    pub proc_reverse: bool,
    /// Árvore de processos (tecla `e`), sem filtro.
    pub proc_tree: bool,
    /// Áreas clicáveis do último desenho (mouse).
    pub hits: Hits,
    /// Caixa da aba Geral ampliada para a aba inteira (tecla `z`).
    pub zoom: Option<Zoom>,
    /// Aba Sensores: primeira linha mostrada e se o último desenho deixou linhas de fora (só
    /// então ela rola para baixo).
    pub sensor_offset: usize,
    pub sensor_overflow: bool,
    pub filter: String,
    /// Digitando o filtro (tecla `/`): as teclas vão para o texto.
    pub filtering: bool,
    /// Aba GPU: processos da 1ª GPU, cursor e os runtimes pedidos (tecla Enter), por PID.
    pub gpu_procs: Option<Metric<Vec<GpuProc>>>,
    pub gpu_cursor: Cursor,
    /// GPU exibida nas abas Geral e GPU (tecla `g`; posição na lista, 1ª se sumir).
    pub gpu_sel: usize,
    pub gpu_sort: tabs::GpuSort,
    pub gpu_reverse: bool,
    /// Tela congelada (tecla `p` ou Espaço): as coletas continuam, mas não entram na tela.
    pub paused: bool,
    /// Janela de ajuda aberta (tecla `?`; qualquer tecla fecha).
    pub help: bool,
    /// Encerrar processo pedido (tecla Del na aba Processos): (PID, nome, criação), à espera
    /// da confirmação.
    pub confirm_end: Option<(u32, String, u64)>,
    /// Mensagem curta de uma ação (fim de processo): texto, se é erro, quando apareceu.
    pub toast: Option<(String, bool, std::time::Instant)>,
    /// Do config (valem na hora quando o arquivo muda).
    pub frametime_alert_ms: f64,
    pub net_bytes: bool,
    /// Tema atual (tecla `t`, gravado no config).
    pub theme_name: String,
    /// Aviso da configuração (arquivo com erro, gravação recusada), no rodapé.
    pub notice: Option<String>,
    /// Por PID, com o nome do processo lido (PID reaproveitado por outro não herda o resultado).
    pub runtimes: HashMap<u32, (String, Metric<Vec<&'static str>>)>,
}

impl State {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            frametime_alert_ms: crate::config::DEFAULT_FRAMETIME_ALERT_MS,
            ..Default::default()
        }
    }

    /// Tecla `d`: inverte o recolhimento do pool que está na tela (no automático com mais de 12
    /// discos ele já começa recolhido: um aperto expande).
    pub fn toggle_fold_pool(&mut self) {
        let total = self
            .fast
            .as_ref()
            .and_then(|f| f.disks.as_ref().ok())
            .map_or(0, Vec::len);
        self.fold_pool = Some(!pool_folded(self, total));
    }

    /// Posição da GPU exibida numa lista de `n` GPUs (a 1ª se a escolhida sumiu).
    pub fn gpu_index(&self, n: usize) -> usize {
        if self.gpu_sel < n { self.gpu_sel } else { 0 }
    }

    /// Quantas GPUs a última coleta trouxe.
    pub fn gpu_count(&self) -> usize {
        self.fast
            .as_ref()
            .and_then(|f| f.gpus.as_ref().ok())
            .map_or(0, Vec::len)
    }

    /// Tecla `g`: próxima GPU. Os extremos da sessão da GPU passam a ser os da nova (zerados);
    /// a lista da aba GPU espera a coleta seguinte, como na troca de aba.
    pub fn next_gpu(&mut self) {
        let n = self.gpu_count();
        if n < 2 {
            return;
        }
        self.select_gpu((self.gpu_index(n) + 1) % n);
    }

    /// Troca a GPU exibida (tecla `g`, ou a selecionada sumiu): extremos zerados, e a lista da
    /// aba GPU vem da última coleta lenta, que já trouxe todas as GPUs (ou a falha).
    fn select_gpu(&mut self, sel: usize) {
        self.gpu_sel = sel;
        self.reset_gpu_session();
        self.gpu_procs = None;
        if self.tab == Tab::Gpu {
            match self.slow.as_ref().map(|s| &s.gpus) {
                Some(Ok(list)) => {
                    self.gpu_procs = self.slow_gpu(list).and_then(|g| g.procs.clone());
                }
                Some(Err(e)) => self.gpu_procs = Some(Err(e.clone())),
                None => {}
            }
        }
        self.gpu_cursor = Cursor::default();
        self.runtimes.clear();
    }

    /// A GPU exibida na lista lenta. As threads refazem a lista cada uma no seu tempo (GPU que
    /// apareceu ou sumiu): pela posição da lista rápida, que dá o título; sem a GPU na lenta,
    /// `None` (aguarda) em vez dos dados de outra. Sem a lista rápida, pela própria lenta.
    pub fn slow_gpu<'a>(
        &self,
        list: &'a [crate::model::GpuSlow],
    ) -> Option<&'a crate::model::GpuSlow> {
        match self.gpu_count() {
            0 => list.get(self.gpu_index(list.len())),
            n => list.get(self.gpu_index(n)),
        }
    }

    /// Extremos da sessão da GPU exibida (zerados ao trocar de GPU).
    fn reset_gpu_session(&mut self) {
        for m in [
            &mut self.session.gpu,
            &mut self.session.gpu_temp,
            &mut self.session.gpu_power,
            &mut self.session.vram,
            &mut self.session.gfx_clock,
        ] {
            *m = MinMax::default();
        }
    }

    /// Zera os extremos da sessão (tecla `r`).
    /// O histórico do jogo também: só o `r` o zera (jogos trocam de executável e congelam ao
    /// carregar, e isso não deve apagar o gráfico).
    /// Abas com coleta própria visíveis (o motor só coleta o que aparece).
    pub fn view(&self) -> View {
        View {
            procs: self.tab == Tab::Processes,
            gpu: self.tab == Tab::Gpu,
        }
    }

    /// Troca de aba: a lista da aba que sai é descartada (ao voltar, espera dado novo em vez
    /// de mostrar um retrato velho).
    pub fn set_tab(&mut self, t: Tab) {
        if t != self.tab {
            self.tab = t;
            self.procs = None;
            self.gpu_procs = None;
            self.filtering = false;
            // A última coleta lenta já trouxe a falha das abas (a thread parou): ela aparece
            // em vez de "waiting" (dado novo, se vier, substitui).
            // Só nas abas que mostram a lista: na Geral nada a limparia (a coleta para) e a
            // falha ficaria presa no rodapé.
            // A lista da GPU herda a falha da coleta da GPU (não a dos processos).
            if matches!(t, Tab::Processes | Tab::Gpu)
                && let Some(Some(Err(e))) = self.slow.as_ref().map(|s| &s.procs)
            {
                self.procs = Some(Err(e.clone()));
            }
            if t == Tab::Gpu
                && let Some(Err(e)) = self.slow.as_ref().map(|s| &s.gpus)
            {
                self.gpu_procs = Some(Err(e.clone()));
            }
        }
    }

    /// Nova coleta da thread lenta.
    pub fn push_slow(&mut self, s: Slow) {
        if let Some(p) = &s.procs
            && self.tab == Tab::Processes
        {
            self.procs = Some(p.clone());
        }
        if self.tab == Tab::Gpu {
            if let Some(p) = &s.procs {
                self.procs = Some(p.clone());
            }
            match &s.gpus {
                Ok(list) => {
                    if let Some(g) = self.slow_gpu(list) {
                        if let Some(p) = &g.procs {
                            self.gpu_procs = Some(p.clone());
                        }
                    } else {
                        // A GPU exibida não está na lista lenta (sumiu, ou a lenta ainda não a
                        // achou): a lista dela não vale mais.
                        self.gpu_procs = None;
                        self.runtimes.clear();
                    }
                }
                Err(e) => self.gpu_procs = Some(Err(e.clone())),
            }
            // Runtimes de processos que saíram da lista não valem mais.
            if let Some(Ok(list)) = &self.gpu_procs {
                self.runtimes
                    .retain(|pid, (name, _)| list.iter().any(|p| p.pid == *pid && &p.name == name));
            }
        }
        self.slow = Some(s);
        self.update_sensors();
    }

    /// PIDs que a leitura de módulos nunca abre: o jogo e os apps com o hook do RTSS.
    /// `None`: o RTSS ainda não deu a lista (início, RTSS fechado ou reconectando): sem ela não
    /// dá para saber qual processo é jogo, então nenhum é aberto.
    pub fn protected_pids(&self) -> Option<Vec<u32>> {
        let f = self.fps.as_ref()?;
        f.rtss.as_ref().ok()?;
        let mut v = f.hooked.clone();
        if let Ok(g) = &f.game {
            v.push(g.pid);
        }
        Some(v)
    }

    /// Enter na aba GPU: pedido de leitura dos runtimes do processo selecionado (aplicando
    /// antes as teclas de seleção ainda pendentes, sobre a lista exibida). A leitura em si roda
    /// fora da thread da interface (`RuntimeJob::run`); até lá, "reading modules".
    pub fn request_runtimes(&mut self) -> Option<RuntimeJob> {
        let Some(Ok(list)) = &self.gpu_procs else {
            return None;
        };
        let rows = tabs::sorted_gpu(list, self.gpu_sort, self.gpu_reverse, &self.filter);
        let pids: Vec<u32> = rows.iter().map(|p| p.pid).collect();
        let page = self.gpu_cursor.page();
        let (Some(i), _) = self.gpu_cursor.resolve(&pids, page) else {
            return None;
        };
        let (pid, name) = (rows[i].pid, rows[i].name.clone());
        // Uma leitura por processo de cada vez (Enter segurado repetiria a tecla ~30 vezes por
        // segundo, cada uma com uma thread).
        if self
            .runtimes
            .get(&pid)
            .is_some_and(|(n, r)| *n == name && is_reading(r))
        {
            return None;
        }
        let create = match &self.procs {
            Some(Ok(l)) => l.iter().find(|p| p.pid == pid).map(|p| p.create),
            _ => None,
        };
        let Some(protected) = self.protected_pids() else {
            self.runtimes.insert(
                pid,
                (
                    name,
                    crate::model::neutral(
                        "skipped: waiting for RTSS to list the hooked apps (anticheat safety)",
                    ),
                ),
            );
            return None;
        };
        self.runtimes
            .insert(pid, (name.clone(), crate::model::neutral(READING)));
        Some(RuntimeJob {
            pid,
            name,
            create,
            protected,
        })
    }

    /// Resultado de uma leitura de runtimes (vale se o processo da linha ainda é o mesmo).
    pub fn finish_runtimes(&mut self, pid: u32, name: String, r: Metric<Vec<&'static str>>) {
        if self.runtimes.get(&pid).is_some_and(|(n, _)| *n == name) {
            self.runtimes.insert(pid, (name, r));
        }
    }

    /// Processo da linha selecionada na aba Processos (na ordem e com o filtro da tela).
    pub fn selected_proc(&self) -> Option<Proc> {
        let Some(Ok(list)) = &self.procs else {
            return None;
        };
        let rows = tabs::proc_rows(self, list);
        let pids: Vec<u32> = rows.iter().map(|(p, _)| p.pid).collect();
        self.proc_cursor.peek(&pids).map(|i| rows[i].0.clone())
    }

    pub fn reset_session(&mut self) {
        self.session = Session::default();
        self.game_fps = Series::default();
        self.game_ft = Series::default();
        self.pending_fps = None;
        self.pending_ft = None;
    }

    /// Nova coleta da thread rápida: guarda, alimenta os históricos (inclusive os do jogo, no
    /// mesmo ritmo) e os extremos da sessão.
    pub fn push_fast(&mut self, f: Fast) {
        let usage = |m: &Metric<f64>| m.as_ref().map_or(f64::NAN, |v| v / 100.0);
        if let Ok(c) = &f.cpu {
            self.cpu.push(usage(&c.usage));
            self.cpu_kernel.push(usage(&c.kernel));
            if let Ok(v) = &c.usage {
                self.session.cpu.update(*v);
            }
            if let Ok(per) = &c.per_logical {
                self.cores.resize_with(per.len(), Series::default);
                self.cores_kernel.resize_with(per.len(), Series::default);
                let kern = c.kernel_per_logical.as_ref().ok();
                for (i, (s, v)) in self.cores.iter_mut().zip(per).enumerate() {
                    s.push(v / 100.0);
                    let k = kern.and_then(|k| k.get(i)).copied().unwrap_or(0.0);
                    self.cores_kernel[i].push(k / 100.0);
                }
            }
        }
        if let Ok(s) = &f.cpu_sensors {
            if let Ok(t) = &s.tctl {
                self.session.tctl.update(*t);
            }
            if let Ok(w) = &s.package_power {
                self.session.cpu_power.update(*w);
            }
        }
        if let Ok(list) = &f.gpus {
            self.gpu.resize_with(list.len(), Series::default);
            for (s, g) in self.gpu.iter_mut().zip(list) {
                let (_, v) = busiest_engine(g);
                let any = [&g.usage, &g.encoder, &g.decoder].iter().any(|m| m.is_ok());
                s.push(if any { f64::from(v) / 100.0 } else { f64::NAN });
            }
        }
        // A GPU selecionada sumiu (eGPU desconectada): volta para a primeira, com os extremos
        // zerados (senão os da antiga recebem os valores da nova).
        if let Ok(list) = &f.gpus
            && !list.is_empty()
            && self.gpu_sel >= list.len()
        {
            // `fast` ainda é a coleta anterior: a lista da aba vem da lenta, já pelo índice 0.
            self.select_gpu(0);
        }
        if let Ok(list) = &f.gpus
            && let Some(g) = list.get(self.gpu_index(list.len()))
        {
            let (_, v) = busiest_engine(g);
            if [&g.usage, &g.encoder, &g.decoder].iter().any(|m| m.is_ok()) {
                self.session.gpu.update(f64::from(v));
            }
            if let Ok(t) = &g.temp {
                self.session.gpu_temp.update(f64::from(*t));
            }
            if let Ok((p, _)) = &g.power {
                self.session.gpu_power.update(*p);
            }
            if let Ok((u, _)) = &g.dedicated {
                self.session.vram.update(*u as f64);
            }
            if let Ok((c, _)) = &g.clock_graphics {
                self.session.gfx_clock.update(f64::from(*c));
            }
        }
        if let Ok(m) = &f.memory {
            if let Ok(u) = &m.used {
                self.session.ram.update(*u as f64);
            }
            if let Ok((c, _)) = &m.commit {
                self.session.commit.update(*c as f64);
            }
        }
        if let Ok(list) = &f.disks {
            self.disks
                .retain(|n, _| list.iter().any(|d| d.number == *n));
            for d in list {
                self.disks
                    .entry(d.number)
                    .or_default()
                    .push(usage(&d.active));
            }
        }
        if let Ok(list) = &f.nics {
            self.send.retain(|a, _| list.iter().any(|n| &n.alias == a));
            self.recv.retain(|a, _| list.iter().any(|n| &n.alias == a));
            for n in list {
                let v = |m: &Metric<f64>| m.as_ref().copied().unwrap_or(f64::NAN);
                self.send
                    .entry(n.alias.clone())
                    .or_default()
                    .push(v(&n.send));
                self.recv
                    .entry(n.alias.clone())
                    .or_default()
                    .push(v(&n.recv));
                if let Ok(s) = &n.send {
                    self.session
                        .send
                        .entry(n.alias.clone())
                        .or_default()
                        .update(*s);
                }
                if let Ok(r) = &n.recv {
                    self.session
                        .recv
                        .entry(n.alias.clone())
                        .or_default()
                        .update(*r);
                }
            }
        }
        // Jogo no ritmo da UI: 1 ponto por tick (FPS mais recente; pior frametime do tick),
        // enquanto houver jogo em foco, mesmo sem dado novo no tick (intervalo menor que o do
        // RTSS, ou jogo congelado: FPS atual, 0 sem frames no último segundo). As duas séries
        // andam juntas; frametime sem frame no tick é lacuna (NaN), nunca um 0 ms falso.
        let (fps, ft) = (self.pending_fps.take(), self.pending_ft.take());
        let game = self.fps.as_ref().and_then(|f| f.game.as_ref().ok());
        if game.is_some() || fps.is_some() || ft.is_some() {
            let now = game.and_then(|g| g.fps.as_ref().ok().copied());
            self.game_fps.push(fps.or(now).unwrap_or(0.0));
            self.game_ft.push(ft.unwrap_or(f64::NAN));
        }
        self.fast = Some(f);
        self.update_sensors();
    }

    /// Novo estado do RTSS (a cada 250 ms com jogo): o número de FPS atualiza na hora; os
    /// gráficos esperam o tick da UI.
    pub fn push_fps(&mut self, s: Fps) {
        if let Ok(g) = &s.game {
            if let Ok(v) = &g.fps {
                self.pending_fps = Some(*v);
                self.session.fps.update(*v);
            }
            // Extremos do frametime: o melhor e o pior frame vistos (coerentes com o atual).
            if let Ok((_, lo, hi)) = &g.frametime {
                self.session.ft.update(*lo);
                self.session.ft.update(*hi);
            }
            if let Some(p) = g.peak_ms {
                self.pending_ft = Some(self.pending_ft.map_or(p, |q| q.max(p)));
            }
        }
        self.fps = Some(s);
    }

    /// Extremos da sessão de cada sensor numérico (aba Sensores), com os dados atuais.
    fn update_sensors(&mut self) {
        for r in tabs::sensor_rows(self) {
            if let (Some(key), tabs::SensorValue::Num(Ok(v), ..)) = (r.key(), &r.value) {
                self.session.sensors.entry(key).or_default().update(*v);
            }
        }
    }
}

/// Leitura de runtimes pedida pela interface, feita numa thread à parte (abrir um processo
/// travado não congela a tela).
pub struct RuntimeJob {
    pub pid: u32,
    pub name: String,
    pub create: Option<u64>,
    pub protected: Vec<u32>,
}

impl RuntimeJob {
    pub fn run(&self) -> Metric<Vec<&'static str>> {
        crate::collect::modules::runtimes(self.pid, &self.name, self.create, &self.protected)
    }
}

// --- Texto ---

/// Valor, estado neutro (cinza) ou falha (vermelho, com a causa); sem controles.
fn metric<T>(m: &Metric<T>, f: impl FnOnce(&T) -> String) -> Span<'static> {
    match m {
        Ok(v) => Span::styled(printable(&tr(&f(v))), Style::new().fg(th().text)),
        Err(Missing::Neutral(s)) => Span::styled(printable(&tr(s)), Style::new().fg(th().neutral)),
        Err(Missing::Fail(s)) => Span::styled(
            format!("! {}", printable(&tr(s))),
            Style::new().fg(th().fail),
        ),
    }
}

fn missing(e: &Missing) -> Span<'static> {
    metric::<()>(&Err(e.clone()), |()| String::new())
}

/// Rótulo, já no idioma da interface (`i18n`).
fn label(s: &str) -> Span<'static> {
    Span::styled(tr(s).into_owned(), Style::new().fg(th().label))
}

/// Valor, já no idioma da interface (nomes e números passam intactos).
fn text(s: impl Into<String>) -> Span<'static> {
    let s: String = s.into();
    let s = match tr(&s) {
        std::borrow::Cow::Owned(t) => t,
        std::borrow::Cow::Borrowed(_) => s,
    };
    Span::styled(s, Style::new().fg(th().text))
}

/// Valor zerado em tom de rótulo (o número fica, só sai do primeiro plano).
fn faded(sp: Span<'static>, zero: bool) -> Span<'static> {
    if zero {
        sp.style(Style::new().fg(th().label))
    } else {
        sp
    }
}

fn sep() -> Span<'static> {
    Span::raw("  ")
}

fn bar_sep() -> Span<'static> {
    Span::styled("  │  ", Style::new().fg(th().dim))
}

/// "min X max Y" de um extremo da sessão.
fn minmax(m: MinMax, f: impl Fn(f64) -> String) -> Span<'static> {
    match m.get() {
        Some((lo, hi)) => label(&format!("min {} · max {}", f(lo), f(hi))),
        None => label(""),
    }
}

/// Caixa com borda neutra, título à esquerda e contexto à direita, na própria borda.
fn boxed(title: &str, right: &str) -> Block<'static> {
    boxed_ready(title, &tr(right))
}

/// Como `boxed`, com o texto da direita já no idioma da interface: o que o usuário digitou (o
/// filtro) não pode passar pela tradução ("free" viraria "livre").
fn boxed_ready(title: &str, right: &str) -> Block<'static> {
    let mut b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(th().border))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                printable(&tr(title)),
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]));
    if !right.is_empty() {
        b = b.title(
            Line::from(Span::styled(
                format!(" {right} "),
                Style::new().fg(th().label),
            ))
            .right_aligned(),
        );
    }
    b
}

/// Nome da caixa sem valor: a causa entra no título, com a cor do estado (vermelho na falha).
fn with_failure(b: Block<'static>, e: Option<&Missing>) -> Block<'static> {
    match e {
        Some(e) => b.title(Line::from(vec![missing(e), Span::raw(" ")])),
        None => b,
    }
}

/// Desenha um parágrafo recortado a `bound`.
/// Valor curto para um cabeçalho sem espaço para a causa: o valor, "-" (estado neutro) ou
/// "! failed" (a causa inteira vai, quebrada, nas linhas de detalhe da caixa).
fn brief<T>(m: &Metric<T>, f: impl FnOnce(&T) -> String) -> Span<'static> {
    match m {
        Ok(v) => text(f(v)),
        Err(Missing::Neutral(_)) => label("-"),
        Err(Missing::Fail(_)) => failed_span(),
    }
}

/// "! failed", no idioma da interface (mesma largura em pt-BR: "! falhou").
fn failed_span() -> Span<'static> {
    Span::styled(tr("! failed").into_owned(), Style::new().fg(th().fail))
}

fn put(frame: &mut Frame<'_>, p: Paragraph<'static>, r: Rect, bound: Rect) {
    frame.render_widget(p, r.intersection(bound));
}

fn line(frame: &mut Frame<'_>, spans: Vec<Span<'static>>, x: u16, y: u16, w: u16, bound: Rect) {
    put(
        frame,
        Paragraph::new(Line::from(spans)),
        Rect::new(x, y, w, 1),
        bound,
    );
}

/// Bytes curtos (477G, 3.1T, 859M) para colunas estreitas.
fn short_bytes(v: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut x = v as f64;
    let mut u = 0;
    while x >= 1024.0 && u < UNITS.len() - 1 {
        x /= 1024.0;
        u += 1;
    }
    if x >= 100.0 || u == 0 {
        format!("{x:.0}{}", UNITS[u])
    } else {
        format!("{}{}", num(x, 1), UNITS[u])
    }
}

fn temp(m: &Metric<f64>) -> Span<'static> {
    metric(m, |v| format!("{} °C", num(*v, 1)))
}

// --- Layout ---

/// Menor terminal em que a aba mostra tudo sem virar ruído (a Geral tem seis caixas; as outras
/// são tabelas); abaixo, o aviso honesto.
pub fn min_size(tab: Tab) -> (u16, u16) {
    match tab {
        Tab::Overview => (80, 24),
        _ => (60, 12),
    }
}

pub fn draw(frame: &mut Frame<'_>, st: &mut State) {
    st.hits = Hits::default();
    let a = frame.area();
    let (min_w, min_h) = min_size(st.tab);
    if a.width < min_w || a.height < min_h {
        let msg = vec![
            Line::from(Span::styled(
                tr("terminal too small").into_owned(),
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                label(&format!("this tab needs {min_w}x{min_h}")),
                label(&format!("; the terminal is {}x{}", a.width, a.height)),
            ]),
            Line::from(label("resize the window, or press q to quit")),
        ];
        let h = (msg.len() as u16).min(a.height);
        let r = Rect::new(a.x, a.y + a.height.saturating_sub(h) / 2, a.width, h);
        frame.render_widget(Paragraph::new(msg).centered(), r);
        return;
    }
    // Aviso do config (arquivo com erro, gravação recusada): linhas próprias acima do rodapé,
    // inteiras, sem disputar espaço com o estado das fontes.
    let mut notice: Vec<Line<'static>> = st.notice.as_ref().map_or_else(Vec::new, |n| {
        wrap(
            vec![Span::styled(
                format!("! {}", printable(n)),
                Style::new().fg(th().fail),
            )],
            frame.area().width,
            1,
            3,
        )
    });
    // Resultado de uma ação (some depois de alguns segundos).
    if let Some((msg, err, at)) = &st.toast
        && at.elapsed() < TOAST_FOR
    {
        let (mark, color) = if *err {
            ("! ", th().fail)
        } else {
            ("", th().title)
        };
        notice.extend(wrap(
            vec![Span::styled(
                format!("{mark}{}", printable(&tr(msg))),
                Style::new().fg(color),
            )],
            frame.area().width,
            1,
            3,
        ));
    }
    let notice_h = (notice.len() as u16).min(frame.area().height.saturating_sub(10));
    let [tabs, body, notice_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(notice_h),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    if notice_h > 0 {
        frame.render_widget(Paragraph::new(notice), notice_area);
    }
    draw_tabs(frame, tabs, st);
    draw_footer(frame, footer, st);
    match st.tab {
        Tab::Overview => draw_overview(frame, body, st),
        Tab::Processes => tabs::draw_processes(frame, body, st),
        Tab::Gpu => tabs::draw_gpu_tab(frame, body, st),
        Tab::Sensors => tabs::draw_sensors(frame, body, st),
    }
    if st.help {
        draw_help(frame, body);
    }
    if let Some((pid, name, _)) = &st.confirm_end {
        draw_confirm_end(frame, body, *pid, name);
    }
}

/// Caixas da aba Geral que a tecla `z` amplia, na ordem em que ela as percorre.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zoom {
    Cpu,
    Gpu,
    Net,
    Disks,
    Game,
}

impl Zoom {
    const ALL: [Self; 5] = [Self::Cpu, Self::Gpu, Self::Net, Self::Disks, Self::Game];

    /// Próxima caixa; depois da última, a aba volta ao normal.
    pub fn next(z: Option<Self>) -> Option<Self> {
        match z {
            None => Some(Self::ALL[0]),
            Some(z) => {
                let i = Self::ALL.iter().position(|x| *x == z).unwrap_or(0);
                Self::ALL.get(i + 1).copied()
            }
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu + memory",
            Self::Gpu => "gpu",
            Self::Net => "net",
            Self::Disks => "disks",
            Self::Game => "game",
        }
    }
}

/// Onde cada coisa clicável ficou no último desenho (o mouse só acelera o que o teclado faz).
#[derive(Default)]
pub struct Hits {
    /// Abas na barra do topo: (coluna inicial, coluna final exclusiva, aba).
    pub tabs: Vec<(u16, u16, Tab)>,
    /// Lista da aba Processos ou GPU.
    pub list: Option<ListHit>,
}

pub struct ListHit {
    /// Linha do cabeçalho e as colunas visíveis: (início, fim exclusivo, índice da coluna).
    pub header_y: u16,
    pub cols: Vec<(u16, u16, usize)>,
    /// Linhas de dados visíveis e a primeira linha da lista que aparece nelas.
    pub body: Rect,
    pub offset: usize,
    pub len: usize,
}

/// Tempo em que a mensagem de uma ação fica na tela.
pub const TOAST_FOR: std::time::Duration = std::time::Duration::from_secs(6);

/// Confirmação de encerrar processo (por cima da aba).
fn draw_confirm_end(frame: &mut Frame<'_>, area: Rect, pid: u32, name: &str) {
    let lines = vec![
        Line::from(vec![
            label(" End "),
            Span::styled(
                format!("{} (PID {pid})", printable(name)),
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            ),
            label("? Unsaved work in it is lost. "),
        ]),
        Line::from(vec![
            Span::styled(
                " y",
                Style::new().fg(th().fail).add_modifier(Modifier::BOLD),
            ),
            label(" end process    "),
            Span::styled(
                "n / esc",
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            ),
            label(" cancel"),
        ]),
    ];
    let w = (lines.iter().map(|l| l.width() as u16).max().unwrap_or(0) + 3).min(area.width);
    let h = 4.min(area.height);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    frame.render_widget(ratatui::widgets::Clear, r);
    frame.render_widget(Paragraph::new(lines).block(boxed("end process", "")), r);
}

/// Atalhos e siglas da interface (tecla `?`), numa janela por cima da aba.
const HELP: &[(&str, &str)] = &[
    ("1-4", "switch tabs (Overview, Processes, GPU, Sensors)"),
    (
        "+ / -",
        "update interval (100 ms steps, saved to config.toml)",
    ),
    ("p, space", "pause the screen (collection keeps running)"),
    (
        "z",
        "Overview: enlarge one box to the whole tab (z next, esc back)",
    ),
    ("r", "reset session min/max and the game history"),
    ("g", "next GPU, with more than one (Overview, GPU)"),
    ("t", "next color theme"),
    (
        "d",
        "fold or expand the Storage Space members in the disk box",
    ),
    ("↑↓ j k", "select a row (PgUp/PgDn, Home/End)"),
    ("←→  i", "sort column / invert order (Processes, GPU)"),
    ("/  esc", "filter by name or PID / clear the filter"),
    (
        "enter",
        "GPU tab: GPU runtimes of the selected process (on demand)",
    ),
    (
        "del",
        "Processes tab: end the selected process (asks to confirm)",
    ),
    ("e", "Processes tab: tree of parent and child processes"),
    ("q, esc", "quit (Ctrl+C also works)"),
    ("", ""),
    (
        "kern",
        "kernel (privileged) share of the CPU time, darker in the graphs",
    ),
    (
        "Tctl, CCD",
        "AMD control and per-chiplet temperatures (PawnIO)",
    ),
    ("S  P", "disk with the system / with a page file"),
    ("act, resp", "disk active time / average response time"),
    (
        "FT, lows",
        "frametime; average FPS of the slowest 1% and 0.1% frames",
    ),
    (
        "pipe: exact",
        "per-frame times from RTSS; approx = shared-memory sampling",
    ),
    (
        "red text",
        "a failed source, with its cause; gray = expected state",
    ),
];

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let keys: Vec<String> = HELP.iter().map(|(k, _)| tr(k).into_owned()).collect();
    let key_w = keys.iter().map(|k| k.chars().count()).max().unwrap_or(0);
    let lines: Vec<Line<'static>> = keys
        .iter()
        .zip(HELP.iter().map(|(_, d)| d))
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(
                    format!(" {k:<key_w$}  "),
                    Style::new().fg(th().title).add_modifier(Modifier::BOLD),
                ),
                label(d),
            ])
        })
        .collect();
    let w = lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(0)
        .saturating_add(3)
        .min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    frame.render_widget(ratatui::widgets::Clear, r);
    frame.render_widget(
        Paragraph::new(lines).block(boxed("help", "any key closes")),
        r,
    );
}

fn draw_overview(frame: &mut Frame<'_>, body: Rect, st: &mut State) {
    // Uma caixa ampliada ocupa a aba inteira.
    if let Some(z) = st.zoom {
        match z {
            Zoom::Cpu => {
                let mem_rows = mem_text(st, body.width).map_or(2, |t| t.height().saturating_sub(2));
                draw_cpu(frame, body, st, mem_rows);
            }
            Zoom::Gpu => draw_gpu(frame, body, st),
            Zoom::Net => draw_net(frame, body, st),
            Zoom::Disks => draw_disks(frame, body, st),
            Zoom::Game => draw_game(frame, body, st),
        }
        return;
    }
    // Em cima, CPU + memória (como GPU + VRAM ao lado); embaixo, rede compacta à esquerda e
    // discos sobre o jogo à direita.
    // Em cima, pelo menos o que CPU + memória e GPU + VRAM precisam para mostrar todas as
    // linhas de detalhe (o conhost do atalho abre com 30 linhas); os gráficos é que encolhem.
    const TOP_MIN: u16 = 23;
    // Sem jogo nesta sessão (nenhum histórico para manter), a caixa do jogo fica numa faixa de
    // 5 linhas e a altura que ela não usa vai para os gráficos de cima; com jogo ou com o
    // histórico do último, ela tem altura.
    let game_known = st.fps.as_ref().is_some_and(|f| f.game.is_ok()) || !st.game_fps.0.is_empty();
    let game_min = if game_known { 8 } else { 5 };
    // Discos: cabeçalho + uma linha por disco visível, mais uma por causa de falha (inteira,
    // embaixo da tabela). Largura da coluna da direita, como no layout abaixo.
    let right_w = body.width - body.width * 34 / 100;
    let disk_rows = (disk_order(st).len() + disk_notes(st, right_w.saturating_sub(2)).len()) as u16;
    let mut top_h = (body.height * 58 / 100).max(TOP_MIN);
    if !game_known {
        // Embaixo, só o que discos e a faixa do jogo pedem (a rede compacta cabe nisso).
        let bottom_need = (disk_rows + 3 + game_min).max(12);
        top_h = top_h.max(body.height.saturating_sub(bottom_need));
    }
    // Embaixo, sempre o mínimo útil: algumas linhas de discos e a faixa do jogo.
    let bottom_min = ((disk_rows.min(3) + 3 + game_min).max(9)).min(body.height / 2);
    let top_h = top_h.min(body.height.saturating_sub(bottom_min));
    let [top, bottom] =
        Layout::vertical([Constraint::Length(top_h), Constraint::Min(0)]).areas(body);
    let [cpu, gpu] =
        Layout::horizontal([Constraint::Percentage(59), Constraint::Percentage(41)]).areas(top);
    let [net, right] =
        Layout::horizontal([Constraint::Percentage(34), Constraint::Percentage(66)]).areas(bottom);
    // Linhas da memória dentro da caixa da CPU (sem as bordas, que são da caixa).
    let mem_rows = mem_text(st, cpu.width).map_or(2, |t| t.height().saturating_sub(2));
    let disks_h = (disk_rows + 3)
        .min(right.height.saturating_sub(game_min))
        .max(4);
    let [disks, game] =
        Layout::vertical([Constraint::Length(disks_h), Constraint::Min(4)]).areas(right);
    draw_cpu(frame, cpu, st, mem_rows);
    draw_gpu(frame, gpu, st);
    draw_net(frame, net, st);
    draw_disks(frame, disks, st);
    draw_game(frame, game, st);
}

fn draw_tabs(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    // Estreito: abas com um espaço só entre elas (sobra para o estado à direita).
    let pad = if area.width < 100 { "" } else { " " };
    let tab = |n: &str, name: &str, t: Tab| -> Span<'static> {
        if st.tab == t {
            Span::styled(
                format!("{pad}[{n} {name}]{pad}"),
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            )
        } else {
            label(&format!(" {pad}{n} {name}{pad} "))
        }
    };
    let spans = vec![
        tab("1", &tr("Overview"), Tab::Overview),
        tab("2", &tr("Processes"), Tab::Processes),
        tab("3", "GPU", Tab::Gpu),
        tab("4", &tr("Sensors"), Tab::Sensors),
    ];
    let mut x = area.x;
    for (sp, t) in spans
        .iter()
        .zip([Tab::Overview, Tab::Processes, Tab::Gpu, Tab::Sensors])
    {
        let w = sp.width() as u16;
        st.hits.tabs.push((x, x + w, t));
        x += w;
    }
    let tabs = Line::from(spans);
    let tw = (tabs.width() as u16).min(area.width);
    let [left, rest] = Layout::horizontal([Constraint::Length(tw), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(tabs), left);
    let room = usize::from(rest.width.saturating_sub(1));
    let width = |v: &[Span<'static>]| v.iter().map(Span::width).sum::<usize>();
    let key = |k: &str| {
        Span::styled(
            tr(k).into_owned(),
            Style::new().fg(th().title).add_modifier(Modifier::BOLD),
        )
    };
    // O essencial primeiro (ampliação, pausa, intervalo); uptime e relógio só se couberem.
    let mut right = Vec::new();
    if let Some(z) = st.zoom
        && st.tab == Tab::Overview
    {
        right.push(key(&format!("zoom: {} ", z.name())));
        right.push(label("(z next, esc back)"));
        right.push(bar_sep());
    }
    if st.paused {
        // Destaque sem a cor de falha (vermelho é só para erro); os colchetes valem em
        // monocromático. Forma curta quando estreito.
        let long = "[PAUSED · p resumes]";
        right.push(Span::styled(
            tr(if room >= 40 { long } else { "[PAUSED]" }).into_owned(),
            Style::new()
                .fg(th().frametime)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ));
        right.push(bar_sep());
    }
    // Intervalo com as teclas que o mudam ao lado (como o btop).
    right.extend([
        key("- "),
        label(&format!("{} ms", st.interval_ms)),
        key(" +"),
    ]);
    let up = st
        .fast
        .as_ref()
        .and_then(|f| f.cpu.as_ref().ok())
        .map(|c| c.uptime.as_secs());
    if let Some(s) = up {
        let extra = [
            bar_sep(),
            label(&format!(
                "up {}d {:02}:{:02}:{:02}",
                s / 86400,
                s / 3600 % 24,
                s / 60 % 60,
                s % 60
            )),
        ];
        if width(&right) + width(&extra) <= room {
            right.extend(extra);
        }
    }
    if !st.clock.is_empty() {
        let extra = [bar_sep(), text(st.clock.clone())];
        if width(&right) + width(&extra) <= room {
            right.extend(extra);
        }
    }
    frame.render_widget(Paragraph::new(Line::from(right)).right_aligned(), rest);
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, st: &State) {
    let key = |k: &str| {
        Span::styled(
            tr(k).into_owned(),
            Style::new().fg(th().title).add_modifier(Modifier::BOLD),
        )
    };
    // Atalhos em grupos inteiros (tecla + rótulo): um grupo que não cabe sai inteiro.
    // Digitando o filtro, as teclas viram texto: só os atalhos do filtro (e Ctrl+C).
    let mut groups: Vec<[Span<'static>; 2]> = if st.filtering {
        Vec::new()
    } else {
        vec![[key(" 1-4"), label(" tabs  ")]]
    };
    // `d` só com Storage Space na tela, dizendo o que a tecla vai fazer agora.
    let disks = st.fast.as_ref().and_then(|f| f.disks.as_ref().ok());
    let pool = disks.is_some_and(|l| l.iter().any(|d| d.pool_member == Ok(true)));
    let fold = if pool_folded(st, disks.map_or(0, Vec::len)) {
        " expand pool  "
    } else {
        " fold pool  "
    };
    let overview: Vec<(&str, &str)> = if pool {
        vec![
            ("r", " reset min/max  "),
            ("z", " zoom  "),
            ("d", fold),
            ("t", " theme  "),
        ]
    } else {
        vec![
            ("r", " reset min/max  "),
            ("z", " zoom  "),
            ("t", " theme  "),
        ]
    };
    let sensors: Vec<(&str, &str)> = if st.sensor_overflow || st.sensor_offset > 0 {
        vec![("↑↓", " scroll  "), ("r", " reset min/max  ")]
    } else {
        vec![("r", " reset min/max  ")]
    };
    let pairs: &[(&str, &str)] = match st.tab {
        Tab::Overview => &overview,
        Tab::Processes | Tab::Gpu if st.filtering => &[
            (" type", " to filter  "),
            ("enter", " keep  "),
            ("esc", " clear  "),
        ],
        Tab::Processes if !st.filter.is_empty() => &[
            ("↑↓", " select  "),
            ("←→", " sort  "),
            ("i", " invert  "),
            ("/", " filter  "),
            ("esc", " clear filter  "),
            ("e", " tree  "),
            ("del", " end process  "),
        ],
        Tab::Processes => &[
            ("↑↓", " select  "),
            ("←→", " sort  "),
            ("i", " invert  "),
            ("/", " filter  "),
            ("e", " tree  "),
            ("del", " end process  "),
        ],
        Tab::Gpu if !st.filter.is_empty() => &[
            ("↑↓", " select  "),
            ("←→", " sort  "),
            ("i", " invert  "),
            ("/", " filter  "),
            ("esc", " clear filter  "),
            ("enter", " runtimes  "),
        ],
        Tab::Gpu => &[
            ("↑↓", " select  "),
            ("←→", " sort  "),
            ("i", " invert  "),
            ("/", " filter  "),
            ("enter", " runtimes  "),
        ],
        Tab::Sensors => &sensors,
    };
    for (k, l) in pairs {
        groups.push([key(k), label(l)]);
    }
    if !st.filtering && matches!(st.tab, Tab::Overview | Tab::Gpu) && st.gpu_count() > 1 {
        groups.push([key("g"), label(" next gpu  ")]);
    }
    if !st.filtering {
        groups.push([key("p"), label(" pause  ")]);
        groups.push([key("?"), label(" help  ")]);
        groups.push([key("q"), label(" quit")]);
    }
    // O estado completo à direita, ou o curto quando as teclas não cabem com ele (uma falha
    // aparece sempre inteira).
    let failing = failing_sources(st);
    let status = |narrow: bool| -> Vec<Span<'static>> {
        let mut right = Vec::new();
        if let Some(f) = &st.fast {
            match &f.own {
                Ok(o) => {
                    right.extend([
                        label("wintop "),
                        metric(&o.cpu, |v| format!("{}%", num(*v, 2))),
                        Span::raw(" "),
                        metric(&o.private_ws, |v| bytes(*v)),
                    ]);
                    if !narrow || o.commit.is_err() {
                        right.extend([
                            label(" ("),
                            metric(&o.commit, |v| format!("{} commit", bytes(*v))),
                            label(")"),
                        ]);
                    }
                }
                Err(e) => right.extend([label("wintop "), missing(e)]),
            }
            if !narrow {
                right.extend([
                    bar_sep(),
                    label(&format!(
                        "collection {} ms",
                        num(f.took.as_secs_f64() * 1e3, 1)
                    )),
                ]);
            }
        }
        if !right.is_empty() {
            right.push(bar_sep());
        }
        right.push(match failing {
            0 if narrow => label("all OK "),
            0 => label("sources: all OK "),
            1 => Span::styled(
                tr("! 1 source failing ").into_owned(),
                Style::new().fg(th().fail),
            ),
            n => Span::styled(
                tr(&format!("! {n} sources failing ")).into_owned(),
                Style::new().fg(th().fail),
            ),
        });
        right
    };
    let keys_w: usize = groups.iter().map(|g| g[0].width() + g[1].width()).sum();
    let full = status(false);
    let full_w: usize = full.iter().map(Span::width).sum();
    let right = if keys_w + full_w + 2 <= usize::from(area.width) {
        full
    } else {
        status(true)
    };
    // O lado direito (estado) tem prioridade; a ajuda usa o que sobra, com 2 colunas de folga.
    let right = Line::from(right);
    let rw = (right.width() as u16).min(area.width);
    let room = usize::from(area.width.saturating_sub(rw).saturating_sub(2));
    // Prioridade na falta de espaço: abas, sair, ajuda, pausa e depois os atalhos da aba;
    // digitando o filtro só há os atalhos dele, na ordem. A ordem na tela continua a natural.
    let n = groups.len();
    let mut priority: Vec<usize> = if st.filtering || n < 4 {
        (0..n).collect()
    } else {
        // Abas, sair, ajuda, os atalhos da aba e por último a pausa.
        [0, n - 1, n - 2]
            .into_iter()
            .chain(1..n - 3)
            .chain([n - 3])
            .collect()
    };
    priority.dedup();
    let mut keep = vec![false; n];
    let mut used = 0;
    for i in priority {
        let w = groups[i][0].width() + groups[i][1].width();
        if used + w <= room {
            used += w;
            keep[i] = true;
        }
    }
    let left: Vec<Span<'static>> = groups
        .into_iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .flat_map(|(g, _)| g)
        .collect();
    let left = Line::from(left);
    let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(rw)]).areas(area);
    frame.render_widget(Paragraph::new(left), l);
    frame.render_widget(Paragraph::new(right).right_aligned(), r);
}

/// Métrica em falha (estado neutro não conta).
fn failed<T>(m: &Metric<T>) -> bool {
    matches!(m, Err(Missing::Fail(_)))
}

/// Fonte em falha: o coletor inteiro, ou qualquer métrica dela (o que aparece em vermelho na
/// tela conta no rodapé). `inner`: as métricas da fonte quando o coletor respondeu.
fn source_failed<T>(m: &Metric<T>, inner: impl FnOnce(&T) -> bool) -> bool {
    match m {
        Ok(v) => inner(v),
        Err(e) => matches!(e, Missing::Fail(_)),
    }
}

/// GPUs que a tela mostra: a selecionada (tecla `g`), ou todas na aba Sensores.
fn shown_gpus<'a, T>(st: &State, l: &'a [T]) -> impl Iterator<Item = &'a T> {
    let (skip, take) = if st.tab == Tab::Sensors {
        (0, l.len())
    } else {
        (st.gpu_index(l.len()), 1)
    };
    l.iter().skip(skip).take(take)
}

/// Fontes com alguma falha.
pub(super) fn failing_sources(st: &State) -> usize {
    let mut n = 0;
    let mut add = |b: bool| n += usize::from(b);
    if let Some(f) = &st.fast {
        add(source_failed(&f.cpu, |c| {
            failed(&c.name)
                || failed(&c.topology)
                || failed(&c.base_mhz)
                || failed(&c.virtualization)
                || failed(&c.usage)
                || failed(&c.per_logical)
                || failed(&c.kernel_per_logical)
                || failed(&c.kernel)
                || failed(&c.freq_mhz)
                || failed(&c.freq_per_logical)
                || failed(&c.counts)
        }));
        add(source_failed(&f.cpu_sensors, |s| {
            failed(&s.tctl) || failed(&s.ccds) || failed(&s.package_power)
        }));
        add(source_failed(&f.memory, |m| {
            failed(&m.total)
                || failed(&m.used)
                || failed(&m.commit)
                || failed(&m.cached)
                || failed(&m.standby)
                || failed(&m.modified)
                || failed(&m.paged_pool)
                || failed(&m.nonpaged_pool)
                || failed(&m.pagefile_usage)
        }));
        add(source_failed(&f.disks, |l| {
            l.iter().any(|d| {
                failed(&d.model)
                    || failed(&d.bus)
                    || failed(&d.ssd)
                    || failed(&d.capacity)
                    || failed(&d.pool_member)
                    || failed(&d.active)
                    || failed(&d.read)
                    || failed(&d.write)
                    || failed(&d.response)
                    || failed(&d.iops)
                    || failed(&d.queue)
            })
        }));
        add(source_failed(&f.nics, |l| {
            l.iter()
                .any(|x| failed(&x.link) || failed(&x.send) || failed(&x.recv) || failed(&x.errors))
        }));
        // Só a GPU exibida (tecla `g`); na aba Sensores, que mostra todas, todas.
        add(source_failed(&f.gpus, |l| {
            shown_gpus(st, l).any(|g| {
                failed(&g.name)
                    || failed(&g.driver)
                    || failed(&g.vbios)
                    || failed(&g.usage)
                    || failed(&g.mem_activity)
                    || failed(&g.dedicated)
                    || failed(&g.shared)
                    || failed(&g.physical)
                    || failed(&g.temp)
                    || failed(&g.temp_slowdown)
                    || failed(&g.clock_graphics)
                    || failed(&g.clock_sm)
                    || failed(&g.clock_mem)
                    || failed(&g.clock_video)
                    || failed(&g.power)
                    || source_failed(&g.fans, |f| {
                        f.iter().any(|x| failed(&x.percent) || failed(&x.rpm))
                    })
                    || failed(&g.pcie_link)
                    || failed(&g.encoder)
                    || failed(&g.decoder)
                    || failed(&g.pstate)
                    || failed(&g.throttle)
            })
        }));
        add(source_failed(&f.own, |o| {
            failed(&o.cpu) || failed(&o.private_ws) || failed(&o.commit)
        }));
    }
    let mut slow_gpus_failed = false;
    if let Some(s) = &st.slow {
        slow_gpus_failed = s.gpus.is_err();
        // GPU sem NVML: o PCIe repete a causa que a coleta rápida já conta (`no_nvml`).
        let no_nvml = |i: u32| {
            st.fast
                .as_ref()
                .and_then(|f| f.gpus.as_ref().ok())
                .and_then(|l| l.iter().find(|g| g.index == i))
                .is_some_and(|g| g.no_nvml.is_some())
        };
        // Na lista lenta, a GPU exibida é a da posição da rápida (`slow_gpu`).
        add(source_failed(&s.gpus, |l| {
            let shown: Vec<&crate::model::GpuSlow> = if st.tab == Tab::Sensors {
                l.iter().collect()
            } else {
                st.slow_gpu(l).into_iter().collect()
            };
            shown.into_iter().any(|g| {
                failed(&g.vram) || (!no_nvml(g.index) && (failed(&g.pcie_tx) || failed(&g.pcie_rx)))
            })
        }));
        // Só dos discos exibidos (um disco removido ou trocado no mesmo número não aparece em
        // aba nenhuma; a lista lenta o tira na coleta seguinte).
        let shown = |t: &crate::model::DiskTemp| match &st.fast.as_ref().map(|f| &f.disks) {
            Some(Ok(disks)) => disks.iter().any(|d| same_disk(t, d)),
            _ => true,
        };
        add(source_failed(&s.disk_temps, |l| {
            l.iter()
                .any(|t| shown(t) && (failed(&t.celsius) || failed(&t.health)))
        }));
    }
    // Fontes das abas Processos e GPU (só quando visíveis). A lista da GPU herda a falha da
    // coleta lenta da GPU, já contada acima.
    if matches!(st.tab, Tab::Processes | Tab::Gpu) {
        add(matches!(&st.procs, Some(Err(Missing::Fail(_)))));
    }
    if st.tab == Tab::Gpu {
        match &st.gpu_procs {
            Some(Err(Missing::Fail(_))) if !slow_gpus_failed => add(true),
            Some(Ok(list)) => add(list.iter().any(|p| failed(&p.engines))),
            _ => {}
        }
    }
    // Runtimes pedidos na aba GPU (aparecem no detalhe dela).
    add(st.tab == Tab::Gpu && st.runtimes.values().any(|(_, r)| failed(r)));
    if let Some(f) = &st.fps {
        add(failed(&f.rtss));
        add(failed(&f.pipe));
        // O jogo sem valor herda a falha do RTSS (já contada): só as métricas dele contam.
        add(f.game.as_ref().is_ok()
            && source_failed(&f.game, |g| {
                failed(&g.resolution)
                    || failed(&g.fps)
                    || failed(&g.frametime)
                    || failed(&g.lows)
                    || failed(&g.gpu_time)
                    || failed(&g.bound)
            }));
    }
    n
}

fn waiting(frame: &mut Frame<'_>, area: Rect, block: Block<'static>) {
    frame.render_widget(
        Paragraph::new(Span::styled(
            tr("waiting for the first collection").into_owned(),
            Style::new().fg(th().neutral),
        ))
        .block(block),
        area,
    );
}

/// Caixa sem dados: espera ou a falha do coletor.
fn empty_box<T>(frame: &mut Frame<'_>, area: Rect, title: &str, m: Option<&Metric<T>>) {
    match m {
        Some(Err(e)) => frame.render_widget(
            Paragraph::new(missing(e))
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(boxed(title, "")),
            area,
        ),
        _ => waiting(frame, area, boxed(title, "")),
    }
}

// --- CPU ---

/// Colunas da grade por lógico, pelo número real de threads.
fn grid_cols(n: usize) -> usize {
    match n {
        0..=4 => 2,
        5..=16 => 4,
        17..=24 => 6,
        _ => 8,
    }
}

/// Largura do gráfico total da CPU (~45%; a grade por lógico fica com o resto).
fn total_width(w: u16) -> u16 {
    (w * 9 / 20).max(16).min(w.saturating_sub(16))
}

/// Caixa da CPU com a memória nas últimas `mem_rows` linhas (depois de um divisor).
fn draw_cpu(frame: &mut Frame<'_>, area: Rect, st: &mut State, mem_rows: u16) {
    let Some(Ok(c)) = st.fast.as_ref().map(|f| &f.cpu) else {
        // CPU sem dado: o estado dela em cima, a memória continua embaixo.
        let [top, bottom] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(mem_rows + 2)]).areas(area);
        empty_box(frame, top, "cpu", st.fast.as_ref().map(|f| &f.cpu));
        let block = boxed("memory", "");
        let inner = block.inner(bottom);
        frame.render_widget(block, bottom);
        draw_memory_body(frame, inner, st);
        return;
    };
    let name = c
        .name
        .as_ref()
        .map_or_else(|_| "cpu".to_string(), |n| format!("cpu · {n}"));
    let per_len = c.per_logical.as_ref().map_or(0, Vec::len);
    // Janela real do gráfico total: 2 amostras por coluna, no intervalo atual.
    let cols = u64::from(total_width(area.width.saturating_sub(2)).saturating_sub(1));
    let secs = cols * 2 * st.interval_ms / 1000;
    let block = with_failure(
        boxed(&name, &format!("total {secs}s · {per_len} logical")),
        c.name.as_ref().err(),
    );
    let whole = block.inner(area);
    frame.render_widget(block, area);
    // Memória embaixo, depois de um divisor rotulado (como a VRAM na caixa da GPU).
    let [inner, divider, mem] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(mem_rows),
    ])
    .areas(whole);
    put(
        frame,
        Paragraph::new(Line::from(vec![
            Span::styled("── ", Style::new().fg(th().border)),
            Span::styled(format!("{} ", tr("memory")), Style::new().fg(th().title)),
            Span::styled(
                "─".repeat(usize::from(divider.width.saturating_sub(10))),
                Style::new().fg(th().border),
            ),
        ])),
        divider,
        whole,
    );
    draw_memory_body(frame, mem, st);
    // Linhas de sensores e topologia.
    let sensors = st.fast.as_ref().map(|f| &f.cpu_sensors);
    // Clock: média de todos os lógicos e o pico (o lógico mais rápido agora).
    let mut l1 = vec![
        label("speed avg "),
        metric(&c.freq_mhz, |v| format!("{} GHz", num(v / 1000.0, 2))),
        label("  peak "),
        metric(&c.freq_per_logical, |f| {
            let peak = f.iter().copied().fold(0.0_f64, f64::max);
            format!("{} GHz", num(peak / 1000.0, 2))
        }),
        Span::raw(" "),
        metric(&c.base_mhz, |m| {
            format!("(base {})", num(f64::from(*m) / 1000.0, 2))
        }),
    ];
    // Sem o tempo de kernel por lógico a grade mostra só o total: a causa aparece.
    if let Err(e) = &c.kernel_per_logical {
        l1.extend([sep(), label("kernel per core "), missing(e)]);
    }
    match sensors {
        Some(Ok(s)) => {
            l1.extend([sep(), label("Tctl "), temp(&s.tctl)]);
            if let Some((_, hi)) = st.session.tctl.get() {
                l1.push(label(&format!(" (max {})", num(hi, 1))));
            }
            match &s.ccds {
                Ok(list) => {
                    for (n, t) in list {
                        l1.extend([sep(), label(&format!("CCD{n} ")), temp(&Ok(*t))]);
                    }
                }
                Err(e) => l1.extend([sep(), label("CCD "), missing(e)]),
            }
            l1.extend([
                sep(),
                label("power "),
                metric(&s.package_power, |v| format!("{} W", num(*v, 1))),
            ]);
            if let Some((_, hi)) = st.session.cpu_power.get() {
                l1.push(label(&format!(" (max {})", num(hi, 1))));
            }
        }
        Some(Err(e)) => l1.extend([sep(), label("sensors "), missing(e)]),
        None => {}
    }
    let mut l2 = vec![metric(&c.topology, |t| {
        format!(
            "{}S · {}C/{}T  L1 {}  L2 {}  L3 {}",
            t.sockets,
            t.cores,
            t.logical,
            bytes(t.l1),
            bytes(t.l2),
            bytes(t.l3)
        )
    })];
    l2.extend([
        sep(),
        label("virt "),
        metric(&c.virtualization, |v| {
            if *v {
                "enabled".into()
            } else {
                "disabled".into()
            }
        }),
        bar_sep(),
        metric(&c.counts, |n| {
            format!(
                "{} proc  {} thr  {} handles",
                n.processes, n.threads, n.handles
            )
        }),
    ]);
    // Uso e kernel no cabeçalho do gráfico, que é estreito: as causas vêm aqui, inteiras.
    let mut causes = Vec::new();
    for (name, m) in [("usage ", &c.usage), ("kernel ", &c.kernel)] {
        if let Err(e) = m {
            causes.push(vec![label(name), missing(e)]);
        }
    }
    let info_lines: Vec<Line<'static>> = causes
        .into_iter()
        .chain([l1, l2])
        .flat_map(|l| wrap(l, inner.width, 0, 2))
        .collect();
    let [graphs, info] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(info_lines.len() as u16),
    ])
    .areas(inner);
    put(frame, Paragraph::new(info_lines), info, info);
    // Total à esquerda (~45%), grade por lógico no resto.
    let total_w = total_width(graphs.width);
    let [total, grid] =
        Layout::horizontal([Constraint::Length(total_w), Constraint::Min(8)]).areas(graphs);

    // Gráfico total, com o valor e os extremos na 1ª linha.
    let head = vec![
        brief(&c.usage, |v| format!("{}%", num(*v, 0))).add_modifier(Modifier::BOLD),
        Span::raw(" "),
        label("kern "),
        brief(&c.kernel, |v| format!("{}%", num(*v, 0))),
    ];
    line(frame, head, total.x, total.y, total.width, total);
    let mm = minmax(st.session.cpu, |v| format!("{}%", num(v, 0)));
    line(frame, vec![mm], total.x, total.y + 1, total.width, total);
    let g = Rect::new(
        total.x,
        total.y + 2,
        total.width.saturating_sub(1),
        total.height.saturating_sub(2),
    );
    frame.render_widget(
        Graph::new(st.cpu.values(), GraphColor::Height).with_under(st.cpu_kernel.values()),
        g.intersection(total),
    );

    match &c.per_logical {
        Ok(per) => draw_core_grid(
            frame,
            grid,
            &mut st.cores,
            &mut st.cores_kernel,
            per,
            c.freq_per_logical.as_deref().ok(),
        ),
        Err(e) => put(
            frame,
            Paragraph::new(wrap(vec![label("per core "), missing(e)], grid.width, 0, 2)),
            grid,
            grid,
        ),
    }
}

/// Grade de gráficos por lógico (4×4 com 16 threads), sempre com todos: se a célula ficar
/// pequena demais para gráfico, vira uma lista compacta de percentuais.
fn draw_core_grid(
    frame: &mut Frame<'_>,
    area: Rect,
    cores: &mut [Series],
    kernel: &mut [Series],
    per: &[f64],
    freq: Option<&[f64]>,
) {
    // Clock de cada lógico ao lado do uso ("C0  90% 4.05 GHz"), se couber.
    let ghz = |i: usize| freq.and_then(|f| f.get(i)).map(|mhz| num(mhz / 1000.0, 2));
    let n = per.len();
    if n == 0 || area.width == 0 || area.height == 0 {
        return;
    }
    let cols = grid_cols(n);
    let rows = n.div_ceil(cols);
    let cw = area.width / cols as u16;
    let ch = area.height / rows as u16;
    if cw < 9 || ch < 2 {
        // Lista compacta: todos os lógicos, quebrando linha só entre um lógico e outro (cada
        // grupo começa com espaço; uso e clock ficam no mesmo trecho, que nunca é partido).
        // Todos os lógicos sempre aparecem: sem altura para os clocks, só o uso (média e pico
        // continuam na linha de velocidade).
        let list = |clocks: bool, gap: &str, k: usize| {
            let mut spans: Vec<Span<'static>> = per
                .iter()
                .take(k)
                .enumerate()
                .flat_map(|(i, v)| {
                    // Sem preenchimento à esquerda: um espaço no começo abriria outro grupo.
                    let pct = format!("{}%", num(*v, 0));
                    let g = if clocks { ghz(i) } else { None };
                    [
                        label(&format!("{gap}C{i} ")),
                        text(g.map_or_else(|| pct.clone(), |g| format!("{pct} {g}"))),
                    ]
                })
                .collect();
            if k < per.len() {
                spans.push(label(&format!(" +{} more (z)", per.len() - k)));
            }
            wrap(spans, area.width, 0, 0)
        };
        let n = per.len();
        let fits = |l: &Vec<Line<'static>>| l.len() <= usize::from(area.height);
        let mut lines = list(true, "  ", n);
        if !fits(&lines) {
            lines = list(false, "  ", n);
        }
        if !fits(&lines) {
            lines = list(false, " ", n);
        }
        // Nem assim: os que couberem e quantos ficaram de fora (a tecla z amplia a caixa).
        let mut k = n;
        while !fits(&lines) && k > 0 {
            k -= 1;
            lines = list(false, " ", k);
        }
        put(frame, Paragraph::new(lines), area, area);
        return;
    }
    // A sobra da divisão (linhas e colunas) vai para as primeiras fileiras e colunas: a grade
    // ocupa a área inteira, sem faixa vazia embaixo.
    let (extra_h, extra_w) = (area.height % rows as u16, area.width % cols as u16);
    let at = |k: u16, size: u16, extra: u16| k * size + k.min(extra);
    for (i, v) in per.iter().enumerate() {
        let (c, r) = ((i % cols) as u16, (i / cols) as u16);
        let (x, y) = (at(c, cw, extra_w), at(r, ch, extra_h));
        let (w, h) = (cw + u16::from(c < extra_w), ch + u16::from(r < extra_h));
        let cell = Rect::new(area.x + x, area.y + y, w.saturating_sub(1), h).intersection(area);
        // Índice e percentual sempre separados ("C12 100%" = 8 colunas, nunca "C12100%").
        let mut head = vec![
            label(&format!("C{i:<2} ")),
            text(format!("{:>4}", format!("{}%", num(*v, 0)))),
        ];
        // Clock na mesma linha do uso: "C12 100% 4.05 GHz" com 17 colunas, "4.05G" com 14, só
        // o número com 13; sem largura, numa linha própria acima do gráfico (se houver altura).
        // O formato vem da célula mais estreita: a grade inteira fica igual.
        let narrowest = cw.saturating_sub(1);
        let mut graph_top = 1;
        if let Some(g) = ghz(i) {
            if narrowest >= 17 {
                head.push(label(&format!(" {g} GHz")));
            } else if narrowest >= 14 {
                head.push(label(&format!(" {g}G")));
            } else if narrowest >= 13 {
                head.push(label(&format!(" {g}")));
            } else if cell.height >= 3 {
                line(
                    frame,
                    vec![label(&format!("{g} GHz"))],
                    cell.x,
                    cell.y + 1,
                    cell.width,
                    cell,
                );
                graph_top = 2;
            }
        }
        line(frame, head, cell.x, cell.y, cell.width, cell);
        if let Some(s) = cores.get_mut(i) {
            let g = Rect::new(
                cell.x,
                cell.y + graph_top,
                cell.width,
                cell.height.saturating_sub(graph_top),
            );
            let mut graph = Graph::new(s.values(), GraphColor::Height);
            if let Some(k) = kernel.get_mut(i) {
                graph = graph.with_under(k.values());
            }
            frame.render_widget(graph, g.intersection(cell));
        }
    }
}

// --- GPU ---

/// Engine mais usada (NVML): 3D/compute, encoder ou decoder, como o Task Manager mostra o
/// máximo dos engines. O Copy não existe no NVML.
fn busiest_engine(g: &Gpu) -> (&'static str, u32) {
    let get = |m: &Metric<u32>| m.as_ref().copied().unwrap_or(0);
    let mut best = (usage_name(g), get(&g.usage));
    for (name, m) in [("encode", &g.encoder), ("decode", &g.decoder)] {
        // Sem NVML o uso geral já é o máximo de todas as engines: no empate, a de vídeo é a
        // mais usada (com NVML a regra fica a mesma).
        if get(m) > best.1 || (get(m) > 0 && get(m) == best.1) {
            best = (name, get(m));
        }
    }
    best
}

/// Nome do uso geral: "3D" no NVML; sem NVML é o máximo de todas as engines (cópia, vídeo,
/// compute), não só o 3D.
fn usage_name(g: &Gpu) -> &'static str {
    if g.no_nvml.is_some() { "util" } else { "3D" }
}

fn engine_color(name: &str) -> Color {
    match name {
        "encode" => th().encode,
        "decode" => th().decode,
        _ => th().text,
    }
}

/// Largura do painel de engines e extremos da caixa da GPU.
const PANEL_W: u16 = 30;

fn draw_gpu(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let gpus = st.fast.as_ref().map(|f| &f.gpus);
    let Some(Ok(list)) = gpus else {
        empty_box(frame, area, "gpu", gpus);
        return;
    };
    let sel = st.gpu_index(list.len());
    let Some(g) = list.get(sel).cloned() else {
        frame.render_widget(
            Paragraph::new(missing(&Missing::Fail("no GPU found".into()))).block(boxed("gpu", "")),
            area,
        );
        return;
    };
    let (engine, engine_v) = busiest_engine(&g);
    // Com mais de uma GPU, a posição da exibida (tecla `g` troca).
    let gpu = if list.len() > 1 {
        format!("gpu {}/{}", sel + 1, list.len())
    } else {
        "gpu".to_string()
    };
    let name = g
        .name
        .as_ref()
        .map_or_else(|_| gpu.clone(), |n| format!("{gpu} · {n}"));
    let block = with_failure(
        // A engine ativa já está no cabeçalho do gráfico e no painel (em negrito).
        boxed(&name, ""),
        g.name.as_ref().err(),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let details = gpu_details(st, &g, inner.width);
    let mut bottom_h = (2 + details.legend.len() + details.info.len()) as u16;
    // Painel das engines e dos extremos da sessão.
    let eng = |name: &str, m: &Metric<u32>| -> Vec<(Vec<Span<'static>>, Option<f64>)> {
        let style = if name == engine {
            Style::new().fg(th().title).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(th().label)
        };
        // Falha: a causa usa a linha toda (sem medidor, que mostraria um 0% falso) e quebra
        // em mais linhas se não couber no painel.
        match m {
            Ok(v) => vec![(
                vec![
                    Span::styled(format!("{name:<8}"), style),
                    text(format!("{v:>3}%")),
                ],
                Some(f64::from(*v)),
            )],
            Err(e) => {
                // Só a falha leva o "!" (como em `metric`); o estado neutro não.
                let mark = if matches!(e, Missing::Fail(_)) {
                    "! "
                } else {
                    ""
                };
                let cause = tr(&e.to_string()).into_owned();
                let words: Vec<Span<'static>> =
                    std::iter::once(Span::styled(format!("{name:<8}"), style))
                        .chain(cause.split(' ').enumerate().map(|(i, w)| {
                            let w = if i == 0 {
                                format!("{mark}{}", printable(w))
                            } else {
                                format!(" {}", printable(w))
                            };
                            let mut sp = missing(e);
                            sp.content = w.into();
                            sp
                        }))
                        .collect();
                wrap(words, PANEL_W - 1, 0, 8)
                    .into_iter()
                    .map(|l| (l.spans, None))
                    .collect()
            }
        }
    };
    // Extremos da sessão: "min x · max y unidade" (a unidade só no fim, para caber).
    let mm = |m: MinMax, f: &dyn Fn(f64) -> String, unit: &str| -> Vec<Span<'static>> {
        m.get().map_or_else(
            || vec![label("-")],
            |(lo, hi)| {
                vec![
                    label("min "),
                    text(f(lo)),
                    label(" · max "),
                    text(format!("{}{unit}", f(hi))),
                ]
            },
        )
    };
    let row = |name: &str, spans: Vec<Span<'static>>| -> (Vec<Span<'static>>, Option<f64>) {
        let mut v = vec![label(&format!("{name:<6}"))];
        v.extend(spans);
        (v, None)
    };
    // Sem NVML, a atividade da memória, a temperatura, a potência e o clock nunca têm valor (o
    // motivo fica numa linha embaixo): só o que existe.
    let nvml = g.no_nvml.is_none();
    let mut engines = vec![
        eng(usage_name(&g), &g.usage),
        eng("encode", &g.encoder),
        eng("decode", &g.decoder),
    ];
    if nvml {
        engines.push(eng("mem act", &g.mem_activity));
    }
    let mut rows: Vec<(Vec<Span<'static>>, Option<f64>)> = engines.into_iter().flatten().collect();
    let engine_rows = rows.len();
    rows.push((vec![label("session")], None));
    let n0 = |v: f64| num(v, 0);
    rows.push(row("util", mm(st.session.gpu, &n0, "%")));
    if nvml {
        rows.push(row("temp", mm(st.session.gpu_temp, &n0, " °C")));
        rows.push(row("power", mm(st.session.gpu_power, &n0, " W")));
    }
    rows.push(row(
        "VRAM",
        mm(st.session.vram, &|v| short_bytes(v as u64), ""),
    ));
    if nvml {
        rows.push(row("gfx", mm(st.session.gfx_clock, &n0, " MHz")));
    }
    // Sem altura para o painel todo: os extremos da sessão viram uma linha (com quebra) embaixo.
    let mut session_lines = Vec::new();
    if inner.height.saturating_sub(bottom_h) < rows.len() as u16 {
        let mut spans = vec![label("session")];
        for (r, _) in rows.drain(engine_rows + 1..) {
            spans.push(label("   "));
            spans.extend(r);
        }
        rows.truncate(engine_rows);
        session_lines = wrap(spans, inner.width, 0, 8);
        // Sem altura nem para isso, a sessão sai da aba Geral (fica na aba Sensores): as
        // engines ao vivo e os detalhes não podem ser cortados por ela.
        if inner.height < bottom_h + session_lines.len() as u16 + engine_rows as u16 {
            session_lines.clear();
        }
        bottom_h += session_lines.len() as u16;
    }
    // Os detalhes (métricas) têm prioridade sobre a altura do gráfico.
    let graph_min = (engine_rows as u16)
        .max(4)
        .min(inner.height.saturating_sub(bottom_h).max(2));
    let [top, bottom] =
        Layout::vertical([Constraint::Min(graph_min), Constraint::Length(bottom_h)]).areas(inner);
    let [graph, panel] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(PANEL_W)]).areas(top);

    // Gráfico da engine dominante, com o nome dela no canto.
    let head = if [&g.usage, &g.encoder, &g.decoder].iter().any(|m| m.is_ok()) {
        vec![
            text(format!("{engine_v}% ")).add_modifier(Modifier::BOLD),
            // Mínimo e máximo do uso ficam no painel da sessão (`util`), uma vez só.
            Span::styled(engine.to_string(), Style::new().fg(engine_color(engine))),
        ]
    } else {
        vec![metric(&g.usage, |v| format!("{v}%"))]
    };
    line(frame, head, graph.x, graph.y, graph.width, graph);
    let gr = Rect::new(
        graph.x,
        graph.y + 1,
        graph.width.saturating_sub(1),
        graph.height.saturating_sub(1),
    );
    frame.render_widget(
        Graph::new(
            st.gpu.get_mut(sel).map_or(&[][..], Series::values),
            GraphColor::Height,
        ),
        gr.intersection(graph),
    );

    for (i, (spans, meter)) in rows.into_iter().enumerate() {
        let y = panel.y + i as u16;
        if y >= panel.y + panel.height {
            break;
        }
        // Linhas com medidor: rótulo + valor em 13 colunas; as de extremos usam o painel todo.
        let w = if meter.is_some() {
            13
        } else {
            panel.width.saturating_sub(1)
        };
        line(frame, spans, panel.x + 1, y, w, panel);
        if let Some(v) = meter {
            frame.render_widget(
                Meter { value: v / 100.0 },
                Rect::new(panel.x + 15, y, panel.width.saturating_sub(15), 1).intersection(panel),
            );
        }
    }

    // VRAM por categoria, compartilhada, física e o resto das leituras.
    let slow = st
        .slow
        .as_ref()
        .and_then(|s| s.gpus.as_ref().ok())
        .and_then(|l| l.iter().find(|x| x.index == g.index));
    let b = bottom;
    match slow.map(|s| &s.vram) {
        Some(Ok(v)) => vram_bar(frame, b, v, details.game_pid, &g.dedicated),
        _ => bar_line(frame, b, 0, "VRAM  ", &g.dedicated),
    }
    let n = details.legend.len() as u16;
    for (i, l) in details.legend.into_iter().enumerate() {
        put(
            frame,
            Paragraph::new(l),
            Rect::new(b.x, b.y + 1 + i as u16, b.width, 1),
            b,
        );
    }
    bar_line(frame, b, 1 + n, "shared", &g.shared);
    let below = details.info.into_iter().chain(session_lines);
    for (i, l) in below.enumerate() {
        put(
            frame,
            Paragraph::new(l),
            Rect::new(b.x, b.y + 2 + n + i as u16, b.width, 1),
            b,
        );
    }
}

/// Linhas de baixo da caixa da GPU, já quebradas na largura (nada é cortado).
struct GpuDetails {
    game_pid: Option<u32>,
    legend: Vec<Line<'static>>,
    info: Vec<Line<'static>>,
}

fn gpu_details(st: &State, g: &Gpu, width: u16) -> GpuDetails {
    let slow = st
        .slow
        .as_ref()
        .and_then(|s| s.gpus.as_ref().ok())
        .and_then(|l| l.iter().find(|x| x.index == g.index))
        .cloned();
    let slow_err = match st.slow.as_ref().map(|s| &s.gpus) {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    let game_pid = st
        .fps
        .as_ref()
        .and_then(|f| f.game.as_ref().ok())
        .map(|g| g.pid);
    let legend = vram_legend(slow.as_ref().map(|s| &s.vram), game_pid, width);
    // GPU sem NVML: os campos só dele numa linha, com o motivo (não repetido em cada um).
    let rows: Vec<Vec<Span<'static>>> = if let Some(m) = &g.no_nvml {
        vec![
            vec![
                label("NVML   "),
                missing(m),
                label(" (clocks, temp, power, fans, PCIe, P-state)"),
            ],
            vec![label("drv    "), metric(&g.driver, Clone::clone)],
        ]
    } else {
        nvml_rows(g, slow.as_ref(), slow_err.as_ref())
    };
    let info = rows
        .into_iter()
        .flat_map(|r| wrap(r, width, 0, 7))
        .collect();
    GpuDetails {
        game_pid,
        legend,
        info,
    }
}

/// Linhas de detalhe de uma GPU do NVML (aba Geral).
fn nvml_rows(
    g: &Gpu,
    slow: Option<&crate::model::GpuSlow>,
    slow_err: Option<&Missing>,
) -> Vec<Vec<Span<'static>>> {
    // Máximo 0 = o NVML não informa (GPUs móveis): só o atual.
    let clk = |m: &Metric<(u32, u32)>| {
        metric(m, |(c, mx)| {
            if *mx > 0 {
                format!("{c}/{mx}")
            } else {
                c.to_string()
            }
        })
    };
    vec![
        match &g.physical {
            Ok([u, f, r, t]) => vec![
                // O total físico é o mesmo da barra da VRAM: só aparece se diferir.
                label("phys   "),
                text(format!("{} used", bytes(*u))),
                sep(),
                text(format!("{} free", bytes(*f))),
                sep(),
                text(format!("{} reserved", bytes(*r))),
                label(&if g.dedicated.as_ref().is_ok_and(|(_, tot)| tot == t) {
                    " (NVML)".to_string()
                } else {
                    format!("  {} total (NVML)", bytes(*t))
                }),
            ],
            Err(e) => vec![label("phys   "), missing(e)],
        },
        vec![
            label("clk    gfx "),
            clk(&g.clock_graphics),
            label("  SM "),
            clk(&g.clock_sm),
            label("  mem "),
            clk(&g.clock_mem),
            label("  video "),
            clk(&g.clock_video),
            label(" MHz"),
        ],
        vec![
            label("temp   "),
            metric(&g.temp, |v| format!("{v} °C")),
            Span::raw(" "),
            metric(&g.temp_slowdown, |v| format!("(slowdown {v} °C)")),
            sep(),
            label("power "),
            metric(&g.power, |(p, l)| {
                if *l > 0.0 {
                    format!("{}/{} W", num(*p, 0), num(*l, 0))
                } else {
                    format!("{} W", num(*p, 0))
                }
            }),
            sep(),
            metric(&g.pstate, |v| format!("P{v}")),
            sep(),
            label("throttle "),
            metric(&g.throttle, |v| {
                if v.is_empty() {
                    "none".into()
                } else {
                    v.join(", ")
                }
            }),
        ],
        {
            let mut v = vec![label("fans   ")];
            match &g.fans {
                Ok(list) => {
                    for (i, f) in list.iter().enumerate() {
                        v.extend([
                            label(&format!("fan{i} ")),
                            metric(&f.percent, |p| format!("{p}%")),
                            Span::raw(" "),
                            metric(&f.rpm, |r| format!("{r} rpm")),
                            sep(),
                        ]);
                    }
                }
                Err(e) => v.push(missing(e)),
            }
            v
        },
        {
            let mut v = vec![
                label("PCIe   "),
                metric(&g.pcie_link, |(gen_, w)| format!("Gen{gen_} x{w}")),
            ];
            if let Some(s) = &slow {
                v.extend([
                    label("  tx "),
                    metric(&s.pcie_tx, |x| format!("{}/s", bytes(*x))),
                    label("  rx "),
                    metric(&s.pcie_rx, |x| format!("{}/s", bytes(*x))),
                ]);
            } else if let Some(e) = &slow_err {
                v.extend([label("  tx/rx "), missing(e)]);
            }
            v.extend([
                bar_sep(),
                label("drv "),
                metric(&g.driver, Clone::clone),
                label("  VBIOS "),
                metric(&g.vbios, Clone::clone),
            ]);
            v
        },
    ]
}

/// Quebra uma linha em várias de até `width` colunas, em vez de cortar. Os pontos de quebra são
/// os spans que começam com espaço (separadores e rótulos como "  SM "); o espaço da quebra
/// some e a continuação começa com `indent` espaços. `first` é o recuo da primeira linha.
pub(super) fn wrap(
    spans: Vec<Span<'static>>,
    width: u16,
    first: u16,
    indent: u16,
) -> Vec<Line<'static>> {
    let starts_group = |sp: &Span<'_>| sp.content.starts_with(char::is_whitespace);
    let mut groups: std::collections::VecDeque<Vec<Span<'static>>> = Default::default();
    for sp in spans {
        if groups.is_empty() || starts_group(&sp) {
            groups.push_back(Vec::new());
        }
        if let Some(g) = groups.back_mut() {
            g.push(sp);
        }
    }
    // Sem o espaço (e o "│") do começo do grupo, para abrir uma linha.
    let trimmed = |g: &[Span<'static>]| -> Vec<Span<'static>> {
        let mut out = Vec::with_capacity(g.len());
        let mut lead = true;
        for sp in g {
            if lead {
                let t = sp.content.trim_start().trim_start_matches('│').trim_start();
                if t.is_empty() {
                    continue;
                }
                lead = false;
                out.push(Span::styled(t.to_string(), sp.style));
            } else {
                out.push(sp.clone());
            }
        }
        out
    };
    // O grupo em palavras (cada palavra vira um grupo); `None` se já é uma palavra só.
    let words = |g: &[Span<'static>]| -> Option<Vec<Vec<Span<'static>>>> {
        let mut out: Vec<Vec<Span<'static>>> = Vec::new();
        // Espaço visto (inclusive no fim de um span): a próxima palavra abre um grupo com ele.
        let mut space = false;
        for sp in g {
            for (i, w) in sp.content.split(' ').enumerate() {
                if i > 0 {
                    space = true;
                }
                if w.is_empty() {
                    continue;
                }
                let new_group = std::mem::take(&mut space);
                let t = if new_group {
                    format!(" {w}")
                } else {
                    w.to_string()
                };
                let part = Span::styled(t, sp.style);
                if out.is_empty() || new_group {
                    out.push(vec![part]);
                } else if let Some(last) = out.last_mut() {
                    last.push(part);
                }
            }
        }
        (out.len() > 1).then_some(out)
    };
    let width = usize::from(width);
    let pad = |n: u16| Span::raw(" ".repeat(usize::from(n)));
    let mut lines = Vec::new();
    let mut cur = vec![pad(first)];
    let mut used = usize::from(first);
    let mut empty = true;
    while let Some(g) = groups.pop_front() {
        let g = if empty { trimmed(&g) } else { g };
        if g.is_empty() {
            continue;
        }
        let w: usize = g.iter().map(Span::width).sum();
        if used + w <= width {
            used += w;
            empty = false;
            cur.extend(g);
            continue;
        }
        // Não cabe nem a partir do recuo de uma linha nova: quebra por palavra.
        let start = if lines.is_empty() && empty {
            first
        } else {
            indent
        };
        let tw: usize = trimmed(&g).iter().map(Span::width).sum();
        if usize::from(start) + tw > width
            && let Some(ws) = words(&g)
        {
            for w in ws.into_iter().rev() {
                groups.push_front(w);
            }
            continue;
        }
        if empty {
            // Linha vazia e uma palavra só maior que a largura: entra cortada.
            used += w;
            empty = false;
            cur.extend(g);
            continue;
        }
        lines.push(Line::from(std::mem::take(&mut cur)));
        let g = trimmed(&g);
        used = usize::from(indent) + g.iter().map(Span::width).sum::<usize>();
        cur.push(pad(indent));
        cur.extend(g);
    }
    if !empty {
        lines.push(Line::from(cur));
    }
    lines
}

/// Legenda da barra da VRAM por categoria (aba Geral e aba GPU), quebrada em `width`.
pub(super) fn vram_legend(
    vram: Option<&Metric<crate::model::VramBreakdown>>,
    game_pid: Option<u32>,
    width: u16,
) -> Vec<Line<'static>> {
    match vram {
        Some(Ok(v)) => {
            let spans: Vec<Span<'static>> = vram_parts(v, game_pid)
                .into_iter()
                .filter(|(_, v)| *v > 0)
                .flat_map(|(k, v)| {
                    [
                        Span::styled("  ■ ", Style::new().fg(vram_color(k))),
                        label(&format!("{} ", k.label())),
                        text(short_bytes(v)),
                    ]
                })
                .collect();
            wrap(spans, width, VRAM_LABEL_W, VRAM_LABEL_W)
        }
        Some(Err(e)) => wrap(
            vec![label("by kind "), missing(e)],
            width,
            VRAM_LABEL_W,
            VRAM_LABEL_W,
        ),
        None => Vec::new(),
    }
}

/// Largura do rótulo da barra da VRAM (a legenda alinha embaixo da barra).
const VRAM_LABEL_W: u16 = 7;

/// Cor de cada categoria da VRAM (próximas às do gputaskman).
fn vram_color(k: VramKind) -> Color {
    match k {
        VramKind::Game => th().game,
        VramKind::App => th().vram_app,
        VramKind::Compositor => th().vram_compositor,
        VramKind::Browser => th().vram_browser,
        VramKind::System => th().vram_system,
        VramKind::Kernel => th().vram_kernel,
        VramKind::Free => th().dim,
    }
}

/// Partes da barra: o jogo do RTSS sai do "app" e vira a própria categoria.
fn vram_parts(b: &VramBreakdown, game_pid: Option<u32>) -> Vec<(VramKind, u64)> {
    let game: u64 = b
        .processes
        .iter()
        .filter(|p| Some(p.pid) == game_pid && p.kind == VramKind::App)
        .map(|p| p.local)
        .sum();
    let mut parts = Vec::with_capacity(b.segments.len() + 1);
    if game > 0 {
        parts.push((VramKind::Game, game));
    }
    for &(k, v) in &b.segments {
        let v = if k == VramKind::App {
            v.saturating_sub(game)
        } else {
            v
        };
        parts.push((k, v));
    }
    parts
}

/// VRAM: barra cortada por categoria (linha 0, com usado/total da dedicada) e legenda (linha 1).
fn vram_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    b: &VramBreakdown,
    game_pid: Option<u32>,
    dedicated: &Metric<(u64, u64)>,
) {
    if area.height == 0 {
        return;
    }
    let parts = vram_parts(b, game_pid);
    let t = metric(dedicated, |(u, t)| {
        format!(
            " {} / {} ({}%)",
            bytes(*u),
            bytes(*t),
            num(*u as f64 / (*t).max(1) as f64 * 100.0, 0)
        )
    });
    let label_w = VRAM_LABEL_W.min(area.width);
    let text_w = text_room(&t, dedicated.is_err(), area.width, label_w);
    line(frame, vec![label("VRAM  ")], area.x, area.y, label_w, area);
    let bar_w = area.width.saturating_sub(label_w + text_w);
    let colored: Vec<(u64, Color)> = parts
        .iter()
        .filter(|(k, _)| *k != VramKind::Free)
        .map(|&(k, v)| (v, vram_color(k)))
        .collect();
    frame.render_widget(
        Segmented {
            parts: &colored,
            total: b.total,
        },
        Rect::new(area.x + label_w, area.y, bar_w, 1).intersection(area),
    );
    put(
        frame,
        Paragraph::new(t),
        Rect::new(area.x + label_w + bar_w, area.y, text_w, 1),
        area,
    );
}

/// Largura do texto ao lado de uma barra: com valor, no máximo metade da linha (a barra fica
/// com o resto); sem valor não há barra, e a causa usa toda a largura depois do rótulo.
fn text_room(t: &Span<'_>, missing: bool, width: u16, label_w: u16) -> u16 {
    let w = t.width() as u16;
    if missing {
        w.min(width.saturating_sub(label_w))
    } else {
        // O valor inteiro antes da barra: a barra encolhe (ex.: "(69%)" não perde o parêntese).
        w.min(width.saturating_sub(label_w + 4))
    }
}

/// Linha "rótulo ■■■■ usado/total" dentro de `area`, na linha `row`.
fn bar_line(frame: &mut Frame<'_>, area: Rect, row: u16, name: &str, m: &Metric<(u64, u64)>) {
    if row >= area.height {
        return;
    }
    let y = area.y + row;
    let t = metric(m, |(u, t)| {
        format!(
            " {} / {} ({}%)",
            bytes(*u),
            bytes(*t),
            num(*u as f64 / (*t).max(1) as f64 * 100.0, 0)
        )
    });
    let label_w = (name.len() as u16 + 1).min(area.width);
    let text_w = text_room(&t, m.is_err(), area.width, label_w);
    line(frame, vec![label(name)], area.x, y, label_w, area);
    let bar_w = area.width.saturating_sub(label_w + text_w);
    if let Ok((u, t)) = m {
        let v = if *t > 0 { *u as f64 / *t as f64 } else { 0.0 };
        frame.render_widget(
            Meter { value: v },
            Rect::new(area.x + label_w, y, bar_w, 1).intersection(area),
        );
    }
    put(
        frame,
        Paragraph::new(t),
        Rect::new(area.x + label_w + bar_w, y, text_w, 1),
        area,
    );
}

// --- Memória ---

/// Linhas de texto da caixa de memória, já quebradas na largura.
struct MemText {
    legend: Vec<Line<'static>>,
    session: Vec<Line<'static>>,
    pools: Vec<Line<'static>>,
}

impl MemText {
    /// Altura da caixa: RAM, legenda, commit, sessão, pagefile, pools e as bordas.
    fn height(&self) -> u16 {
        (3 + self.legend.len() + self.session.len() + self.pools.len() + 2) as u16
    }
}

const RAM_LABEL_W: u16 = 9;

/// Partes da RAM como o Task Manager: (em uso, modificada, standby, livre).
fn ram_parts(m: &Memory) -> Option<(u64, u64, u64, u64, u64)> {
    let (used, total, sb, md) = (
        *m.used.as_ref().ok()?,
        *m.total.as_ref().ok()?,
        *m.standby.as_ref().ok()?,
        *m.modified.as_ref().ok()?,
    );
    // Em uso = usado - modificada; disponível = standby + livre.
    let in_use = used.saturating_sub(md);
    let free = total.saturating_sub(used).saturating_sub(sb);
    Some((in_use, md, sb, free, total))
}

fn mem_text(st: &State, width: u16) -> Option<MemText> {
    let m = st.fast.as_ref()?.memory.as_ref().ok()?;
    let inner = width.saturating_sub(2);
    let legend = match ram_parts(m) {
        Some((in_use, md, sb, free, _)) => {
            let spans: Vec<Span<'static>> = [
                ("in use", in_use, th().ram_in_use),
                ("modified", md, th().ram_modified),
                ("standby", sb, th().ram_standby),
                ("free", free, th().dim),
            ]
            .into_iter()
            .flat_map(|(name, v, c)| {
                [
                    Span::styled("  ■ ", Style::new().fg(c)),
                    label(&format!("{name} ")),
                    text(short_bytes(v)),
                ]
            })
            .collect();
            wrap(spans, inner, RAM_LABEL_W, RAM_LABEL_W)
        }
        None => match m.standby.as_ref().err().or(m.modified.as_ref().err()) {
            Some(e) => wrap(
                vec![label("cache "), missing(e)],
                inner,
                RAM_LABEL_W,
                RAM_LABEL_W,
            ),
            None => wrap(
                vec![
                    Span::styled("  ■ ", Style::new().fg(th().ram_modified)),
                    label("modified "),
                    metric(&m.modified, |v| short_bytes(*v)),
                    Span::styled("  ■ ", Style::new().fg(th().ram_standby)),
                    label("standby "),
                    metric(&m.standby, |v| short_bytes(*v)),
                ],
                inner,
                RAM_LABEL_W,
                RAM_LABEL_W,
            ),
        },
    };
    let gb = |v: f64| bytes(v as u64);
    let session = wrap(
        vec![
            label("session RAM "),
            minmax(st.session.ram, gb),
            label("   commit "),
            minmax(st.session.commit, gb),
        ],
        inner,
        0,
        8,
    );
    let pools = wrap(
        vec![
            label("kernel pools"),
            label("   paged "),
            metric(&m.paged_pool, |v| bytes(*v)),
            label("   non-paged "),
            metric(&m.nonpaged_pool, |v| bytes(*v)),
        ],
        inner,
        0,
        8,
    );
    Some(MemText {
        legend,
        session,
        pools,
    })
}

/// Memória no rodapé da caixa da CPU (como a VRAM na da GPU), em `inner` (sem moldura).
fn draw_memory_body(frame: &mut Frame<'_>, inner: Rect, st: &State) {
    let mem = st.fast.as_ref().map(|f| &f.memory);
    let (Some(Ok(m)), Some(t)) = (mem, mem_text(st, inner.width + 2)) else {
        // Sem a memória: o estado (espera ou a falha com a causa) no lugar dela.
        let msg = match mem {
            Some(Err(e)) => vec![label("memory "), missing(e)],
            _ => vec![label("memory: waiting for the first collection")],
        };
        put(
            frame,
            Paragraph::new(wrap(msg, inner.width, 0, 2)),
            inner,
            inner,
        );
        return;
    };
    let ram = m.used.clone().and_then(|u| Ok((u, m.total.clone()?)));
    ram_bar(frame, inner, m, &ram);
    let mut y = 1u16;
    let lines = |frame: &mut Frame<'_>, ls: Vec<Line<'static>>, y: &mut u16| {
        for l in ls {
            put(
                frame,
                Paragraph::new(l),
                Rect::new(inner.x, inner.y + *y, inner.width, 1),
                inner,
            );
            *y += 1;
        }
    };
    lines(frame, t.legend, &mut y);
    bar_line(frame, inner, y, "commit  ", &m.commit);
    y += 1;
    lines(frame, t.session, &mut y);
    if y < inner.height {
        let row = inner.y + y;
        line(frame, vec![label("pagefile ")], inner.x, row, 9, inner);
        let pt = metric(&m.pagefile_usage, |v| format!(" {}%", num(*v, 1)));
        let tw = text_room(&pt, m.pagefile_usage.is_err(), inner.width, 9);
        let bw = inner.width.saturating_sub(9 + tw);
        if let Ok(v) = &m.pagefile_usage {
            frame.render_widget(
                Meter { value: v / 100.0 },
                Rect::new(inner.x + 9, row, bw, 1).intersection(inner),
            );
        }
        put(
            frame,
            Paragraph::new(pt),
            Rect::new(inner.x + 9 + bw, row, tw, 1),
            inner,
        );
    }
    y += 1;
    lines(frame, t.pools, &mut y);
}

/// Barra da RAM (linha 0): cortada como o Task Manager, ou só o usado sem a divisão do cache.
fn ram_bar(frame: &mut Frame<'_>, area: Rect, m: &Memory, ram: &Metric<(u64, u64)>) {
    if area.height == 0 {
        return;
    }
    let t = metric(ram, |(u, t)| {
        format!(
            " {} / {} ({}%)",
            bytes(*u),
            bytes(*t),
            num(*u as f64 / (*t).max(1) as f64 * 100.0, 0)
        )
    });
    let label_w = RAM_LABEL_W.min(area.width);
    let text_w = text_room(&t, ram.is_err(), area.width, label_w);
    line(
        frame,
        vec![label("RAM      ")],
        area.x,
        area.y,
        label_w,
        area,
    );
    let bar_w = area.width.saturating_sub(label_w + text_w);
    let bar = Rect::new(area.x + label_w, area.y, bar_w, 1).intersection(area);
    match (ram_parts(m), ram) {
        (Some((in_use, md, sb, _, total)), _) => frame.render_widget(
            Segmented {
                parts: &[
                    (in_use, th().ram_in_use),
                    (md, th().ram_modified),
                    (sb, th().ram_standby),
                ],
                total,
            },
            bar,
        ),
        (None, Ok((u, t))) => {
            let v = if *t > 0 { *u as f64 / *t as f64 } else { 0.0 };
            frame.render_widget(Meter { value: v }, bar);
        }
        (None, Err(_)) => {}
    }
    put(
        frame,
        Paragraph::new(t),
        Rect::new(area.x + label_w + bar_w, area.y, text_w, 1),
        area,
    );
}

// --- Rede ---

/// Taxa de rede na unidade do config (bits por segundo, como o Task Manager, ou bytes).
fn net_rate(st: &State, bits_per_s: f64) -> String {
    if st.net_bytes {
        format!("{}/s", bytes((bits_per_s / 8.0).max(0.0) as u64))
    } else {
        bits(bits_per_s)
    }
}

/// Rede compacta: por placa, 1 linha de identificação (nome, descrição, link, erros) e 1 de
/// taxas (↑/↓ com o máximo da sessão e a escala comum); o gráfico espelhado só entra com altura
/// sobrando. Placas desconectadas numa linha só.
fn draw_net(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let nics = st.fast.as_ref().map(|f| &f.nics);
    let Some(Ok(list)) = nics else {
        empty_box(frame, area, "net", nics);
        return;
    };
    let list: Vec<Nic> = list.clone();
    let block = boxed("net", "↑ send  ↓ receive");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Só o estado neutro ("disconnected") vai para a linha de desconectadas; falha aparece.
    let (down, up): (Vec<_>, Vec<_>) = list.iter().partition(|n| {
        matches!(n.link, Err(Missing::Neutral(_))) && !failed(&n.send) && !failed(&n.recv)
    });
    // Linhas fixas por placa: identificação + taxas (que podem quebrar em duas, estreita).
    let rate_lines = |n: &Nic| -> Vec<Line<'static>> {
        let n_samples = usize::from(inner.width) * 2;
        let peak =
            |m: &HashMap<String, Series>| m.get(&n.alias).map_or(0.0, |s| s.max_of_ref(n_samples));
        let scale = peak(&st.send).max(peak(&st.recv)).max(NET_FLOOR);
        let mut spans = Vec::new();
        for (arrow, m, color, mm) in [
            ("↑ ", &n.send, th().net_send, st.session.send.get(&n.alias)),
            (
                "  ↓ ",
                &n.recv,
                th().net_recv,
                st.session.recv.get(&n.alias),
            ),
        ] {
            spans.push(Span::styled(arrow, Style::new().fg(color)));
            spans.push(metric(m, |v| net_rate(st, *v)));
            if let Some((_, hi)) = mm.copied().and_then(MinMax::get) {
                spans.push(label(&format!(" max {}", net_rate(st, hi))));
            }
        }
        // A escala vai no canto do gráfico (como no jogo), não na linha das taxas.
        let _ = scale;
        wrap(spans, inner.width, 0, 2)
    };
    let fixed: Vec<(usize, Vec<Line<'static>>)> = up.iter().map(|n| (1, rate_lines(n))).collect();
    let need = |k: usize| -> u16 {
        fixed
            .iter()
            .take(k)
            .map(|(h, r)| (*h + r.len()) as u16)
            .sum()
    };
    let reserved = u16::from(!down.is_empty());
    let room = inner.height.saturating_sub(reserved);
    let mut shown = up.len();
    while shown > 0 && need(shown) > room.saturating_sub(u16::from(shown < up.len())) {
        shown -= 1;
    }
    let more = shown < up.len();
    let bottom_rows = reserved.max(u16::from(more));
    let room = inner.height.saturating_sub(bottom_rows);
    // Sobra repartida entre as placas como gráfico espelhado (só a partir de 2 linhas).
    let spare = room.saturating_sub(need(shown));
    // Gráfico com teto; com sobra, uma linha em branco separa as placas.
    const NET_GRAPH_MAX: u16 = 6;
    let graph_h = if shown > 0 {
        (spare / shown as u16).min(NET_GRAPH_MAX)
    } else {
        0
    };
    let gap = shown > 1 && spare >= graph_h * shown as u16 + (shown as u16 - 1);
    let mut y = inner.y;
    for (k, (n, (_, rates))) in up.iter().zip(&fixed).take(shown).enumerate() {
        if gap && k > 0 {
            y += 1;
        }
        // Identificação: nome e descrição à esquerda, erros e link à direita.
        let mut right = Vec::new();
        match &n.errors {
            Ok((0, 0)) => {}
            Ok((rx, tx)) => right.push(Span::styled(
                format!("errors ↓{rx} ↑{tx}  "),
                Style::new().fg(th().fail),
            )),
            Err(e) => right.extend([label("errors "), missing(e), Span::raw("  ")]),
        }
        right.extend([label("link "), metric(&n.link, |v| bits(*v as f64))]);
        let right = Line::from(right);
        let rw = (right.width() as u16).min(inner.width);
        line(
            frame,
            vec![
                text(printable(&n.alias)).add_modifier(Modifier::BOLD),
                label(&format!("  {}", printable(&n.description))),
            ],
            inner.x,
            y,
            inner.width.saturating_sub(rw + 1),
            inner,
        );
        put(
            frame,
            Paragraph::new(right).right_aligned(),
            Rect::new(inner.x, y, inner.width, 1),
            inner,
        );
        y += 1;
        for l in rates.clone() {
            put(
                frame,
                Paragraph::new(l),
                Rect::new(inner.x, y, inner.width, 1),
                inner,
            );
            y += 1;
        }
        if graph_h >= 2 {
            let n_samples = usize::from(inner.width) * 2;
            let peak = |m: &HashMap<String, Series>| {
                m.get(&n.alias).map_or(0.0, |s| s.max_of_ref(n_samples))
            };
            let scale = peak(&st.send).max(peak(&st.recv)).max(NET_FLOOR);
            let up_h = graph_h.div_ceil(2);
            for (series, color, top, h, inverted) in [
                (st.send.get_mut(&n.alias), th().net_send, y, up_h, false),
                (
                    st.recv.get_mut(&n.alias),
                    th().net_recv,
                    y + up_h,
                    graph_h - up_h,
                    true,
                ),
            ] {
                if let Some(s) = series {
                    let data = s.over(n_samples, scale);
                    let g = Graph::new(&data, GraphColor::Fixed(color));
                    let g = if inverted { g.inverted() } else { g };
                    frame.render_widget(
                        g,
                        Rect::new(inner.x, top, inner.width, h).intersection(inner),
                    );
                }
            }
            // Escala por cima do canto do gráfico, depois de desenhá-lo (com uma linha só ela
            // cobriria o gráfico de envio inteiro).
            if up_h >= 2 {
                put(
                    frame,
                    Paragraph::new(label(&format!("scale {} ", net_rate(st, scale))))
                        .right_aligned(),
                    Rect::new(inner.x, y, inner.width, 1),
                    inner,
                );
            }
            y += graph_h;
        }
    }
    if !down.is_empty() || more {
        let names: Vec<String> = down.iter().map(|n| printable(&n.alias)).collect();
        let mut spans = Vec::new();
        if more {
            spans.push(label(&format!("+{} more  ", up.len() - shown)));
        }
        if !names.is_empty() {
            spans.extend([
                label("disconnected: "),
                Span::styled(names.join(", "), Style::new().fg(th().neutral)),
            ]);
        }
        line(
            frame,
            spans,
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            inner,
        );
    }
}

impl Series {
    /// `max_of` sem exigir `&mut` (para o cálculo da escala antes do gráfico).
    fn max_of_ref(&self, n: usize) -> f64 {
        let len = self.0.len();
        self.0
            .iter()
            .skip(len.saturating_sub(n))
            .copied()
            .fold(0.0, f64::max)
    }
}

// --- Discos ---

/// Ordem de exibição: discos com letra e o virtual; membros do pool depois (recolhidos se
/// `fold`). Devolve (disco, é membro).
fn disk_order(st: &State) -> Vec<(Disk, bool)> {
    let Some(Ok(list)) = st.fast.as_ref().map(|f| &f.disks) else {
        return Vec::new();
    };
    let mut out: Vec<(Disk, bool)> = list
        .iter()
        .filter(|d| d.pool_member != Ok(true))
        .map(|d| (d.clone(), false))
        .collect();
    out.sort_by_key(|(d, _)| d.letters.first().cloned().unwrap_or_else(|| "~".into()));
    if !pool_folded(st, list.len()) {
        out.extend(
            list.iter()
                .filter(|d| d.pool_member == Ok(true))
                .map(|d| (d.clone(), true)),
        );
    }
    out
}

/// Recolhe os membros do pool por escolha (`d`) ou, no automático, se não couberem.
fn pool_folded(st: &State, total: usize) -> bool {
    st.fold_pool.unwrap_or(total > 12)
}

/// Marca de uma coluna estreita sem valor: "!" para falha, "?" para estado neutro.
fn missing_mark(m: &Missing) -> String {
    match m {
        Missing::Fail(_) => "!".into(),
        Missing::Neutral(_) => "?".into(),
    }
}

/// Nome curto de um disco na tabela (letras ou `dN`).
fn disk_label(d: &Disk) -> String {
    if d.letters.is_empty() {
        format!("d{}", d.number)
    } else {
        d.letters.join(" ")
    }
}

/// Largura da coluna de temperatura (um estado neutro mais longo vai para as notas).
const DISK_TEMP_W: usize = 8;

/// Temperatura do disco, só se a leitura é do mesmo modelo (disco trocado no mesmo número: a
/// coluna mostra "wait" até a thread lenta reler).
fn disk_temp<'a>(st: &'a State, d: &Disk) -> Option<&'a crate::model::DiskTemp> {
    match st.slow.as_ref().map(|s| &s.disk_temps) {
        Some(Ok(list)) => list
            .iter()
            .find(|t| t.number == d.number)
            .filter(|t| same_disk(t, d)),
        _ => None,
    }
}

/// A leitura de temperatura é deste disco: mesmo número e, quando os dois modelos são
/// conhecidos, mesmo modelo (outro disco no mesmo número espera uma leitura nova; um modelo que
/// falhou de um lado não esconde a temperatura).
fn same_disk(t: &crate::model::DiskTemp, d: &Disk) -> bool {
    t.number == d.number
        && match (&t.model, &d.model) {
            (Some(a), Ok(b)) => a == b,
            _ => true,
        }
}

/// Notas embaixo da tabela de discos, inteiras (as colunas não têm espaço para causas): falhas
/// de metadados e de temperatura em vermelho, estados neutros longos em cinza, quebradas em
/// `width` colunas. Só dos discos exibidos.
fn disk_notes(st: &State, width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut note = |name: &str, what: &str, m: &Missing| {
        let style = Style::new().fg(if matches!(m, Missing::Fail(_)) {
            th().fail
        } else {
            th().neutral
        });
        let mark = if matches!(m, Missing::Fail(_)) {
            "! "
        } else {
            ""
        };
        let spans = vec![
            Span::styled(format!("{mark}{} {}: ", tr(name), tr(what)), style),
            Span::styled(printable(&tr(&m.to_string())), style),
        ];
        out.extend(wrap(spans, width, 0, 4));
    };
    if let Some(Err(e)) = st.slow.as_ref().map(|s| &s.disk_temps) {
        note("disks", "temperature", e);
    }
    // Todos os discos, inclusive os membros recolhidos do pool (a falha deles também aparece).
    let all: Vec<Disk> = st
        .fast
        .as_ref()
        .and_then(|f| f.disks.as_ref().ok())
        .cloned()
        .unwrap_or_default();
    for d in all {
        let name = disk_label(&d);
        let fails: [(&str, Option<&Missing>); 5] = [
            ("model", d.model.as_ref().err()),
            ("bus", d.bus.as_ref().err()),
            ("media type", d.ssd.as_ref().err()),
            ("capacity", d.capacity.as_ref().err()),
            ("pool", d.pool_member.as_ref().err()),
        ];
        for (what, m) in fails {
            match m {
                Some(m @ Missing::Fail(_)) => note(&name, what, m),
                // "asleep" cabe na coluna da capacidade; outro estado vai para a nota.
                Some(m @ Missing::Neutral(n)) if n.chars().count() > 6 => note(&name, what, m),
                _ => {}
            }
        }
        // Contadores de I/O (o mesmo IOCTL): uma nota com a causa, a coluna mostra "! failed".
        if let Some(m) = [&d.active, &d.response, &d.read, &d.write]
            .into_iter()
            .find_map(|m| m.as_ref().err().filter(|e| matches!(e, Missing::Fail(_))))
        {
            note(&name, "I/O counters", m);
        }
        if let Some(t) = disk_temp(st, &d)
            && let Err(m) = &t.celsius
            && (matches!(m, Missing::Fail(_)) || tr(&m.to_string()).chars().count() > DISK_TEMP_W)
        {
            note(&name, "temperature", m);
        }
        // Previsão de falha do SMART: aviso em vermelho (não é falha da fonte, é do disco).
        match disk_temp(st, &d).map(|t| &t.health) {
            Some(Ok(true)) => note(
                &name,
                "SMART",
                &Missing::Fail("the drive predicts a failure, back up its data".into()),
            ),
            Some(Err(m @ Missing::Fail(_))) => note(&name, "SMART health", m),
            _ => {}
        }
    }
    out
}

fn draw_disks(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let disks = st.fast.as_ref().map(|f| &f.disks);
    let Some(Ok(all)) = disks else {
        empty_box(frame, area, "disks", disks);
        return;
    };
    let members = all.iter().filter(|d| d.pool_member == Ok(true)).count();
    let order = disk_order(st);
    let folded = pool_folded(st, all.len());
    let block = boxed(
        "disks",
        &format!(
            "S=sys P=page{}",
            match (members > 0, folded) {
                (false, _) => "",
                (true, true) => "  [d] expand pool",
                (true, false) => "  [d] fold pool",
            }
        ),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Atividade (5), resposta (8), leitura e escrita (9 cada) e temperatura (2 + "! failed").
    // Depois do nome: barramento (7), tipo (4), capacidade (6 + espaço), flags (4) e 2 espaços.
    // Sem largura para tudo (o conhost abre com 120 colunas), saem primeiro barramento, tipo e
    // resposta (as falhas deles continuam nas notas); leitura, escrita e temperatura ficam.
    let compact = inner.width < 12 + 24 + 41;
    // Ainda mais estreito (o piso de 80 colunas): as flags S/P vão junto do nome, para a
    // temperatura não ser cortada.
    let tiny = inner.width < 12 + 13 + 33;
    // Com folga (nome inteiro e a atividade), IOPS e fila entram depois da escrita.
    let wide = inner.width >= 26 + 24 + 52 + 12;
    let (left_w, info_full): (u16, u16) = if tiny {
        (8, 33)
    } else if compact {
        (13, 33)
    } else if wide {
        (24, 52)
    } else {
        (24, 41)
    };
    // O nome encolhe antes de tudo (até 12 colunas); a atividade fica com a sobra. As colunas
    // nunca se sobrepõem: sem largura, o lado direito é que perde as últimas colunas.
    let name_w = inner
        .width
        .saturating_sub(left_w + info_full + 4)
        .clamp(12, if wide { 34 } else { 26 });
    let spark_w = inner.width.saturating_sub(name_w + left_w + info_full);
    let info_x = inner.x + name_w + left_w + spark_w;
    let info_w = inner
        .width
        .saturating_sub(name_w + left_w + spark_w)
        .min(info_full);
    let nw = usize::from(name_w);
    // Cabeçalho nas mesmas posições das colunas de cada linha.
    // Pela largura do texto traduzido ("unidade / modelo" tem 16 colunas).
    let full = tr("drive / model");
    let head = if nw > full.chars().count() + 1 {
        full
    } else {
        tr("drive")
    };
    let left_head = if tiny {
        format!("{:>6}  ", "cap")
    } else if compact {
        format!("{:>6} {:<4}  ", "cap", "flg")
    } else {
        format!("{:<7}{:<4}{:>6} {:<4}  ", "bus", "typ", "cap", "flg")
    };
    line(
        frame,
        vec![label(&format!("{head:<nw$}{left_head}"))],
        inner.x,
        inner.y,
        name_w + left_w,
        inner,
    );
    if spark_w >= 6 {
        line(
            frame,
            vec![label("active")],
            inner.x + name_w + left_w,
            inner.y,
            spark_w,
            inner,
        );
    }
    line(
        frame,
        vec![label(&if compact {
            format!("{:>5}{:>9}{:>9}  temp", "act", "read", "write")
        } else if wide {
            format!(
                "{:>5}{:>8}{:>9}{:>9}{:>7}{:>4}  temp",
                "act", "resp", "read", "write", "iops", "q"
            )
        } else {
            format!("{:>5}{:>8}{:>9}{:>9}  temp", "act", "resp", "read", "write")
        })],
        info_x,
        inner.y,
        info_w,
        inner,
    );
    // As notas (causas) têm linhas reservadas no fim da caixa: os discos que não couberem
    // entram no "+N more", nunca as causas.
    let notes = disk_notes(st, inner.width);
    let rows = usize::from(inner.height.saturating_sub(1)).saturating_sub(notes.len());
    // Não cabem todos: a última linha avisa quantos ficaram de fora (nada some sem aviso).
    let shown = if order.len() > rows {
        rows.saturating_sub(1)
    } else {
        order.len()
    };
    if shown < order.len() && rows > 0 {
        line(
            frame,
            vec![label(&format!("+{} more", order.len() - shown))],
            inner.x,
            inner.y + 1 + shown as u16,
            inner.width,
            inner,
        );
    }
    for (i, (d, member)) in order.iter().take(shown).enumerate() {
        let y = inner.y + 1 + i as u16;
        let letters = if d.letters.is_empty() {
            format!("d{}", d.number)
        } else {
            d.letters.join(" ")
        };
        let model = d
            .model
            .as_ref()
            .map_or_else(ToString::to_string, |m| printable(m));
        // Estreito: as flags entram no nome, logo depois das letras.
        let tag = match (tiny, d.system, d.pagefile) {
            (true, true, true) => " S P",
            (true, true, false) => " S",
            (true, false, true) => " P",
            _ => "",
        };
        let mut name = if *member {
            format!("  └ {letters} {model}")
        } else {
            format!("{letters}{tag} {model}")
        };
        if !member
            && folded
            && d.bus
                .as_ref()
                .is_ok_and(|b| matches!(b, crate::model::Bus::Spaces))
            && members > 0
        {
            name = tr(&format!("{letters} {model} ({members} members)")).into_owned();
        }
        // Pela largura no terminal (nome do PnP com caracteres largos não empurra as colunas).
        let name = format!("{} ", tabs::cell(&name, name_w.saturating_sub(1), false));
        // Sem valor: "!" para falha (a causa está nas notas), "?" para estado neutro; a
        // capacidade de um HDD em repouso diz "asleep".
        let bus = d
            .bus
            .as_ref()
            .map_or_else(missing_mark, ToString::to_string);
        let typ = d
            .ssd
            .as_ref()
            .map_or_else(missing_mark, |s| if *s { "SSD" } else { "HDD" }.to_string());
        let cap = match &d.capacity {
            Ok(c) => short_bytes(*c),
            Err(Missing::Neutral(n)) if n.chars().count() <= 6 => n.clone(),
            Err(e) => missing_mark(e),
        };
        let flags = format!(
            "{}{}",
            if d.system { "S " } else { "" },
            if d.pagefile { "P" } else { "" }
        );
        line(
            frame,
            vec![
                Span::styled(
                    name,
                    Style::new().fg(if *member { th().label } else { th().text }),
                ),
                label(&if tiny {
                    format!("{cap:>6}  ")
                } else if compact {
                    format!("{cap:>6} {flags:<4}  ")
                } else {
                    format!("{bus:<7}{typ:<4}{cap:>6} {flags:<4}  ")
                }),
            ],
            inner.x,
            y,
            name_w + left_w,
            inner,
        );
        if spark_w > 0
            && let Some(s) = st.disks.get_mut(&d.number)
        {
            frame.render_widget(
                Graph::new(s.values(), GraphColor::Value),
                Rect::new(inner.x + name_w + left_w, y, spark_w, 1).intersection(inner),
            );
        }
        // A causa não cabe na coluna: vai inteira para as notas embaixo da tabela.
        let t = match disk_temp(st, d).map(|t| &t.celsius) {
            Some(Err(Missing::Fail(_))) => failed_span(),
            Some(Err(m)) if tr(&m.to_string()).chars().count() > DISK_TEMP_W => label("(below)"),
            Some(c) => metric(c, |c| format!("{c} °C")),
            // Há leitura deste número, mas de outro modelo (disco trocado): espera a releitura.
            None if st
                .slow
                .as_ref()
                .and_then(|s| s.disk_temps.as_ref().ok())
                .is_some_and(|l| l.iter().any(|t| t.number == d.number)) =>
            {
                label("wait")
            }
            None => label(""),
        };
        // As quatro vêm do mesmo IOCTL: falhando juntas, "! failed" (causa nas notas) e a
        // temperatura continua no lugar dela.
        // Largura das colunas de I/O (para o "! failed" ocupar o lugar delas).
        let io_w = if compact {
            23
        } else if wide {
            42
        } else {
            31
        };
        let info = match (&d.active, &d.response, &d.read, &d.write) {
            (Err(e), Err(_), Err(_), Err(_)) => vec![
                match e {
                    Missing::Fail(_) => Span::styled(
                        format!("{:<w$}", tr("! failed"), w = io_w),
                        Style::new().fg(th().fail),
                    ),
                    Missing::Neutral(n) => label(&format!("{:<w$}", printable(n), w = io_w)),
                },
                Span::raw("  "),
                t,
            ],
            _ => vec![
                // Valores zerados esmaecidos: os discos com tráfego saltam à vista (falha continua
                // em vermelho).
                faded(
                    metric(&d.active, |v| format!("{:>4}%", num(*v, 0))),
                    d.active.as_ref().is_ok_and(|v| *v < 0.5),
                ),
                if compact {
                    Span::raw("")
                } else {
                    faded(
                        metric(&d.response, |v| format!(" {:>5}ms", num(v * 1000.0, 1))),
                        d.response.as_ref().is_ok_and(|v| *v < 0.000_05),
                    )
                },
                faded(
                    metric(&d.read, |v| {
                        format!(" {:>8}", format!("{}/s", short_bytes(*v as u64)))
                    }),
                    d.read.as_ref().is_ok_and(|v| *v < 1.0),
                ),
                faded(
                    metric(&d.write, |v| {
                        format!(" {:>8}", format!("{}/s", short_bytes(*v as u64)))
                    }),
                    d.write.as_ref().is_ok_and(|v| *v < 1.0),
                ),
                if wide {
                    faded(
                        metric(&d.iops, |v| format!(" {:>6}", num(*v, 0))),
                        d.iops.as_ref().is_ok_and(|v| *v < 0.5),
                    )
                } else {
                    Span::raw("")
                },
                if wide {
                    faded(
                        metric(&d.queue, |v| format!(" {v:>3}")),
                        d.queue.as_ref().is_ok_and(|v| *v == 0),
                    )
                } else {
                    Span::raw("")
                },
                Span::raw("  "),
                t,
            ],
        };
        line(frame, info, info_x, y, info_w, inner);
    }
    // Notas (causas inteiras), nas linhas reservadas.
    let first = inner.y + 1 + shown as u16 + u16::from(shown < order.len() && rows > 0);
    for (i, l) in notes.into_iter().enumerate() {
        let y = first + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        put(
            frame,
            Paragraph::new(l),
            Rect::new(inner.x, y, inner.width, 1),
            inner,
        );
    }
}

// --- Jogo ---

/// Cabeçalho de gráfico: o valor sempre; o mínimo e o máximo só se couberem inteiros (cortado,
/// "min 5." encostaria no rótulo do teto e viraria outro número).
fn head_fit(mut head: Vec<Span<'static>>, mm: Span<'static>, room: u16) -> Vec<Span<'static>> {
    let w: usize = head.iter().map(Span::width).sum::<usize>() + 2 + mm.width();
    if w <= usize::from(room) {
        head.extend([sep(), mm]);
    }
    head
}

/// Teto do gráfico de FPS: o menor degrau acima do máximo visível.
fn fps_ceiling(max: f64) -> f64 {
    FPS_STEPS
        .iter()
        .copied()
        .find(|s| *s >= max)
        .unwrap_or(FPS_STEPS[FPS_STEPS.len() - 1])
}

fn draw_game(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let Some(fps) = st.fps.clone() else {
        waiting(frame, area, boxed("game", ""));
        return;
    };
    let pipe_failed = matches!(fps.pipe, Err(Missing::Fail(_)));
    // Sem jogo em foco: "idle", e os gráficos continuam com o histórico (parado até voltar).
    let g = fps.game.as_ref().ok();
    let rtss_failed = matches!(fps.rtss, Err(Missing::Fail(_)));
    // Sem nenhum jogo nesta sessão, não há gráfico a manter: sem escalas nem aviso sobre ele.
    let has_history = !st.game_fps.0.is_empty();
    let (title, source) = match g {
        Some(g) => {
            // Sem resolução: a causa vai para as linhas de baixo, com a cor do estado.
            let res = g
                .resolution
                .as_ref()
                .map_or(String::new(), |(x, y)| format!(" · {x}x{y}"));
            let title = format!(
                "game · {} ({} · PID {}{res})",
                printable(&g.name),
                g.api,
                g.pid
            );
            let source = match (g.approx, pipe_failed) {
                (false, _) => "pipe: exact",
                (true, false) => "approx. (game opened before wintop)",
                (true, true) => "approx. (frametime pipe failed)",
            };
            (title, source)
        }
        None if rtss_failed => ("game · RTSS unavailable".to_string(), ""),
        None => ("game · idle".to_string(), ""),
    };
    let block = boxed(&title, source);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = match (g, &fps.game) {
        (Some(g), _) => game_lines(g, st.interval_ms),
        (None, Err(_)) if rtss_failed => {
            let mut v = vec![Line::from(vec![
                label("RTSS "),
                metric(&fps.rtss, Clone::clone),
            ])];
            if has_history {
                v.push(Line::from(label("graphs keep the last game until r")));
            }
            v
        }
        (None, Err(e)) => vec![
            Line::from(vec![missing(e)]),
            Line::from(vec![
                label("RTSS "),
                metric(&fps.rtss, Clone::clone),
                if has_history {
                    bar_sep()
                } else {
                    Span::raw("")
                },
                label(if has_history {
                    "graphs keep the last game until r"
                } else {
                    ""
                }),
            ]),
        ],
        (None, Ok(_)) => Vec::new(),
    };
    // Falha do pipe (outro servidor ocupando o nome): linha própria, só quando acontece.
    if pipe_failed {
        lines.push(Line::from(vec![
            label("frametime pipe "),
            metric(&fps.pipe, |_| String::new()),
        ]));
    }
    let lines: Vec<Line<'static>> = lines
        .into_iter()
        .flat_map(|l| wrap(l.spans, inner.width, 0, 2))
        .collect();
    let info_h = lines.len() as u16;
    // Sem jogo: só o "idle" do título; os cabeçalhos ficam com "-".
    let idle = || label("-");
    let [graphs, info] =
        // As linhas de dados (FPS, lows, frametime...) têm prioridade sobre a altura dos gráficos.
        Layout::vertical([
            // Pelo menos o cabeçalho com o FPS e o frametime atuais.
            Constraint::Min(3.min(inner.height.saturating_sub(info_h)).max(1)),
            Constraint::Length(info_h),
        ])
        .areas(inner);
    put(frame, Paragraph::new(lines), info, info);
    let [fps_area, ft_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(graphs);

    // FPS: teto em degraus pelo máximo visível.
    let n = usize::from(fps_area.width) * 2;
    let ceil = fps_ceiling(st.game_fps.max_of(n));
    let ceil_label = format!("{} ", num(ceil, 0));
    let head = head_fit(
        vec![
            label("FPS "),
            // Sem valor: marca curta (a causa inteira está nas linhas de baixo).
            g.map_or_else(idle, |g| match &g.fps {
                Ok(v) => text(num(*v, 0)).add_modifier(Modifier::BOLD),
                Err(Missing::Neutral(_)) => label("-"),
                Err(Missing::Fail(_)) => failed_span(),
            }),
        ],
        minmax(st.session.fps, |v| num(v, 0)),
        fps_area.width.saturating_sub(ceil_label.len() as u16 + 2),
    );
    line(
        frame,
        head,
        fps_area.x,
        fps_area.y,
        fps_area.width.saturating_sub(ceil_label.len() as u16 + 2),
        fps_area,
    );
    put(
        frame,
        Paragraph::new(label(if has_history { &ceil_label } else { "" })).right_aligned(),
        Rect::new(fps_area.x, fps_area.y, fps_area.width, 1),
        fps_area,
    );
    let data = st.game_fps.over(n, ceil);
    let gr = Rect::new(
        fps_area.x,
        fps_area.y + 1,
        fps_area.width.saturating_sub(1),
        fps_area.height.saturating_sub(1),
    );
    frame.render_widget(
        Graph::new(&data, GraphColor::Fixed(th().fps)),
        gr.intersection(fps_area),
    );

    // Frametime: teto fixo de 50 ms; picos acima de 33 ms em vermelho.
    let n = usize::from(ft_area.width) * 2;
    let head = head_fit(
        vec![
            label("FT "),
            // Como no FPS: marca curta sem valor (a causa inteira está nas linhas de baixo).
            g.map_or_else(idle, |g| {
                brief(&g.frametime, |(a, _, _)| format!("{} ms", num(*a, 2)))
                    .add_modifier(Modifier::BOLD)
            }),
        ],
        minmax(st.session.ft, |v| format!("{} ms", num(v, 1))),
        ft_area.width.saturating_sub(1 + 6 + 2),
    );
    line(
        frame,
        head,
        ft_area.x + 1,
        ft_area.y,
        ft_area.width.saturating_sub(1 + 6 + 2),
        ft_area,
    );
    put(
        frame,
        Paragraph::new(label(if has_history { "50 ms " } else { "" })).right_aligned(),
        Rect::new(ft_area.x, ft_area.y, ft_area.width, 1),
        ft_area,
    );
    let data = st.game_ft.over(n, FT_CEIL_MS);
    let gr = Rect::new(
        ft_area.x + 1,
        ft_area.y + 1,
        ft_area.width.saturating_sub(1),
        ft_area.height.saturating_sub(1),
    );
    frame.render_widget(
        Graph::new(
            &data,
            GraphColor::Threshold {
                base: th().frametime,
                alert: th().fail,
                at: st.frametime_alert_ms / FT_CEIL_MS,
            },
        ),
        gr.intersection(ft_area),
    );
}

/// As duas linhas de detalhe do jogo em foco.
fn game_lines(g: &crate::model::Game, interval_ms: u64) -> Vec<Line<'static>> {
    let ms = |v: f64| format!("{} ms", num(v, 2));
    // O que não cabe no cabeçalho dos gráficos (estado do FPS) e no título (resolução).
    let mut extra = Vec::new();
    if let Err(e) = &g.fps {
        extra.push(Line::from(vec![label("FPS "), missing(e)]));
    }
    if let Err(e) = &g.resolution {
        extra.push(Line::from(vec![label("resolution "), missing(e)]));
    }
    extra
        .into_iter()
        .chain(vec![
            Line::from(vec![
                label("1% low "),
                metric(&g.lows, |(a, _)| num(*a, 0)),
                label(&format!("   {}% low ", num(0.1, 1))),
                metric(&g.lows, |(_, b)| num(*b, 0)),
                bar_sep(),
                label("frametime "),
                metric(&g.frametime, |(a, lo, hi)| {
                    format!("{} ({} - {} ms)", ms(*a), num(*lo, 1), num(*hi, 1))
                }),
                bar_sep(),
                label("GPU busy "),
                metric(&g.gpu_time, |(a, f)| {
                    format!("{} / frame {}", ms(*a), ms(*f))
                }),
            ]),
            Line::from(vec![
                label("limited by "),
                metric(&g.bound, ToString::to_string),
                bar_sep(),
                label("lows over 60 s"),
                bar_sep(),
                label(&format!("graphs synced to the {interval_ms} ms tick")),
            ]),
        ])
        .collect()
}

#[cfg(test)]
mod game_graph_tests {
    use super::*;

    /// Jogo em foco: 1 ponto por tick da UI mesmo sem publicação nova do RTSS (intervalo de
    /// 100 ms, jogo congelado), e o frametime sem frame no tick é lacuna, nunca 0 ms.
    #[test]
    fn graficos_do_jogo_andam_no_ritmo_da_ui() {
        let mut st = State::new(100);
        let mut fps = crate::ui::tests::game();
        st.push_fps(fps.clone());
        st.push_fast(crate::ui::tests::fast(0));
        // Dois ticks sem publicação: o FPS atual se repete e o frametime fica sem valor.
        st.push_fast(crate::ui::tests::fast(1));
        st.push_fast(crate::ui::tests::fast(2));
        let (f, t): (Vec<f64>, Vec<f64>) =
            (st.game_fps.values().to_vec(), st.game_ft.values().to_vec());
        assert_eq!(f, [143.6, 143.6, 143.6]);
        assert!(
            (t[0] - 11.4).abs() < 1e-9 && t[1].is_nan() && t[2].is_nan(),
            "{t:?}"
        );
        // Congelado (sem frames no último segundo): FPS 0, ainda 1 ponto por tick.
        if let Ok(g) = &mut fps.game {
            g.fps = crate::model::neutral("no frames in the last second");
            g.peak_ms = None;
        }
        st.push_fps(fps);
        st.push_fast(crate::ui::tests::fast(3));
        assert_eq!(st.game_fps.values().last(), Some(&0.0));
        assert_eq!(st.game_ft.values().len(), 4);
    }
}
