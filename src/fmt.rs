//! Formatação de números para a interface, em inglês americano (ponto decimal).
//!
//! Bytes em unidade binária com os rótulos do Gerenciador de Tarefas ("GB" = 2^30), para os
//! valores baterem com ele (ver `docs/divergencias.md`). Bits de rede em unidade decimal.

/// Número com `dec` casas, com o separador decimal do idioma da interface.
pub fn num(v: f64, dec: usize) -> String {
    crate::i18n::decimal(format!("{v:.dec$}"))
}

pub fn pct(v: f64) -> String {
    format!("{}%", num(v, 1))
}

/// Bytes em unidade binária (B, KB, MB, GB, TB com base 1024).
pub fn bytes(v: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut x = v as f64;
    let mut u = 0;
    while x >= 1024.0 && u < UNITS.len() - 1 {
        x /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{v} B")
    } else {
        format!("{} {}", num(x, 1), UNITS[u])
    }
}

/// bit/s em unidade decimal (bps, Kbps, Mbps, Gbps).
pub fn bits(v: f64) -> String {
    const UNITS: [&str; 4] = ["bps", "Kbps", "Mbps", "Gbps"];
    let mut x = v;
    let mut u = 0;
    while x >= 1000.0 && u < UNITS.len() - 1 {
        x /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{x:.0} {}", UNITS[0])
    } else {
        format!("{} {}", num(x, 1), UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatos() {
        assert_eq!(num(1.25, 1), "1.2");
        assert_eq!(pct(12.34), "12.3%");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(512_110_190_592), "476.9 GB");
        assert_eq!(bits(10e9), "10.0 Gbps");
        assert_eq!(bits(999.0), "999 bps");
    }
}
