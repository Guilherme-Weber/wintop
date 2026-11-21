//! Abas Processos, GPU (por processo) e Sensores.
//!
//! - Processos: tabela no estilo do btop (ordem por coluna com ←→, `i` inverte, `/` filtra),
//!   dados de `SystemProcessInformation` (thread lenta, só com a aba visível).
//! - GPU: VRAM por categoria e uma linha por processo com engines (máximo por tipo, como o Task
//!   Manager), VRAM local/não local/paginada, CPU e RAM; runtimes sob demanda (Enter).
//! - Sensores: todas as leituras físicas (temperaturas, potência, clocks, fans) com mínimo e
//!   máximo da sessão; temperaturas só número, sem cor (direção do usuário).
//!
//! As colunas menos importantes saem primeiro quando a largura não dá (nada é cortado no meio).

use super::{
    BORDER, LABEL, NEUTRAL, State, TEXT, TITLE, bar_sep, boxed, empty_box, label, metric, missing,
    put, text, vram_bar,
};
use crate::fmt::{bytes, num};
use crate::model::{GpuProc, Metric, Missing, Proc, fail};
use crate::ui::widgets::gradient;
use crate::win::printable;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const SELECTED: Color = Color::Rgb(48, 54, 70);
const GAME: Color = Color::Rgb(224, 138, 60);

// --- Cursor (seleção estável por PID) ---

/// Seleção de uma lista que muda a cada coleta: segue o PID; se ele sumir, fica na mesma
/// posição. As teclas só acumulam o movimento; o desenho aplica sobre a lista atual.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cursor {
    pid: Option<u32>,
    index: usize,
    offset: usize,
    pending: isize,
    /// Linhas visíveis no último desenho (PgUp/PgDn).
    page: usize,
}

impl Cursor {
    pub fn up(&mut self, n: usize) {
        self.pending = self
            .pending
            .saturating_sub(n.try_into().unwrap_or(isize::MAX));
    }

    pub fn down(&mut self, n: usize) {
        self.pending = self
            .pending
            .saturating_add(n.try_into().unwrap_or(isize::MAX));
    }

    pub fn page_up(&mut self) {
        self.up(self.page.max(1));
    }

    pub fn page_down(&mut self) {
        self.down(self.page.max(1));
    }

    pub fn home(&mut self) {
        self.pending = isize::MIN / 2;
    }

    pub fn end(&mut self) {
        self.pending = isize::MAX / 2;
    }

    /// PID selecionado no último desenho.
    #[cfg(test)]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Linhas visíveis no último desenho.
    pub(in crate::ui) fn page(&self) -> usize {
        self.page
    }

    /// Aplica o movimento sobre `pids` (na ordem exibida) com `rows` linhas visíveis; devolve
    /// (selecionado, primeira linha visível).
    pub(in crate::ui) fn resolve(&mut self, pids: &[u32], rows: usize) -> (Option<usize>, usize) {
        self.page = rows;
        if pids.is_empty() {
            self.pending = 0;
            self.pid = None;
            return (None, 0);
        }
        let last = pids.len() - 1;
        let base = self
            .pid
            .and_then(|p| pids.iter().position(|&x| x == p))
            .unwrap_or(self.index.min(last));
        let idx = (base as isize)
            .saturating_add(self.pending)
            .clamp(0, last as isize) as usize;
        self.pending = 0;
        self.index = idx;
        self.pid = Some(pids[idx]);
        if idx < self.offset {
            self.offset = idx;
        } else if rows > 0 && idx >= self.offset + rows {
            self.offset = idx + 1 - rows;
        }
        self.offset = self.offset.min(pids.len().saturating_sub(rows.max(1)));
        (Some(idx), self.offset)
    }
}

// --- Tabela ---

/// Coluna: título, largura, alinhada à direita, e ordem de saída quando falta largura
/// (0 = nunca sai; maior sai primeiro). A coluna flexível (`flex`) fica com o que sobra.
struct Col {
    title: &'static str,
    w: u16,
    right: bool,
    drop: u8,
    flex: bool,
}

const fn col(title: &'static str, w: u16, right: bool, drop: u8) -> Col {
    Col {
        title,
        w,
        right,
        drop,
        flex: false,
    }
}

const fn flex(title: &'static str, min: u16) -> Col {
    Col {
        title,
        w: min,
        right: false,
        drop: 0,
        flex: true,
    }
}

