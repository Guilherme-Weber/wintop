//! Aba GPU: VRAM por categoria, engines do adaptador e uma linha por processo (engines, VRAM
//! local/não local/paginada, CPU e RAM) com runtimes sob demanda (Enter).

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::table::{
    Col, SortCol, col, col_ranges, dim, filter_header, fit, flex, game_pid, header, note, pct_cell,
    plain, row,
};
use super::{
    RuntimeStatus, State, bar_sep, boxed_ready, empty_box, label, metric, missing, put, raw_text,
    text, vram_bar,
};
use crate::collect::{is_decode, is_encode};
use crate::fmt::{bytes, num};
use crate::i18n::tr;
use crate::model::{GpuProc, Metric, Proc};
use crate::ui::theme::th;
use crate::win::printable;

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

/// Coluna da ordem da aba GPU (←→ percorre na ordem da tabela; `CPU% · RAM` vem da lista de
/// processos e não ordena).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GpuSort {
    Pid,
    Name,
    #[default]
    Gpu,
    ThreeD,
    Copy,
    Encode,
    Decode,
    Compute,
    Vram,
    Shared,
    Paged,
}

impl GpuSort {
    const ALL: [Self; 11] = [
        Self::Pid,
        Self::Name,
        Self::Gpu,
        Self::ThreeD,
        Self::Copy,
        Self::Encode,
        Self::Decode,
        Self::Compute,
        Self::Vram,
        Self::Shared,
        Self::Paged,
    ];

    /// Valor numérico da coluna (engine sem taxa = 0).
    fn key(self, p: &GpuProc) -> f64 {
        let e = |pred: fn(&str) -> bool| engine(p, pred).unwrap_or(0.0);
        match self {
            Self::Pid | Self::Name => 0.0,
            Self::Gpu => gpu_pct(p).unwrap_or(0.0),
            Self::ThreeD => e(|k| k == "3D"),
            Self::Copy => e(|k| k.starts_with("Copy")),
            Self::Encode => e(is_encode),
            Self::Decode => e(is_decode),
            Self::Compute => e(|k| k.starts_with("Compute") || k.starts_with("Cuda")),
            Self::Vram => p.local as f64,
            Self::Shared => p.non_local as f64,
            Self::Paged => p.paged() as f64,
        }
    }
}

impl SortCol for GpuSort {
    fn from_index(i: usize) -> Option<Self> {
        Self::ALL.get(i).copied()
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    fn natural_desc(self) -> bool {
        !matches!(self, Self::Pid | Self::Name)
    }
}

/// Lista filtrada (nome ou PID, como na aba Processos) e ordenada pela coluna; empate pela
/// VRAM local e depois pelo PID (a lista não pula entre coletas iguais).
pub(in crate::ui) fn sorted_gpu<'a>(
    list: &'a [GpuProc],
    sort: GpuSort,
    reverse: bool,
    filter: &str,
) -> Vec<&'a GpuProc> {
    let f = filter.to_lowercase();
    let mut v: Vec<&GpuProc> = list
        .iter()
        .filter(|p| f.is_empty() || p.name.to_lowercase().contains(&f) || p.pid.to_string() == f)
        .collect();
    let desc = sort.natural_desc() != reverse;
    v.sort_by(|a, b| {
        let o = match sort {
            GpuSort::Pid => a.pid.cmp(&b.pid),
            GpuSort::Name => a
                .name
                .bytes()
                .map(|c| c.to_ascii_lowercase())
                .cmp(b.name.bytes().map(|c| c.to_ascii_lowercase())),
            s => s.key(a).total_cmp(&s.key(b)),
        };
        let o = if desc { o.reverse() } else { o };
        o.then(b.local.cmp(&a.local)).then(a.pid.cmp(&b.pid))
    });
    v
}

