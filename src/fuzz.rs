//! Fuzz dos parsers (Etapa 8): entradas aleatórias e mutações de entradas válidas para cada
//! parser de bytes ou de texto vindos de fora (Windows, Shared Memory e pipe do RTSS, arquivo de
//! configuração). Sem dependências: gerador próprio, roda no `cargo test` estável e na CI.
//!
//! Cada alvo é um teste no módulo do parser (`fuzz_*`), que chama `run` (ou `run_text`, com
//! mutações de texto) com sementes válidas e confere as invariantes. Rodada longa (terminal
//! comum, sem elevação):
//!
//! ```text
//! $env:WINTOP_FUZZ_ITERS = 2000000; cargo test --profile fuzz fuzz_
//! ```
//!
//! `WINTOP_FUZZ_SEED` fixa a semente (a padrão é fixa: a CI é reproduzível). Uma falha imprime
//! o alvo, a semente, a iteração e a entrada em hexadecimal.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

/// Iterações por alvo: `WINTOP_FUZZ_ITERS`, senão 20 mil (rápido no perfil de teste).
pub fn iters() -> u64 {
    std::env::var("WINTOP_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000)
}

fn seed() -> u64 {
    std::env::var("WINTOP_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5EED_F022)
}

/// SplitMix64: rápido, sem estado global, reproduzível pela semente.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Número em `0..n` (`n` > 0).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }

    /// Valor de borda de 16 bits (contagens e temperaturas `i16`).
    pub fn edge_u16(&mut self) -> u16 {
        const V: [u16; 10] = [0, 1, 2, 0x7F, 0x80, 0xFF, 0x100, 0x7FFF, 0x8000, 0xFFFF];
        if self.chance(4) {
            self.next() as u16
        } else {
            V[self.below(V.len())]
        }
    }

    /// Valor "interessante" de 32 bits: bordas de sinal e de tamanho, que pegam overflow e
    /// contagens corrompidas mais cedo que valores uniformes.
    pub fn edge_u32(&mut self) -> u32 {
        const V: [u32; 14] = [
            0,
            1,
            2,
            7,
            8,
            0xFF,
            0x100,
            0x7FFF,
            0x8000,
            0xFFFF,
            0x1_0000,
            0x7FFF_FFFF,
            0x8000_0000,
            0xFFFF_FFFF,
        ];
        if self.chance(4) {
            self.next() as u32
        } else {
            V[self.below(V.len())]
        }
    }

    /// Valor de borda de 64 bits (somas que estouram, truncamento `as u32`).
    pub fn edge_u64(&mut self) -> u64 {
        const V: [u64; 10] = [
            0,
            1,
            0x8000_0000,
            0xFFFF_FFFF,
            1 << 32,
            i64::MAX as u64,
            1 << 63,
            u64::MAX - 1,
            u64::MAX,
            10_000_000,
        ];
        if self.chance(4) {
            self.next()
        } else {
            V[self.below(V.len())]
        }
    }

    /// Posição de um campo de `w` bytes: metade das vezes alinhada (todas as posições alinhadas
    /// com a mesma chance, inclusive a última), metade em qualquer lugar.
    fn field(&mut self, len: usize, w: usize) -> Option<usize> {
        if len < w {
            None
        } else if self.chance(2) {
            Some(self.below(len / w) * w)
        } else {
            Some(self.below(len - w + 1))
        }
    }
}

/// Uma mutação: troca de bits, valor de borda num campo de 2, 4 ou 8 bytes, soma ou subtração
/// pequena num campo (offsets e contagens um a mais ou a menos), cópia de um trecho, corte,
/// extensão ou remoção de bytes.
pub fn mutate(r: &mut Rng, b: &mut Vec<u8>) {
    match r.below(12) {
        0 | 1 if !b.is_empty() => {
            let i = r.below(b.len());
            b[i] ^= 1 << r.below(8);
        }
        2 if !b.is_empty() => {
            let i = r.below(b.len());
            b[i] = r.next() as u8;
        }
        3 => {
            if let Some(i) = r.field(b.len(), 2) {
                b[i..i + 2].copy_from_slice(&r.edge_u16().to_le_bytes());
            }
        }
        4 | 5 => {
            if let Some(i) = r.field(b.len(), 4) {
                b[i..i + 4].copy_from_slice(&r.edge_u32().to_le_bytes());
            }
        }
        6 => {
            if let Some(i) = r.field(b.len(), 8) {
                b[i..i + 8].copy_from_slice(&r.edge_u64().to_le_bytes());
            }
        }
        7 | 8 => {
            // ±1..16 num u32 ou u64 que já está lá.
            let w = if r.chance(2) { 4 } else { 8 };
            if let Some(i) = r.field(b.len(), w) {
                let d = r.below(16) as u64 + 1;
                let mut x = [0u8; 8];
                x[..w].copy_from_slice(&b[i..i + w]);
                let v = u64::from_le_bytes(x);
                let v = if r.chance(2) {
                    v.wrapping_add(d)
                } else {
                    v.wrapping_sub(d)
                };
                b[i..i + w].copy_from_slice(&v.to_le_bytes()[..w]);
            }
        }
        9 if b.len() >= 2 => {
            let n = r.below(b.len().min(32)) + 1;
            let from = r.below(b.len() - n + 1);
            let to = r.below(b.len() - n + 1);
            b.copy_within(from..from + n, to);
        }
        10 => {
            // Corte (raro: invalida a estrutura inteira) ou remoção de um trecho no meio.
            if r.chance(3) {
                b.truncate(r.below(b.len() + 1));
            } else if !b.is_empty() {
                let at = r.below(b.len());
                let n = (r.below(16) + 1).min(b.len() - at);
                b.drain(at..at + n);
            }
        }
        _ => {
            let n = r.below(64) + 1;
            b.extend(r.bytes(n));
        }
    }
}

/// Mutações seguidas: 1 na metade das vezes, raramente mais de 4 (entradas perto das sementes
/// passam dos cabeçalhos e chegam aos campos de dentro).
fn mutations(r: &mut Rng) -> usize {
    let mut n = 1;
    while n < 8 && r.chance(2) {
        n += 1;
    }
    n
}

/// Roda `f` em `iters()` entradas: um oitavo aleatórias (até `max_len` bytes), o resto
/// mutações das sementes. Pânico = falha do alvo, com a entrada impressa.
pub fn run(name: &str, seeds: &[Vec<u8>], max_len: usize, mut f: impl FnMut(&mut Rng, &[u8])) {
    let s = seed();
    let mut r = Rng::new(s ^ fnv(name));
    for it in 0..iters() {
        let mut b = if seeds.is_empty() || r.chance(8) {
            let len = r.below(max_len + 1);
            r.bytes(len)
        } else {
            seeds[r.below(seeds.len())].clone()
        };
        for _ in 0..mutations(&mut r) {
            mutate(&mut r, &mut b);
        }
        b.truncate(max_len);
        check(name, s, it, &b, || f(&mut r, &b));
    }
}

/// Pedaços que mudam a estrutura de um TOML (e os nomes que o wintop lê).
const TOKENS: [&str; 26] = [
    "=",
    "[",
    "]",
    "[[",
    "\"",
    "'",
    "'''",
    "\"\"\"",
    "#",
    "\n",
    ",",
    "{",
    "}",
    ".",
    "\\u0000",
    "\\",
    "-1",
    "1e999",
    "99999999999999999999",
    "true",
    "[rtss]",
    "[colors]",
    "interval_ms",
    "theme",
    "fold_pool",
    "ignored",
];

/// Mutação de texto: tokens da gramática, trechos removidos, linhas duplicadas, trocadas ou
/// removidas, caractere trocado por ASCII imprimível; um oitavo das vezes, uma mutação binária
/// (UTF-8 com perdas).
fn mutate_text(r: &mut Rng, t: &mut String) {
    let at = |r: &mut Rng, t: &str| {
        let mut i = r.below(t.len() + 1);
        while !t.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    match r.below(8) {
        0 | 1 => {
            let i = at(r, t);
            t.insert_str(i, TOKENS[r.below(TOKENS.len())]);
        }
        2 => {
            let (a, b) = (at(r, t), at(r, t));
            t.replace_range(a.min(b)..a.max(b), "");
        }
        3 | 4 => {
            let mut lines: Vec<&str> = t.split('\n').collect();
            let n = lines.len();
            let (i, j) = (r.below(n), r.below(n));
            match r.below(3) {
                0 => lines.swap(i, j),
                1 => lines.insert(j, lines[i]),
                _ if n > 1 => {
                    lines.remove(i);
                }
                _ => {}
            }
            *t = lines.join("\n");
        }
        5 | 6 if !t.is_empty() => {
            let i = at(r, t);
            let end = t[i..].chars().next().map_or(i, |c| i + c.len_utf8());
            let c = char::from(b' ' + r.below(95) as u8);
            t.replace_range(i..end, c.encode_utf8(&mut [0; 4]));
        }
        _ => {
            let mut b = std::mem::take(t).into_bytes();
            mutate(r, &mut b);
            *t = String::from_utf8_lossy(&b).into_owned();
        }
    }
}

/// Como `run`, para parsers de texto, com mutações de texto (`mutate_text`).
pub fn run_text(name: &str, seeds: &[&str], max_len: usize, mut f: impl FnMut(&str)) {
    let s = seed();
    let mut r = Rng::new(s ^ fnv(name));
    for it in 0..iters() {
        let mut t = seeds[r.below(seeds.len())].to_string();
        for _ in 0..mutations(&mut r) {
            mutate_text(&mut r, &mut t);
        }
        if t.len() > max_len {
            let mut i = max_len;
            while !t.is_char_boundary(i) {
                i -= 1;
            }
            t.truncate(i);
        }
        check(name, s, it, t.as_bytes(), || f(&t));
    }
}

/// Roda o alvo; num pânico, imprime o que reproduz a falha e repassa o pânico.
fn check(name: &str, seed: u64, it: u64, input: &[u8], f: impl FnOnce()) {
    if let Err(p) = catch_unwind(AssertUnwindSafe(f)) {
        eprintln!(
            "fuzz {name}: falhou na iteração {it} (WINTOP_FUZZ_SEED={seed}); entrada de {} bytes: {}",
            input.len(),
            hex(&input[..input.len().min(512)])
        );
        resume_unwind(p);
    }
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xCBF2_9CE4_8422_2325, |h, c| {
        (h ^ u64::from(c)).wrapping_mul(0x100_0000_01B3)
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gerador_reproduzivel_e_mutacao_sem_panico() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        assert!((0..100).all(|_| a.next() == b.next()));
        let mut r = Rng::new(1);
        let mut v = Vec::new();
        for _ in 0..100_000 {
            mutate(&mut r, &mut v);
            v.truncate(4096);
        }
    }

    #[test]
    #[should_panic(expected = "alvo")]
    fn panico_do_alvo_sobe() {
        run("teste", &[vec![1, 2, 3]], 16, |_, _| panic!("alvo"));
    }
}