/// Larguras das colunas que cabem em `avail` (`None` = coluna fora). Uma coluna de folga
/// entre cada par.
fn fit(cols: &[Col], avail: u16) -> Vec<Option<u16>> {
    let mut on: Vec<bool> = vec![true; cols.len()];
    let need = |on: &[bool]| -> u16 {
        let n = on.iter().filter(|x| **x).count() as u16;
        cols.iter()
            .zip(on)
            .filter(|(_, o)| **o)
            .map(|(c, _)| c.w)
            .sum::<u16>()
            + n.saturating_sub(1)
    };
    while need(&on) > avail {
        let worst = cols
            .iter()
            .enumerate()
            .filter(|(i, c)| on[*i] && c.drop > 0)
            .max_by_key(|(_, c)| c.drop)
            .map(|(i, _)| i);
        match worst {
            Some(i) => on[i] = false,
            None => break,
        }
    }
    // A sobra vai para a coluna flexível até `FLEX_MAX`; o resto é dividido entre as outras
    // (em tela larga o número não fica longe do nome).
    let spare = avail.saturating_sub(need(&on));
    let flex_extra = cols
        .iter()
        .zip(&on)
        .find(|(c, o)| c.flex && **o)
        .map_or(0, |(c, _)| spare.min(FLEX_MAX.saturating_sub(c.w)));
    let rest = spare - flex_extra;
    let others = cols.iter().zip(&on).filter(|(c, o)| !c.flex && **o).count() as u16;
    let each = rest.checked_div(others).unwrap_or(0);
    let mut extra = rest.checked_rem(others).unwrap_or(0);
    cols.iter()
        .zip(&on)
        .map(|(c, o)| {
            o.then(|| {
                if c.flex {
                    c.w + flex_extra + if others == 0 { rest } else { 0 }
                } else {
                    let e = u16::from(extra > 0);
                    extra = extra.saturating_sub(1);
                    c.w + each + e
                }
            })
        })
        .collect()
}

/// Largura máxima da coluna de nomes.
const FLEX_MAX: u16 = 44;

/// Texto que cabe em `w` colunas do terminal (corta no fim com `…`) e alinhado.
fn cell(s: &str, w: u16, right: bool) -> String {
    let w = usize::from(w);
    let total = Span::raw(s).width();
    // Caminho comum: cabe inteiro, só alinha.
    if total <= w {
        let pad = " ".repeat(w - total);
        return if right {
            format!("{pad}{s}")
        } else {
            format!("{s}{pad}")
        };
    }
    let mut out = String::new();
    let mut used = 0;
    let mut tmp = [0u8; 4];
    for ch in s.chars() {
        let cw = Span::raw(&*ch.encode_utf8(&mut tmp)).width();
        // Reserva 1 coluna para o `…` quando o texto não cabe inteiro.
        let limit = if total > w { w.saturating_sub(1) } else { w };
        if used + cw > limit {
            break;
        }
        out.push(ch);
        used += cw;
    }
    if total > w && w > 0 {
        out.push('…');
        used += 1;
    }
    let pad = " ".repeat(w.saturating_sub(used));
    if right {
        format!("{pad}{out}")
    } else {
        format!("{out}{pad}")
    }
}

/// Linha da tabela: uma célula por coluna visível, cada uma com o seu estilo.
fn row(cols: &[Col], widths: &[Option<u16>], cells: Vec<(String, Style)>) -> Line<'static> {
    let mut spans = Vec::new();
    let mut first = true;
    for ((c, w), (s, style)) in cols.iter().zip(widths).zip(cells) {
        let Some(w) = w else { continue };
        if !first {
            spans.push(Span::raw(" "));
        }
        first = false;
        spans.push(Span::styled(cell(&s, *w, c.right), style));
    }
    Line::from(spans)
}

fn header(cols: &[Col], widths: &[Option<u16>], sorted: Option<(usize, bool)>) -> Line<'static> {
    let cells = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut t = c.title.to_string();
            let mut style = Style::new().fg(LABEL).add_modifier(Modifier::BOLD);
            if let Some((s, desc)) = sorted
                && s == i
            {
                t.push(if desc { '▼' } else { '▲' });
                style = Style::new().fg(TITLE).add_modifier(Modifier::BOLD);
            }
            (t, style)
        })
        .collect();
    row(cols, widths, cells)
}

fn plain(s: String) -> (String, Style) {
    (s, Style::new().fg(TEXT))
}

fn dim(s: String) -> (String, Style) {
    (s, Style::new().fg(LABEL))
}

/// Porcentagem com a cor pela intensidade (cinza no zero).
fn pct_cell(v: f64) -> (String, Style) {
    if v < 0.05 {
        return dim("0.0".into());
    }
    (num(v, 1), Style::new().fg(gradient(v / 100.0)))
}

fn rate(v: f64) -> (String, Style) {
    if v < 1.0 {
        dim("0".into())
    } else {
        plain(format!("{}/s", bytes(v as u64)))
    }
}

/// Mensagem simples (espera, falha) dentro da área.
fn note(frame: &mut Frame<'_>, area: Rect, span: Span<'static>) {
    put(frame, Paragraph::new(Line::from(span)), area, area);
}

// --- Processos ---

/// Coluna da ordem da aba Processos (←→ percorre na ordem da tabela).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcSort {
    Pid,
    Name,
    #[default]
    Cpu,
    Mem,
    Commit,
    Threads,
    Handles,
    Read,
    Write,
}

