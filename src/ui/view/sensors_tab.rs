//! Aba Sensores: todas as leituras físicas (temperaturas, potência, clocks, fans, SMART) em
//! colunas de jornal com mínimo e máximo da sessão.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::table::{Col, cell, col, dim, fit, flex, header, note, row};
use super::{State, boxed, label, metric, metric_styled, missing, put, raw_text, temp_color};
use crate::fmt::num;
use crate::i18n::tr;
use crate::model::{Metric, Missing};
use crate::ui::theme::th;
use crate::win::printable;

pub(in crate::ui) enum SensorValue {
    /// Valor, unidade, casas decimais e limite térmico opcional (para cor semântica de temp).
    Num(Metric<f64>, &'static str, usize, Option<f64>),
    Text(Metric<String>),
}

pub(in crate::ui) struct SensorRow {
    pub group: String,
    pub name: String,
    pub value: SensorValue,
}

impl SensorRow {
    /// Chave dos extremos da sessão (só sensores numéricos).
    pub(in crate::ui) fn key(&self) -> Option<String> {
        matches!(self.value, SensorValue::Num(..)).then(|| format!("{}/{}", self.group, self.name))
    }
}

fn num_row(group: &str, name: &str, m: Metric<f64>, unit: &'static str, dec: usize) -> SensorRow {
    SensorRow {
        group: group.into(),
        name: name.into(),
        value: SensorValue::Num(m, unit, dec, None),
    }
}

fn temp_row(group: &str, name: &str, m: Metric<f64>, dec: usize, limit: f64) -> SensorRow {
    SensorRow {
        group: group.into(),
        name: name.into(),
        value: SensorValue::Num(m, "°C", dec, Some(limit)),
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
pub(in crate::ui) fn sensor_rows(st: &State) -> Vec<SensorRow> {
    let mut out = Vec::new();
    let Some(f) = &st.fast else {
        return out;
    };
    let cpu = "CPU";
    match &f.cpu_sensors {
        Ok(s) => {
            out.push(temp_row(cpu, "Tctl", s.tctl.clone(), 1, 95.0));
            match &s.ccds {
                Ok(list) => {
                    for (n, t) in list {
                        out.push(temp_row(cpu, &format!("CCD{n}"), Ok(*t), 1, 95.0));
                    }
                }
                Err(e) => out.push(temp_row(cpu, "CCD", Err(e.clone()), 1, 95.0)),
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
                // Sem NVML: os sensores dele numa linha, com o motivo.
                if let Some(m) = &g.no_nvml {
                    out.push(text_row(&grp, "NVML sensors", Err(m.clone())));
                    out.push(text_row(&grp, "Name", g.name.clone()));
                    out.push(text_row(&grp, "Driver", g.driver.clone()));
                    continue;
                }
                let u = |m: &Metric<u32>| m.clone().map(f64::from);
                let first = |m: &Metric<(u32, u32)>| m.clone().map(|(a, _)| f64::from(a));
                let slowdown = f64::from(g.temp_slowdown.as_ref().copied().unwrap_or(95));
                out.push(temp_row(&grp, "Temperature", u(&g.temp), 0, slowdown));
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
                    g.power.clone().and_then(|(_, l)| {
                        if l > 0.0 {
                            Ok(l)
                        } else {
                            crate::model::neutral("not reported by the driver")
                        }
                    }),
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
                // Tráfego PCIe (coleta lenta) associado pelo `g.index` (não pela posição no vetor).
                match st.slow.as_ref().map(|s| &s.gpus) {
                    Some(Ok(slow)) => {
                        if let Some(s) = slow.iter().find(|x| x.index == g.index) {
                            let mb = |m: &Metric<u64>| m.clone().map(|v| v as f64 / 1e6);
                            out.push(num_row(&grp, "PCIe TX", mb(&s.pcie_tx), "MB/s", 1));
                            out.push(num_row(&grp, "PCIe RX", mb(&s.pcie_rx), "MB/s", 1));
                        }
                    }
                    Some(Err(e)) => out.push(text_row(&grp, "PCIe TX/RX", Err(e.clone()))),
                    None => {}
                }
                out.push(text_row(&grp, "Name", g.name.clone()));
                out.push(text_row(&grp, "Driver", g.driver.clone()));
                out.push(text_row(&grp, "VBIOS", g.vbios.clone()));
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
                // Só exibe temperatura e SMART do disco certo (o modelo confere com o da coleta rápida).
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
                let changed = t.model.is_some() && model.is_some() && t.model != model;
                let value = if changed {
                    crate::model::neutral("disk changed: waiting for a new reading")
                } else {
                    t.celsius.clone().map(f64::from)
                };
                let health = if changed {
                    crate::model::neutral("disk changed: waiting for a new reading")
                } else {
                    t.health.clone().map(|p| {
                        if p {
                            "FAILURE PREDICTED".to_string()
                        } else {
                            "OK".to_string()
                        }
                    })
                };
                out.push(temp_row(disks, &name, value, 0, 70.0));
                out.push(text_row(
                    disks,
                    // Recuada sob o disco da linha de cima (sem repetir o nome inteiro).
                    "  SMART", health,
                ));
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
    // Rolagem (↑↓, PgUp/PgDn, rodinha) só quando nem com colunas tudo cabe.
    let all = sensor_rows(st);
    let offset = st.sensor_offset.min(all.len().saturating_sub(1));
    st.sensor_offset = offset;
    let rows: Vec<SensorRow> = all.into_iter().skip(offset).collect();
    let title = if offset > 0 {
        format!("from row {} · session min · max (r resets)", offset + 1)
    } else {
        "session min · max (r resets)".to_string()
    };
    let block = boxed("sensors", &title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if rows.is_empty() {
        note(
            frame,
            inner,
            Span::styled(
                tr("waiting for the first collection").into_owned(),
                Style::new().fg(th().neutral),
            ),
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
    let geometry = |ncols: usize| {
        let cw = (inner.width.saturating_sub(gap * (ncols as u16 - 1))) / ncols as u16;
        (cw, fit(&SENSOR_COLS, cw))
    };
    // Pré-calcula as linhas de cada sensor uma única vez por largura de coluna (evita recalcular
    // durante a distribuição e depois no desenho de cada célula).
    let mut ncols = 1;
    let (mut cw, mut widths, mut rendered, mut layout);
    loop {
        (cw, widths) = geometry(ncols);
        rendered = rows
            .iter()
            .map(|r| sensor_lines(r, cw, &widths, st))
            .collect::<Vec<_>>();
        layout = sensor_layout(&rows, h, split, |i| rendered[i].len().saturating_sub(1));
        let want = layout.len().clamp(1, fits);
        if want <= ncols {
            break;
        }
        ncols = want;
    }
    // Não cabem nem com as colunas mais estreitas: a última linha avisa quantos ficaram fora.
    let sensor_of = |it: &SensorItem| match it {
        SensorItem::Row(i) | SensorItem::Cause(i, _) => Some(*i),
        _ => None,
    };
    let beyond: std::collections::BTreeSet<usize> = layout
        .iter()
        .skip(ncols)
        .flatten()
        .filter_map(sensor_of)
        .collect();
    st.sensor_overflow = !beyond.is_empty();
    st.sensor_beyond = beyond.len();
    if !beyond.is_empty()
        && let Some(last) = layout.get_mut(ncols - 1)
    {
        // A linha do aviso sai da última coluna; o corte recua até o fim de um sensor inteiro
        // (nem sensor pela metade, nem título solto antes do aviso).
        let mut keep = h.saturating_sub(1).min(last.len());
        let cut_sensor = |k: usize| last.get(k).and_then(sensor_of);
        while keep > 0
            && (cut_sensor(keep).is_some() && cut_sensor(keep) == cut_sensor(keep - 1)
                || matches!(last[keep - 1], SensorItem::Title(..) | SensorItem::Blank))
        {
            keep -= 1;
        }
        let mut hidden = beyond;
        hidden.extend(last[keep..].iter().filter_map(sensor_of));
        last.truncate(keep);
        last.push(SensorItem::More(hidden.len()));
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
            match item {
                SensorItem::Blank => {}
                SensorItem::More(n) => {
                    note(frame, at, label(&format!("+{n} more (taller terminal)")));
                }
                SensorItem::Title(i, cont) => {
                    let group = printable(&tr(&rows[*i].group));
                    let t = if *cont {
                        format!("{group} (cont.)")
                    } else {
                        group
                    };
                    put(
                        frame,
                        Paragraph::new(Span::styled(
                            t,
                            Style::new().fg(th().accent).add_modifier(Modifier::BOLD),
                        )),
                        at,
                        inner,
                    );
                }
                SensorItem::Cause(i, k) => {
                    if let Some(l) = rendered[*i].get(*k + 1) {
                        put(frame, Paragraph::new(l.clone()), at, inner);
                    }
                }
                SensorItem::Row(i) => {
                    if let Some(l) = rendered[*i].first() {
                        put(frame, Paragraph::new(l.clone()), at, inner);
                    }
                }
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
        SensorValue::Num(m, unit, dec, limit) => {
            let color = match (m, limit) {
                (Ok(v), Some(lim)) => temp_color(*v, *lim),
                _ => th().text,
            };
            metric_styled(m, color, |v| format!("{} {unit}", num(*v, *dec)))
        }
        SensorValue::Text(m) => match m {
            Ok(v) if matches!(r.name.as_str(), "Name" | "Driver" | "VBIOS") => {
                raw_text(printable(v))
            }
            Ok(v) if r.name.trim() == "SMART" && v == "OK" => {
                Span::styled(v.clone(), Style::new().fg(th().ok))
            }
            Ok(v) if r.name.trim() == "SMART" && v == "FAILURE PREDICTED" => Span::styled(
                tr(v).into_owned(),
                Style::new().fg(th().fail).add_modifier(Modifier::BOLD),
            ),
            Ok(v) if r.name == "Throttle" && v == "none" => {
                Span::styled(tr(v).into_owned(), Style::new().fg(th().ok))
            }
            Ok(v) if r.name == "Throttle" && v != "idle" => {
                Span::styled(printable(&tr(v)), Style::new().fg(th().warn))
            }
            Err(e) => missing(e),
            Ok(_) => metric(m, Clone::clone),
        },
    }
}

/// Todas as linhas de um sensor, nada cortado: na forma comum, uma linha (nome, valor, mínimo,
/// máximo). Nome mais largo que a coluna: o nome quebra nas linhas de cima e os valores vêm na
/// última. Valor (ou causa) mais largo que as colunas de valor: vai inteiro para as linhas de
/// baixo, quebrado por palavra.
fn sensor_lines(r: &SensorRow, cw: u16, widths: &[Option<u16>], st: &State) -> Vec<Line<'static>> {
    let (value, lo, hi) = match &r.value {
        SensorValue::Num(_, unit, dec, _) => {
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
    let name = printable(&tr(&r.name));
    let name_w = usize::from(widths[0].unwrap_or(0));
    let mut out = Vec::new();
    let name_fits = Span::raw(name.as_str()).width() <= name_w;
    if !name_fits {
        out.extend(super::wrap(
            vec![Span::styled(name.clone(), Style::new().fg(th().label))],
            cw,
            0,
            2,
        ));
    }
    let shown_name = if name_fits { name } else { String::new() };
    let room = value_room(widths);
    if value.width() > room {
        if name_fits {
            out.push(Line::from(Span::styled(
                shown_name,
                Style::new().fg(th().label),
            )));
        }
        out.extend(super::wrap(vec![value], cw, 2, 2));
    } else if value.width() > usize::from(widths[1].unwrap_or(0)) {
        // Valor que não cabe na coluna dele ocupa também as de mínimo e máximo, começando onde
        // a coluna do valor começa (alinhado à direita, pularia para baixo do "Max").
        out.push(Line::from(vec![
            Span::styled(
                cell(&shown_name, name_w as u16, false),
                Style::new().fg(th().label),
            ),
            Span::raw(" "),
            Span::styled(cell(&value.content, room as u16, false), value.style),
        ]));
    } else {
        out.push(row(
            &SENSOR_COLS,
            widths,
            vec![
                dim(shown_name),
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
