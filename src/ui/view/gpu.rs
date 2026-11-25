//! Caixa da GPU: engine dominante, grade de engines, termômetro vertical, leituras da placa e
//! seção de VRAM (por categoria, compartilhada e física do NVML) separada por divisor.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::state::{MinMax, Series, Session, State};
use super::{
    TEMP_BAR_W, aligned_bar_text_w, bar_line, bar_sep, bar_value, boxed, brief, draw_temp_bar,
    empty_box, faded, failed_span, label, line, metric, metric_styled, minmax, missing, put,
    section_divider, sep, short_bytes, temp_color, text, text_room, usage_color, v_divider,
    with_failure, wrap,
};
use crate::fmt::{bytes, num};
use crate::i18n::tr;
use crate::model::{Gpu, Metric, Missing, VramBreakdown, VramKind};
use crate::ui::theme::th;
use crate::ui::widgets::{Graph, GraphColor, Segmented};
use crate::win::printable;
// --- GPU ---

/// Engine mais usada (NVML): 3D/compute, encoder ou decoder, como o Task Manager mostra o
/// máximo dos engines. O Copy não existe no NVML.
pub(super) fn busiest_engine(g: &Gpu) -> (&'static str, u32) {
    // Só engines com valor: uso geral em falha nunca vira um "0% 3D" falso (a falha aparece
    // na grade e nas causas da caixa).
    let mut best: Option<(&'static str, u32)> = g.usage.as_ref().ok().map(|v| (usage_name(g), *v));
    for (name, m) in [("encode", &g.encoder), ("decode", &g.decoder)] {
        let Ok(v) = m else { continue };
        // Sem NVML o uso geral já é o máximo de todas as engines: no empate, a de vídeo é a
        // mais usada (com NVML a regra fica a mesma).
        best = match best {
            Some((_, b)) if *v > b || (*v > 0 && *v == b) => Some((name, *v)),
            None => Some((name, *v)),
            keep => keep,
        };
    }
    best.unwrap_or((usage_name(g), 0))
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

/// Largura padrão do painel de engines quando não há grade por núcleo da CPU de referência.
const PANEL_W: u16 = 30;

fn gpu_causes(g: &Gpu, width: u16) -> Vec<Line<'static>> {
    let mut causes: Vec<Line<'static>> = Vec::new();
    let mut engines = vec![
        (usage_name(g), &g.usage),
        ("encode", &g.encoder),
        ("decode", &g.decoder),
    ];
    // Sem NVML, a causa da memória já está na linha do NVML (e ela nem entra na grade).
    if g.no_nvml.is_none() {
        engines.push(("mem act", &g.mem_activity));
    }
    for (name, m) in engines {
        if let Err(e @ Missing::Fail(_)) = m {
            causes.extend(wrap(
                vec![label(&format!("{name} ")), missing(e)],
                width,
                0,
                2,
            ));
        }
    }
    if let Err(e @ Missing::Fail(_)) = &g.other_engine {
        causes.extend(wrap(vec![label("other engine "), missing(e)], width, 0, 2));
    }
    // `drv` e `VBIOS` não ocupam linha na aba Geral (ficam em Sensores), mas falhas aparecem.
    if let Err(e @ Missing::Fail(_)) = &g.driver {
        causes.extend(wrap(vec![label("drv "), missing(e)], width, 0, 2));
    }
    if g.no_nvml.is_none()
        && let Err(e @ Missing::Fail(_)) = &g.vbios
    {
        causes.extend(wrap(vec![label("VBIOS "), missing(e)], width, 0, 2));
    }
    causes
}

/// `(n_cells, avail_top)` da caixa da GPU para ajustar `top_h` na aba Geral e manter todos os
/// gráficos de engines com altura idêntica sem linhas vazias.
fn gpu_layout_metrics(st: &State, g: &Gpu, inner_w: u16, inner_h: u16) -> (u16, u16) {
    let (engine, _) = busiest_engine(g);
    let details = gpu_details(st, g, inner_w);
    let gb = |v: f64| bytes(v as u64);
    let session_len = if st.session.vram.get().is_some() {
        wrap(
            vec![label("session VRAM "), minmax(st.session.vram, gb)],
            inner_w,
            0,
            8,
        )
        .len() as u16
    } else {
        0
    };
    let card_h = (details.card.len() + gpu_causes(g, inner_w).len()) as u16;
    let vram_base_h = (2 + details.legend.len() + details.phys.len()) as u16;
    let vram_extra_h = if inner_h >= vram_base_h + card_h + 1 + session_len + 4 {
        details.phys.len() as u16 + session_len
    } else {
        details.phys.len() as u16
    };
    let vram_h = 2 + details.legend.len() as u16 + vram_extra_h;
    let div_h = u16::from(inner_h >= vram_h + card_h + 3);
    let n_cells = engine_cells(g, engine).len() as u16;
    let avail_top = inner_h.saturating_sub(vram_h + card_h + div_h);
    (n_cells, avail_top)
}

/// Ajusta `top_h` em no máximo ±3 linhas (dentro de `min_top..=max_top`) para que a altura da
/// área de gráficos da GPU divida exato pelo número de engines — eliminando qualquer linha em
/// branco acima de `clk` ou `── vram ──` e mantendo todas as engines com a mesma altura.
pub(super) fn snap_top_height(
    st: &State,
    gpu_w: u16,
    top_h: u16,
    min_top: u16,
    max_top: u16,
) -> u16 {
    let Some(Ok(list)) = st.fast.as_ref().map(|f| &f.gpus) else {
        return top_h;
    };
    let Some(g) = list.get(st.gpu_index(list.len())) else {
        return top_h;
    };
    let inner_w = gpu_w.saturating_sub(2);
    let (n_cells, avail_top) = gpu_layout_metrics(st, g, inner_w, top_h.saturating_sub(2));
    if n_cells < 2 || avail_top < n_cells * 2 || avail_top.is_multiple_of(n_cells) {
        return top_h;
    }
    // Prefere o menor ajuste (favorecendo crescer os gráficos em empate).
    for delta in [1i16, -1, 2, -2, 3, -3] {
        let Ok(cand) = u16::try_from(i16::try_from(top_h).unwrap_or(0) + delta) else {
            continue;
        };
        if cand < min_top || cand > max_top {
            continue;
        }
        let (n, avail) = gpu_layout_metrics(st, g, inner_w, cand.saturating_sub(2));
        if n >= 2 && avail >= n * 2 && avail.is_multiple_of(n) {
            return cand;
        }
    }
    top_h
}

/// Caixa da GPU. `core_graph_w`: largura do gráfico de cada célula por núcleo da CPU, para os
/// mini gráficos de copy/encode/decode/mem act terem a mesma largura (e altura idêntica entre si).
pub(super) fn draw_gpu(frame: &mut Frame<'_>, area: Rect, st: &State, core_graph_w: Option<u16>) {
    let gpus = st.fast.as_ref().map(|f| &f.gpus);
    let Some(Ok(list)) = gpus else {
        empty_box(frame, area, "gpu", gpus);
        return;
    };
    let sel = st.gpu_index(list.len());
    let Some(g) = list.get(sel) else {
        frame.render_widget(
            Paragraph::new(missing(&Missing::Fail("no GPU found".into()))).block(boxed("gpu", "")),
            area,
        );
        return;
    };
    let (engine, engine_v) = busiest_engine(g);
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
    let mut details = gpu_details(st, g, inner.width);
    // Extremos da sessão como na caixa da CPU: a VRAM numa linha ("session RAM" da memória);
    // temperatura, potência e clock com o máximo ao lado do valor (`nvml_rows`).
    let gb = |v: f64| bytes(v as u64);
    // Sem extremos ainda (troca de GPU, `r`, VRAM sem valor), a linha não aparece vazia.
    let session = if st.session.vram.get().is_some() {
        wrap(
            vec![label("session VRAM "), minmax(st.session.vram, gb)],
            inner.width,
            0,
            8,
        )
    } else {
        Vec::new()
    };
    // Falhas das engines e de drv/VBIOS inteiras na largura da caixa; a grade só leva marca curta.
    let causes = gpu_causes(g, inner.width);
    // Sem altura para tudo, os extremos da VRAM saem primeiro (continuam na aba Sensores): as
    // leituras ao vivo têm prioridade.
    details.card.splice(0..0, causes);
    let card_h = details.card.len() as u16;
    let vram_base_h = (2 + details.legend.len() + details.phys.len()) as u16;
    let mut vram_extra = details.phys;
    if inner.height >= vram_base_h + card_h + 1 + session.len() as u16 + 4 {
        vram_extra.extend(session);
    }
    let vram_h = (2 + details.legend.len() + vram_extra.len()) as u16;
    let div_h = u16::from(inner.height >= vram_h + card_h + 3);
    // Engines ao lado do gráfico, como a grade por lógico da CPU: o 3D (ou, com ele no gráfico
    // principal, a engine mais usada fora ele e o vídeo), encoder, decoder e a memória.
    let cells = engine_cells(g, engine);
    let n_cells = cells.len() as u16;
    // Os detalhes (métricas) têm prioridade sobre a altura do gráfico; sem linhas em branco
    // acima de `clk` ou `── vram ──` (todo o espaço vai para os gráficos).
    let avail_top = inner.height.saturating_sub(vram_h + card_h + div_h);
    let graph_min = n_cells.max(4).min(avail_top.max(2));
    let [top, card_area, div_area, vram_area] = Layout::vertical([
        Constraint::Min(graph_min),
        Constraint::Length(card_h),
        Constraint::Length(div_h),
        Constraint::Length(vram_h),
    ])
    .areas(inner);
    // Termômetro vertical à direita quando há espaço e leitura de temperatura (NVML).
    let gpu_temp = if g.no_nvml.is_none() {
        g.temp.as_ref().ok().copied()
    } else {
        None
    };
    // Largura do painel de engines = largura do gráfico por núcleo da CPU, ou `PANEL_W` quando a
    // CPU está em lista compacta / GPU ampliada.
    let target_panel_w = core_graph_w.map_or(PANEL_W, |w| w.max(12));
    let show_temp = gpu_temp.is_some() && top.width >= target_panel_w + 20 && top.height >= 4;
    let (top_main, temp_area) = if show_temp {
        let [left, t] = Layout::horizontal([
            Constraint::Min(target_panel_w + 14),
            Constraint::Length(TEMP_BAR_W),
        ])
        .areas(top);
        (left, Some(t))
    } else {
        (top, None)
    };
    if let (Some(t_val), Some(t_area)) = (gpu_temp, temp_area) {
        let limit = f64::from(g.temp_slowdown.as_ref().copied().unwrap_or(95));
        draw_temp_bar(frame, t_area, f64::from(t_val), limit);
    }
    let panel_w = target_panel_w.min(top_main.width.saturating_sub(12));
    let [graph, sep_col, panel] = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Length(2),
        Constraint::Length(panel_w),
    ])
    .areas(top_main);
    v_divider(frame, sep_col.x, sep_col.y, sep_col.height, top_main);

    // Gráfico da engine dominante: valor colorido pela carga + nome + min/max na mesma linha
    // quando couber (libera +1 linha inteira para o gráfico).
    let mut head = if [&g.usage, &g.encoder, &g.decoder].iter().any(|m| m.is_ok()) {
        vec![
            Span::styled(
                format!("{engine_v}%"),
                Style::new()
                    .fg(usage_color(f64::from(engine_v)))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(engine.to_string(), Style::new().fg(engine_color(engine))),
        ]
    } else {
        // Causa inteira nas linhas de baixo (`causes`).
        vec![brief(&g.usage, |v| format!("{v}%"))]
    };
    let mm = minmax(st.session.gpu, |v| format!("{}%", num(v, 0)));
    let head_w: usize = head.iter().map(Span::width).sum();
    let mm_w = mm.width();
    let graph_top = if mm_w == 0 {
        1
    } else if head_w + 2 + mm_w <= usize::from(graph.width) {
        head.extend([sep(), mm]);
        1
    } else if head_w + 1 + mm_w <= usize::from(graph.width) {
        head.extend([Span::raw(" "), mm]);
        1
    } else if graph.height >= 3 {
        line(frame, vec![mm], graph.x, graph.y + 1, graph.width, graph);
        2
    } else {
        1
    };
    line(frame, head, graph.x, graph.y, graph.width, graph);
    let gr = Rect::new(
        graph.x,
        graph.y + graph_top,
        graph.width.saturating_sub(1),
        graph.height.saturating_sub(graph_top),
    );
    frame.render_widget(
        Graph::new(
            st.gpu.get(sel).map_or(&[][..], Series::values),
            GraphColor::Height,
        )
        .with_baseline(th().dim),
        gr.intersection(graph),
    );
    // Folga de 1 coluna antes do divisor do termômetro (espelhando a grade da CPU).
    let panel_draw = if show_temp {
        Rect::new(
            panel.x,
            panel.y,
            panel.width.saturating_sub(1),
            panel.height,
        )
    } else {
        panel
    };
    draw_engine_cells(frame, panel_draw, &cells, st.gpu_video.get(sel));

    // Leituras da placa (clocks, temp, potência, ventoinhas, PCIe/driver) acima do divisor.
    put(frame, Paragraph::new(details.card), card_area, inner);
    if div_h > 0 {
        section_divider(frame, div_area, inner, "vram");
    }

    // VRAM por categoria, compartilhada, física (NVML) e extremos da sessão abaixo do divisor.
    let slow = st
        .slow
        .as_ref()
        .and_then(|s| s.gpus.as_ref().ok())
        .and_then(|l| l.iter().find(|x| x.index == g.index));
    let b = vram_area;
    let min_text_w = aligned_bar_text_w(&[&g.dedicated, &g.shared]);
    match slow.map(|s| &s.vram) {
        Some(Ok(v)) => vram_bar(frame, b, v, details.game_pid, &g.dedicated, min_text_w),
        _ => bar_line(frame, b, 0, "VRAM  ", &g.dedicated, min_text_w),
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
    bar_line(frame, b, 1 + n, "shared", &g.shared, min_text_w);
    for (i, l) in vram_extra.into_iter().enumerate() {
        put(
            frame,
            Paragraph::new(l),
            Rect::new(b.x, b.y + 2 + n + i as u16, b.width, 1),
            b,
        );
    }
}

/// Uma engine da grade ao lado do gráfico da GPU: nome, valor e a série em `gpu_video`.
struct EngineCell {
    name: String,
    value: Metric<u32>,
    series: usize,
}

/// Séries de `gpu_video`: encoder, decoder, memória, uso geral (3D) e a outra engine.
const SERIES_3D: usize = 3;
const SERIES_OTHER: usize = 4;

/// Nome curto de um tipo de engine do PDH ("Compute_0" → "compute 0").
pub(super) fn engine_label(t: &str) -> String {
    printable(&t.replace('_', " ").to_lowercase())
}

/// Engines da grade: o 3D, ou a outra engine quando o 3D já é o gráfico principal; depois
/// encoder, decoder e (com NVML) a atividade da memória.
fn engine_cells(g: &Gpu, main: &str) -> Vec<EngineCell> {
    // O uso geral fica na grade quando não é o gráfico principal (vídeo dominante, ou ele em
    // falha); senão, a outra engine.
    let first = if matches!(main, "encode" | "decode") || g.usage.is_err() {
        EngineCell {
            name: usage_name(g).into(),
            value: g.usage.clone(),
            series: SERIES_3D,
        }
    } else {
        EngineCell {
            name: g
                .other_engine
                .as_ref()
                .map_or_else(|_| "other".into(), |(t, _)| engine_label(t)),
            value: g
                .other_engine
                .as_ref()
                .map(|(_, v)| *v)
                .map_err(Clone::clone),
            series: SERIES_OTHER,
        }
    };
    let mut cells = vec![
        first,
        EngineCell {
            name: "encode".into(),
            value: g.encoder.clone(),
            series: 0,
        },
        EngineCell {
            name: "decode".into(),
            value: g.decoder.clone(),
            series: 1,
        },
    ];
    // Sem NVML a atividade da memória nunca tem valor (o motivo está na linha do NVML).
    if g.no_nvml.is_none() {
        cells.push(EngineCell {
            name: "mem act".into(),
            value: g.mem_activity.clone(),
            series: 2,
        });
    }
    cells
}

/// Grade das engines (uma coluna), no estilo da grade por lógico da CPU: rótulo e valor na 1ª
/// linha de cada célula e o gráfico com o gradiente embaixo, sempre com a mesma altura entre si
/// (sem distorcer uma engine maior que outra). Sem altura para gráfico, uma lista compacta.
fn draw_engine_cells(
    frame: &mut Frame<'_>,
    area: Rect,
    cells: &[EngineCell],
    series: Option<&[Series; 5]>,
) {
    let n = cells.len() as u16;
    if n == 0 || area.width == 0 || area.height == 0 {
        return;
    }
    let name_span = |c: &EngineCell, txt: String| -> Span<'static> {
        let color = match c.name.as_str() {
            "encode" => th().encode,
            "decode" => th().decode,
            _ => th().label,
        };
        Span::styled(tr(&txt).into_owned(), Style::new().fg(color))
    };
    let head_w = |c: &EngineCell, w: usize| -> Vec<Span<'static>> {
        match &c.value {
            Ok(x) => {
                let s = format!("{x}%");
                let max_name = w.saturating_sub(s.len() + 1);
                let name: String = c.name.chars().take(max_name).collect();
                let pad = w.saturating_sub(name.chars().count() + s.len()).max(1);
                vec![
                    name_span(c, format!("{name}{:pad$}", "")),
                    faded(text(s), *x == 0),
                ]
            }
            // A causa inteira de uma falha está nas linhas de baixo da caixa.
            Err(Missing::Fail(_)) => {
                let f = failed_span();
                let fw = f.width();
                let max_name = w.saturating_sub(fw + 1);
                let name: String = c.name.chars().take(max_name).collect();
                let pad = w.saturating_sub(name.chars().count() + fw).max(1);
                vec![name_span(c, format!("{name}{:pad$}", "")), f]
            }
            Err(e) => vec![name_span(c, format!("{:<8} ", c.name)), missing(e)],
        }
    };
    let head_compact = |c: &EngineCell| -> Vec<Span<'static>> { head_w(c, 13) };
    let ch = area.height / n;
    if ch < 2 {
        // Lista compacta: uma engine por linha (a causa de uma falha quebra em mais linhas,
        // inteira); sem altura para isso, quebrando só entre uma engine e outra.
        let mut lines: Vec<Line<'static>> = cells
            .iter()
            .flat_map(|c| wrap(head_compact(c), area.width, 0, 8))
            .collect();
        if lines.len() > usize::from(area.height) {
            let spans: Vec<Span<'static>> = cells
                .iter()
                .enumerate()
                .flat_map(|(i, c)| {
                    let mut h = head_compact(c);
                    if i > 0 {
                        h.insert(0, Span::raw("  "));
                    }
                    h
                })
                .collect();
            lines = wrap(spans, area.width, 0, 0);
        }
        put(frame, Paragraph::new(lines), area, area);
        return;
    }
    // Todas as células com exatamente a mesma altura `ch` e o mesmo passo vertical. Quando a
    // altura é `n * ch + (n - 1)` (ex.: zoom), usa 1 linha uniforme entre células.
    let (ch, step) = if area.height >= n * 2 + (n - 1) && (area.height - (n - 1)).is_multiple_of(n)
    {
        let c = (area.height - (n - 1)) / n;
        (c, c + 1)
    } else {
        (ch, ch)
    };
    let cell_w = usize::from(area.width);
    for (k, c) in cells.iter().enumerate() {
        let y = area.y + k as u16 * step;
        let cell = Rect::new(area.x, y, area.width, ch).intersection(area);
        if let Err(e @ Missing::Neutral(_)) = &c.value {
            // Estado neutro: rótulo + "-" na 1ª linha e motivo quebrado em largura cheia abaixo
            // (evita coluna estreita de 2 letras quando o painel herda a largura de um núcleo).
            let max_name = cell_w.saturating_sub(2);
            let name: String = c.name.chars().take(max_name).collect();
            let pad = cell_w.saturating_sub(name.chars().count() + 1).max(1);
            line(
                frame,
                vec![
                    name_span(c, format!("{name}{:pad$}", "")),
                    Span::styled("-", Style::new().fg(th().neutral)),
                ],
                cell.x,
                cell.y,
                cell.width,
                cell,
            );
            if cell.height > 1 {
                let body = Rect::new(cell.x, cell.y + 1, cell.width, cell.height - 1);
                let lines = wrap(vec![missing(e)], body.width, 0, 0);
                put(frame, Paragraph::new(lines), body, cell);
            }
            continue;
        }
        if c.value.is_err() {
            line(frame, head_w(c, cell_w), cell.x, cell.y, cell.width, cell);
            continue;
        }
        line(frame, head_w(c, cell_w), cell.x, cell.y, cell.width, cell);
        if let Some(s) = series.and_then(|s| s.get(c.series)) {
            let g = Rect::new(
                cell.x,
                cell.y + 1,
                cell.width,
                cell.height.saturating_sub(1),
            );
            frame.render_widget(
                Graph::new(s.values(), GraphColor::Height).with_baseline(th().dim),
                g.intersection(cell),
            );
        }
    }
}