impl ProcSort {
    const ALL: [Self; 9] = [
        Self::Pid,
        Self::Name,
        Self::Cpu,
        Self::Mem,
        Self::Commit,
        Self::Threads,
        Self::Handles,
        Self::Read,
        Self::Write,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// Ordem natural: texto e PID crescentes; números decrescentes (o maior no topo).
    fn natural_desc(self) -> bool {
        !matches!(self, Self::Pid | Self::Name)
    }
}

const PROC_COLS: [Col; 9] = [
    col("PID", 7, true, 0),
    flex("Name", 16),
    col("CPU%", 6, true, 0),
    col("Memory", 10, true, 0),
    col("Commit", 10, true, 4),
    col("Threads", 8, true, 3),
    col("Handles", 8, true, 5),
    col("Read", 11, true, 1),
    col("Write", 11, true, 2),
];

/// Lista filtrada e ordenada (separada do desenho para teste).
pub(super) fn sorted_procs<'a>(
    list: &'a [Proc],
    sort: ProcSort,
    reverse: bool,
    filter: &str,
) -> Vec<&'a Proc> {
    let f = filter.to_lowercase();
    let mut v: Vec<&Proc> = list
        .iter()
        .filter(|p| f.is_empty() || p.name.to_lowercase().contains(&f) || p.pid.to_string() == f)
        .collect();
    let desc = sort.natural_desc() != reverse;
    v.sort_by(|a, b| {
        let o = match sort {
            ProcSort::Pid => a.pid.cmp(&b.pid),
            // Sem alocar no comparador (ASCII basta para nomes de executáveis).
            ProcSort::Name => a
                .name
                .bytes()
                .map(|c| c.to_ascii_lowercase())
                .cmp(b.name.bytes().map(|c| c.to_ascii_lowercase())),
            ProcSort::Cpu => a.cpu.total_cmp(&b.cpu),
            ProcSort::Mem => a.mem.cmp(&b.mem),
            ProcSort::Commit => a.commit.cmp(&b.commit),
            ProcSort::Threads => a.threads.cmp(&b.threads),
            ProcSort::Handles => a.handles.cmp(&b.handles),
            ProcSort::Read => a.io_read.total_cmp(&b.io_read),
            ProcSort::Write => a.io_write.total_cmp(&b.io_write),
        };
        // Desempate estável pelo PID (a lista não "pula" entre coletas iguais).
        let o = if desc { o.reverse() } else { o };
        o.then(a.pid.cmp(&b.pid))
    });
    v
}

fn game_pid(st: &State) -> Option<u32> {
    st.fps
        .as_ref()
        .and_then(|f| f.game.as_ref().ok())
        .map(|g| g.pid)
}

pub(super) fn draw_processes(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let Some(Ok(list)) = &st.procs else {
        empty_box(frame, area, "processes", st.procs.as_ref());
        return;
    };
    let rows = sorted_procs(list, st.proc_sort, st.proc_reverse, &st.filter);
    let desc = st.proc_sort.natural_desc() != st.proc_reverse;
    let mut right = format!("{} processes", list.len());
    if !st.filter.is_empty() || st.filtering {
        right = format!(
            "{} of {} · filter: {}{}",
            rows.len(),
            list.len(),
            printable(&st.filter),
            if st.filtering { "_" } else { "" }
        );
    }
    let block = boxed("processes", &right);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 || inner.width < 4 {
        return;
    }
    let widths = fit(&PROC_COLS, inner.width);
    let head = Rect::new(inner.x, inner.y, inner.width, 1);
    put(
        frame,
        Paragraph::new(header(
            &PROC_COLS,
            &widths,
            Some((st.proc_sort.index(), desc)),
        )),
        head,
        inner,
    );
    let body_h = usize::from(inner.height - 1);
    let pids: Vec<u32> = rows.iter().map(|p| p.pid).collect();
    let (sel, offset) = st.proc_cursor.resolve(&pids, body_h);
    if rows.is_empty() {
        note(
            frame,
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
            label("no process matches the filter"),
        );
        return;
    }
    let game = game_pid(st);
    for (i, p) in rows.iter().enumerate().skip(offset).take(body_h) {
        let name_style = if Some(p.pid) == game {
            Style::new().fg(GAME).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(TEXT)
        };
        let mut line = row(
            &PROC_COLS,
            &widths,
            vec![
                dim(p.pid.to_string()),
                (printable(&p.name), name_style),
                pct_cell(p.cpu),
                plain(bytes(p.mem)),
                plain(bytes(p.commit)),
                plain(p.threads.to_string()),
                plain(p.handles.to_string()),
                rate(p.io_read),
                rate(p.io_write),
            ],
        );
        if Some(i) == sel {
            line = line.style(Style::new().bg(SELECTED));
        }
        let y = inner.y + 1 + (i - offset) as u16;
        put(
            frame,
            Paragraph::new(line),
            Rect::new(inner.x, y, inner.width, 1),
            inner,
        );
    }
}

// --- GPU por processo ---

/// Uso de um tipo de engine (0 se o processo não o usa).
fn engine(p: &GpuProc, pred: impl Fn(&str) -> bool) -> Option<f64> {
    p.engines.as_ref().ok().map(|e| {
        e.iter()
            .filter(|(k, _)| pred(k))
            .map(|(_, v)| *v)
            .fold(0.0, f64::max)
    })
}