pub(super) fn draw_gpu_tab(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let n = st.gpu_count();
    let sel = st.gpu_index(n);
    let gpu = st
        .fast
        .as_ref()
        .and_then(|f| f.gpus.as_ref().ok())
        .and_then(|l| l.get(sel));
    // Com mais de uma GPU, a posição da exibida (tecla `g` troca).
    let head = if n > 1 {
        format!("gpu {}/{n} processes", sel + 1)
    } else {
        "gpu processes".to_string()
    };
    let title = gpu
        .and_then(|g| g.name.as_ref().ok())
        .map_or_else(|| head.clone(), |name| format!("{head} · {name}"));
    let Some(Ok(list)) = &st.gpu_procs else {
        empty_box(frame, area, &title, st.gpu_procs.as_ref());
        return;
    };
    let rows = sorted_gpu(list, st.gpu_table.sort, st.gpu_table.reverse, &st.filter);
    let desc = st.gpu_table.desc();
    let right = if !st.filter.is_empty() || st.is_filtering() {
        filter_header(rows.len(), list.len(), &st.filter, st.is_filtering())
    } else {
        tr(&format!("{} processes", list.len())).into_owned()
    };
    let block = boxed_ready(&title, &right);
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
        // Pela lista lenta, como a tabela (`push_slow`): a rápida pode ter falhado no ciclo.
        .and_then(|l| st.slow_gpu(l))
        .map(|g| &g.vram);
    let r0 = Rect::new(inner.x, inner.y, inner.width, 1);
    let fast_gpu_err = st.fast.as_ref().and_then(|f| f.gpus.as_ref().err());
    match (vram, gpu, fast_gpu_err) {
        (Some(Ok(b)), Some(g), _) => vram_bar(frame, r0, 0, "VRAM  ", b, game, &g.dedicated, 0),
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
    // Embaixo da barra: a legenda das cores da VRAM (a mesma da aba Geral) e as engines do
    // adaptador, quebradas em quantas linhas precisarem; a tabela começa depois delas.
    let mut head_lines = super::vram_legend(vram, game, inner.width);
    if let Some(g) = gpu {
        let pctm = |m: &Metric<u32>| metric(m, |v| format!("{v}%"));
        // Sem NVML o uso é a engine mais ocupada (como o Gerenciador de Tarefas), e a atividade
        // da memória não existe.
        let mut spans = vec![
            label(if g.no_nvml.is_some() {
                "adapter  busiest engine "
            } else {
                "adapter  3D/compute "
            }),
            pctm(&g.usage),
            bar_sep(),
            Span::styled(tr("encoder ").into_owned(), Style::new().fg(th().encode)),
            pctm(&g.encoder),
            bar_sep(),
            Span::styled(tr("decoder ").into_owned(), Style::new().fg(th().decode)),
            pctm(&g.decoder),
        ];
        if g.no_nvml.is_none() {
            spans.extend([bar_sep(), label("memory activity "), pctm(&g.mem_activity)]);
        }
        spans.extend([
            bar_sep(),
            label("other engine "),
            metric(&g.other_engine, |(t, v)| {
                format!("{} {v}%", super::engine_label(t))
            }),
        ]);
        head_lines.extend(super::wrap(spans, inner.width, 0, 9));
    }
    // Sem altura, a tabela fica com pelo menos o cabeçalho e 3 linhas.
    let head_max = usize::from(inner.height.saturating_sub(1 + 4 + 3));
    head_lines.truncate(head_max.max(1));
    let head_count = head_lines.len() as u16;
    for (k, l) in head_lines.into_iter().enumerate() {
        put(
            frame,
            Paragraph::new(l),
            Rect::new(inner.x, inner.y + 1 + k as u16, inner.width, 1),
            inner,
        );
    }
    // Detalhe do selecionado embaixo (linhas quebradas, nada cortado); a tabela fica com o
    // meio. A seleção é resolvida antes, para o detalhe ser do processo que vai aparecer.
    let table_y = inner.y + 1 + head_count;
    let pids: Vec<u32> = rows.iter().map(|p| p.pid).collect();
    let detail = gpu_detail(
        st,
        st.gpu_table
            .cursor
            .peek(&pids)
            .and_then(|i| rows.get(i).copied()),
        inner.width,
    );
    // Pelo menos uma linha de processo fica (cabeçalho + 1 + divisória).
    let head_h = 1 + head_count;
    let detail_h = (detail.len() as u16).clamp(2, inner.height.saturating_sub(head_h + 3).max(2));
    let table_h = inner.height.saturating_sub(head_h + 1 + detail_h);
    let widths = fit(&GPU_COLS, inner.width);
    put(
        frame,
        Paragraph::new(header(
            &GPU_COLS,
            &widths,
            Some((st.gpu_table.sort.index(), desc)),
        )),
        Rect::new(inner.x, table_y, inner.width, 1),
        inner,
    );
    let body_h = usize::from(table_h.saturating_sub(1));
    let (sel, offset) = st.gpu_table.cursor.resolve(&pids, body_h);
    st.hits.list = Some(super::ListHit {
        header_y: table_y,
        cols: col_ranges(inner.x, &widths),
        body: Rect::new(inner.x, table_y + 1, inner.width, body_h as u16),
        offset,
        len: rows.len(),
    });
    let procs: HashMap<u32, &Proc> = match &st.procs {
        Some(Ok(l)) => l.iter().map(|p| (p.pid, p)).collect(),
        _ => HashMap::new(),
    };
    let e = |v: Option<f64>| v.map_or_else(|| dim("-".into()), pct_cell);
    for (i, p) in rows.iter().enumerate().skip(offset).take(body_h) {
        let name_style = if Some(p.pid) == game {
            Style::new().fg(th().game).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(th().text)
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
                e(engine(p, is_encode)),
                e(engine(p, is_decode)),
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
            // Fundo e negrito: a seleção não depende só da cor (NO_COLOR, temas claros).
            line = line.style(Style::new().bg(th().selected).add_modifier(Modifier::BOLD));
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
            Style::new().fg(th().border),
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
        let mut l1 = vec![
            raw_text(format!("{} ({})", printable(&p.name), p.pid)).add_modifier(Modifier::BOLD),
        ];
        l1.push(label("  engines"));
        match &p.engines {
            Ok(list) if list.iter().any(|(_, v)| *v >= 0.05) => {
                for (i, (k, v)) in list.iter().filter(|(_, v)| *v >= 0.05).enumerate() {
                    let sepr = if i == 0 { " " } else { " · " };
                    l1.push(raw_text(format!("{sepr}{} {}%", printable(k), num(*v, 1))));
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
            Some(RuntimeStatus::Reading) => l2.push(Span::styled(
                tr("reading modules...").into_owned(),
                Style::new().fg(th().neutral),
            )),
            Some(RuntimeStatus::Ready(Ok(r))) if r.is_empty() => {
                l2.push(label("none of the known GPU runtimes"))
            }
            Some(RuntimeStatus::Ready(Ok(r))) => l2.push(text(r.join(", "))),
            Some(RuntimeStatus::Ready(Err(e))) => l2.push(missing(e)),
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
