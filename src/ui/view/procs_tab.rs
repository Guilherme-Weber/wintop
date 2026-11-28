//! Aba Processos: tabela no estilo do btop (ordem por coluna com ←→, `i` inverte, `/` filtra,
//! `e` mostra árvore pai → filhos), com dados de `SystemProcessInformation`.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Paragraph;

use super::table::{
    Col, SortCol, col, col_ranges, dim, filter_header, fit, flex, game_pid, header, note, pct_cell,
    plain, rate, row,
};
use super::{State, boxed_ready, empty_box, label, put};
use crate::fmt::bytes;
use crate::i18n::tr;
use crate::model::Proc;
use crate::ui::theme::th;
use crate::win::printable;

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
}

impl SortCol for ProcSort {
    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    fn from_index(i: usize) -> Option<Self> {
        Self::ALL.get(i).copied()
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

/// Linhas da aba Processos: a lista plana ordenada, ou a árvore (tecla `e`) com o prefixo de
/// cada nome. Com filtro, sempre plana (a árvore esconderia quem casa sob quem não casa).
pub(in crate::ui) fn proc_rows<'a>(st: &State, list: &'a [Proc]) -> Vec<(&'a Proc, String)> {
    if !st.proc_tree || !st.filter.is_empty() {
        return sorted_procs(list, st.proc_table.sort, st.proc_table.reverse, &st.filter)
            .into_iter()
            .map(|p| (p, String::new()))
            .collect();
    }
    tree(list, st.proc_table.sort, st.proc_table.reverse)
}

/// Árvore pai → filhos; irmãos na ordem da coluna. Um pai só vale se foi criado antes do filho
/// (o PID do pai pode ter sido reaproveitado por um processo mais novo); sem pai válido, o
/// processo é raiz.
pub(in crate::ui) fn tree(list: &[Proc], sort: ProcSort, reverse: bool) -> Vec<(&Proc, String)> {
    let ordered = sorted_procs(list, sort, reverse, "");
    let by_pid: HashMap<u32, &Proc> = list.iter().map(|p| (p.pid, p)).collect();
    let parent_of = |p: &Proc| -> Option<u32> {
        by_pid
            .get(&p.parent)
            .filter(|q| q.pid != p.pid && q.create <= p.create)
            .map(|q| q.pid)
    };
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    let mut roots = Vec::new();
    for p in &ordered {
        match parent_of(p) {
            Some(pp) => children.entry(pp).or_default().push(p),
            None => roots.push(*p),
        }
    }
    let mut out = Vec::with_capacity(list.len());
    // Pilha explícita (árvores fundas não estouram a pilha): (processo, recuo dos ancestrais,
    // é o último irmão).
    let mut stack: Vec<(&Proc, String, bool, bool)> = roots
        .iter()
        .rev()
        .map(|p| (*p, String::new(), false, true))
        .collect();
    while let Some((p, indent, last, root)) = stack.pop() {
        let prefix = if root {
            String::new()
        } else {
            format!("{indent}{}", if last { "└─ " } else { "├─ " })
        };
        out.push((p, prefix));
        if let Some(kids) = children.get(&p.pid) {
            let child_indent = if root {
                String::new()
            } else {
                format!("{indent}{}", if last { "   " } else { "│  " })
            };
            for (k, c) in kids.iter().enumerate().rev() {
                stack.push((*c, child_indent.clone(), k + 1 == kids.len(), false));
            }
        }
    }
    out
}

pub(super) fn draw_processes(frame: &mut Frame<'_>, area: Rect, st: &mut State) {
    let Some(Ok(list)) = &st.procs else {
        empty_box(frame, area, "processes", st.procs.as_ref());
        return;
    };
    let rows = proc_rows(st, list);
    let desc = st.proc_table.desc();
    let right = if !st.filter.is_empty() || st.is_filtering() {
        filter_header(rows.len(), list.len(), &st.filter, st.is_filtering())
    } else {
        tr(&format!(
            "{} processes{}",
            list.len(),
            if st.proc_tree { " · tree" } else { "" }
        ))
        .into_owned()
    };
    let block = boxed_ready("processes", &right);
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
            Some((st.proc_table.sort.index(), desc)),
        )),
        head,
        inner,
    );
    let body_h = usize::from(inner.height - 1);
    let pids: Vec<u32> = rows.iter().map(|(p, _)| p.pid).collect();
    let (sel, offset) = st.proc_table.cursor.resolve(&pids, body_h);
    st.hits.list = Some(super::ListHit {
        header_y: head.y,
        cols: col_ranges(inner.x, &widths),
        body: Rect::new(inner.x, inner.y + 1, inner.width, body_h as u16),
        offset,
        len: rows.len(),
    });
    if rows.is_empty() {
        note(
            frame,
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
            label("no process matches the filter"),
        );
        return;
    }
    let game = game_pid(st);
    for (i, (p, prefix)) in rows.iter().enumerate().skip(offset).take(body_h) {
        let name_style = if Some(p.pid) == game {
            Style::new().fg(th().game).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(th().text)
        };
        let mut line = row(
            &PROC_COLS,
            &widths,
            vec![
                dim(p.pid.to_string()),
                (format!("{prefix}{}", printable(&p.name)), name_style),
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
            // Fundo e negrito; em NO_COLOR/mono usa REVERSED para não colidir com o negrito do jogo.
            let mut style = Style::new().bg(th().selected).add_modifier(Modifier::BOLD);
            if crate::ui::theme::no_color() || th().name == "mono" {
                style = style.add_modifier(Modifier::REVERSED);
            }
            line = line.style(style);
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
