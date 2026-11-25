//! Primitivos compartilhados pelas tabelas das abas Processos, GPU e Sensores: cursor de
//! seleção, estado genérico de ordenação (`TableState<S>`), colunas responsivas e células.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{State, put};
use crate::fmt::{bytes, num};
use crate::i18n::tr;
use crate::ui::theme::th;
use crate::ui::widgets::gradient;
use crate::win::printable;

// --- Cursor (seleção por posição) ---

/// Seleção de uma lista que muda a cada coleta: mantém a linha parada na posição escolhida
/// (em vez de pular atrás de um PID quando a ordenação muda) e registra o PID exibido nela.
/// As teclas só acumulam o movimento; o desenho aplica sobre a lista atual.
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

    /// Seleciona a linha `i` (clique do mouse).
    pub fn select(&mut self, i: usize) {
        self.index = i;
        self.pending = 0;
    }

    /// PID selecionado no último desenho.
    #[cfg(test)]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// O que `resolve` selecionaria, sem mudar nada (para montar o detalhe antes da tabela).
    pub(in crate::ui) fn peek(&self, pids: &[u32]) -> Option<usize> {
        let last = pids.len().checked_sub(1)?;
        let base = self.index.min(last);
        Some(
            (base as isize)
                .saturating_add(self.pending)
                .clamp(0, last as isize) as usize,
        )
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
        let base = self.index.min(last);
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

/// Coluna ordenável de uma tabela (Processos ou GPU).
pub trait SortCol: Copy + Eq + Default {
    fn index(self) -> usize;
    fn from_index(i: usize) -> Option<Self>;
    fn next(self) -> Self;
    fn prev(self) -> Self;
    /// Ordem natural: texto e PID crescentes; números decrescentes (o maior no topo).
    fn natural_desc(self) -> bool;
}

/// Cursor, coluna de ordenação e direção de uma tabela interativa.
#[derive(Clone, Copy, Debug, Default)]
pub struct TableState<S> {
    pub cursor: Cursor,
    pub sort: S,
    /// Inverte a ordem natural da coluna (tecla `i`).
    pub reverse: bool,
}

impl<S: SortCol> TableState<S> {
    pub fn desc(&self) -> bool {
        self.sort.natural_desc() != self.reverse
    }

    pub fn prev_sort(&mut self) {
        self.sort = self.sort.prev();
    }

    pub fn next_sort(&mut self) {
        self.sort = self.sort.next();
    }

    pub fn invert(&mut self) {
        self.reverse = !self.reverse;
    }

    /// Clique na coluna da ordem inverte; em outra, ordena por ela.
    pub fn click_col(&mut self, col: usize) {
        if let Some(s) = S::from_index(col) {
            if s == self.sort {
                self.reverse = !self.reverse;
            } else {
                self.sort = s;
                self.reverse = false;
            }
        }
    }
}

// --- Tabela ---

/// Coluna: título, largura, alinhada à direita, e ordem de saída quando falta largura
/// (0 = nunca sai; maior sai primeiro). A coluna flexível (`flex`) fica com o que sobra.
pub(super) struct Col {
    pub title: &'static str,
    pub w: u16,
    pub right: bool,
    pub drop: u8,
    pub flex: bool,
}

pub(super) const fn col(title: &'static str, w: u16, right: bool, drop: u8) -> Col {
    Col {
        title,
        w,
        right,
        drop,
        flex: false,
    }
}

pub(super) const fn flex(title: &'static str, min: u16) -> Col {
    Col {
        title,
        w: min,
        right: false,
        drop: 0,
        flex: true,
    }
}

/// Largura máxima da coluna de nomes.
const FLEX_MAX: u16 = 60;

/// Larguras das colunas que cabem em `avail` (`None` = coluna fora). Uma coluna de folga
/// entre cada par.
pub(super) fn fit(cols: &[Col], avail: u16) -> Vec<Option<u16>> {
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
    // A 1ª coluna (PID) não cresce (encostada na borda); as outras ganham no máximo
    // `EXTRA_MAX` cada (sem vãos largos entre os números); o que sobrar fica à direita.
    const EXTRA_MAX: u16 = 4;
    let others = cols
        .iter()
        .zip(&on)
        .enumerate()
        .filter(|(i, (c, o))| *i > 0 && !c.flex && **o)
        .count() as u16;
    let each = rest.checked_div(others).unwrap_or(0).min(EXTRA_MAX);
    cols.iter()
        .zip(&on)
        .enumerate()
        .map(|(i, (c, o))| {
            o.then(|| {
                if c.flex {
                    c.w + flex_extra
                } else if i == 0 {
                    c.w
                } else {
                    c.w + each
                }
            })
        })
        .collect()
}

/// Texto que cabe em `w` colunas do terminal (corta no fim com `…`) e alinhado.
pub(super) fn cell(s: &str, w: u16, right: bool) -> String {
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
pub(super) fn row(
    cols: &[Col],
    widths: &[Option<u16>],
    cells: Vec<(String, Style)>,
) -> Line<'static> {
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

pub(super) fn header(
    cols: &[Col],
    widths: &[Option<u16>],
    sorted: Option<(usize, bool)>,
) -> Line<'static> {
    let cells = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut t = tr(c.title).into_owned();
            let mut style = Style::new().fg(th().label).add_modifier(Modifier::BOLD);
            if let Some((s, desc)) = sorted
                && s == i
            {
                t.push(if desc { '▾' } else { '▴' });
                style = Style::new().fg(th().accent).add_modifier(Modifier::BOLD);
            }
            (t, style)
        })
        .collect();
    row(cols, widths, cells)
}

