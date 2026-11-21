//! Fuzz dos parsers (Etapa 8): entradas aleatórias e mutações de entradas válidas para cada
//! parser de bytes ou de texto vindos de fora (Windows, Shared Memory e pipe do RTSS, arquivo de
//! configuração). Sem dependências: gerador próprio, roda no `cargo test` estável e na CI.
//!
//! Cada alvo é um teste no módulo do parser (`fuzz_*`), que chama `run` com sementes válidas e
//! confere as invariantes. Rodada longa (terminal comum, sem elevação):
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
}

/// Uma mutação: troca de bits, valor de borda num campo de 4 ou 8 bytes, corte, extensão,
/// cópia de um trecho para outro lugar.
pub fn mutate(r: &mut Rng, b: &mut Vec<u8>) {
    match r.below(7) {
        0 if !b.is_empty() => {
            let i = r.below(b.len());
            b[i] ^= 1 << r.below(8);
        }
        1 if !b.is_empty() => {
            let i = r.below(b.len());
            b[i] = r.next() as u8;
        }
        2 if b.len() >= 4 => {
            // Campos costumam ser alinhados: metade das vezes num múltiplo de 4.
            let mut i = r.below(b.len() - 3);
            if r.chance(2) {
                i &= !3;
            }
            b[i..i + 4].copy_from_slice(&r.edge_u32().to_le_bytes());
        }
        3 if b.len() >= 8 => {
            let i = r.below(b.len() - 7) & !7;
            let v = if r.chance(2) {
                u64::MAX - r.below(4) as u64
            } else {
                r.next()
            };
            b[i..i + 8].copy_from_slice(&v.to_le_bytes());
        }
        4 => b.truncate(r.below(b.len() + 1)),
        5 => {
            let n = r.below(64) + 1;
            b.extend(r.bytes(n));
        }
        _ if b.len() >= 2 => {
            let n = r.below(b.len().min(32)) + 1;
            let from = r.below(b.len() - n + 1);
            let to = r.below(b.len() - n + 1);
            b.copy_within(from..from + n, to);
        }
        _ => b.push(r.next() as u8),
    }
}

/// Roda `f` em `iters()` entradas: um quarto aleatórias (até `max_len` bytes), o resto
/// mutações (1 a 8) das sementes. Pânico = falha do alvo, com a entrada impressa.
pub fn run(name: &str, seeds: &[Vec<u8>], max_len: usize, mut f: impl FnMut(&mut Rng, &[u8])) {
    let s = seed();
    let mut r = Rng::new(s ^ fnv(name));
    let n = iters();
    for it in 0..n {
        let mut b = if seeds.is_empty() || r.chance(4) {
            let len = r.below(max_len + 1);
            r.bytes(len)
        } else {
            seeds[r.below(seeds.len())].clone()
        };
        for _ in 0..=r.below(8) {
            mutate(&mut r, &mut b);
        }
        b.truncate(max_len);
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| f(&mut r, &b))) {
            eprintln!(
                "fuzz {name}: falhou na iteração {it} (WINTOP_FUZZ_SEED={s}); entrada de {} bytes: {}",
                b.len(),
                hex(&b[..b.len().min(512)])
            );
            resume_unwind(p);
        }
    }
}

/// Como `run`, para parsers de texto: as entradas mutadas viram texto (UTF-8 com perdas).
pub fn run_text(name: &str, seeds: &[&str], max_len: usize, mut f: impl FnMut(&str)) {
    let seeds: Vec<Vec<u8>> = seeds.iter().map(|s| s.as_bytes().to_vec()).collect();
    run(name, &seeds, max_len, |_, b| f(&String::from_utf8_lossy(b)));
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
