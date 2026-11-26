//! Estado da interface: últimas coletas, histórico dos gráficos, extremos da sessão e seleção.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use ratatui::layout::Rect;

use super::disks::pool_folded;
use super::gpu::busiest_engine;
use super::gpu_tab::{self, GpuSort};
use super::procs_tab::{self, ProcSort};
use super::table::TableState;
use crate::model::{Fast, Fps, GpuProc, Metric, Proc, Slow, View};
/// Amostras guardadas por série (cobre ~500 colunas de braille).
pub(super) const HISTORY: usize = 1000;

/// Sobreposição ativa que captura o teclado (exclusivas por construção).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Modal {
    #[default]
    None,
    /// Digitando o filtro (tecla `/`): as teclas vão para o texto.
    Filter,
    /// Janela de ajuda aberta (tecla `?`; qualquer tecla fecha).
    Help,
    /// Encerrar processo pedido (tecla Del na aba Processos), à espera da confirmação.
    ConfirmEnd { pid: u32, name: String, create: u64 },
    /// Pergunta de atualização (versão nova encontrada) ou atualização em andamento.
    Update(UpdateUi),
}

/// Estado dos runtimes pedidos sob demanda para um processo na aba GPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeStatus {
    /// Leitura em andamento em thread dedicada.
    Reading,
    /// Resultado pronto (lista de runtimes ou motivo).
    Ready(Metric<Vec<&'static str>>),
}

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
pub struct Series(pub(super) VecDeque<f64>);

impl Series {
    pub fn push(&mut self, v: f64) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        // NaN = coleta sem valor: lacuna no gráfico (um 0 falso pareceria uso zero).
        self.0
            .push_back(if v.is_finite() || v.is_nan() { v } else { 0.0 });
        // Mantém a fatia única contígua na escrita (1×/tick) para o desenho ler via `&self`.
        self.0.make_contiguous();
    }

    pub(super) fn values(&self) -> &[f64] {
        debug_assert!(self.0.as_slices().1.is_empty());
        self.0.as_slices().0
    }

    /// Maior valor entre as últimas `n` amostras.
    pub(super) fn max_of(&self, n: usize) -> f64 {
        let v = self.values();
        v[v.len().saturating_sub(n)..]
            .iter()
            .copied()
            .fold(0.0, f64::max)
    }

    /// Há ao menos um ponto finito na série inteira (falso quando só sobraram lacunas `NaN`).
    pub(super) fn has_finite(&self) -> bool {
        self.0.iter().any(|v| v.is_finite())
    }

    /// Há ao menos um ponto finito nas últimas `n` amostras (janela visível do gráfico).
    pub(super) fn has_finite_in(&self, n: usize) -> bool {
        let len = self.0.len();
        self.0
            .iter()
            .skip(len.saturating_sub(n))
            .any(|v| v.is_finite())
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

    pub(super) fn get(self) -> Option<(f64, f64)> {
        self.0
    }
}

/// Extremos da sessão de cada série exibida.
#[derive(Default)]
pub(super) struct Session {
    pub(super) cpu: MinMax,
    pub(super) tctl: MinMax,
    pub(super) cpu_power: MinMax,
    pub(super) gpu: MinMax,
    pub(super) gpu_temp: MinMax,
    pub(super) gpu_power: MinMax,
    pub(super) vram: MinMax,
    pub(super) gfx_clock: MinMax,
    pub(super) ram: MinMax,
    pub(super) commit: MinMax,
    pub(super) send: HashMap<String, MinMax>,
    pub(super) recv: HashMap<String, MinMax>,
    pub(super) fps: MinMax,
    pub(super) ft: MinMax,
    /// Aba Sensores: extremos de cada sensor numérico, pela chave "grupo/nome".
    pub(super) sensors: HashMap<String, MinMax>,
}

