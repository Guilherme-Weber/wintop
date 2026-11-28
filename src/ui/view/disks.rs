//! Caixa de discos: tabela com modelo, barramento, tipo, capacidade, flags S/P, atividade,
//! resposta, leitura/escrita, IOPS/fila em telas largas, temperatura e notas de causa.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::state::State;
use super::table;
use super::{
    boxed, compact_bytes, empty_box, faded, failed_span, label, line, metric, metric_styled, put,
    temp_color, wrap,
};
use crate::fmt::num;
use crate::i18n::tr;
use crate::model::{Disk, Missing};
use crate::ui::theme::th;
use crate::ui::widgets::{Graph, GraphColor};
use crate::win::printable;
// --- Discos ---

/// Ordem de exibição: discos com letra e o virtual; membros do pool depois (recolhidos se
/// `fold`). Devolve (&disco, é membro).
pub(super) fn disk_order(st: &State) -> Vec<(&Disk, bool)> {
    let Some(Ok(list)) = st.fast.as_ref().map(|f| &f.disks) else {
        return Vec::new();
    };
    let mut out: Vec<(&Disk, bool)> = list
        .iter()
        .filter(|d| d.pool_member != Ok(true))
        .map(|d| (d, false))
        .collect();
    out.sort_by_key(|(d, _)| d.letters.first().map_or("~", String::as_str));
    if !pool_folded(st, list.len()) {
        out.extend(
            list.iter()
                .filter(|d| d.pool_member == Ok(true))
                .map(|d| (d, true)),
        );
    }
    out
}