/// "GPU" do Task Manager: a engine mais usada pelo processo.
fn gpu_pct(p: &GpuProc) -> Option<f64> {
    engine(p, |_| true)
}

const GPU_COLS: [Col; 12] = [
    col("PID", 7, true, 0),
    flex("Name", 16),
    col("GPU%", 5, true, 0),
    col("3D", 5, true, 0),
    col("Copy", 5, true, 6),
    col("Enc", 5, true, 4),
    col("Dec", 5, true, 5),
    col("Comp", 5, true, 7),
    col("VRAM", 10, true, 0),
    col("Shared", 10, true, 3),
    col("Paged", 10, true, 8),
    col("CPU% · RAM", 18, true, 2),
];

/// Do mais ocupado para o menos; empate pela VRAM local.
pub(super) fn sorted_gpu(list: &[GpuProc]) -> Vec<&GpuProc> {
    let mut v: Vec<&GpuProc> = list.iter().collect();
    v.sort_by(|a, b| {
        let g = |p: &GpuProc| gpu_pct(p).unwrap_or(0.0);
        g(b).total_cmp(&g(a))
            .then(b.local.cmp(&a.local))
            .then(a.pid.cmp(&b.pid))
    });
    v
}

pub(super) fn draw_gpu_tab(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let gpu = st
        .fast
        .as_ref()
        .and_then(|f| f.gpus.as_ref().ok())
        .and_then(|l| l.first())
        .cloned();
    let title = gpu.as_ref().and_then(|g| g.name.as_ref().ok()).map_or_else(
        || "gpu processes".to_string(),
        |n| format!("gpu processes · {n}"),
    );
    let Some(Ok(list)) = &st.gpu_procs else {
        empty_box(frame, area, &title, st.gpu_procs.as_ref());
        return;
    };
    let rows = sorted_gpu(list);
    let block = boxed(&title, &format!("{} processes", list.len()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // VRAM, adapter, folga, cabeçalho, 1 linha, divisória e 2 de detalhe.
    if inner.height < 8 || inner.width < 4 {
        return;
    }
    // Linha 0: VRAM por categoria; linha 1: engines do adaptador (NVML).
    let game = game_pid(st);
    let vram = st
        .slow
        .as_ref()
        .and_then(|s| s.gpus.as_ref().ok())
        .and_then(|l| l.first())
        .map(|g| g.vram.clone());
    let r0 = Rect::new(inner.x, inner.y, inner.width, 1);
    let fast_gpu_err = st
        .fast
        .as_ref()
        .and_then(|f| f.gpus.as_ref().err().cloned());
    match (&vram, &gpu, &fast_gpu_err) {
        (Some(Ok(b)), Some(g), _) => vram_bar(frame, r0, b, game, &g.dedicated),
        (Some(Err(e)), _, _) | (_, _, Some(e)) => {
            note(frame, r0, label("VRAM "));
            note(
                frame,
                Rect::new(inner.x + 5, inner.y, inner.width.saturating_sub(5), 1),
                missing(e),
            );
        }
        _ => note(frame, r0, label("VRAM: waiting")),
    }
    if let Some(g) = &gpu {
        let pctm = |m: &Metric<u32>| metric(m, |v| format!("{v}%"));
        let spans = vec![
            label("adapter  3D/compute "),
            pctm(&g.usage),
            bar_sep(),
            label("encoder "),
            pctm(&g.encoder),
            bar_sep(),
            label("decoder "),
            pctm(&g.decoder),
            bar_sep(),
            label("memory activity "),
            pctm(&g.mem_activity),
        ];
        put(
            frame,
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
            inner,
        );
    }
    // Detalhe do selecionado embaixo (linhas quebradas, nada cortado); a tabela fica com o
    // meio. A seleção é resolvida antes, para o detalhe ser do processo que vai aparecer.
    let table_y = inner.y + 3;
    let pids: Vec<u32> = rows.iter().map(|p| p.pid).collect();
    let estimate = usize::from(inner.height.saturating_sub(3 + 1 + 3));
    let (first_sel, _) = st.gpu_cursor.resolve(&pids, estimate);
    let detail = gpu_detail(
        st,
        first_sel.and_then(|i| rows.get(i).copied()),
        inner.width,
    );
    let detail_h = (detail.len() as u16).clamp(2, inner.height.saturating_sub(5).max(2));
    let table_h = inner.height.saturating_sub(3 + 1 + detail_h);
    let widths = fit(&GPU_COLS, inner.width);
    put(
        frame,
        Paragraph::new(header(&GPU_COLS, &widths, None)),
        Rect::new(inner.x, table_y, inner.width, 1),
        inner,
    );
    let body_h = usize::from(table_h.saturating_sub(1));
    let (sel, offset) = st.gpu_cursor.resolve(&pids, body_h);
    let procs: std::collections::HashMap<u32, &Proc> = match &st.procs {
        Some(Ok(l)) => l.iter().map(|p| (p.pid, p)).collect(),
        _ => std::collections::HashMap::new(),
    };
    let e = |v: Option<f64>| v.map_or_else(|| dim("-".into()), pct_cell);
    for (i, p) in rows.iter().enumerate().skip(offset).take(body_h) {
        let name_style = if Some(p.pid) == game {
            Style::new().fg(GAME).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(TEXT)
        };
        let cpu_ram = procs.get(&p.pid).map_or_else(
            || dim("-".into()),
            |r| plain(format!("{}% · {}", num(r.cpu, 1), bytes(r.mem))),
        );
        let b = |v: u64| {
            if v == 0 {
                dim("0".into())
            } else {
                plain(bytes(v))
            }
        };
        let mut line = row(
            &GPU_COLS,
            &widths,
            vec![
                dim(p.pid.to_string()),
                (printable(&p.name), name_style),
                e(gpu_pct(p)),
                e(engine(p, |k| k == "3D")),
                e(engine(p, |k| k.starts_with("Copy"))),
                e(engine(p, |k| k.starts_with("VideoEncode"))),
                e(engine(p, |k| k.starts_with("VideoDecode"))),
                e(engine(p, |k| {
                    k.starts_with("Compute") || k.starts_with("Cuda")
                })),
                b(p.local),
                b(p.non_local),
                b(p.paged()),
                cpu_ram,
            ],
        );
        if Some(i) == sel {
            line = line.style(Style::new().bg(SELECTED));
        }
        let y = table_y + 1 + (i - offset) as u16;
        put(
            frame,
            Paragraph::new(line),
            Rect::new(inner.x, y, inner.width, 1),
            inner,
        );
    }
    let detail_y = inner.y + inner.height - detail_h;
    for (k, l) in detail.into_iter().take(usize::from(detail_h)).enumerate() {
        put(
            frame,
            Paragraph::new(l),
            Rect::new(inner.x, detail_y + k as u16, inner.width, 1),
            inner,
        );
    }
    // Linha divisória acima do detalhe.
    put(
        frame,
        Paragraph::new(Span::styled(
            "─".repeat(usize::from(inner.width)),
            Style::new().fg(BORDER),
        )),
        Rect::new(inner.x, detail_y - 1, inner.width, 1),
        inner,
    );
}

/// Detalhe da aba GPU: engines do selecionado (todas), runtimes e a falha da lista de
/// processos (sem ela, a coluna "CPU% · RAM" fica com "-"), quebrados em `width`.
fn gpu_detail(st: &State, sel: Option<&GpuProc>, width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Some(p) = sel {
        let mut l1 =
            vec![text(format!("{} ({})", printable(&p.name), p.pid)).add_modifier(Modifier::BOLD)];
        l1.push(label("  engines"));
        match &p.engines {
            Ok(list) if list.iter().any(|(_, v)| *v >= 0.05) => {
                for (i, (k, v)) in list.iter().filter(|(_, v)| *v >= 0.05).enumerate() {
                    let sepr = if i == 0 { " " } else { " · " };
                    l1.push(text(format!("{sepr}{} {}%", printable(k), num(*v, 1))));
                }
            }
            Ok(_) => l1.push(label(" idle")),
            Err(e) => {
                l1.push(Span::raw(" "));
                l1.push(missing(e));
            }
        }
        out.extend(super::wrap(l1, width, 0, 2));
        let mut l2 = vec![label("runtimes ")];
        match st
            .runtimes
            .get(&p.pid)
            .filter(|(n, _)| n == &p.name)
            .map(|(_, r)| r)
        {
            Some(Ok(r)) if r.is_empty() => l2.push(label("none of the known GPU runtimes")),
            Some(Ok(r)) => l2.push(text(r.join(", "))),
            Some(Err(e)) => l2.push(missing(e)),
            None => l2.push(label(
                "press enter to read the loaded modules (never on RTSS-hooked apps)",
            )),
        }
        out.extend(super::wrap(l2, width, 0, 2));
    }
    if let Some(Err(e)) = &st.procs {
        out.extend(super::wrap(
            vec![label("CPU% · RAM "), missing(e)],
            width,
            0,
            2,
        ));
    }
    out
}

// --- Sensores ---

pub(super) enum SensorValue {
    /// Valor, unidade e casas decimais.
    Num(Metric<f64>, &'static str, usize),
    Text(Metric<String>),
}

pub(super) struct SensorRow {
    pub group: String,
    pub name: String,
    pub value: SensorValue,
}

impl SensorRow {
    /// Chave dos extremos da sessão (só sensores numéricos).
    pub(super) fn key(&self) -> Option<String> {
        matches!(self.value, SensorValue::Num(..)).then(|| format!("{}/{}", self.group, self.name))
    }
}

fn num_row(group: &str, name: &str, m: Metric<f64>, unit: &'static str, dec: usize) -> SensorRow {
    SensorRow {
        group: group.into(),
        name: name.into(),
        value: SensorValue::Num(m, unit, dec),
    }
}

fn text_row(group: &str, name: &str, m: Metric<String>) -> SensorRow {
    SensorRow {
        group: group.into(),
        name: name.into(),
        value: SensorValue::Text(m),
    }
}

/// Todas as leituras físicas, agrupadas (CPU, GPU n, discos).
pub(super) fn sensor_rows(st: &State) -> Vec<SensorRow> {
    let mut out = Vec::new();
    let Some(f) = &st.fast else {
        return out;
    };
    let cpu = "CPU";
    match &f.cpu_sensors {
        Ok(s) => {
            out.push(num_row(cpu, "Tctl", s.tctl.clone(), "°C", 1));
            match &s.ccds {
                Ok(list) => {
                    for (n, t) in list {
                        out.push(num_row(cpu, &format!("CCD{n}"), Ok(*t), "°C", 1));
                    }
                }
                Err(e) => out.push(num_row(cpu, "CCD", Err(e.clone()), "°C", 1)),
            }
            out.push(num_row(
                cpu,
                "Package power",
                s.package_power.clone(),
                "W",
                1,
            ));
        }
        Err(e) => out.push(text_row(cpu, "Sensors", Err(e.clone()))),
    }
    if let Ok(c) = &f.cpu {
        out.push(num_row(cpu, "Clock", c.freq_mhz.clone(), "MHz", 0));
    }
    match &f.gpus {
        Ok(list) => {
            for g in list {
                let grp = format!("GPU {}", g.index);
                let u = |m: &Metric<u32>| m.clone().map(f64::from);
                let first = |m: &Metric<(u32, u32)>| m.clone().map(|(a, _)| f64::from(a));
                out.push(num_row(&grp, "Temperature", u(&g.temp), "°C", 0));
                out.push(num_row(
                    &grp,
                    "Slowdown limit",
                    u(&g.temp_slowdown),
                    "°C",
                    0,
                ));
                out.push(num_row(
                    &grp,
                    "Power",
                    g.power.clone().map(|(p, _)| p),
                    "W",
                    1,
                ));
                out.push(num_row(
                    &grp,
                    "Power limit",
                    g.power.clone().map(|(_, l)| l),
                    "W",
                    0,
                ));
                out.push(num_row(
                    &grp,
                    "Graphics clock",
                    first(&g.clock_graphics),
                    "MHz",
                    0,
                ));
                out.push(num_row(&grp, "SM clock", first(&g.clock_sm), "MHz", 0));
                out.push(num_row(&grp, "Memory clock", first(&g.clock_mem), "MHz", 0));
                out.push(num_row(
                    &grp,
                    "Video clock",
                    first(&g.clock_video),
                    "MHz",
                    0,
                ));
                match &g.fans {
                    Ok(fans) => {
                        for (i, fan) in fans.iter().enumerate() {
                            out.push(num_row(
                                &grp,
                                &format!("Fan {} speed", i + 1),
                                fan.percent.clone().map(f64::from),
                                "%",
                                0,
                            ));
                            out.push(num_row(
                                &grp,
                                &format!("Fan {} RPM", i + 1),
                                fan.rpm.clone().map(f64::from),
                                "RPM",
                                0,
                            ));
                        }
                    }
                    Err(e) => out.push(text_row(&grp, "Fans", Err(e.clone()))),
                }
                out.push(text_row(
                    &grp,
                    "P-state",
                    g.pstate.clone().map(|p| format!("P{p}")),
                ));
                out.push(text_row(
                    &grp,
                    "Throttle",
                    g.throttle.clone().map(|t| {
                        if t.is_empty() {
                            "none".into()
                        } else {
                            t.join(", ")
                        }
                    }),
                ));
                out.push(text_row(
                    &grp,
                    "PCIe link",
                    g.pcie_link
                        .clone()
                        .map(|(gen_, w)| format!("Gen{gen_} x{w}")),
                ));
            }
        }
        Err(e) => out.push(text_row("GPU", "Sensors", Err(e.clone()))),
    }
    let disks = "Disks";
    match st.slow.as_ref().map(|s| &s.disk_temps) {
        Some(Ok(list)) => {
            // Disco que saiu da coleta rápida (USB removido): a leitura velha da lenta não
            // aparece.
            let present = |n: u32| {
                f.disks
                    .as_ref()
                    .map_or(true, |l| l.iter().any(|d| d.number == n))
            };
            for t in list.iter().filter(|t| present(t.number)) {
                // Só exibe a leitura do disco certo (o modelo confere com o da coleta rápida).
                let model = f
                    .disks
                    .as_ref()
                    .ok()
                    .and_then(|l| l.iter().find(|d| d.number == t.number))
                    .and_then(|d| d.model.as_ref().ok().cloned());
                let name = match &model {
                    Some(m) => format!("Disk {} {}", t.number, printable(m)),
                    None => format!("Disk {}", t.number),
                };
                let value = if t.model.is_some() && model.is_some() && t.model != model {
                    fail("disk changed: waiting for a new reading")
                } else {
                    t.celsius.clone().map(f64::from)
                };
                out.push(num_row(disks, &name, value, "°C", 0));
            }
        }
        Some(Err(e)) => out.push(text_row(disks, "Temperature", Err(e.clone()))),
        None => out.push(text_row(
            disks,
            "Temperature",
            Err(Missing::Neutral("waiting for the first reading".into())),
        )),
    }
    out
}

/// Largura de uma coluna da aba Sensores com mínimo e máximo e 40 colunas de nome.
const SENSOR_FULL_W: u16 = 40 + 1 + 12 + 1 + 11 + 1 + 11;

const SENSOR_COLS: [Col; 4] = [
    flex("Sensor", 18),
    col("Value", 12, true, 0),
    col("Min", 11, true, 1),
    col("Max", 11, true, 1),
];

pub(super) fn draw_sensors(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let rows = sensor_rows(st);
    let block = boxed("sensors", "session min · max (r resets)");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if rows.is_empty() {
        note(
            frame,
            inner,
            Span::styled("waiting for the first collection", Style::new().fg(NEUTRAL)),
        );
        return;
    }
    // Colunas de jornal: um grupo continua na coluna seguinte quando não cabe; a largura de
    // cada coluna encolhe (mínimo e máximo saem antes) para nenhum sensor sumir.
    let h = usize::from(inner.height.saturating_sub(1)).max(1);
    // Com largura para isso, um grupo por coluna (CPU, GPU, discos lado a lado).
    let groups = rows.windows(2).filter(|w| w[0].group != w[1].group).count() + 1;
    let roomy = usize::from((inner.width + 3) / (SENSOR_FULL_W + 3));
    let split = groups <= roomy;
    let gap = 3u16;
    let min_w = 18 + 1 + 12;
    let fits = usize::from((inner.width + gap) / (min_w + gap)).max(1);
    // Causas longas ocupam linhas a mais, que dependem da largura da coluna, que depende do
    // número de colunas: uma estimativa (uma coluna só) e depois a conta com a largura real.
    let geometry = |ncols: usize| {
        let cw = (inner.width.saturating_sub(gap * (ncols as u16 - 1))) / ncols as u16;
        (cw, fit(&SENSOR_COLS, cw))
    };
    // Repete até o número de colunas parar de mudar (cada passo só aumenta; teto em `fits`).
    let mut ncols = 1;
    let (mut cw, mut widths, mut layout);
    loop {
        (cw, widths) = geometry(ncols);
        layout = sensor_layout(&rows, h, split, |i| {
            sensor_lines(&rows[i], cw, &widths, st)
                .len()
                .saturating_sub(1)
        });
        let want = layout.len().clamp(1, fits);
        if want <= ncols {
            break;
        }
        ncols = want;
    }
    // Não cabem nem com as colunas mais estreitas: a última linha avisa quantos ficaram fora.
    let hidden: usize = layout
        .iter()
        .skip(ncols)
        .flatten()
        .filter(|it| matches!(it, SensorItem::Row(_)))
        .count();
    if hidden > 0
        && let Some(last) = layout.get_mut(ncols - 1)
    {
        // A linha do aviso sai da última coluna: o que estava nela também conta.
        let keep = h.saturating_sub(1).min(last.len());
        let dropped = last[keep..]
            .iter()
            .filter(|it| matches!(it, SensorItem::Row(_)))
            .count();
        last.truncate(keep);
        last.push(SensorItem::More(hidden + dropped));
    }
    for (col_i, items) in layout.iter().enumerate().take(ncols) {
        let x = inner.x + col_i as u16 * (cw + gap);
        put(
            frame,
            Paragraph::new(header(&SENSOR_COLS, &widths, None)),
            Rect::new(x, inner.y, cw, 1),
            inner,
        );
        for (y, item) in items.iter().enumerate() {
            let at = Rect::new(x, inner.y + 1 + y as u16, cw, 1);
            let r = match item {
                SensorItem::Blank => continue,
                SensorItem::More(n) => {
                    note(frame, at, label(&format!("+{n} more (taller terminal)")));
                    continue;
                }
                SensorItem::Title(i, cont) => {
                    let t = if *cont {
                        format!("{} (cont.)", printable(&rows[*i].group))
                    } else {
                        printable(&rows[*i].group)
                    };
                    put(
                        frame,
                        Paragraph::new(Span::styled(
                            t,
                            Style::new().fg(TITLE).add_modifier(Modifier::BOLD),
                        )),
                        at,
                        inner,
                    );
                    continue;
                }
                SensorItem::Cause(i, k) => {
                    if let Some(l) = sensor_lines(&rows[*i], cw, &widths, st)
                        .into_iter()
                        .nth(*k + 1)
                    {
                        put(frame, Paragraph::new(l), at, inner);
                    }
                    continue;
                }
                SensorItem::Row(i) => &rows[*i],
            };
            if let Some(l) = sensor_lines(r, cw, &widths, st).into_iter().next() {
                put(frame, Paragraph::new(l), at, inner);
            }
        }
    }
}

/// Item de uma coluna da aba Sensores (índices em `rows`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SensorItem {
    Blank,
    /// Título do grupo do sensor `i`; `true` = continuação de outra coluna.
    Title(usize, bool),
    Row(usize),
    /// Linha `k` (a partir da 2ª) do sensor `i`: nome ou valor longo quebrado.
    Cause(usize, usize),
    /// Sensores que não couberam (só na última coluna).
    More(usize),
}

/// Espaço das colunas de valor, mínimo e máximo (com as folgas).
fn value_room(widths: &[Option<u16>]) -> usize {
    widths[1..]
        .iter()
        .flatten()
        .map(|w| usize::from(*w) + 1)
        .sum::<usize>()
        .saturating_sub(1)
}

/// O valor do sensor como texto (número com unidade, texto, estado ou falha).
fn value_span(r: &SensorRow) -> Span<'static> {
    match &r.value {
        SensorValue::Num(m, unit, dec) => metric(m, |v| format!("{} {unit}", num(*v, *dec))),
        SensorValue::Text(m) => metric(m, Clone::clone),
    }
}

/// Todas as linhas de um sensor, nada cortado: na forma comum, uma linha (nome, valor, mínimo,
/// máximo). Nome mais largo que a coluna: o nome quebra nas linhas de cima e os valores vêm na
/// última. Valor (ou causa) mais largo que as colunas de valor: vai inteiro para as linhas de
/// baixo, quebrado por palavra.
fn sensor_lines(r: &SensorRow, cw: u16, widths: &[Option<u16>], st: &State) -> Vec<Line<'static>> {
    let (value, lo, hi) = match &r.value {
        SensorValue::Num(_, unit, dec) => {
            let f = |v: f64| format!("{} {unit}", num(v, *dec));
            let mm = r
                .key()
                .and_then(|k| st.session.sensors.get(&k).copied())
                .and_then(super::MinMax::get);
            let (lo, hi) = mm.map_or((String::new(), String::new()), |(a, b)| (f(a), f(b)));
            (value_span(r), lo, hi)
        }
        SensorValue::Text(_) => (value_span(r), String::new(), String::new()),
    };
    let name = printable(&r.name);
    let name_w = usize::from(widths[0].unwrap_or(0));
    let mut out = Vec::new();
    let name_fits = Span::raw(name.as_str()).width() <= name_w;
    if !name_fits {
        out.extend(super::wrap(vec![text(name.clone())], cw, 0, 2));
    }
    let shown_name = if name_fits { name } else { String::new() };
    let room = value_room(widths);
    if value.width() > room {
        if name_fits {
            out.push(Line::from(Span::styled(shown_name, Style::new().fg(TEXT))));
        }
        out.extend(super::wrap(vec![value], cw, 2, 2));
    } else if value.width() > usize::from(widths[1].unwrap_or(0)) {
        // Valor que não cabe na coluna dele ocupa também as de mínimo e máximo.
        out.push(Line::from(vec![
            Span::styled(
                cell(&shown_name, name_w as u16, false),
                Style::new().fg(TEXT),
            ),
            Span::raw(" "),
            Span::styled(cell(&value.content, room as u16, true), value.style),
        ]));
    } else {
        out.push(row(
            &SENSOR_COLS,
            widths,
            vec![
                plain(shown_name),
                (value.content.to_string(), value.style),
                dim(lo),
                dim(hi),
            ],
        ));
    }
    out
}