/// Linhas de detalhe da caixa da GPU, separadas entre placa (acima do divisor) e VRAM (abaixo),
/// já quebradas na largura (nada é cortado).
struct GpuDetails {
    game_pid: Option<u32>,
    card: Vec<Line<'static>>,
    legend: Vec<Line<'static>>,
    phys: Vec<Line<'static>>,
}

fn gpu_details(st: &State, g: &Gpu, width: u16) -> GpuDetails {
    let slow = st
        .slow
        .as_ref()
        .and_then(|s| s.gpus.as_ref().ok())
        .and_then(|l| l.iter().find(|x| x.index == g.index));
    let slow_err = match st.slow.as_ref().map(|s| &s.gpus) {
        Some(Err(e)) => Some(e),
        _ => None,
    };
    let game_pid = st
        .fps
        .as_ref()
        .and_then(|f| f.game.as_ref().ok())
        .map(|g| g.pid);
    let legend = vram_legend(slow.map(|s| &s.vram), game_pid, width);
    // GPU sem NVML: os campos só dele numa linha, com o motivo (não repetido em cada um).
    let (card_rows, phys_row): (Vec<Vec<Span<'static>>>, Option<Vec<Span<'static>>>) =
        if let Some(m) = &g.no_nvml {
            (
                vec![vec![
                    label("NVML   "),
                    missing(m),
                    label(" (clocks, temp, power, fans, PCIe, P-state)"),
                ]],
                None,
            )
        } else {
            nvml_rows(g, slow, slow_err, &st.session)
        };
    let card = card_rows
        .into_iter()
        .flat_map(|r| wrap(r, width, 0, 7))
        .collect();
    let phys = phys_row
        .into_iter()
        .flat_map(|r| wrap(r, width, 0, 7))
        .collect();
    GpuDetails {
        game_pid,
        card,
        legend,
        phys,
    }
}