/// Estado da interface: os últimos dados, o histórico dos gráficos e os extremos da sessão.
#[derive(Default)]
pub struct State {
    pub fast: Option<Arc<Fast>>,
    pub slow: Option<Arc<Slow>>,
    pub fps: Option<Arc<Fps>>,
    pub interval_ms: u64,
    pub tab: Tab,
    /// Membros do Storage Space recolhidos: `None` = automático (recolhe se não couberem).
    pub fold_pool: Option<bool>,
    /// Relógio da barra de abas (vem de fora: o desenho é puro).
    pub clock: String,
    pub(super) cpu: Series,
    pub(super) cores: Vec<Series>,
    /// Tempo de kernel (total e por lógico), alinhado com `cpu` e `cores`.
    pub(super) cpu_kernel: Series,
    pub(super) cores_kernel: Vec<Series>,
    /// Histórico de cada GPU (todas, para a troca não começar vazia).
    pub(super) gpu: Vec<Series>,
    /// Encoder, decoder, atividade da memória, uso geral e a outra engine de cada GPU (grade
    /// ao lado do gráfico).
    pub(super) gpu_video: Vec<[Series; 5]>,
    pub(super) disks: HashMap<u32, Series>,
    pub(super) send: HashMap<String, Series>,
    pub(super) recv: HashMap<String, Series>,
    pub(super) game_fps: Series,
    /// FPS real com frame gen (NaN sem frame gen), alinhado com `game_fps`.
    pub(super) game_real: Series,
    pub(super) game_ft: Series,
    /// FPS mais recente e o pior frametime (ms) desde o último tick da UI.
    pending_fps: Option<f64>,
    pending_ft: Option<f64>,
    pub(super) session: Session,
    /// Aba Processos: a última lista (só chega com a aba visível), cursor, ordem e filtro.
    pub procs: Option<Metric<Vec<Proc>>>,
    pub proc_table: TableState<ProcSort>,
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
    /// Aba GPU: processos da GPU exibida, cursor, ordem e runtimes pedidos (Enter), por PID.
    pub gpu_procs: Option<Metric<Vec<GpuProc>>>,
    pub gpu_table: TableState<GpuSort>,
    /// GPU exibida nas abas Geral e GPU (tecla `g`; posição na lista, 1ª se sumir).
    pub gpu_sel: usize,
    /// Tela congelada (tecla `p` ou Espaço): as coletas continuam, mas não entram na tela.
    pub paused: bool,
    /// Sobreposição ativa que captura o teclado (filtro, ajuda, confirmação ou atualização).
    pub modal: Modal,
    /// Mensagem curta de uma ação (fim de processo): texto, se é erro, quando apareceu.
    pub toast: Option<(String, bool, std::time::Instant)>,
    /// Do config (valem na hora quando o arquivo muda).
    pub frametime_alert_ms: f64,
    pub net_bytes: bool,
    /// Taxa de atualização do monitor principal (teto do gráfico de FPS); vem de fora.
    pub refresh_hz: Option<u32>,
    /// Tema atual (tecla `t`, gravado no config).
    pub theme_name: String,
    /// Aviso da configuração (arquivo com erro, gravação recusada), no rodapé.
    pub notice: Option<String>,
    /// Por PID, com o nome do processo lido (PID reaproveitado por outro não herda o resultado).
    pub runtimes: HashMap<u32, (String, RuntimeStatus)>,
}