/// Distribui os sensores em colunas de `h` linhas (puro, para teste).
/// `split`: cada grupo começa numa coluna nova.
/// `extra(i)`: linhas de causa embaixo do sensor `i`.
pub(super) fn sensor_layout(
    rows: &[SensorRow],
    h: usize,
    split: bool,
    extra: impl Fn(usize) -> usize,
) -> Vec<Vec<SensorItem>> {
    let h = h.max(2);
    let mut cols: Vec<Vec<SensorItem>> = vec![Vec::new()];
    let mut last_group: Option<&str> = None;
    for (i, r) in rows.iter().enumerate() {
        let cur = cols.last_mut().expect("uma coluna");
        if last_group != Some(r.group.as_str()) {
            last_group = Some(&r.group);
            let len = rows[i..].iter().take_while(|x| x.group == r.group).count();
            // Linha em branco + título + ao menos 2 sensores (ou o grupo todo, se menor).
            let need = usize::from(!cur.is_empty()) + 1 + len.min(2);
            if !cur.is_empty() && (split || cur.len() + need > h) {
                cols.push(Vec::new());
            } else if !cur.is_empty() {
                cur.push(SensorItem::Blank);
            }
            cols.last_mut()
                .expect("uma coluna")
                .push(SensorItem::Title(i, false));
        } else if cur.len() >= h {
            cols.push(vec![SensorItem::Title(i, true)]);
        }
        cols.last_mut()
            .expect("uma coluna")
            .push(SensorItem::Row(i));
        for k in 0..extra(i) {
            if cols.last().is_some_and(|c| c.len() >= h) {
                cols.push(vec![SensorItem::Title(i, true)]);
            }
            cols.last_mut()
                .expect("uma coluna")
                .push(SensorItem::Cause(i, k));
        }
    }
    cols
}