/// Linhas de detalhe de uma GPU do NVML (aba Geral): `(placa, phys_vram)`.
fn nvml_rows(
    g: &Gpu,
    slow: Option<&crate::model::GpuSlow>,
    slow_err: Option<&Missing>,
    session: &Session,
) -> (Vec<Vec<Span<'static>>>, Option<Vec<Span<'static>>>) {
    // Máximo da sessão ao lado do valor, como o Tctl e a potência da CPU.
    let max = |m: MinMax| -> Span<'static> {
        m.get().map_or_else(
            || Span::raw(""),
            |(_, hi)| label(&format!(" (max {})", num(hi, 0))),
        )
    };
    // Máximo 0 = o NVML não informa (GPUs móveis): só o atual.
    let clk = |m: &Metric<(u32, u32)>| {
        metric_styled(m, th().accent, |(c, mx)| {
            if *mx > 0 {
                format!("{c}/{mx}")
            } else {
                c.to_string()
            }
        })
    };
    let slowdown = f64::from(g.temp_slowdown.as_ref().copied().unwrap_or(95));
    let phys = match &g.physical {
        Ok([u, f, r, t]) => vec![
            // O total físico é o mesmo da barra da VRAM: só aparece se diferir.
            label("phys   "),
            text(format!("{} ", bytes(*u))),
            label("used"),
            sep(),
            text(format!("{} ", bytes(*f))),
            label("free"),
            sep(),
            text(format!("{} ", bytes(*r))),
            label("reserved"),
            label(&if g.dedicated.as_ref().is_ok_and(|(_, tot)| tot == t) {
                " (NVML)".to_string()
            } else {
                format!("  {} total (NVML)", bytes(*t))
            }),
        ],
        Err(e) => vec![label("phys   "), missing(e)],
    };
    let throttle_span = match &g.throttle {
        Ok(v) if v.is_empty() => Span::styled(tr("none").into_owned(), Style::new().fg(th().ok)),
        Ok(v) => Span::styled(printable(&tr(&v.join(", "))), Style::new().fg(th().warn)),
        Err(e) => missing(e),
    };
    let card = vec![
        vec![
            label("clk    gfx "),
            clk(&g.clock_graphics),
            max(session.gfx_clock),
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
            metric_styled(
                &g.temp,
                g.temp
                    .as_ref()
                    .map_or(th().text, |v| temp_color(f64::from(*v), slowdown)),
                |v| format!("{v} °C"),
            ),
            max(session.gpu_temp),
            Span::raw(" "),
            metric_styled(&g.temp_slowdown, th().label, |v| {
                format!("(slowdown {v} °C)")
            }),
            sep(),
            label("power "),
            metric_styled(&g.power, th().accent, |(p, l)| {
                if *l > 0.0 {
                    format!("{}/{} W", num(*p, 0), num(*l, 0))
                } else {
                    format!("{} W", num(*p, 0))
                }
            }),
            max(session.gpu_power),
            sep(),
            metric_styled(&g.pstate, th().title, |v| format!("P{v}")),
            sep(),
            label("throttle "),
            throttle_span,
        ],
        {
            let mut v = vec![label("fans   ")];
            match &g.fans {
                Ok(list) => {
                    for (i, f) in list.iter().enumerate() {
                        if i > 0 {
                            v.push(sep());
                        }
                        v.extend([
                            label(&format!("fan{i} ")),
                            faded(metric(&f.percent, |p| format!("{p}%")), f.percent == Ok(0)),
                            Span::raw(" "),
                            metric(&f.rpm, |r| format!("{r} rpm")),
                        ]);
                    }
                }
                Err(e) => v.push(missing(e)),
            }
            v.extend([
                bar_sep(),
                label("PCIe "),
                metric_styled(&g.pcie_link, th().accent, |(gen_, w)| {
                    format!("Gen{gen_} x{w}")
                }),
            ]);
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
            v
        },
    ];
    (card, Some(phys))
}

/// Legenda da barra da VRAM por categoria (aba Geral e aba GPU), quebrada em `width`.
pub(in crate::ui) fn vram_legend(
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
        VramKind::Free => th().neutral,
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
pub(super) fn vram_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    b: &VramBreakdown,
    game_pid: Option<u32>,
    dedicated: &Metric<(u64, u64)>,
    min_text_w: u16,
) {
    if area.height == 0 {
        return;
    }
    let parts = vram_parts(b, game_pid);
    let (val_line, raw_w) = bar_value(dedicated);
    let label_w = VRAM_LABEL_W.min(area.width);
    let target_w = if dedicated.is_err() {
        raw_w
    } else {
        raw_w.max(min_text_w)
    };
    let text_w = text_room(target_w, dedicated.is_err(), area.width, label_w);
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
    let p = if dedicated.is_err() {
        Paragraph::new(val_line)
    } else {
        Paragraph::new(val_line).right_aligned()
    };
    put(
        frame,
        p,
        Rect::new(area.x + label_w + bar_w, area.y, text_w, 1),
        area,
    );
}