impl State {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            frametime_alert_ms: crate::config::DEFAULT_FRAMETIME_ALERT_MS,
            ..Default::default()
        }
    }

    /// Digitando o filtro (`/`): o teclado vai para o texto do filtro.
    pub fn is_filtering(&self) -> bool {
        matches!(self.modal, Modal::Filter)
    }

    /// Expira a mensagem curta (`toast`) depois de `TOAST_FOR`; devolve `true` se ela sumiu
    /// (pede redesenho mesmo com a tela pausada).
    pub fn expire_toast(&mut self, now: std::time::Instant) -> bool {
        if let Some((_, _, at)) = &self.toast
            && now.saturating_duration_since(*at) >= TOAST_FOR
        {
            self.toast = None;
            return true;
        }
        false
    }

    #[cfg(test)]
    pub(in crate::ui) fn session_sensor(&self, key: &str) -> Option<(f64, f64)> {
        self.session.sensors.get(key).copied().and_then(MinMax::get)
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
        self.gpu_table.cursor = super::table::Cursor::default();
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
            if matches!(self.modal, Modal::Filter) {
                self.modal = Modal::None;
            }
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
    pub fn push_slow(&mut self, s: impl Into<Arc<Slow>>) {
        let s = s.into();
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
        let rows = gpu_tab::sorted_gpu(
            list,
            self.gpu_table.sort,
            self.gpu_table.reverse,
            &self.filter,
        );
        let pids: Vec<u32> = rows.iter().map(|p| p.pid).collect();
        let page = self.gpu_table.cursor.page();
        let (Some(i), _) = self.gpu_table.cursor.resolve(&pids, page) else {
            return None;
        };
        let (pid, name) = (rows[i].pid, rows[i].name.clone());
        // Uma leitura por processo de cada vez (Enter segurado repetiria a tecla ~30 vezes por
        // segundo, cada uma com uma thread).
        if self
            .runtimes
            .get(&pid)
            .is_some_and(|(n, r)| *n == name && matches!(r, RuntimeStatus::Reading))
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
                    RuntimeStatus::Ready(crate::model::neutral(
                        "skipped: waiting for RTSS to list the hooked apps (anticheat safety)",
                    )),
                ),
            );
            return None;
        };
        self.runtimes
            .insert(pid, (name.clone(), RuntimeStatus::Reading));
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
            self.runtimes.insert(pid, (name, RuntimeStatus::Ready(r)));
        }
    }

    /// Processo da linha selecionada na aba Processos (na ordem e com o filtro da tela).
    pub fn selected_proc(&self) -> Option<Proc> {
        let Some(Ok(list)) = &self.procs else {
            return None;
        };
        let rows = procs_tab::proc_rows(self, list);
        let pids: Vec<u32> = rows.iter().map(|(p, _)| p.pid).collect();
        self.proc_table
            .cursor
            .peek(&pids)
            .map(|i| rows[i].0.clone())
    }

    /// Zera os extremos da sessão (tecla `r`). O histórico do jogo também: só o `r` o zera
    /// (jogos trocam de executável e congelam ao carregar, e isso não deve apagar o gráfico).
    pub fn reset_session(&mut self) {
        self.session = Session::default();
        self.game_fps = Series::default();
        self.game_real = Series::default();
        self.game_ft = Series::default();
        self.pending_fps = None;
        self.pending_ft = None;
    }

    /// Nova coleta da thread rápida: guarda, alimenta os históricos (inclusive os do jogo, no
    /// mesmo ritmo) e os extremos da sessão.
    pub fn push_fast(&mut self, f: impl Into<Arc<Fast>>) {
        let f = f.into();
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
            self.gpu_video.resize_with(list.len(), Default::default);
            for (s, g) in self.gpu_video.iter_mut().zip(list) {
                let other = g
                    .other_engine
                    .as_ref()
                    .map(|(_, v)| *v)
                    .map_err(Clone::clone);
                let all = [&g.encoder, &g.decoder, &g.mem_activity, &g.usage, &other];
                for (s, m) in s.iter_mut().zip(all) {
                    s.push(m.as_ref().map_or(f64::NAN, |v| f64::from(*v) / 100.0));
                }
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
        // andam juntas; frametime sem frame no tick é lacuna (NaN), nunca um 0 ms falso. No
        // ocioso (sem jogo em foco), quando já há histórico, empurra lacuna (NaN) para os
        // gráficos continuarem andando com o tempo e o trecho antigo sair pela esquerda.
        let (fps, ft) = (self.pending_fps.take(), self.pending_ft.take());
        let game = self.fps.as_ref().and_then(|f| f.game.as_ref().ok());
        if game.is_some() || fps.is_some() || ft.is_some() {
            let now = game.and_then(|g| g.fps.as_ref().ok().copied());
            self.game_fps.push(fps.or(now).unwrap_or(0.0));
            // Frames reais (frame gen): a parte de baixo do gráfico de FPS, mais escura.
            let real = game.and_then(|g| g.framegen).map(|fg| fg.real_fps);
            self.game_real.push(real.unwrap_or(f64::NAN));
            self.game_ft.push(ft.unwrap_or(f64::NAN));
        } else if !self.game_fps.0.is_empty() {
            self.game_fps.push(f64::NAN);
            self.game_real.push(f64::NAN);
            self.game_ft.push(f64::NAN);
            // Depois que toda a série antiga rolou para fora do histórico, limpa para o ocioso
            // voltar ao estado inicial sem rótulos de escala vazios.
            if !self.game_fps.has_finite() && !self.game_ft.has_finite() {
                self.game_fps.0.clear();
                self.game_real.0.clear();
                self.game_ft.0.clear();
            }
        }
        self.fast = Some(f);
        self.update_sensors();
    }

    /// Novo estado do RTSS (a cada 250 ms com jogo): o número de FPS atualiza na hora; os
    /// gráficos esperam o tick da UI.
    pub fn push_fps(&mut self, s: impl Into<Arc<Fps>>) {
        let s = s.into();
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

    /// Extremos da sessão de cada sensor numérico (aba Sensores), atualizados direto das
    /// leituras atuais sem montar a lista de linhas de texto a cada tick.
    fn update_sensors(&mut self) {
        let Some(f) = self.fast.as_deref() else {
            return;
        };
        let sensors = &mut self.session.sensors;
        let mut bump = |key: &str, v: f64| {
            if let Some(m) = sensors.get_mut(key) {
                m.update(v);
            } else {
                let mut m = MinMax::default();
                m.update(v);
                sensors.insert(key.to_string(), m);
            }
        };
        if let Ok(s) = &f.cpu_sensors {
            if let Ok(v) = s.tctl {
                bump("CPU/Tctl", v);
            }
            if let Ok(list) = &s.ccds {
                for (n, t) in list {
                    bump(&format!("CPU/CCD{n}"), *t);
                }
            }
            if let Ok(v) = s.package_power {
                bump("CPU/Package power", v);
            }
        }
        if let Ok(c) = &f.cpu
            && let Ok(v) = c.freq_mhz
        {
            bump("CPU/Clock", v);
        }
        let slow_gpus = self.slow.as_deref().and_then(|s| s.gpus.as_ref().ok());
        if let Ok(list) = &f.gpus {
            for g in list {
                if g.no_nvml.is_some() {
                    continue;
                }
                let idx = g.index;
                if let Ok(v) = g.temp {
                    bump(&format!("GPU {idx}/Temperature"), f64::from(v));
                }
                if let Ok(v) = g.temp_slowdown {
                    bump(&format!("GPU {idx}/Slowdown limit"), f64::from(v));
                }
                if let Ok((p, l)) = g.power {
                    bump(&format!("GPU {idx}/Power"), p);
                    if l > 0.0 {
                        bump(&format!("GPU {idx}/Power limit"), l);
                    }
                }
                if let Ok((a, _)) = g.clock_graphics {
                    bump(&format!("GPU {idx}/Graphics clock"), f64::from(a));
                }
                if let Ok((a, _)) = g.clock_sm {
                    bump(&format!("GPU {idx}/SM clock"), f64::from(a));
                }
                if let Ok((a, _)) = g.clock_mem {
                    bump(&format!("GPU {idx}/Memory clock"), f64::from(a));
                }
                if let Ok((a, _)) = g.clock_video {
                    bump(&format!("GPU {idx}/Video clock"), f64::from(a));
                }
                if let Ok(fans) = &g.fans {
                    for (i, fan) in fans.iter().enumerate() {
                        if let Ok(v) = fan.percent {
                            bump(&format!("GPU {idx}/Fan {} speed", i + 1), f64::from(v));
                        }
                        if let Ok(v) = fan.rpm {
                            bump(&format!("GPU {idx}/Fan {} RPM", i + 1), f64::from(v));
                        }
                    }
                }
                if let Some(s) = slow_gpus.and_then(|l| l.iter().find(|x| x.index == idx)) {
                    if let Ok(v) = s.pcie_tx {
                        bump(&format!("GPU {idx}/PCIe TX"), v as f64 / 1e6);
                    }
                    if let Ok(v) = s.pcie_rx {
                        bump(&format!("GPU {idx}/PCIe RX"), v as f64 / 1e6);
                    }
                }
            }
        }
        if let Some(Ok(list)) = self.slow.as_deref().map(|s| &s.disk_temps) {
            for t in list {
                let present = f
                    .disks
                    .as_ref()
                    .map_or(true, |l| l.iter().any(|d| d.number == t.number));
                if !present {
                    continue;
                }
                let model = f
                    .disks
                    .as_ref()
                    .ok()
                    .and_then(|l| l.iter().find(|d| d.number == t.number))
                    .and_then(|d| d.model.as_ref().ok());
                let changed = t.model.is_some()
                    && model.is_some()
                    && t.model.as_deref() != model.map(String::as_str);
                if !changed && let Ok(c) = t.celsius {
                    let key = match model {
                        Some(m) => {
                            format!("Disks/Disk {} {}", t.number, crate::win::printable(m))
                        }
                        None => format!("Disks/Disk {}", t.number),
                    };
                    bump(&key, f64::from(c));
                }
            }
        }
    }
}

/// Leitura de runtimes pedida pela interface, feita numa thread à parte (abrir um processo
/// travado não congela a tela).
#[derive(Debug, PartialEq, Eq)]
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

/// Estado da atualização na tela.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateUi {
    /// Versão nova encontrada: pergunta.
    Offer(String),
    /// Baixando, conferindo e trocando.
    Applying(String),
}