/// Recolhe os membros do pool por escolha (`d`) ou, no automático, se não couberem.
pub(super) fn pool_folded(st: &State, total: usize) -> bool {
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
pub(super) fn same_disk(t: &crate::model::DiskTemp, d: &Disk) -> bool {
    t.number == d.number
        && match (&t.model, &d.model) {
            (Some(a), Ok(b)) => a == b,
            _ => true,
        }
}

/// Notas embaixo da tabela de discos, inteiras (as colunas não têm espaço para causas): falhas
/// de metadados e de temperatura em vermelho, estados neutros longos em cinza, quebradas em
/// `width` colunas. Só dos discos exibidos.
pub(super) fn disk_notes(st: &State, width: u16) -> Vec<Line<'static>> {
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
    let all: &[Disk] = st
        .fast
        .as_ref()
        .and_then(|f| f.disks.as_ref().ok())
        .map_or(&[], Vec::as_slice);
    for d in all {
        let name = disk_label(d);
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
                // "asleep" e "repouso" cabem na coluna da capacidade (7 cols); textos mais longos
                // vão para a nota em vez de estourar a coluna.
                Some(m @ Missing::Neutral(n)) if tr(n).chars().count() > 7 => {
                    note(&name, what, m);
                }
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
        if let Some(t) = disk_temp(st, d)
            && let Err(m) = &t.celsius
            && (matches!(m, Missing::Fail(_)) || tr(&m.to_string()).chars().count() > DISK_TEMP_W)
        {
            note(&name, "temperature", m);
        }
        // Previsão de falha do SMART: aviso em vermelho (não é falha da fonte, é do disco).
        match disk_temp(st, d).map(|t| &t.health) {
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

pub(super) fn draw_disks(
    frame: &mut Frame<'_>,
    area: Rect,
    st: &State,
    order: &[(&Disk, bool)],
    precomputed_notes: Option<(u16, Vec<Line<'static>>)>,
) {
    let disks = st.fast.as_ref().map(|f| &f.disks);
    let Some(Ok(all)) = disks else {
        empty_box(frame, area, "disks", disks);
        return;
    };
    let members = all.iter().filter(|d| d.pool_member == Ok(true)).count();
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
    // Atividade (5), resposta (8), leitura e escrita (10 cada) e temperatura (1 ou 2 + "! failed").
    // Depois do nome: barramento (7), tipo (4), capacidade (7 + espaço), flags (4) e 2 espaços.
    // Sem largura para tudo (o conhost abre com 120 colunas), saem primeiro barramento, tipo e
    // resposta (as falhas deles continuam nas notas); leitura, escrita e temperatura ficam.
    let compact = inner.width < 12 + 25 + 43;
    // Ainda mais estreito (o piso de 80 colunas): as flags S/P vão junto do nome, para a
    // temperatura não ser cortada.
    let tiny = inner.width < 12 + 14 + 35;
    // Com folga (nome inteiro e a atividade), IOPS e fila entram depois da escrita.
    let wide = inner.width >= 26 + 25 + 54 + 12;
    let (left_w, info_full): (u16, u16) = if tiny {
        (8, 34)
    } else if compact {
        (14, 35)
    } else if wide {
        (25, 54)
    } else {
        (25, 43)
    };
    // O nome encolhe antes de tudo (até 9 cols no piso 80×24 para "! failed" de temperatura
    // caber inteiro); a atividade fica com a sobra. As colunas nunca se sobrepõem.
    let name_min = if tiny { 9 } else { 12 };
    let name_w = inner
        .width
        .saturating_sub(left_w + info_full + 4)
        .clamp(name_min, if wide { 34 } else { 26 });
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
        format!("{:>7} ", "cap")
    } else if compact {
        format!("{:>7} {:<4}  ", "cap", "flg")
    } else {
        format!("{:<7}{:<4}{:>7} {:<4}  ", "bus", "typ", "cap", "flg")
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
    let temp_gap = if tiny { " " } else { "  " };
    line(
        frame,
        vec![label(&if compact {
            format!("{:>5}{:>10}{:>10}{temp_gap}temp", "act", "read", "write")
        } else if wide {
            format!(
                "{:>5}{:>8}{:>10}{:>10}{:>7}{:>4}  temp",
                "act", "resp", "read", "write", "iops", "q"
            )
        } else {
            format!(
                "{:>5}{:>8}{:>10}{:>10}  temp",
                "act", "resp", "read", "write"
            )
        })],
        info_x,
        inner.y,
        info_w,
        inner,
    );
    // As notas (causas) têm linhas reservadas no fim da caixa: os discos que não couberem
    // entram no "+N more", nunca as causas.
    let notes = match precomputed_notes {
        Some((w, n)) if w == inner.width => n,
        _ => disk_notes(st, inner.width),
    };
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
        let name_cell = table::cell(&name, name_w.saturating_sub(1), false);
        // Sem valor: "!" para falha (a causa está nas notas), "?" para estado neutro; a
        // capacidade de um HDD em repouso diz "asleep" se couber em 6 cols no idioma atual.
        let bus = d
            .bus
            .as_ref()
            .map_or_else(missing_mark, ToString::to_string);
        let typ = match &d.ssd {
            Ok(s) => if *s { "SSD" } else { "HDD" }.to_string(),
            // "n/a" (pendrive/ponte USB sem a consulta) cabe na coluna.
            Err(Missing::Neutral(n)) if tr(n).chars().count() <= 3 => tr(n).into_owned(),
            Err(e) => missing_mark(e),
        };
        let cap = match &d.capacity {
            Ok(c) => compact_bytes(*c),
            Err(Missing::Neutral(n)) if tr(n).chars().count() <= 7 => tr(n).into_owned(),
            Err(e) => missing_mark(e),
        };
        let flags = format!(
            "{}{}",
            if d.system { "S " } else { "" },
            if d.pagefile { "P" } else { "" }
        );
        let mut left_spans = Vec::with_capacity(4);
        if !member && let Some(rest) = name_cell.strip_prefix(&letters) {
            left_spans.push(Span::styled(
                letters,
                Style::new().fg(th().title).add_modifier(Modifier::BOLD),
            ));
            left_spans.push(Span::styled(format!("{rest} "), Style::new().fg(th().text)));
        } else {
            left_spans.push(Span::styled(
                format!("{name_cell} "),
                Style::new().fg(if *member { th().label } else { th().text }),
            ));
        }
        if tiny {
            left_spans.push(label(&format!("{cap:>7} ")));
        } else if compact {
            left_spans.push(label(&format!("{cap:>7} ")));
            left_spans.push(Span::styled(
                format!("{flags:<4}  "),
                Style::new().fg(th().accent),
            ));
        } else {
            left_spans.push(label(&format!("{bus:<7}{typ:<4}{cap:>7} ")));
            left_spans.push(Span::styled(
                format!("{flags:<4}  "),
                Style::new().fg(th().accent),
            ));
        }
        line(frame, left_spans, inner.x, y, name_w + left_w, inner);
        if spark_w > 0
            && let Some(s) = st.disks.get(&d.number)
        {
            frame.render_widget(
                Graph::new(s.values(), GraphColor::Value).with_baseline(th().dim),
                Rect::new(inner.x + name_w + left_w, y, spark_w, 1).intersection(inner),
            );
        }
        // A causa não cabe na coluna: vai inteira para as notas embaixo da tabela.
        let t = match disk_temp(st, d).map(|t| &t.celsius) {
            Some(Err(Missing::Fail(_))) => failed_span(),
            Some(Err(m)) if tr(&m.to_string()).chars().count() > DISK_TEMP_W => label("(below)"),
            Some(c) => metric_styled(
                c,
                c.as_ref()
                    .map_or(th().text, |v| temp_color(f64::from(*v), 70.0)),
                |c| format!("{c} °C"),
            ),
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
            25
        } else if wide {
            44
        } else {
            33
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
                Span::raw(temp_gap),
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
                        format!(" {:>9}", format!("{}/s", compact_bytes(*v as u64)))
                    }),
                    d.read.as_ref().is_ok_and(|v| *v < 1.0),
                ),
                faded(
                    metric(&d.write, |v| {
                        format!(" {:>9}", format!("{}/s", compact_bytes(*v as u64)))
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
                Span::raw(temp_gap),
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