/// Colunas visíveis na tela: (início, fim exclusivo, índice da coluna), a partir de `x`.
pub(super) fn col_ranges(x: u16, widths: &[Option<u16>]) -> Vec<(u16, u16, usize)> {
    let mut out = Vec::new();
    let mut at = x;
    for (i, w) in widths.iter().enumerate() {
        let Some(w) = w else { continue };
        if !out.is_empty() {
            at += 1;
        }
        out.push((at, at + w, i));
        at += w;
    }
    out
}

pub(super) fn plain(s: String) -> (String, Style) {
    (s, Style::new().fg(th().text))
}

pub(super) fn dim(s: String) -> (String, Style) {
    (s, Style::new().fg(th().label))
}

/// Porcentagem com a cor pela intensidade (cinza no zero).
pub(super) fn pct_cell(v: f64) -> (String, Style) {
    if v < 0.05 {
        return dim(num(0.0, 1));
    }
    // Texto a partir de 40% do gradiente: a base (índigo escuro no tema padrão) sumiria no
    // fundo e na linha selecionada.
    (
        num(v, 1),
        Style::new().fg(gradient(0.4 + 0.6 * (v / 100.0).clamp(0.0, 1.0))),
    )
}

pub(super) fn rate(v: f64) -> (String, Style) {
    if v < 1.0 {
        dim("0".into())
    } else {
        plain(format!("{}/s", bytes(v as u64)))
    }
}

/// Mensagem simples (espera, falha) dentro da área.
pub(super) fn note(frame: &mut Frame<'_>, area: Rect, span: Span<'static>) {
    put(frame, Paragraph::new(Line::from(span)), area, area);
}

/// Texto da borda direita quando há filtro aplicado ou em digitação (o texto do usuário não
/// passa por `tr`).
pub(super) fn filter_header(shown: usize, total: usize, filter: &str, filtering: bool) -> String {
    format!(
        "{shown} {} {total} · {} {}{}",
        tr("of"),
        tr("filter:"),
        printable(filter),
        if filtering { "_" } else { "" }
    )
}

pub(super) fn game_pid(st: &State) -> Option<u32> {
    st.fps
        .as_ref()
        .and_then(|f| f.game.as_ref().ok())
        .map(|g| g.pid)
}
